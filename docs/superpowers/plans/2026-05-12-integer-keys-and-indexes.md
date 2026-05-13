# Integer Keys & Index Redesign — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace string (UUID) keys with u64 integer keys and redesign secondary indexes as composite-key B+ trees.

**Architecture:** The data B+ tree switches to fixed 8-byte u64 keys (LE in rows, BE in branch keys for sort order). A new `IndexTree` (byte-key B+ tree in `src/index.rs`) handles secondary indexes with composite `(encoded_value, rowid)` keys that sort correctly via `memcmp`. The existing chunked ID-list index code is removed entirely.

**Tech Stack:** Rust, existing boogy-db crate (crc32fast, tempfile for tests). The `uuid` crate is removed.

**Spec:** `docs/superpowers/specs/2026-05-12-integer-keys-and-indexes-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/row.rs` | Modify | Row format: `[rowid:8]` replaces `[id_len:2][id_bytes]` |
| `src/btree.rs` | Modify | Data tree: u64 keys, fixed 12-byte branch entries |
| `src/table.rs` | Modify | Add `next_rowid: u64` to `TableMeta` |
| `src/error.rs` | Modify | `DuplicateKey(u64)`, add `TypeMismatch` |
| `src/db.rs` | Modify | `Row.id: u64`, auto-increment, remove old index code, wire up new indexes |
| `src/index.rs` | Create | Composite key encoding + `IndexTree` (byte-key B+ tree) |
| `src/lib.rs` | Modify | Add `pub mod index;` |
| `Cargo.toml` | Modify | Remove `uuid` dependency |
| `tests/crud_test.rs` | Modify | Update all tests for u64 IDs |
| `benches/point_ops.rs` | Modify | Update for u64 IDs |
| `benches/sqlite_comparison.rs` | Modify | Update boogy side for u64 IDs (SQLite side keeps uuid) |

---

## Task 1: Row Format — u64 Rowid

**Files:**
- Modify: `src/row.rs`

The row format changes from `[id_len:2][id_bytes:variable]` to `[rowid:8]` (u64 LE, fixed). This affects `encode_row`, `decode_row`, `extract_id`, and `extract_column`.

- [ ] **Step 1: Write failing tests for u64 row format**

Replace all existing tests in `src/row.rs` to use `u64` rowids. The key changes:
- `encode_row("row_1", &cols)` → `encode_row(1, &cols)`
- `decoded.id` is `u64` not `String`
- `extract_id` returns `Result<u64>`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_round_trip() {
        let v0 = Value::Text("alice".into());
        let v1 = Value::Integer(42);
        let v2 = Value::Real(3.14);
        let v3 = Value::Boolean(true);
        let v4 = Value::Null;
        let cols = vec![
            (0u16, &v0),
            (1, &v1),
            (2, &v2),
            (3, &v3),
            (4, &v4),
        ];
        let encoded = encode_row(1, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.id, 1u64);
        assert_eq!(decoded.columns.len(), 5);
        assert_eq!(decoded.columns[0], (0, Value::Text("alice".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(42)));
        assert_eq!(decoded.columns[3], (3, Value::Boolean(true)));
        assert_eq!(decoded.columns[4], (4, Value::Null));
    }

    #[test]
    fn test_extract_id() {
        let encoded = encode_row(42, &[(0, &Value::Integer(1))]);
        assert_eq!(extract_id(&encoded).unwrap(), 42u64);
    }

    #[test]
    fn test_extract_column() {
        let v0 = Value::Text("alice".into());
        let v1 = Value::Integer(42);
        let v2 = Value::Boolean(false);
        let cols = vec![(0u16, &v0), (1, &v1), (2, &v2)];
        let encoded = encode_row(1, &cols);
        assert_eq!(extract_column(&encoded, 1).unwrap(), Some(Value::Integer(42)));
        assert_eq!(extract_column(&encoded, 2).unwrap(), Some(Value::Boolean(false)));
        assert_eq!(extract_column(&encoded, 99).unwrap(), None);
    }

    #[test]
    fn test_extract_column_binary_search() {
        let values: Vec<Value> = (0..20).map(|i| Value::Integer(i * 100)).collect();
        let cols: Vec<(u16, &Value)> = values.iter().enumerate().map(|(i, v)| (i as u16, v)).collect();
        let encoded = encode_row(1, &cols);
        assert_eq!(extract_column(&encoded, 19).unwrap(), Some(Value::Integer(1900)));
        assert_eq!(extract_column(&encoded, 0).unwrap(), Some(Value::Integer(0)));
        assert_eq!(extract_column(&encoded, 10).unwrap(), Some(Value::Integer(1000)));
        assert_eq!(extract_column(&encoded, 20).unwrap(), None);
    }

    #[test]
    fn test_blob_round_trip() {
        let blob_data = vec![0xFF, 0x00, 0xAB, 0xCD];
        let v0 = Value::Blob(blob_data.clone());
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row(1, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Blob(blob_data));
    }

    #[test]
    fn test_empty_string() {
        let v0 = Value::Text(String::new());
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row(1, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Text(String::new()));
    }

    #[test]
    fn test_unsorted_columns_get_sorted() {
        let v0 = Value::Integer(100);
        let v1 = Value::Integer(200);
        let v2 = Value::Integer(300);
        let cols = vec![(2u16, &v2), (0, &v0), (1, &v1)];
        let encoded = encode_row(1, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0], (0, Value::Integer(100)));
        assert_eq!(decoded.columns[1], (1, Value::Integer(200)));
        assert_eq!(decoded.columns[2], (2, Value::Integer(300)));
    }
}
```

- [ ] **Step 2: Update encode_row, DecodedRow, decode_row, extract_id, extract_column**

Change `encode_row` signature from `(id: &str, ...)` to `(rowid: u64, ...)`. The row now starts with `[rowid:8]` (u64 LE) instead of `[id_len:2][id_bytes]`.

```rust
/// Encode a row (rowid + columns) into compact binary format with offset directory.
///
/// Layout:
///   [rowid:8]  (u64 little-endian)
///   [num_cols:2]
///   [offset_directory: num_cols × 4 bytes]
///     for each column (sorted by col_id): [col_id:2][data_offset:2]
///   [column_data]
///     for each column: [type_tag:1][value_bytes]
pub fn encode_row(rowid: u64, columns: &[(u16, &Value)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&rowid.to_le_bytes());
    // ... rest unchanged from current encode_row (num_cols, offset directory, column data)
    buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());
    let mut sorted: Vec<(u16, &Value)> = columns.to_vec();
    sorted.sort_by_key(|(id, _)| *id);
    let mut col_data = Vec::with_capacity(48);
    let mut offsets: Vec<(u16, u16)> = Vec::with_capacity(sorted.len());
    for &(col_id, val) in &sorted {
        let data_offset = col_data.len() as u16;
        offsets.push((col_id, data_offset));
        encode_value(&mut col_data, val);
    }
    for &(col_id, data_offset) in &offsets {
        buf.extend_from_slice(&col_id.to_le_bytes());
        buf.extend_from_slice(&data_offset.to_le_bytes());
    }
    buf.extend_from_slice(&col_data);
    buf
}

