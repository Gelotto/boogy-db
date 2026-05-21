use crate::error::Result;
use crate::file::{PageFile, WriteGuard};
use crate::page::{Page, PAGE_BRANCH, PAGE_HEADER_SIZE, PAGE_LEAF, PAGE_SIZE};
use crate::value::{Type, Value};

// ---------------------------------------------------------------------------
// Composite key encoding
// ---------------------------------------------------------------------------

/// Encode a composite index key for the given column type, value, and rowid.
/// Returns `None` if the value is Null (nulls are not indexed).
pub fn encode_index_key(col_type: Type, val: &Value, rowid: u64) -> Option<Vec<u8>> {
    match (col_type, val) {
        (_, Value::Null) => None,
        (Type::Integer, Value::Integer(i)) => Some(encode_index_key_integer(*i, rowid)),
        (Type::Real, Value::Real(f)) => Some(encode_index_key_real(*f, rowid)),
        (Type::Text, Value::Text(s)) => encode_index_key_text(s, rowid),
        // Cross-type coercion: integer column with real value, etc.
        (Type::Integer, Value::Real(f)) => Some(encode_index_key_integer(*f as i64, rowid)),
        (Type::Real, Value::Integer(i)) => Some(encode_index_key_real(*i as f64, rowid)),
        _ => None,
    }
}

/// Encode a prefix for range/equality scanning (no rowid suffix).
/// Returns `None` for Null.
pub fn encode_value_prefix(col_type: Type, val: &Value) -> Option<Vec<u8>> {
    match (col_type, val) {
        (_, Value::Null) => None,
        (Type::Integer, Value::Integer(i)) => Some(encode_integer_prefix(*i)),
        (Type::Real, Value::Real(f)) => Some(encode_real_prefix(*f)),
        (Type::Text, Value::Text(s)) => Some(encode_text_prefix(s)),
        (Type::Integer, Value::Real(f)) => Some(encode_integer_prefix(*f as i64)),
        (Type::Real, Value::Integer(i)) => Some(encode_real_prefix(*i as f64)),
        _ => None,
    }
}

/// Encode a composite (multi-column) index key: each column's sortable
/// value-encoding in column order, then the 8-byte big-endian rowid suffix.
/// Returns `None` if ANY component is Null (nulls are not indexed — matches
/// the single-column behavior of `encode_index_key`).
///
/// Ordering: because every per-column encoding is self-delimiting — integers
/// and reals are fixed 8-byte sortable encodings, text is null-terminated
/// (and `0x00` is rejected inside text values) — concatenation preserves the
/// nesting of the per-column orderings, so byte comparison sorts by col1, then
/// col2, …, then rowid. The big-endian rowid suffix makes byte order match
/// numeric order for the tiebreaker.
pub fn encode_composite_index_key(col_types: &[Type], vals: &[Value], rowid: u64) -> Option<Vec<u8>> {
    let mut out = encode_composite_value_prefix(col_types, vals)?;
    out.extend_from_slice(&rowid.to_be_bytes());
    Some(out)
}

/// Composite value prefix (no rowid) for range/equality scans. Concatenates
/// each column's `encode_value_prefix` bytes in column order. Returns `None`
/// if any component is Null.
pub fn encode_composite_value_prefix(col_types: &[Type], vals: &[Value]) -> Option<Vec<u8>> {
    debug_assert_eq!(col_types.len(), vals.len());
    let mut out = Vec::new();
    for (t, v) in col_types.iter().zip(vals.iter()) {
        // Reuse the existing per-type value encoding so multi-column ordering
        // is the nesting of the per-column orderings. Each part is
        // self-delimiting (fixed-width int/real, null-terminated text), so
        // concatenation stays prefix-unambiguous and no extra length prefix
        // is needed.
        let part = encode_value_prefix(*t, v)?;
        out.extend_from_slice(&part);
    }
    Some(out)
}

/// Extract the rowid from a composite index key.
/// The rowid is always the last 8 bytes, stored big-endian.
pub fn extract_rowid(col_type: Type, key: &[u8]) -> u64 {
    match col_type {
        Type::Integer | Type::Real => {
            // Fixed 16-byte keys: [sortable:8][rowid:8 BE]
            debug_assert!(key.len() == 16);
            u64::from_be_bytes(key[8..16].try_into().unwrap())
        }
        Type::Text => {
            // Variable: [utf8_bytes][0x00][rowid:8 BE]
            // Rowid is last 8 bytes.
            debug_assert!(key.len() >= 10); // at least 1 byte text + 0x00 + 8 rowid
            let len = key.len();
            u64::from_be_bytes(key[len - 8..len].try_into().unwrap())
        }
        _ => panic!("extract_rowid: unsupported type {:?}", col_type),
    }
}

// --- Individual type encoders ---

/// Integer: `[i64_sortable:8][rowid:8 BE]` — 16 bytes.
/// i64 -> big-endian, XOR first byte with 0x80 (flip sign bit).
pub fn encode_index_key_integer(val: i64, rowid: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&encode_i64_sortable(val));
    buf.extend_from_slice(&rowid.to_be_bytes());
    buf
}

/// Real: `[f64_sortable:8][rowid:8 BE]` — 16 bytes.
/// Positive: flip sign bit. Negative: flip ALL bits.
pub fn encode_index_key_real(val: f64, rowid: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&encode_f64_sortable(val));
    buf.extend_from_slice(&rowid.to_be_bytes());
    buf
}

/// Text: `[utf8_bytes][0x00][rowid:8 BE]` — variable length.
/// Text must not contain 0x00 bytes. Returns `None` if null bytes are present,
/// since they break the null-terminated encoding.
pub fn encode_index_key_text(val: &str, rowid: u64) -> Option<Vec<u8>> {
    if val.as_bytes().contains(&0x00) {
        return None; // null bytes break the null-terminated encoding
    }
    let mut buf = Vec::with_capacity(val.len() + 1 + 8);
    buf.extend_from_slice(val.as_bytes());
    buf.push(0x00); // null terminator
    buf.extend_from_slice(&rowid.to_be_bytes());
    Some(buf)
}

/// Integer prefix (no rowid) for scan_prefix matching.
pub fn encode_integer_prefix(val: i64) -> Vec<u8> {
    encode_i64_sortable(val).to_vec()
}

/// Real prefix (no rowid) for scan_prefix matching.
pub fn encode_real_prefix(val: f64) -> Vec<u8> {
    encode_f64_sortable(val).to_vec()
}

/// Text prefix (no rowid) for scan_prefix matching.
/// Includes the null terminator so it only matches keys with this exact text value.
pub fn encode_text_prefix(val: &str) -> Vec<u8> {
    let bytes = val.as_bytes();
    let mut prefix = Vec::with_capacity(bytes.len() + 1);
    prefix.extend_from_slice(bytes);
    prefix.push(0x00);
    prefix
}

