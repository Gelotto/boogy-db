use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::page::{Page, PAGE_BRANCH, PAGE_HEADER_SIZE, PAGE_LEAF, PAGE_SIZE};
use crate::row;

/// Checksum occupies the last 4 bytes of each page.
const CHECKSUM_SIZE: usize = 4;

/// A B+ tree rooted at a given page number.
pub struct BTree<'a> {
    file: &'a mut PageFile,
    root: u32,
}

impl<'a> BTree<'a> {
    pub fn new(file: &'a mut PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    /// Create a new empty B+ tree (single empty leaf page).
    pub fn create(file: &mut PageFile) -> Result<u32> {
        let page_no = file.allocate_page()?;
        let page = Page::new_leaf();
        file.put_page(page_no, page);
        Ok(page_no)
    }

    /// Insert a row. Returns the (possibly new) root page number.
    pub fn insert(&mut self, id: &str, row_data: &[u8]) -> Result<u32> {
        let result = self.insert_recursive(self.root, id, row_data)?;
        match result {
            InsertResult::Fit => Ok(self.root),
            InsertResult::Split {
                new_page,
                separator,
            } => {
                let new_root = self.file.allocate_page()?;
                let mut root_page = Page::new_branch();
                write_branch_entry(&mut root_page, 0, self.root, &separator);
                set_branch_child(&mut root_page, 1, new_page);
                root_page.set_num_rows(1);
                root_page.update_checksum();
                self.file.put_page(new_root, root_page);
                self.root = new_root;
                Ok(self.root)
            }
        }
    }

    /// Search for a row by _id. Returns the raw row bytes if found.
    pub fn search(&mut self, id: &str) -> Result<Option<Vec<u8>>> {
        self.search_recursive(self.root, id)
    }

    /// Delete a row by _id. Returns true if the row existed.
    pub fn delete(&mut self, id: &str) -> Result<bool> {
        self.delete_recursive(self.root, id)
    }

    /// Iterate all rows in key order. Returns (id, row_bytes) pairs.
    pub fn scan_all(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        let first_leaf = self.find_leftmost_leaf(self.root)?;
        let mut results = Vec::new();
        let mut current = first_leaf;
        loop {
            let page = self.file.read_page(current)?.clone();
            let num_rows = page.num_rows() as usize;
            for i in 0..num_rows {
                let (start, end) = row_bounds(&page, i, num_rows);
                if start < end && end <= PAGE_SIZE {
                    let data = &page.data[start..end];
                    if let Ok(id) = row::extract_id(data) {
                        results.push((id.to_string(), data.to_vec()));
                    }
                }
            }
            let next = page.next_leaf();
            if next == 0 {
                break;
            }
            current = next;
        }
        Ok(results)
    }

    // --- Internal methods ---

    fn find_leftmost_leaf(&mut self, page_no: u32) -> Result<u32> {
        let page = self.file.read_page(page_no)?.clone();
        if page.is_leaf() {
            Ok(page_no)
        } else {
            let child = get_branch_child(&page, 0);
            self.find_leftmost_leaf(child)
        }
    }

    fn insert_recursive(
        &mut self,
        page_no: u32,
        id: &str,
        row_data: &[u8],
    ) -> Result<InsertResult> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            self.insert_into_leaf(page_no, &page, id, row_data)
        } else {
            let (child_idx, child_page_no) = find_child(&page, id);
            let result = self.insert_recursive(child_page_no, id, row_data)?;

            match result {
                InsertResult::Fit => Ok(InsertResult::Fit),
                InsertResult::Split {
                    new_page,
                    separator,
                } => self.insert_into_branch(page_no, child_idx, &separator, new_page),
            }
        }
    }

    fn insert_into_leaf(
        &mut self,
        page_no: u32,
        page: &Page,
        id: &str,
        row_data: &[u8],
    ) -> Result<InsertResult> {
        let num_rows = page.num_rows() as usize;

        // Binary search for insertion point and duplicate check.
        let (pos, found) = find_insertion_point(page, id)?;
        if found {
            return Err(BoogyError::DuplicateKey(id.to_string()));
        }

        // Check if the new row fits in the current page.
        // After insert we need:  header + (num_rows+1)*2 offsets + existing_data + new_row + checksum
        let offset_array_start = PAGE_HEADER_SIZE + num_rows * 2;
        let current_free = page.free_space_offset() as usize;
        // existing_data_size: bytes of row data currently stored
        // (row data lives between the old offset array end and free_space_offset)
        let existing_data_size = if current_free > offset_array_start {
            current_free - offset_array_start
        } else {
            0
        };
        let needed = PAGE_HEADER_SIZE
            + (num_rows + 1) * 2
            + existing_data_size
            + row_data.len()
            + CHECKSUM_SIZE;

        if needed <= PAGE_SIZE {
            // --- Fits: in-place insert ---
            // Take a snapshot of the old page data so we can read from it while
            // writing to the mutable page. This avoids per-row heap allocation:
            // one stack-sized copy of 4 KiB instead of N String+Vec pairs.
            let snapshot = page.data;

            let page = self.file.write_page(page_no)?;
            write_leaf_with_insert(page, &snapshot, num_rows, pos, row_data);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // --- Split ---
            // Snapshot the page, then build two halves directly.
            let snapshot = page.data;
            let next_leaf = page.next_leaf();
            let prev_leaf = page.prev_leaf();
            let total = num_rows + 1;
            let mid = total / 2;

            // Allocate right page first so we have its page_no for linking.
            let new_page_no = self.file.allocate_page()?;

            // Extract separator: the _id of the first row in the right half.
            // We need to figure out which original row index that corresponds to.
            let separator = extract_id_at_virtual_pos(&snapshot, num_rows, pos, row_data, mid)?;

            // Write left half (indices 0..mid).
            let left_page = self.file.write_page(page_no)?;
            write_leaf_range(left_page, &snapshot, num_rows, pos, row_data, 0, mid);
            left_page.set_next_leaf(new_page_no);
            left_page.set_prev_leaf(prev_leaf);
            left_page.update_checksum();

            // Write right half (indices mid..total).
            let mut right_page = Page::new_leaf();
            write_leaf_range(
                &mut right_page,
                &snapshot,
                num_rows,
                pos,
                row_data,
                mid,
                total,
            );
            right_page.set_prev_leaf(page_no);
            right_page.set_next_leaf(next_leaf);
            right_page.update_checksum();
            self.file.put_page(new_page_no, right_page);

            // Fix up the old next page's prev pointer.
            if next_leaf != 0 {
                let np = self.file.write_page(next_leaf)?;
                np.set_prev_leaf(new_page_no);
                np.update_checksum();
            }

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator,
            })
        }
    }

    fn insert_into_branch(
        &mut self,
        page_no: u32,
        child_idx: usize,
        separator: &str,
        new_child: u32,
    ) -> Result<InsertResult> {
        let page = self.file.read_page(page_no)?.clone();
        let num_keys = page.num_rows() as usize;

        let max_keys = (PAGE_SIZE - PAGE_HEADER_SIZE - 4 - CHECKSUM_SIZE) / BRANCH_ENTRY_SIZE;

        if num_keys < max_keys {
            let page = self.file.write_page(page_no)?;
            insert_branch_entry(page, child_idx, separator, new_child);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            let (children, keys) = collect_branch_flat(&page);
            let mut new_children = children;
            let mut new_keys = keys;
            new_keys.insert(child_idx, separator.to_string());
            new_children.insert(child_idx + 1, new_child);

            let total_keys = new_keys.len();
            let mid = total_keys / 2;
            let split_key = new_keys[mid].clone();

            let left_keys = &new_keys[..mid];
            let left_children = &new_children[..mid + 1];
            let right_keys = &new_keys[mid + 1..];
            let right_children = &new_children[mid + 1..];

            let left_page = self.file.write_page(page_no)?;
            rebuild_branch_flat(left_page, left_children, left_keys);
            left_page.update_checksum();

            let new_page_no = self.file.allocate_page()?;
            let mut new_page = Page::new_branch();
            rebuild_branch_flat(&mut new_page, right_children, right_keys);
            new_page.update_checksum();
            self.file.put_page(new_page_no, new_page);

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator: split_key,
            })
        }
    }

    fn search_recursive(&mut self, page_no: u32, id: &str) -> Result<Option<Vec<u8>>> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            let num_rows = page.num_rows() as usize;
            if num_rows == 0 {
                return Ok(None);
            }

            // Binary search for the target id.
            let (pos, found) = find_insertion_point(&page, id)?;
            if found {
                let (start, end) = row_bounds(&page, pos, num_rows);
                if start < end && end <= PAGE_SIZE {
                    return Ok(Some(page.data[start..end].to_vec()));
                }
            }
            Ok(None)
        } else {
            let (_, child_page_no) = find_child(&page, id);
            self.search_recursive(child_page_no, id)
        }
    }

    fn delete_recursive(&mut self, page_no: u32, id: &str) -> Result<bool> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            let num_rows = page.num_rows() as usize;
            if num_rows == 0 {
                return Ok(false);
            }

            // Binary search for the row to delete.
            let (pos, found) = find_insertion_point(&page, id)?;
            if !found {
                return Ok(false);
            }

            // Snapshot old data, then rebuild without the deleted row.
            let snapshot = page.data;
            let next_leaf = page.next_leaf();
            let prev_leaf = page.prev_leaf();

            let page = self.file.write_page(page_no)?;
            write_leaf_without(page, &snapshot, num_rows, pos);
            page.set_next_leaf(next_leaf);
            page.set_prev_leaf(prev_leaf);
            page.update_checksum();
            Ok(true)
        } else {
            let (_, child_page_no) = find_child(&page, id);
            self.delete_recursive(child_page_no, id)
        }
    }
}