pub struct DecodedRow {
    pub id: u64,  // was String
    pub columns: Vec<(u16, Value)>,
}

pub fn decode_row(data: &[u8]) -> Result<DecodedRow> {
    let mut offset = 0;
    // rowid
    ensure_bytes(data, offset, 8)?;
    let id = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;
    // ... rest unchanged (num_cols, offset directory, column data)
    // just use `id` as u64 instead of String
}

pub fn extract_id(data: &[u8]) -> Result<u64> {
    ensure_bytes(data, 0, 8)?;
    Ok(u64::from_le_bytes(data[0..8].try_into().unwrap()))
}

pub fn extract_column(data: &[u8], target_col_id: u16) -> Result<Option<Value>> {
    let mut offset = 0;
    // Skip rowid (fixed 8 bytes instead of variable id_len)
    ensure_bytes(data, offset, 8)?;
    offset += 8;
    // ... rest unchanged (num_cols, binary search offset directory)
}
```

- [ ] **Step 3: Run row.rs tests**

Run: `cargo test --lib row::tests`
Expected: All 7 row tests pass. Other modules (btree, db) will have compile errors — that's expected, we fix them in subsequent tasks.

- [ ] **Step 4: Commit**

```bash
git add src/row.rs
git commit -m "refactor: row format uses u64 rowid instead of string id"
```

---

## Task 2: B+ Tree — u64 Keys

**Files:**
- Modify: `src/btree.rs`

Convert the data B+ tree from string keys to u64 keys. Branch entries become fixed 12-byte `[child:4][key:8]`. All key comparisons use `u64::cmp`. Remove `scan_prefix` and `find_leaf_for_prefix` (only used by old index code).

- [ ] **Step 1: Update all BTree method signatures and InsertResult**

```rust
// Public methods:
pub fn insert(&mut self, rowid: u64, row_data: &[u8]) -> Result<u32>
pub fn search(&mut self, rowid: u64) -> Result<Option<Vec<u8>>>
pub fn delete(&mut self, rowid: u64) -> Result<bool>
pub fn scan_all(&mut self) -> Result<Vec<(u64, Vec<u8>)>>
pub fn scan_filtered(...) -> Result<(Vec<(u64, Vec<u8>)>, u64)>
// count_filtered unchanged (doesn't return IDs)

// Internal:
fn insert_recursive(&mut self, page_no: u32, rowid: u64, row_data: &[u8]) -> Result<InsertResult>
fn insert_into_leaf(&mut self, page_no: u32, page: &Page, rowid: u64, row_data: &[u8]) -> Result<InsertResult>
fn insert_into_branch(&mut self, page_no: u32, child_idx: usize, separator: u64, new_child: u32) -> Result<InsertResult>
fn search_recursive(&mut self, page_no: u32, rowid: u64) -> Result<Option<Vec<u8>>>
fn delete_recursive(&mut self, page_no: u32, rowid: u64) -> Result<bool>

enum InsertResult {
    Fit,
    Split { new_page: u32, separator: u64 },  // was String
}
```

- [ ] **Step 2: Update branch page format to fixed-size u64 keys**

Branch entries change from 42 bytes `[child:4][key_len:2][key_data:36]` to 12 bytes `[child:4][key:8]`.

```rust
const BRANCH_ENTRY_SIZE: usize = 12; // child:4 + key:8

fn get_branch_child(page: &Page, idx: usize) -> u32 {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap())
}

fn get_branch_key(page: &Page, idx: usize) -> u64 {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE + 4;
    u64::from_le_bytes(page.data[offset..offset + 8].try_into().unwrap())
}

fn find_child(page: &Page, rowid: u64) -> (usize, u32) {
    let num_keys = page.num_rows() as usize;
    for i in 0..num_keys {
        let key = get_branch_key(page, i);
        if rowid < key {
            return (i, get_branch_child(page, i));
        }
    }
    let last_child_offset = PAGE_HEADER_SIZE + num_keys * BRANCH_ENTRY_SIZE;
    let child = u32::from_le_bytes(
        page.data[last_child_offset..last_child_offset + 4].try_into().unwrap(),
    );
    (num_keys, child)
}

fn write_branch_entry(page: &mut Page, idx: usize, child: u32, key: u64) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
    page.data[offset + 4..offset + 12].copy_from_slice(&key.to_le_bytes());
}

