use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::page::{Page, PAGE_BRANCH, PAGE_HEADER_SIZE, PAGE_LEAF, PAGE_SIZE};
use crate::row;

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
                // Create new root branch page with old root as left child and new_page as right
                let new_root = self.file.allocate_page()?;
                let mut root_page = Page::new_branch();
                // Layout: child0 | key0 | child1
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
        // Find the leftmost leaf
        let first_leaf = self.find_leftmost_leaf(self.root)?;
        let mut results = Vec::new();
        let mut current = first_leaf;
        loop {
            let page = self.file.read_page(current)?.clone();
            let rows = collect_leaf_rows(&page);
            for (id, data) in rows {
                results.push((id, data));
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
        // Check for duplicate
        let num_rows = page.num_rows() as usize;
        for i in 0..num_rows {
            let (start, end) = row_bounds(page, i, num_rows);
            if start < end && end <= PAGE_SIZE {
                let existing_id = row::extract_id(&page.data[start..end])?;
                if existing_id == id {
                    return Err(BoogyError::DuplicateKey(id.to_string()));
                }
            }
        }

        // Collect all existing rows and add the new one
        let mut all_rows = collect_leaf_rows(page);
        all_rows.push((id.to_string(), row_data.to_vec()));

        // Check if all rows fit in a single page
        let total_row_bytes: usize = all_rows.iter().map(|(_, d)| d.len()).sum();
        let offset_array_bytes = all_rows.len() * 2;
        let total_needed = PAGE_HEADER_SIZE + offset_array_bytes + total_row_bytes + 4; // +4 checksum

        if total_needed <= PAGE_SIZE {
            // Fits: rebuild the page with all rows
            let next = page.next_leaf();
            let prev = page.prev_leaf();
            let page = self.file.write_page(page_no)?;
            rebuild_leaf(page, &all_rows);
            page.set_next_leaf(next);
            page.set_prev_leaf(prev);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // Split: sort all rows, split in half
            all_rows.sort_by(|a, b| a.0.cmp(&b.0));

            let mid = all_rows.len() / 2;
            let left_rows = &all_rows[..mid];
            let right_rows = &all_rows[mid..];
            let separator = right_rows[0].0.clone();

            // Allocate new page for the right half
            let new_page_no = self.file.allocate_page()?;

            // Get the old next_leaf before we modify this page
            let old_next = {
                let p = self.file.read_page(page_no)?.clone();
                p.next_leaf()
            };

            // Rebuild left page
            let left_page = self.file.write_page(page_no)?;
            rebuild_leaf(left_page, left_rows);
            left_page.set_next_leaf(new_page_no);
            left_page.update_checksum();

            // Build right page
            let mut new_page = Page::new_leaf();
            rebuild_leaf(&mut new_page, right_rows);
            new_page.set_prev_leaf(page_no);
            new_page.set_next_leaf(old_next);
            new_page.update_checksum();
            self.file.put_page(new_page_no, new_page);

            // Update old next page's prev pointer if it exists
            if old_next != 0 {
                let next_page = self.file.write_page(old_next)?;
                next_page.set_prev_leaf(new_page_no);
                next_page.update_checksum();
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

        // Max keys that fit in a branch page
        // Layout: header(16) + entries * BRANCH_ENTRY_SIZE + last_child(4) + checksum(4)
        let max_keys = (PAGE_SIZE - PAGE_HEADER_SIZE - 4 - 4) / BRANCH_ENTRY_SIZE;

        if num_keys < max_keys {
            let page = self.file.write_page(page_no)?;
            insert_branch_entry(page, child_idx, separator, new_child);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // Split the branch
            let (children, keys) = collect_branch_flat(&page);
            // children: [c0, c1, ..., cN] (N+1 children for N keys)
            // keys: [k0, k1, ..., kN-1]
            // Insert separator at position child_idx, and new_child after children[child_idx]
            let mut new_children = children.clone();
            let mut new_keys = keys.clone();
            new_keys.insert(child_idx, separator.to_string());
            new_children.insert(child_idx + 1, new_child);

            // Now split: total keys = new_keys.len()
            let total_keys = new_keys.len();
            let mid = total_keys / 2;
            let split_key = new_keys[mid].clone();

            // Left branch: keys[0..mid], children[0..mid+1]
            // Right branch: keys[mid+1..], children[mid+1..]
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
            for i in 0..num_rows {
                let (start, end) = row_bounds(&page, i, num_rows);
                if start < end && end <= PAGE_SIZE {
                    let row_bytes = &page.data[start..end];
                    let row_id = row::extract_id(row_bytes)?;
                    if row_id == id {
                        return Ok(Some(row_bytes.to_vec()));
                    }
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
            let mut rows = collect_leaf_rows(&page);
            let original_len = rows.len();
            rows.retain(|r| r.0 != id);
            if rows.len() == original_len {
                return Ok(false);
            }
            let next = page.next_leaf();
            let prev = page.prev_leaf();
            let page = self.file.write_page(page_no)?;
            rebuild_leaf(page, &rows);
            page.set_next_leaf(next);
            page.set_prev_leaf(prev);
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

// --- Leaf page layout ---
//
// Rows are stored sequentially after the offset array.
// Layout:
//   [header: 16 bytes]
//   [offset_array: num_rows * 2 bytes] -- each entry is a u16 offset into the page
//   [row_data: variable] -- rows packed one after another
//   [... free space ...]
//   [checksum: 4 bytes at PAGE_SIZE-4]
//
// free_space_offset tracks the end of the row data region (next write position).

fn row_bounds(page: &Page, i: usize, num_rows: usize) -> (usize, usize) {
    let start = page.row_offset(i as u16) as usize;
    let end = if i + 1 < num_rows {
        page.row_offset((i + 1) as u16) as usize
    } else {
        page.free_space_offset() as usize
    };
    (start, end)
}

fn append_row_to_leaf(page: &mut Page, row_data: &[u8]) {
    let num_rows = page.num_rows() as usize;
    let free = page.free_space_offset() as usize;
    // Ensure we don't write into the offset array area.
    // After this insert, the offset array will have (num_rows + 1) entries.
    let offset_array_end = PAGE_HEADER_SIZE + (num_rows + 1) * 2;
    let write_at = free.max(offset_array_end);

    page.data[write_at..write_at + row_data.len()].copy_from_slice(row_data);
    page.set_row_offset(num_rows as u16, write_at as u16);
    page.set_num_rows((num_rows + 1) as u16);
    page.set_free_space_offset((write_at + row_data.len()) as u16);
}

fn collect_leaf_rows(page: &Page) -> Vec<(String, Vec<u8>)> {
    let num_rows = page.num_rows() as usize;
    let mut rows = Vec::with_capacity(num_rows);
    for i in 0..num_rows {
        let (start, end) = row_bounds(page, i, num_rows);
        if start < end && end <= PAGE_SIZE {
            let data = page.data[start..end].to_vec();
            if let Ok(id) = row::extract_id(&data) {
                rows.push((id.to_string(), data));
            }
        }
    }
    rows
}

fn rebuild_leaf(page: &mut Page, rows: &[(String, Vec<u8>)]) {
    // Reset the leaf page (keep magic + leaf flag, clear everything else)
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    // Clear the data area (between header and checksum)
    for b in page.data[PAGE_HEADER_SIZE..PAGE_SIZE - 4].iter_mut() {
        *b = 0;
    }

    // Pre-compute offset array size so row data starts after it
    let offset_array_size = rows.len() * 2;
    let data_start = PAGE_HEADER_SIZE + offset_array_size;
    page.set_free_space_offset(data_start as u16);

    for (_, data) in rows {
        append_row_to_leaf(page, data);
    }
}

// --- Branch page layout ---
//
// Fixed-size entry format for simplicity:
//   [header: 16 bytes]
//   [child0: 4 bytes][key0_len: 2 bytes][key0: 36 bytes] = entry 0
//   [child1: 4 bytes][key1_len: 2 bytes][key1: 36 bytes] = entry 1
//   ...
//   [childN: 4 bytes] = last child (just the pointer, no key)
//   [checksum: 4 bytes]
//
// Each entry is BRANCH_ENTRY_SIZE = 42 bytes (4 + 2 + 36).
// num_rows stores num_keys.
// Total children = num_keys + 1.
// The last child pointer is at offset = header + num_keys * 42, occupying just 4 bytes.

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
    // Return last child
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
    // Clear the key area first
    page.data[offset + 6..offset + 6 + 36].fill(0);
    page.data[offset + 6..offset + 6 + key_len].copy_from_slice(&key_bytes[..key_len]);
}

fn set_branch_child(page: &mut Page, idx: usize, child: u32) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
}

fn insert_branch_entry(page: &mut Page, child_idx: usize, key: &str, new_child: u32) {
    let num_keys = page.num_rows() as usize;

    // We need to insert a new key between child_idx and child_idx+1.
    // Shift entries from child_idx..num_keys right by one slot.
    // Also shift the last child pointer.

    // Shift from right to left to avoid overwriting
    // The last child (at position num_keys) needs to move to num_keys+1
    let last_child = get_branch_child(page, num_keys);
    set_branch_child(page, num_keys + 1, last_child);

    // Shift entries [child_idx .. num_keys-1] right by one
    for i in (child_idx..num_keys).rev() {
        let src = PAGE_HEADER_SIZE + i * BRANCH_ENTRY_SIZE;
        let dst = PAGE_HEADER_SIZE + (i + 1) * BRANCH_ENTRY_SIZE;
        page.data.copy_within(src..src + BRANCH_ENTRY_SIZE, dst);
    }

    // Write the new entry at child_idx position
    // The child at child_idx stays (it's the left child of the new key).
    // We write: key at position child_idx, and new_child at position child_idx+1.
    // But wait - the entry format is [child][key], so entry at child_idx has the left child.
    // We need to set the key of entry child_idx to the separator,
    // and child of entry child_idx+1 to new_child.
    // But we already shifted, so entry child_idx+1 has the old entry child_idx.
    // The old child at child_idx is still correct (left pointer).
    // We just need to write the key at child_idx and set child at child_idx to...
    //
    // Actually, let's think step by step:
    // Before: entries = [c0,k0], [c1,k1], ..., [cN-1,kN-1], [cN]
    // After shifting entries[child_idx..] right by one:
    //   [c0,k0], ..., [c_{idx-1},k_{idx-1}], [GAP], [c_{idx},k_{idx}], ..., [cN-1,kN-1], [cN]
    // We fill the gap: the left child of the new key is the old c_{idx} which we need to keep,
    // but we shifted it. So the gap's child should be the original child_idx's child.
    //
    // Hmm, this is getting complex. Let me just do it with the flat representation.
    let (children, keys) = collect_branch_flat(page);
    let mut new_children = children;
    let mut new_keys = keys;
    new_keys.insert(child_idx, key.to_string());
    new_children.insert(child_idx + 1, new_child);
    rebuild_branch_flat(page, &new_children, &new_keys);
    page.set_num_rows(new_keys.len() as u16);
}

/// Collect branch page as flat arrays of children and keys.
/// Returns (children, keys) where children.len() == keys.len() + 1.
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

/// Rebuild a branch page from flat arrays.
/// children.len() == keys.len() + 1
fn rebuild_branch_flat(page: &mut Page, children: &[u32], keys: &[String]) {
    page.set_flags(PAGE_BRANCH);
    page.set_num_rows(keys.len() as u16);

    // Clear data area
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - 4].fill(0);

    for (i, key) in keys.iter().enumerate() {
        write_branch_entry(page, i, children[i], key);
    }
    // Write last child
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
}