enum InsertResult {
    Fit,
    Split { new_page: u32, separator: String },
}

// ---------------------------------------------------------------------------
// Leaf page helpers
// ---------------------------------------------------------------------------

/// Compute the byte range [start, end) for row `i` in the page data.
fn row_bounds(page: &Page, i: usize, num_rows: usize) -> (usize, usize) {
    let start = page.row_offset(i as u16) as usize;
    let end = if i + 1 < num_rows {
        page.row_offset((i + 1) as u16) as usize
    } else {
        page.free_space_offset() as usize
    };
    (start, end)
}

/// Same as row_bounds but operates on a raw data snapshot.
fn row_bounds_raw(data: &[u8; PAGE_SIZE], i: usize, num_rows: usize) -> (usize, usize) {
    let start = raw_row_offset(data, i) as usize;
    let end = if i + 1 < num_rows {
        raw_row_offset(data, i + 1) as usize
    } else {
        raw_free_space_offset(data) as usize
    };
    (start, end)
}

fn raw_row_offset(data: &[u8; PAGE_SIZE], i: usize) -> u16 {
    let base = PAGE_HEADER_SIZE + i * 2;
    u16::from_le_bytes([data[base], data[base + 1]])
}

fn raw_free_space_offset(data: &[u8; PAGE_SIZE]) -> u16 {
    u16::from_le_bytes([data[6], data[7]])
}