fn set_branch_child(page: &mut Page, idx: usize, child: u32) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
}
```

Update `collect_branch_flat` to return `(Vec<u32>, Vec<u64>)` and `rebuild_branch_flat` to take `&[u64]` keys. Update `insert_branch_entry` similarly.

- [ ] **Step 3: Update leaf page binary search to use u64**

```rust
fn find_insertion_point(page: &Page, rowid: u64) -> Result<(usize, bool)> {
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
        match mid_id.cmp(&rowid) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok((mid, true)),
        }
    }
    Ok((lo, false))
}
```

- [ ] **Step 4: Update extract_id_at_virtual_pos to return u64**

```rust
fn extract_id_at_virtual_pos(
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
    virtual_pos: usize,
) -> Result<u64> {
    if virtual_pos == insert_pos {
        row::extract_id(new_row)
    } else {
        let orig_idx = if virtual_pos < insert_pos { virtual_pos } else { virtual_pos - 1 };
        let (s, e) = row_bounds_raw(snapshot, orig_idx, old_count);
        row::extract_id(&snapshot[s..e])
    }
}
```

- [ ] **Step 5: Update insert_into_leaf and insert_into_branch**

In `insert_into_leaf`: change `id: &str` to `rowid: u64`, change `find_insertion_point(page, id)` to `find_insertion_point(page, rowid)`, change `DuplicateKey(id.to_string())` to `DuplicateKey(rowid)`.

In `insert_into_branch`: change `separator: &str` to `separator: u64`.

In `insert` (the public method): update the root split to use `u64` separator — `write_branch_entry(&mut root_page, 0, self.root, separator)` (no `&`).

- [ ] **Step 6: Update scan_all and scan_filtered to return u64 IDs**

`scan_all`: change return to `Vec<(u64, Vec<u8>)>`. Replace `row::extract_id(data)` → `row::extract_id(data)?` (returns u64), push `(id, data.to_vec())`.

`scan_filtered`: same change — return `Vec<(u64, Vec<u8>)>`, use `row::extract_id(data)?` which returns `u64`.

- [ ] **Step 7: Remove scan_prefix and find_leaf_for_prefix**

Delete `scan_prefix` and `find_leaf_for_prefix` methods entirely — they were only used by the old string-key index system.

- [ ] **Step 8: Update btree tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::row;
    use crate::value::Value;
    use tempfile::NamedTempFile;

    fn make_row(rowid: u64, name: &str) -> Vec<u8> {
        row::encode_row(rowid, &[(0, &Value::Text(name.into()))])
    }

    #[test]
    fn test_insert_and_search() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);
        let row = make_row(1, "alice");
        tree.insert(1, &row).unwrap();
        let found = tree.search(1).unwrap();
        assert!(found.is_some());
        let decoded = row::decode_row(&found.unwrap()).unwrap();
        assert_eq!(decoded.id, 1);
        assert_eq!(decoded.columns[0].1, Value::Text("alice".into()));
    }

    #[test]
    fn test_search_not_found() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);
        assert!(tree.search(999).unwrap().is_none());
    }

    #[test]
    fn test_duplicate_key_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);
        let row = make_row(1, "alice");
        tree.insert(1, &row).unwrap();
        assert!(tree.insert(1, &row).is_err());
    }

    #[test]
    fn test_many_inserts_trigger_split() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();
        for i in 0..100u64 {
            let row = make_row(i, &format!("name_{i}"));
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(i, &row).unwrap();
        }
        let mut tree = BTree::new(&mut pf, root);
        for i in 0..100u64 {
            assert!(tree.search(i).unwrap().is_some(), "missing: {i}");
        }
    }

    #[test]
    fn test_delete() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);
        let row = make_row(1, "alice");
        tree.insert(1, &row).unwrap();
        assert!(tree.delete(1).unwrap());
        assert!(tree.search(1).unwrap().is_none());
        assert!(!tree.delete(1).unwrap());
    }

    #[test]
    fn test_scan_all() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();
        for i in 0..20u64 {
            let row = make_row(i, &format!("name_{i}"));
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(i, &row).unwrap();
        }
        let mut tree = BTree::new(&mut pf, root);
        let all = tree.scan_all().unwrap();
        assert_eq!(all.len(), 20);
    }

    #[test]
    fn test_500_inserts_separate_tree_instances() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();
        for i in 0..500u64 {
            let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(i, &row).unwrap();
            pf.flush().unwrap();
        }
        for i in 0..500u64 {
            let mut tree = BTree::new(&mut pf, root);
            let result = tree.search(i).unwrap();
            assert!(result.is_some(), "missing row at i={i}");
        }
    }
}
```

- [ ] **Step 9: Run btree tests**

Run: `cargo test --lib btree::tests`
Expected: All 7 btree tests pass.

- [ ] **Step 10: Commit**

```bash
git add src/btree.rs
git commit -m "refactor: B+ tree uses u64 keys, fixed 12-byte branch entries"
```

---

## Task 3: Table Metadata & Error Types

**Files:**
- Modify: `src/table.rs`
- Modify: `src/error.rs`

- [ ] **Step 1: Add next_rowid to TableMeta**

In `src/table.rs`, add `next_rowid: u64` to `TableMeta`:

```rust
pub struct TableMeta {
    pub name: String,
    pub table_id: u32,
    pub columns: Vec<ColumnDef>,
    pub col_name_to_id: HashMap<String, u16>,
    pub root_page: u32,
    pub row_count: u64,
    pub next_rowid: u64,  // NEW
    pub indexes: Vec<IndexMeta>,
}
```

Update `TableMeta::new` to initialize `next_rowid: 1`.

- [ ] **Step 2: Update error types**

In `src/error.rs`:
- Change `DuplicateKey(String)` to `DuplicateKey(u64)`
- Add `TypeMismatch(String)` variant for index type enforcement

```rust
pub enum BoogyError {
    // ... existing variants ...
    DuplicateKey(u64),      // was DuplicateKey(String)
    TypeMismatch(String),   // NEW: indexed column type mismatch
    // ... rest unchanged ...
}
```

Update the `Display` impl for both variants.

- [ ] **Step 3: Commit**

```bash
git add src/table.rs src/error.rs
git commit -m "refactor: add next_rowid to TableMeta, DuplicateKey(u64), TypeMismatch error"
```

---

## Task 4: Core DB API — u64 Keys & Auto-Increment

**Files:**
- Modify: `src/db.rs`

This is the largest task. It updates the public API to u64, adds auto-increment, updates the system page format, and removes ALL old index code (which will be replaced in Task 6).

- [ ] **Step 1: Update Row struct and decoded_to_row**

```rust
#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,  // was String
    pub columns: Vec<(String, Value)>,
}

fn decoded_to_row(decoded: &row::DecodedRow, meta: &TableMeta) -> Row {
    let columns: Vec<(String, Value)> = decoded
        .columns
        .iter()
        .filter_map(|(col_id, val)| {
            meta.columns
                .get(*col_id as usize)
                .map(|def| (def.name.clone(), val.clone()))
        })
        .collect();
    Row {
        id: decoded.id,  // u64 now, no .clone() needed
        columns,
    }
}
```

- [ ] **Step 2: Update system page serialization**

Add `next_rowid: u64` after `row_count` in the system page format. Update the comment at the top of db.rs too.

In `serialize_system_page`, after writing `row_count`:
```rust
// next_rowid
data[offset..offset + 8].copy_from_slice(&meta.next_rowid.to_le_bytes());
offset += 8;
```

In `deserialize_system_page`, after reading `row_count`:
```rust
let next_rowid = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
offset += 8;
// ... later:
meta.next_rowid = next_rowid;
```

- [ ] **Step 3: Remove ALL old index helper methods from BoogyDb**

Delete these methods entirely — they will be replaced by the new index system in Task 6:
- `value_to_key_string`
- `chunk_key`
- `encode_id_list_as_entry`
- `decode_id_list`
- `read_all_chunks`
- `delete_all_chunks`
- `write_chunked_ids`
- `index_add`
- `index_remove`
- `index_lookup`
- The constants `INDEX_IDS_PER_CHUNK`

- [ ] **Step 4: Update insert() — auto-increment, returns u64**