// --- Sortable encoding helpers ---

fn encode_i64_sortable(val: i64) -> [u8; 8] {
    let mut bytes = val.to_be_bytes();
    bytes[0] ^= 0x80; // flip sign bit for correct unsigned byte comparison
    bytes
}

fn encode_f64_sortable(val: f64) -> [u8; 8] {
    let bits = val.to_bits();
    let encoded = if val.is_sign_negative() {
        // Negative (including -0.0): flip ALL bits
        !bits
    } else {
        // Positive (including +0.0): flip sign bit only
        bits ^ (1u64 << 63)
    };
    encoded.to_be_bytes()
}

// ---------------------------------------------------------------------------
// IndexTreeReader — read-only access via &PageFile
// ---------------------------------------------------------------------------

/// Checksum occupies the last 4 bytes of each page.
const CHECKSUM_SIZE: usize = 4;

/// Maximum B+ tree depth (same as btree.rs).
const MAX_TREE_DEPTH: usize = 64;

/// Branch entry: [child:4][key_len:2][key_data:36] = 42 bytes.
const IDX_BRANCH_ENTRY_SIZE: usize = 42;
const IDX_BRANCH_KEY_MAX: usize = 36;

pub struct IndexTreeReader<'a> {
    file: &'a PageFile,
    root: u32,
}