/// Binary search within a leaf page for the insertion point of `id`.
/// Returns (index, true) if an exact match is found, or (index, false)
/// for the position where `id` should be inserted to maintain sort order.
fn find_insertion_point(page: &Page, id: &str) -> Result<(usize, bool)> {
    let num_rows = page.num_rows() as usize;
    if num_rows == 0 {
        return Ok((0, false));
    }
    let mut lo = 0usize;
    let mut hi = num_rows;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (start, end) = row_bounds(page, mid, num_rows);
        let mid_id = row::extract_id(&page.data[start..end])?;
        match mid_id.cmp(id) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok((mid, true)),
        }
    }
    Ok((lo, false))
}

/// Write a leaf page that contains the rows from `snapshot` (which has
/// `old_count` rows) plus a new row inserted at position `insert_pos`.
/// No heap allocation: reads from the snapshot, writes into `page`.
fn write_leaf_with_insert(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
) {
    let total = old_count + 1;
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    // Clear data area.
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + total * 2;
    let mut write_pos = data_start;

    let mut dst_idx = 0usize;
    let mut src_idx = 0usize;
    while dst_idx < total {
        if dst_idx == insert_pos {
            // Write the new row.
            page.data[write_pos..write_pos + new_row.len()].copy_from_slice(new_row);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += new_row.len();
            dst_idx += 1;
        } else {
            // Copy existing row from snapshot.
            let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += len;
            src_idx += 1;
            dst_idx += 1;
        }
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a leaf page that contains the rows from `snapshot` except the row at
/// `skip_pos`. No heap allocation.
fn write_leaf_without(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    skip_pos: usize,
) {
    let total = old_count - 1;
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + total * 2;
    let mut write_pos = data_start;
    let mut dst_idx = 0usize;

    for src_idx in 0..old_count {
        if src_idx == skip_pos {
            continue;
        }
        let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
        let len = e - s;
        page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
        page.set_row_offset(dst_idx as u16, write_pos as u16);
        write_pos += len;
        dst_idx += 1;
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a subset [range_start..range_end) of the "virtual" row sequence
/// (the old rows with a new row inserted at `insert_pos`) into `page`.
fn write_leaf_range(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
    range_start: usize,
    range_end: usize,
) {
    let count = range_end - range_start;
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + count * 2;
    let mut write_pos = data_start;

    for dst_idx in 0..count {
        let virtual_idx = range_start + dst_idx;
        if virtual_idx == insert_pos {
            // This is the newly inserted row.
            page.data[write_pos..write_pos + new_row.len()].copy_from_slice(new_row);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += new_row.len();
        } else {
            // Map virtual index back to original index.
            let orig_idx = if virtual_idx < insert_pos {
                virtual_idx
            } else {
                virtual_idx - 1
            };
            let (s, e) = row_bounds_raw(snapshot, orig_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += len;
        }
    }

    page.set_num_rows(count as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Extract the _id of the row at a given position in the virtual sequence
/// (old rows + new row inserted at `insert_pos`).
fn extract_id_at_virtual_pos(
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
    virtual_pos: usize,
) -> Result<String> {
    if virtual_pos == insert_pos {
        Ok(row::extract_id(new_row)?.to_string())
    } else {
        let orig_idx = if virtual_pos < insert_pos {
            virtual_pos
        } else {
            virtual_pos - 1
        };
        let (s, e) = row_bounds_raw(snapshot, orig_idx, old_count);
        Ok(row::extract_id(&snapshot[s..e])?.to_string())
    }
}

// ---------------------------------------------------------------------------
// Branch page helpers (unchanged fixed-42-byte entry format)
// ---------------------------------------------------------------------------

const BRANCH_ENTRY_SIZE: usize = 42; // 4 (child) + 2 (key_len) + 36 (key data)

fn get_branch_child(page: &Page, idx: usize) -> u32 {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap())
}

fn get_branch_key(page: &Page, idx: usize) -> String {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE + 4;
    let key_len = u16::from_le_bytes(page.data[offset..offset + 2].try_into().unwrap()) as usize;
    String::from_utf8_lossy(&page.data[offset + 2..offset + 2 + key_len]).to_string()
}

fn find_child(page: &Page, id: &str) -> (usize, u32) {
    let num_keys = page.num_rows() as usize;
    for i in 0..num_keys {
        let key = get_branch_key(page, i);
        if id < key.as_str() {
            return (i, get_branch_child(page, i));
        }
    }
    let last_child_offset = PAGE_HEADER_SIZE + num_keys * BRANCH_ENTRY_SIZE;
    let child = u32::from_le_bytes(
        page.data[last_child_offset..last_child_offset + 4]
            .try_into()
            .unwrap(),
    );
    (num_keys, child)
}

fn write_branch_entry(page: &mut Page, idx: usize, child: u32, key: &str) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
    let key_bytes = key.as_bytes();
    let key_len = key_bytes.len().min(36);
    page.data[offset + 4..offset + 6].copy_from_slice(&(key_len as u16).to_le_bytes());
    page.data[offset + 6..offset + 6 + 36].fill(0);
    page.data[offset + 6..offset + 6 + key_len].copy_from_slice(&key_bytes[..key_len]);
}

fn set_branch_child(page: &mut Page, idx: usize, child: u32) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
}

fn insert_branch_entry(page: &mut Page, child_idx: usize, key: &str, new_child: u32) {
    let (children, keys) = collect_branch_flat(page);
    let mut new_children = children;
    let mut new_keys = keys;
    new_keys.insert(child_idx, key.to_string());
    new_children.insert(child_idx + 1, new_child);
    rebuild_branch_flat(page, &new_children, &new_keys);
    page.set_num_rows(new_keys.len() as u16);
}

fn collect_branch_flat(page: &Page) -> (Vec<u32>, Vec<String>) {
    let num_keys = page.num_rows() as usize;
    let mut children = Vec::with_capacity(num_keys + 1);
    let mut keys = Vec::with_capacity(num_keys);
    for i in 0..num_keys {
        children.push(get_branch_child(page, i));
        keys.push(get_branch_key(page, i));
    }
    children.push(get_branch_child(page, num_keys));
    (children, keys)
}

fn rebuild_branch_flat(page: &mut Page, children: &[u32], keys: &[String]) {
    page.set_flags(PAGE_BRANCH);
    page.set_num_rows(keys.len() as u16);
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);
    for (i, key) in keys.iter().enumerate() {
        write_branch_entry(page, i, children[i], key);
    }
    set_branch_child(page, keys.len(), children[keys.len()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row;
    use crate::value::Value;
    use tempfile::NamedTempFile;

    fn make_row(id: &str, name: &str) -> Vec<u8> {
        row::encode_row(id, &[(0, &Value::Text(name.into()))])
    }

    #[test]
    fn test_insert_and_search() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        let row = make_row("id1", "alice");
        tree.insert("id1", &row).unwrap();

        let found = tree.search("id1").unwrap();
        assert!(found.is_some());
        let decoded = row::decode_row(&found.unwrap()).unwrap();
        assert_eq!(decoded.id, "id1");
        assert_eq!(decoded.columns[0].1, Value::Text("alice".into()));
    }

    #[test]
    fn test_search_not_found() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        assert!(tree.search("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_duplicate_key_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        let row = make_row("id1", "alice");
        tree.insert("id1", &row).unwrap();
        assert!(tree.insert("id1", &row).is_err());
    }

    #[test]
    fn test_many_inserts_trigger_split() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();

        for i in 0..100 {
            let id = format!("id_{i:04}");
            let row = make_row(&id, &format!("name_{i}"));
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(&id, &row).unwrap();
        }

        // Verify all rows are findable
        let mut tree = BTree::new(&mut pf, root);
        for i in 0..100 {
            let id = format!("id_{i:04}");
            assert!(tree.search(&id).unwrap().is_some(), "missing: {id}");
        }
    }

    #[test]
    fn test_delete() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        let row = make_row("id1", "alice");
        tree.insert("id1", &row).unwrap();
        assert!(tree.delete("id1").unwrap());
        assert!(tree.search("id1").unwrap().is_none());
        assert!(!tree.delete("id1").unwrap()); // already deleted
    }

    #[test]
    fn test_scan_all() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();

        for i in 0..20 {
            let id = format!("id_{i:04}");
            let row = make_row(&id, &format!("name_{i}"));
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(&id, &row).unwrap();
        }

        let mut tree = BTree::new(&mut pf, root);
        let all = tree.scan_all().unwrap();
        assert_eq!(all.len(), 20);
    }

    #[test]
    fn test_500_uuid_inserts_separate_tree_instances() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();

        let mut ids = Vec::new();
        for i in 0..500 {
            let id = uuid::Uuid::new_v4().to_string();
            let row = row::encode_row(&id, &[(0, &Value::Integer(i))]);
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(&id, &row).unwrap();
            pf.flush().unwrap();
            ids.push(id);
        }

        // Verify all rows are findable
        for (i, id) in ids.iter().enumerate() {
            let mut tree = BTree::new(&mut pf, root);
            let result = tree.search(id).unwrap();
            assert!(result.is_some(), "missing row at i={i}, id={id}");
        }
    }
}