```rust
pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
    let table_state = {
        let tables = self.tables.read().unwrap();
        tables.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone()
    };
    let mut state = table_state.write().unwrap();

    // Auto-increment rowid
    let rowid = state.meta.next_rowid;
    state.meta.next_rowid += 1;

    let col_values: Vec<(u16, &Value)> = data
        .iter()
        .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
        .collect();
    let row_bytes = row::encode_row(rowid, &col_values);

    let durability = self.durability();
    let new_root = {
        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, state.meta.root_page);
        let new_root = tree.insert(rowid, &row_bytes)?;

        // Index maintenance — temporarily disabled, re-enabled in Task 6
        // For now, just skip index updates since the old code is removed.

        if matches!(durability, Durability::None) {
            file.take_before_images();
        } else {
            let mut wal = self.wal.lock().unwrap();
            Self::commit_with_wal(&mut file, &mut wal, durability, state.meta.table_id)?;
        }
        new_root
    };

    if new_root != state.meta.root_page {
        state.meta.root_page = new_root;
    }
    state.meta.row_count += 1;
    Ok(rowid)
}
```

- [ ] **Step 5: Add insert_with_id()**

```rust
/// Insert a row with a caller-supplied rowid.
/// If rowid >= next_rowid, bumps next_rowid to rowid + 1.
pub fn insert_with_id(&self, table: &str, rowid: u64, data: &[(&str, Value)]) -> Result<()> {
    let table_state = {
        let tables = self.tables.read().unwrap();
        tables.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone()
    };
    let mut state = table_state.write().unwrap();

    if rowid >= state.meta.next_rowid {
        state.meta.next_rowid = rowid + 1;
    }

    let col_values: Vec<(u16, &Value)> = data
        .iter()
        .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
        .collect();
    let row_bytes = row::encode_row(rowid, &col_values);

    let durability = self.durability();
    let new_root = {
        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, state.meta.root_page);
        let new_root = tree.insert(rowid, &row_bytes)?;

        if matches!(durability, Durability::None) {
            file.take_before_images();
        } else {
            let mut wal = self.wal.lock().unwrap();
            Self::commit_with_wal(&mut file, &mut wal, durability, state.meta.table_id)?;
        }
        new_root
    };

    if new_root != state.meta.root_page {
        state.meta.root_page = new_root;
    }
    state.meta.row_count += 1;
    Ok(())
}
```

- [ ] **Step 6: Update get(), update(), delete() to take u64**

```rust
pub fn get(&self, table: &str, id: u64) -> Result<Option<Row>>
pub fn update(&self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool>
pub fn delete(&self, table: &str, id: u64) -> Result<bool>
```

In each method, replace `id: &str` with `id: u64`. Internal calls like `tree.search(id)`, `tree.delete(id)`, `tree.insert(id, &new_row)` all work directly since BTree now takes u64.

In `update()`: the re-insert after delete uses the same `id` (u64). The `row::encode_row(id, &col_values)` call works directly.

In `delete()`: reading the row for index maintenance uses `tree.search(id)` which returns `Option<Vec<u8>>`. The decoded row has `decoded.id` as u64.

**Important:** Temporarily comment out or stub the index maintenance code in `update()` and `delete()` (the loops over `state.meta.indexes`). It will be re-wired in Task 6.

- [ ] **Step 7: Update insert_many() to return Vec<u64>**

```rust
pub fn insert_many(&self, table: &str, rows: &[Vec<(&str, Value)>]) -> Result<Vec<u64>> {
    // ... same structure, but:
    // - use auto-increment: let rowid = state.meta.next_rowid; state.meta.next_rowid += 1;
    // - row::encode_row(rowid, &col_values)
    // - tree.insert(rowid, &row_bytes)
    // - ids.push(rowid)
    // - skip index maintenance for now
}
```

- [ ] **Step 8: Update update_where() and delete_where()**

These methods scan rows and use decoded row IDs internally. Update:
- `to_update` type: `Vec<(u64, HashMap<u16, Value>)>` (was `Vec<(String, ...)>`)
- `to_delete` type: same
- `tree.delete(&id)` → `tree.delete(id)` (u64, no &)
- `tree.insert(&id, &new_row)` → `tree.insert(id, &new_row)`
- `row::encode_row(&id, &col_values)` → `row::encode_row(id, &col_values)`
- Skip index maintenance loops for now

- [ ] **Step 9: Update TransactionCtx**

```rust
impl<'a> TransactionCtx<'a> {
    pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        self.db.insert(table, data)
    }
    pub fn get(&self, table: &str, id: u64) -> Result<Option<Row>> {
        self.db.get(table, id)
    }
    pub fn update(&self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
        self.db.update(table, id, fields)
    }
    pub fn delete(&self, table: &str, id: u64) -> Result<bool> {
        self.db.delete(table, id)
    }
}
```

- [ ] **Step 10: Update find() and count()**

In `find()`:
- The index path is temporarily disabled (old `index_lookup` is removed). Replace the index candidate block with a `// TODO: re-enable in Task 6` comment and fall through to the scan path.
- `scan_all` now returns `(u64, Vec<u8>)` — update pattern matches.
- `scan_filtered` now returns `(u64, Vec<u8>)` — update pattern matches.

In `count()`: no changes needed beyond what the scan methods already handle.

- [ ] **Step 11: Update db.rs tests**

Update all tests in `src/db.rs`:
- `db.insert(...)` now returns `u64` not `String`
- `db.get("t", id)` takes `u64` not `&str` / `&id`
- `db.update("t", id, ...)` takes `u64`
- `db.delete("t", id)` takes `u64`
- `row.id` is `u64`
- Remove/comment out index-specific tests (test_index_basic_roundtrip) — they'll be rewritten in Task 6

- [ ] **Step 12: Run all tests**

Run: `cargo test --lib`
Expected: All non-index tests pass. Index tests should be removed or marked `#[ignore]`.

- [ ] **Step 13: Commit**

```bash
git add src/db.rs
git commit -m "refactor: db API uses u64 keys, auto-increment rowid, remove old index code"
```

---

## Task 5: Index Module — Composite Key Encoding & IndexTree

**Files:**
- Create: `src/index.rs`
- Modify: `src/lib.rs` (add `pub mod index;`)

This creates a new byte-key B+ tree for secondary indexes. The IndexTree stores composite keys `(encoded_value, rowid)` as entries in leaf pages. Branch pages use variable-length key prefixes for routing.

### Subtask 5a: Composite Key Encoding

- [ ] **Step 1: Write failing tests for key encoding sort order**