impl<'a> IndexTreeReader<'a> {
    pub fn new(file: &'a PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    /// Return all keys that start with `prefix`, in sorted order.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        let first_leaf = self.find_leaf_for_key(self.root, prefix)?;
        let max_pages = self.file.page_count();
        let mut pages_visited = 0u32;
        let mut results = Vec::new();
        let mut current = first_leaf;
        let mut found_start = false;

        loop {
            pages_visited += 1;
            if pages_visited > max_pages {
                return Err(crate::error::BoogyError::Corruption(
                    "index leaf chain cycle detected in scan_prefix".into(),
                ));
            }
            let page = self.file.read_page(current)?;
            let num_entries = page.num_rows() as usize;

            for i in 0..num_entries {
                let entry_key = decode_leaf_entry(&page, i, num_entries);
                if let Some(k) = entry_key {
                    if k.starts_with(prefix) {
                        found_start = true;
                        results.push(k.to_vec());
                    } else if found_start {
                        // Past the prefix range -- done
                        return Ok(results);
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

    /// Count entries whose key starts with `prefix` without collecting them.
    pub fn count_prefix(&self, prefix: &[u8]) -> Result<u64> {
        let first_leaf = self.find_leaf_for_key(self.root, prefix)?;
        let max_pages = self.file.page_count();
        let mut pages_visited = 0u32;
        let mut count = 0u64;
        let mut current = first_leaf;
        let mut found_start = false;

        loop {
            pages_visited += 1;
            if pages_visited > max_pages {
                return Err(crate::error::BoogyError::Corruption(
                    "index leaf chain cycle detected in count_prefix".into(),
                ));
            }
            let page = self.file.read_page(current)?;
            let num_entries = page.num_rows() as usize;

            for i in 0..num_entries {
                let entry_key = decode_leaf_entry(&page, i, num_entries);
                if let Some(k) = entry_key {
                    if k.starts_with(prefix) {
                        found_start = true;
                        count += 1;
                    } else if found_start {
                        return Ok(count);
                    }
                }
            }

            let next = page.next_leaf();
            if next == 0 { break; }
            current = next;
        }

        Ok(count)
    }

    /// Same as scan_prefix but stops after collecting `max` keys.
    pub fn scan_prefix_limit(&self, prefix: &[u8], max: usize) -> Result<Vec<Vec<u8>>> {
        let first_leaf = self.find_leaf_for_key(self.root, prefix)?;
        let max_pages = self.file.page_count();
        let mut pages_visited = 0u32;
        let mut results = Vec::with_capacity(max.min(256));
        let mut current = first_leaf;
        let mut found_start = false;

        loop {
            pages_visited += 1;
            if pages_visited > max_pages {
                return Err(crate::error::BoogyError::Corruption(
                    "index leaf chain cycle detected in scan_prefix_limit".into(),
                ));
            }
            let page = self.file.read_page(current)?;
            let num_entries = page.num_rows() as usize;

            for i in 0..num_entries {
                let entry_key = decode_leaf_entry(&page, i, num_entries);
                if let Some(k) = entry_key {
                    if k.starts_with(prefix) {
                        found_start = true;
                        results.push(k.to_vec());
                        if results.len() >= max {
                            return Ok(results);
                        }
                    } else if found_start {
                        return Ok(results);
                    }
                }
            }

            let next = page.next_leaf();
            if next == 0 { break; }
            current = next;
        }

        Ok(results)
    }

    // --- Internal methods ---

    /// Navigate the B+ tree to find the leaf page containing (or nearest to) `key`.
    fn find_leaf_for_key(&self, mut page_no: u32, key: &[u8]) -> Result<u32> {
        for _ in 0..MAX_TREE_DEPTH {
            let page = self.file.read_page(page_no)?;
            if page.is_leaf() {
                return Ok(page_no);
            }
            let (_, child) = find_idx_child(&page, key);
            page_no = child;
        }
        Err(crate::error::BoogyError::Corruption(
            "index B+ tree depth exceeds maximum in find_leaf_for_key".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// IndexTreeWriter — write access via &mut WriteGuard
// ---------------------------------------------------------------------------

pub struct IndexTreeWriter<'a, 'b> {
    guard: &'a mut WriteGuard<'b>,
    root: u32,
}

impl<'a, 'b> IndexTreeWriter<'a, 'b> {
    pub fn new(guard: &'a mut WriteGuard<'b>, root: u32) -> Self {
        Self { guard, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    /// Create a new empty IndexTree (single empty leaf page). Returns the root page number.
    pub fn create(guard: &mut WriteGuard) -> Result<u32> {
        let page_no = guard.allocate_page()?;
        let page = Page::new_leaf();
        guard.put_page(page_no, page);
        Ok(page_no)
    }

    /// Insert a key. Returns the (possibly new) root page number.
    pub fn insert(&mut self, key: &[u8]) -> Result<u32> {
        let result = self.insert_recursive(self.root, key, 0)?;
        match result {
            InsertResult::Fit => Ok(self.root),
            InsertResult::Split {
                new_page,
                separator,
            } => {
                let new_root = self.guard.allocate_page()?;
                let mut root_page = Page::new_branch();
                write_idx_branch_entry(&mut root_page, 0, self.root, &separator);
                set_idx_branch_child(&mut root_page, 1, new_page);
                root_page.set_num_rows(1);
                root_page.update_checksum();
                self.guard.put_page(new_root, root_page);
                self.root = new_root;
                Ok(self.root)
            }
        }
    }

    /// Delete a key. Returns true if the key existed.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.delete_recursive(self.root, key, 0)
    }

    // --- Internal methods ---

    fn insert_recursive(&mut self, page_no: u32, key: &[u8], depth: usize) -> Result<InsertResult> {
        if depth >= MAX_TREE_DEPTH {
            return Err(crate::error::BoogyError::Corruption(
                "index B+ tree depth exceeds maximum in insert_recursive".into(),
            ));
        }
        // Check dirty overlay first (zero-copy), then cache (Arc deref without clone).
        // Only clone at the leaf where we need page data for rebuild.
        let (is_leaf, child_idx, child_page_no) = if let Some(p) = self.guard.peek_dirty(page_no) {
            if p.is_leaf() {
                (true, 0, 0)
            } else {
                let (ci, cp) = find_idx_child(p, key);
                (false, ci, cp)
            }
        } else {
            let arc = self.guard.page_file().read_page(page_no)?;
            if arc.is_leaf() {
                (true, 0, 0)
            } else {
                let (ci, cp) = find_idx_child(&arc, key);
                (false, ci, cp)
            }
        };

        if is_leaf {
            let page = self.guard.read_page_cloned(page_no)?;
            self.insert_into_leaf(page_no, &page, key)
        } else {
            let result = self.insert_recursive(child_page_no, key, depth + 1)?;
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
        key: &[u8],
    ) -> Result<InsertResult> {
        let num_entries = page.num_rows() as usize;

        // Binary search for insertion point.
        let pos = find_idx_insertion_point(page, key, num_entries);

        // Entry bytes: [len:2][key_bytes]
        let entry_len = 2 + key.len();

        // Check if the new entry fits.
        let offset_array_start = PAGE_HEADER_SIZE + num_entries * 2;
        let current_free = page.free_space_offset() as usize;
        let existing_data_size = if current_free > offset_array_start {
            current_free - offset_array_start
        } else {
            0
        };
        let needed = PAGE_HEADER_SIZE
            + (num_entries + 1) * 2
            + existing_data_size
            + entry_len
            + CHECKSUM_SIZE;

        if needed <= PAGE_SIZE {
            // Fits: in-place insert
            let snapshot = page.data;
            let page = self.guard.write_page(page_no)?;
            write_idx_leaf_with_insert(page, &snapshot, num_entries, pos, key);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // Split
            let snapshot = page.data;
            let next_leaf = page.next_leaf();
            let prev_leaf = page.prev_leaf();
            let total = num_entries + 1;
            let mid = total / 2;

            let new_page_no = self.guard.allocate_page()?;

            // Separator: the key of the first entry in the right half.
            let separator = extract_key_at_virtual_pos(&snapshot, num_entries, pos, key, mid);

            // Write left half (indices 0..mid).
            let left_page = self.guard.write_page(page_no)?;
            write_idx_leaf_range(left_page, &snapshot, num_entries, pos, key, 0, mid);
            left_page.set_next_leaf(new_page_no);
            left_page.set_prev_leaf(prev_leaf);
            left_page.update_checksum();

            // Write right half (indices mid..total).
            let mut right_page = Page::new_leaf();
            write_idx_leaf_range(
                &mut right_page,
                &snapshot,
                num_entries,
                pos,
                key,
                mid,
                total,
            );
            right_page.set_prev_leaf(page_no);
            right_page.set_next_leaf(next_leaf);
            right_page.update_checksum();
            self.guard.put_page(new_page_no, right_page);

            // Fix old next page's prev pointer.
            if next_leaf != 0 {
                let np = self.guard.write_page(next_leaf)?;
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
        separator: &[u8],
        new_child: u32,
    ) -> Result<InsertResult> {
        let page = self.guard.read_page_cloned(page_no)?;
        let num_keys = page.num_rows() as usize;

        let max_keys =
            (PAGE_SIZE - PAGE_HEADER_SIZE - 4 - CHECKSUM_SIZE) / IDX_BRANCH_ENTRY_SIZE;

        if num_keys < max_keys {
            let page = self.guard.write_page(page_no)?;
            insert_idx_branch_entry(page, child_idx, separator, new_child);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            let (children, keys) = collect_idx_branch_flat(&page);
            let mut new_children = children;
            let mut new_keys = keys;
            new_keys.insert(child_idx, separator.to_vec());
            new_children.insert(child_idx + 1, new_child);

            let total_keys = new_keys.len();
            let mid = total_keys / 2;
            let split_key = new_keys[mid].clone();

            let left_keys = &new_keys[..mid];
            let left_children = &new_children[..mid + 1];
            let right_keys = &new_keys[mid + 1..];
            let right_children = &new_children[mid + 1..];

            let left_page = self.guard.write_page(page_no)?;
            rebuild_idx_branch_flat(left_page, left_children, left_keys);
            left_page.update_checksum();

            let new_page_no = self.guard.allocate_page()?;
            let mut new_page = Page::new_branch();
            rebuild_idx_branch_flat(&mut new_page, right_children, right_keys);
            new_page.update_checksum();
            self.guard.put_page(new_page_no, new_page);

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator: split_key,
            })
        }
    }

    fn delete_recursive(&mut self, page_no: u32, key: &[u8], depth: usize) -> Result<bool> {
        if depth >= MAX_TREE_DEPTH {
            return Err(crate::error::BoogyError::Corruption(
                "index B+ tree depth exceeds maximum in delete_recursive".into(),
            ));
        }
        // Branch navigation without clone
        let (is_leaf, child_page_no) = if let Some(p) = self.guard.peek_dirty(page_no) {
            if p.is_leaf() {
                (true, 0)
            } else {
                let (_, cp) = find_idx_child(p, key);
                (false, cp)
            }
        } else {
            let arc = self.guard.page_file().read_page(page_no)?;
            if arc.is_leaf() {
                (true, 0)
            } else {
                let (_, cp) = find_idx_child(&arc, key);
                (false, cp)
            }
        };

        if !is_leaf {
            return self.delete_recursive(child_page_no, key, depth + 1);
        }

        // Leaf — clone for rebuild
        let page = self.guard.read_page_cloned(page_no)?;
        let num_entries = page.num_rows() as usize;
        if num_entries == 0 {
            return Ok(false);
        }

        // Linear scan for exact key match.
        let mut found_pos = None;
        for i in 0..num_entries {
            if let Some(k) = decode_leaf_entry(&page, i, num_entries) {
                if k == key {
                    found_pos = Some(i);
                    break;
                }
                if k > key {
                    break; // past where it would be
                }
            }
        }

        let pos = match found_pos {
            Some(p) => p,
            None => return Ok(false),
        };

        // Rebuild page without the deleted entry.
        let snapshot = page.data;
        let next_leaf = page.next_leaf();
        let prev_leaf = page.prev_leaf();

        let page = self.guard.write_page(page_no)?;
        write_idx_leaf_without(page, &snapshot, num_entries, pos);
        page.set_next_leaf(next_leaf);
        page.set_prev_leaf(prev_leaf);
        page.update_checksum();
        Ok(true)
    }
}

enum InsertResult {
    Fit,
    Split {
        new_page: u32,
        separator: Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// Leaf page helpers for IndexTree
// ---------------------------------------------------------------------------
// Leaf entries: [len:2 LE][key_bytes] packed sequentially with offset array.

/// Decode the key at entry `i` in a leaf page.
fn decode_leaf_entry(page: &Page, i: usize, num_entries: usize) -> Option<&[u8]> {
    let (start, end) = idx_entry_bounds(page, i, num_entries);
    if start + 2 > end || end > PAGE_SIZE {
        return None;
    }
    let key_len =
        u16::from_le_bytes([page.data[start], page.data[start + 1]]) as usize;
    let key_start = start + 2;
    let key_end = key_start + key_len;
    if key_end > end {
        return None;
    }
    Some(&page.data[key_start..key_end])
}

fn decode_leaf_entry_raw(data: &[u8; PAGE_SIZE], i: usize, num_entries: usize) -> Option<Vec<u8>> {
    let (start, end) = idx_entry_bounds_raw(data, i, num_entries);
    if start + 2 > end || end > PAGE_SIZE {
        return None;
    }
    let key_len = u16::from_le_bytes([data[start], data[start + 1]]) as usize;
    let key_start = start + 2;
    let key_end = key_start + key_len;
    if key_end > end {
        return None;
    }
    Some(data[key_start..key_end].to_vec())
}

fn idx_entry_bounds(page: &Page, i: usize, num_entries: usize) -> (usize, usize) {
    let start = page.row_offset(i as u16) as usize;
    let end = if i + 1 < num_entries {
        page.row_offset((i + 1) as u16) as usize
    } else {
        page.free_space_offset() as usize
    };
    (start, end)
}

fn idx_entry_bounds_raw(data: &[u8; PAGE_SIZE], i: usize, num_entries: usize) -> (usize, usize) {
    let start = raw_row_offset(data, i) as usize;
    let end = if i + 1 < num_entries {
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

/// Encode a key into entry bytes: [len:2 LE][key_bytes].
fn encode_entry(key: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + key.len());
    buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
    buf.extend_from_slice(key);
    buf
}

/// Binary search within a leaf page for the insertion point of `key`.
/// Returns the index where `key` should be inserted.
fn find_idx_insertion_point(page: &Page, key: &[u8], num_entries: usize) -> usize {
    if num_entries == 0 {
        return 0;
    }
    let mut lo = 0usize;
    let mut hi = num_entries;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if let Some(mid_key) = decode_leaf_entry(page, mid, num_entries) {
            match mid_key.cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return mid, // exact match (shouldn't happen with unique composite keys)
            }
        } else {
            lo = mid + 1; // corrupted entry, skip
        }
    }
    lo
}

/// Write a leaf page with entries from `snapshot` plus a new key inserted at `insert_pos`.
fn write_idx_leaf_with_insert(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_key: &[u8],
) {
    let total = old_count + 1;
    let new_entry = encode_entry(new_key);

    let saved_next = u32::from_le_bytes(snapshot[8..12].try_into().unwrap());
    let saved_prev = u32::from_le_bytes(snapshot[12..16].try_into().unwrap());

    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(saved_next);
    page.set_prev_leaf(saved_prev);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + total * 2;
    let mut write_pos = data_start;

    let mut dst_idx = 0usize;
    let mut src_idx = 0usize;
    while dst_idx < total {
        if dst_idx == insert_pos {
            page.data[write_pos..write_pos + new_entry.len()]
                .copy_from_slice(&new_entry);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += new_entry.len();
            dst_idx += 1;
        } else {
            let (s, e) = idx_entry_bounds_raw(snapshot, src_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len]
                .copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += len;
            src_idx += 1;
            dst_idx += 1;
        }
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a leaf page without the entry at `skip_pos`.
fn write_idx_leaf_without(
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
        let (s, e) = idx_entry_bounds_raw(snapshot, src_idx, old_count);
        let len = e - s;
        page.data[write_pos..write_pos + len]
            .copy_from_slice(&snapshot[s..e]);
        page.set_row_offset(dst_idx as u16, write_pos as u16);
        write_pos += len;
        dst_idx += 1;
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a subset [range_start..range_end) of the virtual entry sequence
/// (old entries with a new key inserted at `insert_pos`) into `page`.
fn write_idx_leaf_range(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_key: &[u8],
    range_start: usize,
    range_end: usize,
) {
    let count = range_end - range_start;
    let new_entry = encode_entry(new_key);

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
            page.data[write_pos..write_pos + new_entry.len()]
                .copy_from_slice(&new_entry);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += new_entry.len();
        } else {
            let orig_idx = if virtual_idx < insert_pos {
                virtual_idx
            } else {
                virtual_idx - 1
            };
            let (s, e) = idx_entry_bounds_raw(snapshot, orig_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len]
                .copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += len;
        }
    }

    page.set_num_rows(count as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Extract the key at a virtual position in the entry sequence
/// (old entries with a new key inserted at `insert_pos`).
fn extract_key_at_virtual_pos(
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_key: &[u8],
    virtual_pos: usize,
) -> Vec<u8> {
    if virtual_pos == insert_pos {
        new_key.to_vec()
    } else {
        let orig_idx = if virtual_pos < insert_pos {
            virtual_pos
        } else {
            virtual_pos - 1
        };
        let num_entries = old_count;
        decode_leaf_entry_raw(snapshot, orig_idx, num_entries)
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Branch page helpers for IndexTree
// ---------------------------------------------------------------------------
// Branch entries: [child:4][key_len:2][key_data:36] = 42 bytes.
// Keys are truncated to 36 bytes for routing (leaf pages have full keys).

fn get_idx_branch_child(page: &Page, idx: usize) -> u32 {
    let offset = PAGE_HEADER_SIZE + idx * IDX_BRANCH_ENTRY_SIZE;
    u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap())
}

fn get_idx_branch_key(page: &Page, idx: usize) -> Vec<u8> {
    let offset = PAGE_HEADER_SIZE + idx * IDX_BRANCH_ENTRY_SIZE + 4;
    let key_len =
        u16::from_le_bytes([page.data[offset], page.data[offset + 1]]) as usize;
    let key_len = key_len.min(IDX_BRANCH_KEY_MAX);
    page.data[offset + 2..offset + 2 + key_len].to_vec()
}

fn find_idx_child(page: &Page, key: &[u8]) -> (usize, u32) {
    let num_keys = page.num_rows() as usize;
    for i in 0..num_keys {
        let branch_key = get_idx_branch_key(page, i);
        // Compare using the truncated branch key.
        // The search key may be longer, so we only compare the prefix.
        if key < branch_key.as_slice() {
            return (i, get_idx_branch_child(page, i));
        }
    }
    // Key >= all branch keys, go to rightmost child.
    let last_child_offset = PAGE_HEADER_SIZE + num_keys * IDX_BRANCH_ENTRY_SIZE;
    let child = u32::from_le_bytes(
        page.data[last_child_offset..last_child_offset + 4]
            .try_into()
            .unwrap(),
    );
    (num_keys, child)
}

fn write_idx_branch_entry(page: &mut Page, idx: usize, child: u32, key: &[u8]) {
    let offset = PAGE_HEADER_SIZE + idx * IDX_BRANCH_ENTRY_SIZE;
    // child: 4 bytes
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
    // key_len: 2 bytes
    let truncated_len = key.len().min(IDX_BRANCH_KEY_MAX);
    page.data[offset + 4..offset + 6]
        .copy_from_slice(&(truncated_len as u16).to_le_bytes());
    // key_data: up to 36 bytes, zero-padded
    page.data[offset + 6..offset + 6 + IDX_BRANCH_KEY_MAX].fill(0);
    page.data[offset + 6..offset + 6 + truncated_len]
        .copy_from_slice(&key[..truncated_len]);
}

fn set_idx_branch_child(page: &mut Page, idx: usize, child: u32) {
    let offset = PAGE_HEADER_SIZE + idx * IDX_BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
}

fn insert_idx_branch_entry(
    page: &mut Page,
    child_idx: usize,
    key: &[u8],
    new_child: u32,
) {
    let (children, keys) = collect_idx_branch_flat(page);
    let mut new_children = children;
    let mut new_keys = keys;
    new_keys.insert(child_idx, key.to_vec());
    new_children.insert(child_idx + 1, new_child);
    rebuild_idx_branch_flat(page, &new_children, &new_keys);
    page.set_num_rows(new_keys.len() as u16);
}

fn collect_idx_branch_flat(page: &Page) -> (Vec<u32>, Vec<Vec<u8>>) {
    let num_keys = page.num_rows() as usize;
    let mut children = Vec::with_capacity(num_keys + 1);
    let mut keys = Vec::with_capacity(num_keys);
    for i in 0..num_keys {
        children.push(get_idx_branch_child(page, i));
        keys.push(get_idx_branch_key(page, i));
    }
    children.push(get_idx_branch_child(page, num_keys));
    (children, keys)
}

fn rebuild_idx_branch_flat(page: &mut Page, children: &[u32], keys: &[Vec<u8>]) {
    page.set_flags(PAGE_BRANCH);
    page.set_num_rows(keys.len() as u16);
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);
    for (i, key) in keys.iter().enumerate() {
        write_idx_branch_entry(page, i, children[i], key);
    }
    set_idx_branch_child(page, keys.len(), children[keys.len()]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::PageFile;
    use tempfile::NamedTempFile;

    // --- Key encoding tests ---

    #[test]
    fn test_composite_key_orders_lexicographically_by_column() {
        use crate::value::{Type, Value};
        // (Integer, Integer) composite. Keys must sort by col1 then col2 then rowid.
        let types = [Type::Integer, Type::Integer];
        let k = |a: i64, b: i64, rid: u64|
            encode_composite_index_key(&types, &[Value::Integer(a), Value::Integer(b)], rid).unwrap();
        let mut keys = vec![k(5, 9, 1), k(5, 2, 2), k(4, 100, 3), k(5, 9, 0)];
        keys.sort();
        // Expected order: (4,100,r3) < (5,2,r2) < (5,9,r0) < (5,9,r1)
        assert_eq!(keys, vec![k(4,100,3), k(5,2,2), k(5,9,0), k(5,9,1)]);
    }

    #[test]
    fn test_composite_key_text_then_integer() {
        use crate::value::{Type, Value};
        let types = [Type::Text, Type::Integer];
        let k = |s: &str, n: i64, rid: u64|
            encode_composite_index_key(&types, &[Value::Text(s.into()), Value::Integer(n)], rid).unwrap();
        let mut keys = vec![k("bob", 1, 1), k("alice", 9, 2), k("alice", 1, 3)];
        keys.sort();
        assert_eq!(keys, vec![k("alice",1,3), k("alice",9,2), k("bob",1,1)]);
    }

    #[test]
    fn test_composite_key_null_component_not_indexed() {
        use crate::value::{Type, Value};
        let types = [Type::Integer, Type::Integer];
        // A null in any key component → None (consistent with single-column null handling).
        assert!(encode_composite_index_key(&types, &[Value::Integer(1), Value::Null], 1).is_none());
    }

    #[test]
    fn test_composite_prefix_is_key_without_rowid() {
        use crate::value::{Type, Value};
        let types = [Type::Integer, Type::Integer];
        let full = encode_composite_index_key(&types, &[Value::Integer(5), Value::Integer(9)], 7).unwrap();
        let prefix = encode_composite_value_prefix(&types, &[Value::Integer(5), Value::Integer(9)]).unwrap();
        assert!(full.starts_with(&prefix));
        assert_eq!(full.len(), prefix.len() + 8); // rowid is 8 bytes appended
    }

    #[test]
    fn test_integer_key_sort_order() {
        // Negative < zero < positive under byte comparison
        let k_neg = encode_index_key_integer(-100, 1);
        let k_zero = encode_index_key_integer(0, 1);
        let k_pos = encode_index_key_integer(100, 1);

        assert!(k_neg < k_zero, "negative should sort before zero");
        assert!(k_zero < k_pos, "zero should sort before positive");
    }

    #[test]
    fn test_integer_key_same_value_different_rowids() {
        let k1 = encode_index_key_integer(42, 1);
        let k2 = encode_index_key_integer(42, 2);
        let k3 = encode_index_key_integer(42, 100);

        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn test_integer_key_wide_range() {
        let k_min = encode_index_key_integer(i64::MIN, 1);
        let k_neg1 = encode_index_key_integer(-1, 1);
        let k_zero = encode_index_key_integer(0, 1);
        let k_pos1 = encode_index_key_integer(1, 1);
        let k_max = encode_index_key_integer(i64::MAX, 1);

        assert!(k_min < k_neg1);
        assert!(k_neg1 < k_zero);
        assert!(k_zero < k_pos1);
        assert!(k_pos1 < k_max);
    }

    #[test]
    fn test_text_key_sort_order() {
        let k_a = encode_index_key_text("apple", 1).unwrap();
        let k_b = encode_index_key_text("banana", 1).unwrap();
        let k_c = encode_index_key_text("cherry", 1).unwrap();

        assert!(k_a < k_b, "apple should sort before banana");
        assert!(k_b < k_c, "banana should sort before cherry");
    }

    #[test]
    fn test_text_key_same_value_different_rowids() {
        let k1 = encode_index_key_text("hello", 1).unwrap();
        let k2 = encode_index_key_text("hello", 2).unwrap();
        let k3 = encode_index_key_text("hello", 999).unwrap();

        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn test_text_key_prefix_ordering() {
        // "ab" < "abc" because after matching "ab", the next byte of the
        // shorter key is 0x00 (null terminator) while "abc" has 'c' (0x63).
        let k_short = encode_index_key_text("ab", 1).unwrap();
        let k_long = encode_index_key_text("abc", 1).unwrap();
        assert!(k_short < k_long);
    }

    #[test]
    fn test_real_key_sort_order() {
        let k_neg = encode_index_key_real(-1.5, 1);
        let k_zero = encode_index_key_real(0.0, 1);
        let k_pos = encode_index_key_real(1.5, 1);

        assert!(k_neg < k_zero, "negative should sort before zero");
        assert!(k_zero < k_pos, "zero should sort before positive");
    }

    #[test]
    fn test_real_key_wide_range() {
        let k_neg_inf = encode_index_key_real(f64::NEG_INFINITY, 1);
        let k_neg = encode_index_key_real(-1000.0, 1);
        let k_neg_zero = encode_index_key_real(-0.0, 1);
        let k_zero = encode_index_key_real(0.0, 1);
        let k_pos = encode_index_key_real(1000.0, 1);
        let k_pos_inf = encode_index_key_real(f64::INFINITY, 1);

        assert!(k_neg_inf < k_neg);
        assert!(k_neg < k_neg_zero);
        // -0.0 and +0.0 should sort to the same position (or at least be adjacent)
        assert!(k_neg_zero <= k_zero);
        assert!(k_zero < k_pos);
        assert!(k_pos < k_pos_inf);
    }

    #[test]
    fn test_real_key_same_value_different_rowids() {
        let k1 = encode_index_key_real(3.14, 1);
        let k2 = encode_index_key_real(3.14, 2);
        assert!(k1 < k2);
    }

    // --- Prefix matching tests ---

    #[test]
    fn test_integer_prefix_matching() {
        let prefix = encode_integer_prefix(42);
        let key1 = encode_index_key_integer(42, 1);
        let key2 = encode_index_key_integer(42, 100);
        let key3 = encode_index_key_integer(43, 1);

        assert!(key1.starts_with(&prefix));
        assert!(key2.starts_with(&prefix));
        assert!(!key3.starts_with(&prefix));
    }

    #[test]
    fn test_text_prefix_matching() {
        let prefix = encode_text_prefix("hello");
        let key1 = encode_index_key_text("hello", 1).unwrap();
        let key2 = encode_index_key_text("hello", 999).unwrap();
        let key3 = encode_index_key_text("world", 1).unwrap();

        assert!(key1.starts_with(&prefix));
        assert!(key2.starts_with(&prefix));
        assert!(!key3.starts_with(&prefix));
    }

    #[test]
    fn test_real_prefix_matching() {
        let prefix = encode_real_prefix(3.14);
        let key1 = encode_index_key_real(3.14, 1);
        let key2 = encode_index_key_real(3.14, 50);
        let key3 = encode_index_key_real(2.71, 1);

        assert!(key1.starts_with(&prefix));
        assert!(key2.starts_with(&prefix));
        assert!(!key3.starts_with(&prefix));
    }

    // --- Extract rowid tests ---

    #[test]
    fn test_extract_rowid_integer() {
        let key = encode_index_key_integer(42, 12345);
        assert_eq!(extract_rowid(Type::Integer, &key), 12345);
    }

    #[test]
    fn test_extract_rowid_real() {
        let key = encode_index_key_real(3.14, 99999);
        assert_eq!(extract_rowid(Type::Real, &key), 99999);
    }

    #[test]
    fn test_extract_rowid_text() {
        let key = encode_index_key_text("hello", 42).unwrap();
        assert_eq!(extract_rowid(Type::Text, &key), 42);
    }

    // --- encode_index_key dispatch tests ---

    #[test]
    fn test_encode_index_key_null_returns_none() {
        assert!(encode_index_key(Type::Integer, &Value::Null, 1).is_none());
        assert!(encode_index_key(Type::Text, &Value::Null, 1).is_none());
        assert!(encode_index_key(Type::Real, &Value::Null, 1).is_none());
    }

    #[test]
    fn test_encode_index_key_type_dispatch() {
        let int_key = encode_index_key(Type::Integer, &Value::Integer(42), 1);
        assert!(int_key.is_some());
        assert_eq!(int_key.unwrap().len(), 16);

        let real_key = encode_index_key(Type::Real, &Value::Real(3.14), 1);
        assert!(real_key.is_some());
        assert_eq!(real_key.unwrap().len(), 16);

        let text_key = encode_index_key(Type::Text, &Value::Text("hi".into()), 1);
        assert!(text_key.is_some());
        // "hi" (2 bytes) + 0x00 (1 byte) + rowid (8 bytes) = 11
        assert_eq!(text_key.unwrap().len(), 11);
    }

    // --- IndexTree tests ---

    #[test]
    fn test_index_tree_insert_and_scan_prefix() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert keys for value=42, rowids 1,2,3
            for rowid in 1..=3u64 {
                let key = encode_index_key_integer(42, rowid);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }

            // Also insert value=99, rowid=10
            {
                let key = encode_index_key_integer(99, 10);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        // Scan prefix for value=42
        let prefix = encode_integer_prefix(42);
        let reader = IndexTreeReader::new(&pf, root);
        let results = reader.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 3);

        // Verify rowids
        for (i, key) in results.iter().enumerate() {
            let rowid = extract_rowid(Type::Integer, key);
            assert_eq!(rowid, (i + 1) as u64);
        }

        // Scan prefix for value=99
        let prefix = encode_integer_prefix(99);
        let results = reader.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(extract_rowid(Type::Integer, &results[0]), 10);
    }

    #[test]
    fn test_index_tree_delete() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert 3 keys
            for rowid in 1..=3u64 {
                let key = encode_index_key_integer(42, rowid);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }

            // Delete middle key (rowid=2)
            {
                let key = encode_index_key_integer(42, 2);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                assert!(tree.delete(&key).unwrap());
            }
            guard.commit().unwrap();
        }

        // Verify only 2 remain
        let prefix = encode_integer_prefix(42);
        let reader = IndexTreeReader::new(&pf, root);
        let results = reader.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(extract_rowid(Type::Integer, &results[0]), 1);
        assert_eq!(extract_rowid(Type::Integer, &results[1]), 3);
    }

    #[test]
    fn test_index_tree_delete_nonexistent() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        let mut guard = pf.begin_write();
        let key = encode_index_key_integer(42, 1);
        let mut tree = IndexTreeWriter::new(&mut guard, root);
        assert!(!tree.delete(&key).unwrap());
        guard.commit().unwrap();
    }

    #[test]
    fn test_index_tree_many_inserts_trigger_splits() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert 200 keys -- enough to trigger multiple splits.
            for i in 0..200u64 {
                let key = encode_index_key_integer(i as i64, i);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        // Verify all keys are findable via scan_prefix
        let reader = IndexTreeReader::new(&pf, root);
        for i in 0..200u64 {
            let prefix = encode_integer_prefix(i as i64);
            let results = reader.scan_prefix(&prefix).unwrap();
            assert_eq!(results.len(), 1, "missing key for value {i}");
            assert_eq!(extract_rowid(Type::Integer, &results[0]), i);
        }
    }

    #[test]
    fn test_index_tree_many_same_value_inserts() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // 150 rows all with the same column value (42), different rowids
            for rowid in 0..150u64 {
                let key = encode_index_key_integer(42, rowid);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        // scan_prefix should return all 150
        let prefix = encode_integer_prefix(42);
        let reader = IndexTreeReader::new(&pf, root);
        let results = reader.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 150);

        // Verify sorted by rowid
        for (i, key) in results.iter().enumerate() {
            assert_eq!(extract_rowid(Type::Integer, key), i as u64);
        }
    }

    #[test]
    fn test_index_tree_with_text_keys() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            let words = ["apple", "banana", "cherry", "apple", "banana"];
            for (rowid, word) in words.iter().enumerate() {
                let key = encode_index_key_text(word, rowid as u64).unwrap();
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);

        // Scan for "apple" -- should get rowids 0, 3
        let prefix = encode_text_prefix("apple");
        let results = reader.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(extract_rowid(Type::Text, &results[0]), 0);
        assert_eq!(extract_rowid(Type::Text, &results[1]), 3);

        // Scan for "banana" -- should get rowids 1, 4
        let prefix = encode_text_prefix("banana");
        let results = reader.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(extract_rowid(Type::Text, &results[0]), 1);
        assert_eq!(extract_rowid(Type::Text, &results[1]), 4);
    }

    #[test]
    fn test_index_tree_text_many_inserts_trigger_splits() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert 100 different text values.
            for i in 0..100u64 {
                let text = format!("item_{:04}", i);
                let key = encode_index_key_text(&text, i).unwrap();
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        // Verify each is findable
        let reader = IndexTreeReader::new(&pf, root);
        for i in 0..100u64 {
            let text = format!("item_{:04}", i);
            let prefix = encode_text_prefix(&text);
            let results = reader.scan_prefix(&prefix).unwrap();
            assert_eq!(results.len(), 1, "missing key for text {text}");
            assert_eq!(extract_rowid(Type::Text, &results[0]), i);
        }
    }

    #[test]
    fn test_index_tree_delete_after_splits() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert enough to cause splits
            for i in 0..100u64 {
                let key = encode_index_key_integer(i as i64, i);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        // Delete every other key
        {
            let mut guard = pf.begin_write();
            for i in (0..100u64).step_by(2) {
                let key = encode_index_key_integer(i as i64, i);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                assert!(tree.delete(&key).unwrap(), "should find key {i}");
            }
            guard.commit().unwrap();
        }

        // Verify only odd keys remain
        let reader = IndexTreeReader::new(&pf, root);
        for i in 0..100u64 {
            let prefix = encode_integer_prefix(i as i64);
            let results = reader.scan_prefix(&prefix).unwrap();
            if i % 2 == 0 {
                assert_eq!(results.len(), 0, "key {i} should be deleted");
            } else {
                assert_eq!(results.len(), 1, "key {i} should still exist");
            }
        }
    }

    #[test]
    fn test_encode_value_prefix_returns_none_for_null() {
        assert!(encode_value_prefix(Type::Integer, &Value::Null).is_none());
        assert!(encode_value_prefix(Type::Text, &Value::Null).is_none());
        assert!(encode_value_prefix(Type::Real, &Value::Null).is_none());
    }

    #[test]
    fn test_index_tree_real_keys_with_splits() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert real-valued keys including negatives
            let values: Vec<f64> = (-50..50).map(|i| i as f64 * 0.1).collect();
            for (rowid, &val) in values.iter().enumerate() {
                let key = encode_index_key_real(val, rowid as u64);
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        // Verify each is findable
        let reader = IndexTreeReader::new(&pf, root);
        let values: Vec<f64> = (-50..50).map(|i| i as f64 * 0.1).collect();
        for (rowid, &val) in values.iter().enumerate() {
            let prefix = encode_real_prefix(val);
            let results = reader.scan_prefix(&prefix).unwrap();
            assert!(
                !results.is_empty(),
                "missing key for real value {val} (rowid {rowid})"
            );
            let found_rowid = extract_rowid(Type::Real, &results[0]);
            assert_eq!(found_rowid, rowid as u64);
        }
    }

    // --- count_prefix / scan_prefix_limit tests ---

    fn extract_rowid_integer(key: &[u8]) -> u64 {
        extract_rowid(Type::Integer, key)
    }

    #[test]
    fn test_count_prefix() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();
            let mut tree = IndexTreeWriter::new(&mut guard, root);

            for rowid in 1..=5u64 {
                let key = encode_index_key_integer(42, rowid);
                tree.insert(&key).unwrap();
            }
            for rowid in 1..=3u64 {
                let key = encode_index_key_integer(99, rowid);
                tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);
        let prefix_42 = encode_integer_prefix(42);
        assert_eq!(reader.count_prefix(&prefix_42).unwrap(), 5);
        let prefix_99 = encode_integer_prefix(99);
        assert_eq!(reader.count_prefix(&prefix_99).unwrap(), 3);
        let prefix_0 = encode_integer_prefix(0);
        assert_eq!(reader.count_prefix(&prefix_0).unwrap(), 0);
    }

    #[test]
    fn test_count_prefix_text() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();
            let mut tree = IndexTreeWriter::new(&mut guard, root);

            for rowid in 1..=10u64 {
                tree.insert(&encode_index_key_text("alice", rowid).unwrap()).unwrap();
            }
            for rowid in 1..=7u64 {
                tree.insert(&encode_index_key_text("bob", rowid).unwrap()).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);
        assert_eq!(reader.count_prefix(&encode_text_prefix("alice")).unwrap(), 10);
        assert_eq!(reader.count_prefix(&encode_text_prefix("bob")).unwrap(), 7);
        assert_eq!(reader.count_prefix(&encode_text_prefix("charlie")).unwrap(), 0);
    }

    #[test]
    fn test_scan_prefix_limit() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();
            let mut tree = IndexTreeWriter::new(&mut guard, root);

            for rowid in 1..=100u64 {
                tree.insert(&encode_index_key_integer(42, rowid)).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);

        // Limit to 5
        let prefix = encode_integer_prefix(42);
        let results = reader.scan_prefix_limit(&prefix, 5).unwrap();
        assert_eq!(results.len(), 5);
        // Should be first 5 rowids
        assert_eq!(extract_rowid_integer(&results[0]), 1);
        assert_eq!(extract_rowid_integer(&results[4]), 5);

        // Limit higher than available
        let results = reader.scan_prefix_limit(&prefix, 200).unwrap();
        assert_eq!(results.len(), 100);
    }

    // --- Composite key edge cases ---

    #[test]
    fn test_integer_key_extreme_negative() {
        let k_min = encode_index_key_integer(i64::MIN, 1);
        let k_min_plus1 = encode_index_key_integer(i64::MIN + 1, 1);
        assert!(k_min < k_min_plus1, "i64::MIN should sort before i64::MIN+1");
    }

    #[test]
    fn test_integer_key_around_zero() {
        let k_neg1 = encode_index_key_integer(-1, 1);
        let k_zero = encode_index_key_integer(0, 1);
        let k_pos1 = encode_index_key_integer(1, 1);
        assert!(k_neg1 < k_zero);
        assert!(k_zero < k_pos1);
    }

    #[test]
    fn test_text_key_empty_string() {
        let k_empty = encode_index_key_text("", 1).unwrap();
        let k_a = encode_index_key_text("a", 1).unwrap();
        // Empty string: just null terminator + rowid
        assert!(k_empty < k_a, "empty string should sort before 'a'");
    }

    #[test]
    fn test_text_key_empty_string_different_rowids() {
        let k1 = encode_index_key_text("", 1).unwrap();
        let k2 = encode_index_key_text("", 2).unwrap();
        assert!(k1 < k2);
    }

    #[test]
    fn test_text_key_long_value() {
        // Long text value that would exceed branch key truncation limit (36 bytes)
        let long_text = "a".repeat(100);
        let k = encode_index_key_text(&long_text, 1).unwrap();
        // Should be: 100 bytes text + 1 null terminator + 8 rowid = 109 bytes
        assert_eq!(k.len(), 100 + 1 + 8);
        assert_eq!(extract_rowid(Type::Text, &k), 1);
    }

    #[test]
    fn test_text_key_unicode() {
        let k_emoji = encode_index_key_text("hello", 1).unwrap();
        let k_z = encode_index_key_text("zzz", 1).unwrap();
        // Both should round-trip through extract_rowid
        assert_eq!(extract_rowid(Type::Text, &k_emoji), 1);
        assert_eq!(extract_rowid(Type::Text, &k_z), 1);
    }

    #[test]
    fn test_real_key_negative_zero_vs_positive_zero() {
        let k_neg_zero = encode_index_key_real(-0.0, 1);
        let k_pos_zero = encode_index_key_real(0.0, 1);
        // -0.0 and +0.0 should be adjacent in sort order
        assert!(k_neg_zero <= k_pos_zero);
    }

    #[test]
    fn test_real_key_very_small_positive() {
        let k_tiny = encode_index_key_real(f64::MIN_POSITIVE, 1);
        let k_zero = encode_index_key_real(0.0, 1);
        let k_one = encode_index_key_real(1.0, 1);
        assert!(k_zero < k_tiny);
        assert!(k_tiny < k_one);
    }

    #[test]
    fn test_integer_key_rowid_max() {
        // Test with u64::MAX rowid
        let k = encode_index_key_integer(42, u64::MAX);
        assert_eq!(extract_rowid(Type::Integer, &k), u64::MAX);
    }

    #[test]
    fn test_integer_key_rowid_zero() {
        let k = encode_index_key_integer(42, 0);
        assert_eq!(extract_rowid(Type::Integer, &k), 0);
    }

    #[test]
    fn test_encode_index_key_cross_type_coercion() {
        // Integer column with Real value -> coerced
        let key = encode_index_key(Type::Integer, &Value::Real(42.0), 1);
        assert!(key.is_some());

        // Real column with Integer value -> coerced
        let key = encode_index_key(Type::Real, &Value::Integer(42), 1);
        assert!(key.is_some());

        // Unsupported type combination -> None
        let key = encode_index_key(Type::Text, &Value::Integer(42), 1);
        assert!(key.is_none());

        let key = encode_index_key(Type::Integer, &Value::Text("42".into()), 1);
        assert!(key.is_none());
    }

    #[test]
    fn test_encode_value_prefix_cross_type_coercion() {
        // Integer column with Real value
        let prefix = encode_value_prefix(Type::Integer, &Value::Real(42.0));
        assert!(prefix.is_some());

        // Real column with Integer value
        let prefix = encode_value_prefix(Type::Real, &Value::Integer(42));
        assert!(prefix.is_some());
    }

    // --- IndexTree with long text keys that trigger branch truncation ---

    #[test]
    fn test_index_tree_long_text_keys_trigger_splits() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();

            // Insert keys with long text values (exceeding the 36-byte branch key limit)
            for i in 0..50u64 {
                let text = format!("long_key_value_that_exceeds_branch_limit_{:04}", i);
                let key = encode_index_key_text(&text, i).unwrap();
                let mut tree = IndexTreeWriter::new(&mut guard, root);
                root = tree.insert(&key).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);
        for i in 0..50u64 {
            let text = format!("long_key_value_that_exceeds_branch_limit_{:04}", i);
            let prefix = encode_text_prefix(&text);
            let results = reader.scan_prefix(&prefix).unwrap();
            assert_eq!(results.len(), 1, "missing key for text '{text}'");
            assert_eq!(extract_rowid(Type::Text, &results[0]), i);
        }
    }

    #[test]
    fn test_scan_prefix_empty_tree() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);
        let prefix = encode_integer_prefix(42);
        let results = reader.scan_prefix(&prefix).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_count_prefix_empty_tree() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = IndexTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        let reader = IndexTreeReader::new(&pf, root);
        let prefix = encode_integer_prefix(42);
        assert_eq!(reader.count_prefix(&prefix).unwrap(), 0);
    }
}