```rust
// In src/index.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_integer_key_sort_order() {
        // Negative, zero, positive should sort correctly via memcmp
        let k1 = encode_index_key_integer(-100, 1);
        let k2 = encode_index_key_integer(0, 1);
        let k3 = encode_index_key_integer(100, 1);
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn test_integer_key_same_value_different_rowid() {
        let k1 = encode_index_key_integer(42, 1);
        let k2 = encode_index_key_integer(42, 2);
        assert!(k1 < k2);
        // Same value prefix
        assert_eq!(&k1[..8], &k2[..8]);
    }

    #[test]
    fn test_text_key_sort_order() {
        let k1 = encode_index_key_text("apple", 1);
        let k2 = encode_index_key_text("banana", 1);
        assert!(k1 < k2);
    }

    #[test]
    fn test_text_key_same_value_different_rowid() {
        let k1 = encode_index_key_text("hello", 1);
        let k2 = encode_index_key_text("hello", 2);
        assert!(k1 < k2);
    }

    #[test]
    fn test_real_key_sort_order() {
        let k1 = encode_index_key_real(-1.5, 1);
        let k2 = encode_index_key_real(0.0, 1);
        let k3 = encode_index_key_real(1.5, 1);
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn test_integer_prefix_matches() {
        let key = encode_index_key_integer(42, 99);
        let prefix = encode_integer_prefix(42);
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn test_text_prefix_matches() {
        let key = encode_index_key_text("hello", 99);
        let prefix = encode_text_prefix("hello");
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn test_extract_rowid_from_integer_key() {
        let key = encode_index_key_integer(42, 777);
        assert_eq!(extract_rowid_integer(&key), 777);
    }

    #[test]
    fn test_extract_rowid_from_text_key() {
        let key = encode_index_key_text("hello", 777);
        assert_eq!(extract_rowid_text(&key), 777);
    }
}
```

- [ ] **Step 2: Implement composite key encoding functions**

```rust
use crate::error::Result;
use crate::file::PageFile;
use crate::page::{Page, PAGE_BRANCH, PAGE_HEADER_SIZE, PAGE_LEAF, PAGE_SIZE};
use crate::value::{Type, Value};

// --- Composite key encoding ---
// All encodings produce byte sequences that sort correctly via memcmp.
// Format: [encoded_value][rowid:8 big-endian]

/// Encode an integer index key. Total: 16 bytes.
/// i64 → big-endian with sign bit flipped (XOR 0x80 on first byte).
pub fn encode_index_key_integer(val: i64, rowid: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    let mut be = val.to_be_bytes();
    be[0] ^= 0x80; // flip sign bit for unsigned sort order
    key.extend_from_slice(&be);
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

/// Encode a float index key. Total: 16 bytes.
/// f64 → big-endian. Positive: flip sign bit. Negative: flip all bits.
pub fn encode_index_key_real(val: f64, rowid: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    let mut be = val.to_be_bytes();
    if val.is_sign_negative() {
        // Flip all bits for negative values
        for b in &mut be { *b ^= 0xFF; }
    } else {
        // Flip sign bit for positive values (and +0.0)
        be[0] ^= 0x80;
    }
    key.extend_from_slice(&be);
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

/// Encode a text index key. Variable length: utf8_bytes + 0x00 + rowid:8.
pub fn encode_index_key_text(val: &str, rowid: u64) -> Vec<u8> {
    let bytes = val.as_bytes();
    let mut key = Vec::with_capacity(bytes.len() + 1 + 8);
    key.extend_from_slice(bytes);
    key.push(0x00); // null separator
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

/// Encode a Value as an index key based on column type.
pub fn encode_index_key(col_type: Type, val: &Value, rowid: u64) -> Option<Vec<u8>> {
    match (col_type, val) {
        (Type::Integer, Value::Integer(i)) => Some(encode_index_key_integer(*i, rowid)),
        (Type::Real, Value::Real(f)) => Some(encode_index_key_real(*f, rowid)),
        (Type::Text, Value::Text(s)) => Some(encode_index_key_text(s, rowid)),
        (_, Value::Null) => None, // nulls not indexed
        _ => None, // type mismatch — caller should reject
    }
}

/// Prefix for integer value (for range/eq scans). 8 bytes.
pub fn encode_integer_prefix(val: i64) -> Vec<u8> {
    let mut be = val.to_be_bytes();
    be[0] ^= 0x80;
    be.to_vec()
}

/// Prefix for text value (for range/eq scans). utf8_bytes + 0x00.
pub fn encode_text_prefix(val: &str) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(val.len() + 1);
    prefix.extend_from_slice(val.as_bytes());
    prefix.push(0x00);
    prefix
}

/// Prefix for real value (for range/eq scans). 8 bytes.
pub fn encode_real_prefix(val: f64) -> Vec<u8> {
    let mut be = val.to_be_bytes();
    if val.is_sign_negative() {
        for b in &mut be { *b ^= 0xFF; }
    } else {
        be[0] ^= 0x80;
    }
    be.to_vec()
}

/// Encode a Value as a prefix for index scanning.
pub fn encode_value_prefix(col_type: Type, val: &Value) -> Option<Vec<u8>> {
    match (col_type, val) {
        (Type::Integer, Value::Integer(i)) => Some(encode_integer_prefix(*i)),
        (Type::Real, Value::Real(f)) => Some(encode_real_prefix(*f)),
        (Type::Text, Value::Text(s)) => Some(encode_text_prefix(s)),
        _ => None,
    }
}

/// Extract rowid from a 16-byte integer or real index key.
pub fn extract_rowid_integer(key: &[u8]) -> u64 {
    u64::from_be_bytes(key[8..16].try_into().unwrap())
}

/// Alias — same layout as integer keys.
pub fn extract_rowid_real(key: &[u8]) -> u64 {
    u64::from_be_bytes(key[8..16].try_into().unwrap())
}

/// Extract rowid from a text index key (last 8 bytes).
pub fn extract_rowid_text(key: &[u8]) -> u64 {
    u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap())
}

/// Extract rowid from an index key, given the column type.
pub fn extract_rowid(col_type: Type, key: &[u8]) -> u64 {
    match col_type {
        Type::Integer | Type::Real => u64::from_be_bytes(key[8..16].try_into().unwrap()),
        Type::Text => u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap()),
        _ => panic!("unsupported index type"),
    }
}
```

- [ ] **Step 3: Run encoding tests**

Run: `cargo test --lib index::tests`
Expected: All 9 encoding tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/index.rs src/lib.rs
git commit -m "feat: composite key encoding for secondary indexes"
```

### Subtask 5b: IndexTree — Byte-Key B+ Tree

- [ ] **Step 5: Write failing tests for IndexTree**

Add to the tests module in `src/index.rs`:

```rust
    #[test]
    fn test_index_tree_insert_and_scan() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = IndexTree::create(&mut pf).unwrap();
        let mut tree = IndexTree::new(&mut pf, root);

        // Insert 3 entries for value=42 with different rowids
        for rowid in [3u64, 1, 2] {
            let key = encode_index_key_integer(42, rowid);
            tree.insert(&key).unwrap();
        }

        // Prefix scan should return all 3, sorted by rowid
        let prefix = encode_integer_prefix(42);
        let results = tree.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(extract_rowid_integer(&results[0]), 1);
        assert_eq!(extract_rowid_integer(&results[1]), 2);
        assert_eq!(extract_rowid_integer(&results[2]), 3);
    }

    #[test]
    fn test_index_tree_delete() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = IndexTree::create(&mut pf).unwrap();
        let mut tree = IndexTree::new(&mut pf, root);

        let k1 = encode_index_key_integer(42, 1);
        let k2 = encode_index_key_integer(42, 2);
        tree.insert(&k1).unwrap();
        tree.insert(&k2).unwrap();

        assert!(tree.delete(&k1).unwrap());
        let prefix = encode_integer_prefix(42);
        let results = tree.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(extract_rowid_integer(&results[0]), 2);
    }

    #[test]
    fn test_index_tree_many_inserts() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = IndexTree::create(&mut pf).unwrap();

        // Insert 200 entries across 10 different values
        for i in 0..200u64 {
            let val = (i % 10) as i64;
            let key = encode_index_key_integer(val, i);
            let mut tree = IndexTree::new(&mut pf, root);
            root = tree.insert(&key).unwrap();
        }

        // Each value should have 20 entries
        let mut tree = IndexTree::new(&mut pf, root);
        let prefix = encode_integer_prefix(5);
        let results = tree.scan_prefix(&prefix).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn test_index_tree_text_keys() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = IndexTree::create(&mut pf).unwrap();
        let mut tree = IndexTree::new(&mut pf, root);

        for rowid in 1..=5u64 {
            let key = encode_index_key_text("alice", rowid);
            tree.insert(&key).unwrap();
        }
        for rowid in 1..=3u64 {
            let key = encode_index_key_text("bob", rowid);
            tree.insert(&key).unwrap();
        }

        let alice_prefix = encode_text_prefix("alice");
        let results = tree.scan_prefix(&alice_prefix).unwrap();
        assert_eq!(results.len(), 5);

        let bob_prefix = encode_text_prefix("bob");
        let results = tree.scan_prefix(&bob_prefix).unwrap();
        assert_eq!(results.len(), 3);
    }
```

- [ ] **Step 6: Implement IndexTree**

The IndexTree is a B+ tree that stores raw byte keys as entries. It's adapted from the original string-key BTree. Leaf pages store entries as `[len:2][key_bytes]` packed sequentially. Branch pages use `[child:4][key_len:2][key_data:36]` (42-byte entries, same as the original BTree branches, keys truncated to 36 bytes for routing).

```rust
const CHECKSUM_SIZE: usize = 4;
const IDX_BRANCH_ENTRY_SIZE: usize = 42; // child:4 + key_len:2 + key_data:36

/// A B+ tree for secondary index entries (byte-comparable keys, no payload).
pub struct IndexTree<'a> {
    file: &'a mut PageFile,
    root: u32,
}

impl<'a> IndexTree<'a> {
    pub fn new(file: &'a mut PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    pub fn create(file: &mut PageFile) -> Result<u32> {
        let page_no = file.allocate_page()?;
        let page = Page::new_leaf();
        file.put_page(page_no, page);
        Ok(page_no)
    }

    /// Insert a key. Returns the (possibly new) root page number.
    pub fn insert(&mut self, key: &[u8]) -> Result<u32> {
        // Encode key as an entry: [len:2][key_bytes]
        let entry = encode_entry(key);
        let result = self.insert_recursive(self.root, key, &entry)?;
        match result {
            IdxInsertResult::Fit => Ok(self.root),
            IdxInsertResult::Split { new_page, separator } => {
                let new_root = self.file.allocate_page()?;
                let mut root_page = Page::new_branch();
                write_idx_branch_entry(&mut root_page, 0, self.root, &separator);
                set_idx_branch_child(&mut root_page, 1, new_page);
                root_page.set_num_rows(1);
                root_page.update_checksum();
                self.file.put_page(new_root, root_page);
                self.root = new_root;
                Ok(self.root)
            }
        }
    }

    /// Delete a key. Returns true if it existed.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.delete_recursive(self.root, key)
    }

    /// Scan all keys that start with `prefix`. Returns matching keys in order.
    pub fn scan_prefix(&mut self, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        let (leaf_page, start_idx) = self.find_leaf_for_prefix(self.root, prefix)?;
        let mut results = Vec::new();
        let mut current = leaf_page;
        let mut skip = start_idx;
        loop {
            let page = self.file.read_page(current)?.clone();
            let num_rows = page.num_rows() as usize;
            for i in skip..num_rows {
                let (start, end) = idx_row_bounds(&page, i, num_rows);
                if start < end && end <= PAGE_SIZE {
                    let entry = &page.data[start..end];
                    if let Some(k) = decode_entry(entry) {
                        if k.starts_with(prefix) {
                            results.push(k.to_vec());
                        } else {
                            return Ok(results);
                        }
                    }
                }
            }
            skip = 0;
            let next = page.next_leaf();
            if next == 0 { break; }
            current = next;
        }
        Ok(results)
    }

    // --- Internal methods ---
    // Adapted from BTree, using byte-slice key comparison everywhere.
    // Key functions:
    //   find_idx_insertion_point: binary search on leaf entries using memcmp
    //   find_idx_child: navigate branch pages using memcmp
    //   find_leaf_for_prefix: find first leaf where keys >= prefix
    //   insert_recursive, insert_into_leaf, insert_into_branch: same split logic
    //   delete_recursive: same delete logic
    //
    // Entry format in leaf pages: [len:2][key_bytes]
    // extract_entry_key: reads len, returns &[u8] slice of the key
    //
    // The implementation follows the same structure as BTree but with:
    //   - id: &str → key: &[u8]
    //   - string comparison → slice comparison (.cmp())
    //   - String separator → Vec<u8> separator
    //   - row::extract_id → decode_entry
}

/// Leaf entry format: [len:2][key_bytes]
fn encode_entry(key: &[u8]) -> Vec<u8> {
    let mut entry = Vec::with_capacity(2 + key.len());
    entry.extend_from_slice(&(key.len() as u16).to_le_bytes());
    entry.extend_from_slice(key);
    entry
}

fn decode_entry(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 2 { return None; }
    let len = u16::from_le_bytes([data[0], data[1]]) as usize;
    if 2 + len > data.len() { return None; }
    Some(&data[2..2 + len])
}

// The remaining helper functions (idx_row_bounds, find_idx_insertion_point,
// find_idx_child, write_idx_branch_entry, etc.) follow the same patterns
// as BTree's helpers but use byte-slice keys. The branch format is identical
// to the original BTree's 42-byte entries.
```

The full implementation of `IndexTree`'s internal methods (`insert_recursive`, `insert_into_leaf`, `insert_into_branch`, `delete_recursive`, `find_leaf_for_prefix`, and all the branch/leaf helpers) follows the exact same algorithmic structure as `BTree`'s methods. The differences are mechanical:
- `&str` → `&[u8]` for keys
- `String` → `Vec<u8>` for owned keys
- `mid_id.cmp(id)` → `mid_key.cmp(key)` (byte slice comparison)
- `row::extract_id(data)` → `decode_entry(data)` to get key bytes
- `DuplicateKey` → use a generic error (index entries should never duplicate since the rowid makes each key unique)

- [ ] **Step 7: Add `pub mod index;` to lib.rs**

In `src/lib.rs`, add `pub mod index;` after the other module declarations.

- [ ] **Step 8: Run IndexTree tests**

Run: `cargo test --lib index::tests`
Expected: All 13 tests pass (9 encoding + 4 IndexTree).

- [ ] **Step 9: Commit**

```bash
git add src/index.rs src/lib.rs
git commit -m "feat: IndexTree — byte-key B+ tree for secondary indexes"
```

---

## Task 6: Wire Up New Index System in db.rs

**Files:**
- Modify: `src/db.rs`

Re-implement index maintenance using the new `IndexTree` and composite key encoding.

- [ ] **Step 1: Add index helper methods to BoogyDb**

```rust
use crate::index::{self, IndexTree};

impl BoogyDb {
    /// Update all indexes for a row, using the encoded row bytes to extract column values.
    /// Used by both insert and delete paths.
    fn index_update_row(
        file: &mut PageFile,
        meta: &mut TableMeta,
        rowid: u64,
        row_bytes: &[u8],
        remove: bool,  // true = delete from index, false = insert into index
    ) -> Result<()> {
        for idx in &mut meta.indexes {
            let col_id = meta.col_name_to_id.get(&idx.column).copied();
            let col_type = meta.columns.iter()
                .find(|c| c.name == idx.column)
                .map(|c| c.col_type);
            if let (Some(cid), Some(ct)) = (col_id, col_type) {
                let val = crate::row::extract_column(row_bytes, cid)?
                    .unwrap_or(Value::Null);
                if let Some(key) = index::encode_index_key(ct, &val, rowid) {
                    let mut tree = IndexTree::new(file, idx.root_page);
                    if remove {
                        tree.delete(&key)?;
                    } else {
                        tree.insert(&key)?;
                    }
                    idx.root_page = tree.root_page();
                }
            }
        }
        Ok(())
    }

    /// Look up rowids matching a filter value using an index.
    fn index_lookup_eq(
        file: &mut PageFile,
        idx_meta: &IndexMeta,
        col_type: Type,
        filter_val: &Value,
    ) -> Result<Vec<u64>> {
        let prefix = match index::encode_value_prefix(col_type, filter_val) {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        let mut tree = IndexTree::new(file, idx_meta.root_page);
        let keys = tree.scan_prefix(&prefix)?;
        Ok(keys.iter().map(|k| index::extract_rowid(col_type, k)).collect())
    }
}
```

- [ ] **Step 2: Wire index maintenance into insert()**

In `insert()`, after the `tree.insert(rowid, &row_bytes)?` call, add:

```rust
if !state.meta.indexes.is_empty() {
    Self::index_update_row(&mut file, &mut state.meta, rowid, &row_bytes, false)?;
}
```

- [ ] **Step 3: Wire index maintenance into delete()**

In `delete()`, before `tree.delete(id)`, read the row for index removal:

```rust
let row_bytes = if !state.meta.indexes.is_empty() {
    let mut tree = BTree::new(&mut file, state.meta.root_page);
    tree.search(id)?
} else {
    None
};

let mut tree = BTree::new(&mut file, state.meta.root_page);
let deleted = tree.delete(id)?;

if deleted {
    if let Some(bytes) = &row_bytes {
        Self::index_update_row(&mut file, &mut state.meta, id, bytes, true)?;
    }
}
```

- [ ] **Step 4: Wire index maintenance into update()**

In `update()`, delete old index entries using old row bytes, then insert new ones using the new row bytes:

```rust
// Read existing row
let existing_bytes = match tree.search(id)? { Some(b) => b, None => return Ok(false) };
// ... merge columns, encode new_row, delete + reinsert ...

// Index maintenance: remove old entries, add new entries
if !state.meta.indexes.is_empty() {
    Self::index_update_row(&mut file, &mut state.meta, id, &existing_bytes, true)?;
    Self::index_update_row(&mut file, &mut state.meta, id, &new_row, false)?;
}
```

- [ ] **Step 5: Wire index maintenance into insert_many(), update_where(), delete_where()**

Same pattern: call `index_insert_row` after each insert, `index_delete_row` before each delete, both for updates.

- [ ] **Step 6: Update create_index() to use IndexTree**

```rust
pub fn create_index(&self, table: &str, index_name: &str, column: &str) -> Result<()> {
    // ... existing validation ...

    let col_type = state.meta.columns.iter()
        .find(|c| c.name == column)
        .map(|c| c.col_type)
        .ok_or_else(|| BoogyError::SchemaMismatch(
            format!("column '{column}' not found in table '{table}'")
        ))?;

    let idx_root = {
        let mut file = self.file.lock().unwrap();
        let mut wal = self.wal.lock().unwrap();
        let durability = self.durability();

        let idx_root = IndexTree::create(&mut file)?;

        // Scan all existing rows and populate the index
        let all = {
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            tree.scan_all()?
        };

        let col_id = state.meta.col_id(column).unwrap();
        let mut current_root = idx_root;
        for (rowid, bytes) in &all {
            let col_val = row::extract_column(bytes, col_id)?
                .unwrap_or(Value::Null);
            if let Some(key) = index::encode_index_key(col_type, &col_val, *rowid) {
                let mut tree = IndexTree::new(&mut file, current_root);
                current_root = tree.insert(&key)?;
            }
        }

        Self::commit_with_wal(&mut file, &mut wal, durability, state.meta.table_id)?;
        current_root
    };
    // ... register index in metadata, persist registry ...
}
```

- [ ] **Step 7: Add type enforcement on insert/update**

In `insert()` and `update()`, before writing, check that values for indexed columns match the column's declared type. Also reject NaN for real columns and 0x00 bytes in text columns (per spec):

```rust
// In insert(), after encoding the row but before tree.insert():
for idx in &state.meta.indexes {
    if let Some(val) = data.iter().find(|(name, _)| *name == idx.column).map(|(_, v)| v) {
        let col_type = state.meta.columns.iter()
            .find(|c| c.name == idx.column)
            .map(|c| c.col_type);
        if let Some(ct) = col_type {
            if !val.is_null() && val.value_type() != Some(ct) {
                return Err(BoogyError::TypeMismatch(format!(
                    "column '{}' expects {:?}, got {:?}",
                    idx.column, ct, val.value_type()
                )));
            }
            // Reject NaN for real indexes (can't sort)
            if let Value::Real(f) = val {
                if f.is_nan() {
                    return Err(BoogyError::TypeMismatch(format!(
                        "column '{}': NaN not supported in indexed columns", idx.column
                    )));
                }
            }
            // Reject 0x00 bytes in text indexes (used as separator)
            if let Value::Text(s) = val {
                if s.as_bytes().contains(&0x00) {
                    return Err(BoogyError::TypeMismatch(format!(
                        "column '{}': null bytes not supported in indexed text columns", idx.column
                    )));
                }
            }
        }
    }
}
```

- [ ] **Step 8: Re-enable index-aware find()**

In `find()`, restore the index candidate check and use the new `index_lookup_eq`:

```rust
let index_candidate = opts.filters.iter().find(|f| {
    f.op == FilterOp::Eq
        && state.meta.find_index_for_column(&f.column).is_some()
});

let (matching, total) = if let Some(idx_filter) = index_candidate {
    let idx_meta = state.meta.find_index_for_column(&idx_filter.column).unwrap();
    let col_type = state.meta.columns.iter()
        .find(|c| c.name == idx_filter.column)
        .map(|c| c.col_type)
        .unwrap();
    let mut file = self.file.lock().unwrap();
    let matching_rowids = Self::index_lookup_eq(
        &mut file, idx_meta, col_type, &idx_filter.value
    )?;

    let mut rows = Vec::with_capacity(matching_rowids.len());
    for rowid in &matching_rowids {
        let mut tree = BTree::new(&mut file, state.meta.root_page);
        if let Some(bytes) = tree.search(*rowid)? {
            let decoded = row::decode_row(&bytes)?;
            rows.push(decoded_to_row(&decoded, &state.meta));
        }
    }
    let total = rows.len() as u64;
    (rows, total)
} else if opts.filters.len() == 1 {
    // ... existing scan_filtered path ...
```

- [ ] **Step 9: Restore index tests in db.rs**

Un-ignore `test_index_basic_roundtrip` and update it for u64 IDs. All other index tests in `tests/crud_test.rs` should also pass once the wiring is complete.

- [ ] **Step 10: Run all tests**

Run: `cargo test`
Expected: All tests pass, including index tests.

- [ ] **Step 11: Commit**

```bash
git add src/db.rs
git commit -m "feat: wire up composite-key indexes via IndexTree"
```

---

## Task 7: Update External Tests & Benchmarks

**Files:**
- Modify: `tests/crud_test.rs`
- Modify: `benches/point_ops.rs`
- Modify: `benches/sqlite_comparison.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Update tests/crud_test.rs**

All test changes follow this pattern:
- `db.insert(...)` returns `u64` → remove `.unwrap()` on string operations, use `let id: u64 = ...`
- `db.get("t", &id)` → `db.get("t", id)` (u64, not &str)
- `db.update("t", &id, ...)` → `db.update("t", id, ...)`
- `db.delete("t", &id)` → `db.delete("t", id)`
- `row.id` is `u64` — comparisons like `assert_eq!(row.id, id)` work directly
- `test_get_not_found`: change `db.get("users", "nonexistent")` → `db.get("users", 999999)`
- `test_table_not_found`: change `db.get("nonexistent", "id")` → `db.get("nonexistent", 0)`
- `test_index_maintained_on_update`: `db.update("t", &id, ...)` → `db.update("t", id, ...)`
- `test_index_maintained_on_delete`: `db.delete("t", &id)` → `db.delete("t", id)`

- [ ] **Step 2: Update benches/point_ops.rs**

- `ids` type: `Vec<u64>` instead of `Vec<String>`
- `db.get("t", &ids[...])` → `db.get("t", ids[...])`

```rust
let mut ids: Vec<u64> = Vec::new();
// ...
ids.push(db.insert("t", &[("v", Value::Integer(i))]).unwrap());
// ...
let _ = db.get("t", ids[i % ids.len()]).unwrap();
```

- [ ] **Step 3: Update benches/sqlite_comparison.rs**

The boogy-db side:
- `boogy_ids` type: `Vec<u64>` instead of `Vec<String>`
- `db.get("notes", &ids[idx])` → `db.get("notes", ids[idx])`
- `run_boogy` signature: `ids: &mut Vec<u64>`

The SQLite side stays the same (still uses UUIDs for a fair comparison of the SQL engine).

- [ ] **Step 4: Remove uuid from Cargo.toml dependencies**

In `Cargo.toml`, remove `uuid = { version = "1", features = ["v4"] }` from `[dependencies]`. Keep it in `[dev-dependencies]` if benchmarks need it for the SQLite side. Actually, the `sqlite_comparison.rs` bench uses `uuid::Uuid::new_v4()` for the SQLite side, so move it to dev-dependencies:

```toml
[dependencies]
crc32fast = "1"

[dev-dependencies]
tempfile = "3"
rand = "0.9"
uuid = { version = "1", features = ["v4"] }
rusqlite = { version = "0.38", features = ["bundled"] }
```

- [ ] **Step 5: Run all tests and benchmarks compile-check**

Run: `cargo test`
Run: `cargo bench --no-run` (just compile, don't run benchmarks)
Expected: All tests pass, benchmarks compile.

- [ ] **Step 6: Commit**

```bash
git add tests/ benches/ Cargo.toml
git commit -m "refactor: update tests and benchmarks for u64 keys, remove uuid from deps"
```

---

## Task 8: Final Cleanup & Verification

- [ ] **Step 1: Remove dead code warnings**

Run `cargo clippy` and fix any warnings from the refactor (unused imports, dead code, etc.).

- [ ] **Step 2: Run full test suite**

Run: `cargo test`
Expected: All tests pass (should be 27+ tests).

- [ ] **Step 3: Quick smoke-test benchmark**

Run: `cargo bench --bench point_ops`
Expected: Should show significantly better insert/get performance than before (u64 comparisons vs UUID string comparisons, higher fanout from smaller branch entries).

- [ ] **Step 4: Commit any cleanup**

```bash
git add -A
git commit -m "chore: cleanup dead code and warnings after integer key migration"
```

- [ ] **Step 5: Update design.md**

Update `docs/design.md` to reflect the new row format, branch format, and index architecture. Key changes:
- Row format section: `[rowid:8]` instead of `[_id_len:2][_id_bytes]`
- Branch page section: `[child:4][key:8]` instead of `[child:4][key_len:2][key_bytes]`
- Public API section: `insert()` returns `u64`, `get/update/delete` take `u64`
- Secondary indexes section: describe composite-key IndexTree

- [ ] **Step 6: Commit design doc update**

```bash
git add docs/design.md
git commit -m "docs: update design.md for integer keys and composite-key indexes"
```
