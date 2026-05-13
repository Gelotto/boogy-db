# Bulk Operations Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Speed up `delete_where` and `update_where` by walking the leaf chain once with batch page rebuilds, instead of per-row B+ tree delete+reinsert.

**Architecture:** Add `delete_matching` and `update_matching` methods to BTreeWriter that walk all leaf pages via next_leaf pointers, evaluate a predicate on each row, and rebuild modified pages in a single pass. `delete_where` and `update_where` in db.rs call these instead of per-row tree operations.

**Tech Stack:** Rust, existing boogy-db crate.

**Spec:** `docs/superpowers/specs/2026-05-13-bulk-ops-optimization-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/btree.rs` | Modify | Add `delete_matching`, `update_matching`, leaf rebuild helpers to BTreeWriter |
| `src/db.rs` | Modify | Rewrite `delete_where` and `update_where` to use batch methods |

---

## Task 1: Leaf Rebuild Helpers + delete_matching

**Files:**
- Modify: `src/btree.rs`

Add two leaf page helpers and the `delete_matching` method to BTreeWriter.

- [ ] **Step 1: Add write_leaf_without_multiple helper**

Add this free function near the existing `write_leaf_without`:

```rust
/// Rebuild a leaf page excluding rows at the given indices (must be sorted ascending).
fn write_leaf_without_multiple(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    skip_indices: &[usize],
) {
    let total = old_count - skip_indices.len();
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
    let mut skip_ptr = 0usize;

    for src_idx in 0..old_count {
        if skip_ptr < skip_indices.len() && skip_indices[skip_ptr] == src_idx {
            skip_ptr += 1;
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
    page.update_checksum();
}
```

- [ ] **Step 2: Add find_leftmost_leaf_w to BTreeWriter**

BTreeWriter needs a leaf-chain entry point. Add this internal method:

```rust
fn find_leftmost_leaf_w(&self, page_no: u32) -> Result<u32> {
    let page = (*self.guard.read_page(page_no)?).clone();
    if page.is_leaf() {
        Ok(page_no)
    } else {
        let child = get_branch_child(&page, 0);
        self.find_leftmost_leaf_w(child)
    }
}
```

- [ ] **Step 3: Add delete_matching to BTreeWriter**

```rust
/// Walk all leaf pages, delete rows where `pred(row_bytes)` returns true.
/// Returns Vec<(rowid, old_row_bytes)> for each deleted row (needed for index maintenance).
/// Much faster than per-row delete() for bulk operations.
pub fn delete_matching<F>(&mut self, pred: F) -> Result<Vec<(u64, Vec<u8>)>>
where
    F: Fn(&[u8]) -> bool,
{
    let first_leaf = self.find_leftmost_leaf_w(self.root)?;
    let mut deleted = Vec::new();
    let mut current = first_leaf;

    loop {
        let page = (*self.guard.read_page(current)?).clone();
        let num_rows = page.num_rows() as usize;
        let next = page.next_leaf();

        // Find which rows on this page match the predicate
        let mut skip_indices = Vec::new();
        for i in 0..num_rows {
            let (start, end) = row_bounds_raw(&page.data, i, num_rows);
            if start >= end || end > PAGE_SIZE {
                continue;
            }
            let data = &page.data[start..end];
            if pred(data) {
                if let Ok(rowid) = row::extract_id(data) {
                    deleted.push((rowid, data.to_vec()));
                    skip_indices.push(i);
                }
            }
        }

        // Rebuild page if any rows were deleted
        if !skip_indices.is_empty() {
            let snapshot = page.data;
            let wp = self.guard.write_page(current)?;
            write_leaf_without_multiple(wp, &snapshot, num_rows, &skip_indices);
        }

        if next == 0 {
            break;
        }
        current = next;
    }

    Ok(deleted)
}
```

- [ ] **Step 4: Write test for delete_matching**

Add to btree tests:

```rust
#[test]
fn test_delete_matching() {
    let tmp = NamedTempFile::new().unwrap();
    let pf = PageFile::open(tmp.path()).unwrap();

    // Insert 100 rows
    let mut root = {
        let mut guard = pf.begin_write();
        let root = BTreeWriter::create(&mut guard).unwrap();
        let mut tree = BTreeWriter::new(&mut guard, root);
        for i in 0..100u64 {
            let row = row::encode_row(i, &[(0, &Value::Integer(i as i64 % 10))]);
            tree.insert(i, &row).unwrap();
        }
        guard.commit(false).unwrap();
        root
    };

    // Delete all rows where value column == 5 (should be 10 rows)
    let deleted = {
        let mut guard = pf.begin_write();
        let mut tree = BTreeWriter::new(&mut guard, root);
        let result = tree.delete_matching(|row_bytes| {
            row::extract_column(row_bytes, 0)
                .ok()
                .flatten()
                .map(|v| v == Value::Integer(5))
                .unwrap_or(false)
        }).unwrap();
        guard.commit(false).unwrap();
        result
    };

    assert_eq!(deleted.len(), 10);

    // Verify 90 rows remain
    let reader = BTreeReader::new(&pf, root);
    let all = reader.scan_all().unwrap();
    assert_eq!(all.len(), 90);

    // Verify none of the remaining rows have value == 5
    for (_, bytes) in &all {
        let val = row::extract_column(bytes, 0).unwrap().unwrap();
        assert_ne!(val, Value::Integer(5));
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib btree::tests`
Expected: All tests pass including the new one.

- [ ] **Step 6: Commit**

```bash
git add src/btree.rs
git commit -m "feat: BTreeWriter::delete_matching — bulk delete via leaf-chain walk"
```

---

## Task 2: update_matching

**Files:**
- Modify: `src/btree.rs`

- [ ] **Step 1: Add write_leaf_with_replacements helper**

```rust
/// Rebuild a leaf page with some rows replaced by new data.
/// `replacements` is sorted by row index: (row_index, new_row_bytes).
/// Returns false if the rebuilt page would exceed PAGE_SIZE (overflow).
fn write_leaf_with_replacements(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    replacements: &[(usize, &[u8])],
) -> bool {
    let saved_next = u32::from_le_bytes(snapshot[8..12].try_into().unwrap());
    let saved_prev = u32::from_le_bytes(snapshot[12..16].try_into().unwrap());

    // First, compute total size to check if it fits
    let data_start = PAGE_HEADER_SIZE + old_count * 2;
    let mut total_data_size = 0usize;
    let mut repl_ptr = 0usize;
    for i in 0..old_count {
        if repl_ptr < replacements.len() && replacements[repl_ptr].0 == i {
            total_data_size += replacements[repl_ptr].1.len();
            repl_ptr += 1;
        } else {
            let (s, e) = row_bounds_raw(snapshot, i, old_count);
            total_data_size += e - s;
        }
    }
    if data_start + total_data_size + CHECKSUM_SIZE > PAGE_SIZE {
        return false; // overflow
    }

    // Rebuild
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(saved_next);
    page.set_prev_leaf(saved_prev);
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let mut write_pos = data_start;
    let mut repl_ptr = 0usize;
    for i in 0..old_count {
        if repl_ptr < replacements.len() && replacements[repl_ptr].0 == i {
            let new_data = replacements[repl_ptr].1;
            page.data[write_pos..write_pos + new_data.len()].copy_from_slice(new_data);
            page.set_row_offset(i as u16, write_pos as u16);
            write_pos += new_data.len();
            repl_ptr += 1;
        } else {
            let (s, e) = row_bounds_raw(snapshot, i, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(i as u16, write_pos as u16);
            write_pos += len;
        }
    }

    page.set_num_rows(old_count as u16);
    page.set_free_space_offset(write_pos as u16);
    page.update_checksum();
    true
}
```

- [ ] **Step 2: Add update_matching to BTreeWriter**

```rust
/// Walk all leaf pages, update rows where `pred(row_bytes)` returns true.
/// For each match, `updater(old_row_bytes)` produces the new row bytes.
/// Returns (updated_in_place, overflow). Overflow rows didn't fit on their page
/// and need slow-path delete+reinsert by the caller.
/// Each entry is (rowid, old_bytes, new_bytes).
pub fn update_matching<F, U>(
    &mut self,
    pred: F,
    updater: U,
) -> Result<(Vec<(u64, Vec<u8>, Vec<u8>)>, Vec<(u64, Vec<u8>, Vec<u8>)>)>
where
    F: Fn(&[u8]) -> bool,
    U: Fn(&[u8]) -> Vec<u8>,
{
    let first_leaf = self.find_leftmost_leaf_w(self.root)?;
    let mut updated = Vec::new();
    let mut overflow = Vec::new();
    let mut current = first_leaf;

    loop {
        let page = (*self.guard.read_page(current)?).clone();
        let num_rows = page.num_rows() as usize;
        let next = page.next_leaf();

        // Collect replacements for this page
        let mut replacements: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut page_matches: Vec<(u64, Vec<u8>, Vec<u8>)> = Vec::new();

        for i in 0..num_rows {
            let (start, end) = row_bounds_raw(&page.data, i, num_rows);
            if start >= end || end > PAGE_SIZE {
                continue;
            }
            let old_data = &page.data[start..end];
            if pred(old_data) {
                let new_data = updater(old_data);
                let rowid = row::extract_id(old_data)?;
                page_matches.push((rowid, old_data.to_vec(), new_data.clone()));
                replacements.push((i, new_data));
            }
        }

        if !replacements.is_empty() {
            // Try in-place replacement
            let repl_refs: Vec<(usize, &[u8])> = replacements.iter()
                .map(|(idx, data)| (*idx, data.as_slice()))
                .collect();
            let snapshot = page.data;
            let wp = self.guard.write_page(current)?;
            let fits = write_leaf_with_replacements(wp, &snapshot, num_rows, &repl_refs);

            if fits {
                updated.extend(page_matches);
            } else {
                // Overflow: revert page write, add to overflow list
                // Restore the original page
                let wp = self.guard.write_page(current)?;
                wp.data = snapshot;
                overflow.extend(page_matches);
            }
        }

        if next == 0 {
            break;
        }
        current = next;
    }

    Ok((updated, overflow))
}
```

- [ ] **Step 3: Write test for update_matching**

```rust
#[test]
fn test_update_matching() {
    let tmp = NamedTempFile::new().unwrap();
    let pf = PageFile::open(tmp.path()).unwrap();

    let root = {
        let mut guard = pf.begin_write();
        let root = BTreeWriter::create(&mut guard).unwrap();
        let mut tree = BTreeWriter::new(&mut guard, root);
        for i in 0..100u64 {
            let status = if i % 2 == 0 { "active" } else { "inactive" };
            let row = row::encode_row(i, &[
                (0, &Value::Text(format!("item_{i}"))),
                (1, &Value::Text(status.into())),
            ]);
            tree.insert(i, &row).unwrap();
        }
        guard.commit(false).unwrap();
        root
    };

    // Update all "active" rows to "archived"
    let (in_place, overflow) = {
        let mut guard = pf.begin_write();
        let mut tree = BTreeWriter::new(&mut guard, root);
        let result = tree.update_matching(
            |row_bytes| {
                row::extract_column(row_bytes, 1)
                    .ok()
                    .flatten()
                    .map(|v| v == Value::Text("active".into()))
                    .unwrap_or(false)
            },
            |old_bytes| {
                let decoded = row::decode_row(old_bytes).unwrap();
                let mut cols: std::collections::HashMap<u16, crate::value::Value> =
                    decoded.columns.into_iter().collect();
                cols.insert(1, Value::Text("archived".into()));
                let col_vals: Vec<(u16, &crate::value::Value)> =
                    cols.iter().map(|(k, v)| (*k, v)).collect();
                row::encode_row(decoded.id, &col_vals)
            },
        ).unwrap();
        guard.commit(false).unwrap();
        result
    };

    assert_eq!(in_place.len(), 50); // 50 even-numbered rows
    assert_eq!(overflow.len(), 0);  // same-length replacement, no overflow

    // Verify updates applied
    let reader = BTreeReader::new(&pf, root);
    let all = reader.scan_all().unwrap();
    assert_eq!(all.len(), 100);
    let mut archived_count = 0;
    for (_, bytes) in &all {
        let val = row::extract_column(bytes, 1).unwrap().unwrap();
        if val == Value::Text("archived".into()) {
            archived_count += 1;
        }
    }
    assert_eq!(archived_count, 50);
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib btree::tests`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/btree.rs
git commit -m "feat: BTreeWriter::update_matching — bulk update via leaf-chain walk"
```

---

## Task 3: Wire delete_where and update_where to Batch Methods

**Files:**
- Modify: `src/db.rs`

- [ ] **Step 1: Rewrite delete_where**

Replace the current per-row deletion loop with `delete_matching`:

```rust
pub fn delete_where(&self, table: &str, filters: &[Filter]) -> Result<u64> {
    let table_state = {
        let tables = self.tables.read().unwrap();
        tables.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone()
    };
    let mut state = table_state.write().unwrap();

    let durability = self.durability();
    let mut guard = self.file.begin_write();

    // Build predicate closure that evaluates filters on raw row bytes
    let pred = |row_bytes: &[u8]| -> bool {
        filters.iter().all(|f| {
            if let Some(col_id) = state.meta.col_id(&f.column) {
                if let Ok(Some(raw)) = row::extract_column_raw(row_bytes, col_id) {
                    if let Some(result) = crate::filter::eval_filter_raw(raw, &f.op, &f.value) {
                        return result;
                    }
                }
                let col_val = row::extract_column(row_bytes, col_id).ok().flatten();
                let actual = col_val.as_ref().unwrap_or(&Value::Null);
                f.matches(actual)
            } else {
                f.matches(&Value::Null)
            }
        })
    };

    // Batch delete via leaf-chain walk
    let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
    let deleted_rows = tree.delete_matching(pred)?;
    state.meta.root_page = tree.root_page();
    let count = deleted_rows.len() as u64;

    // Index maintenance: remove entries for each deleted row
    if !state.meta.indexes.is_empty() {
        for (rowid, old_bytes) in &deleted_rows {
            Self::index_update_row(&mut guard, &mut state.meta, *rowid, old_bytes, true)?;
        }
    }

    Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
    state.meta.row_count -= count;
    Ok(count)
}
```

- [ ] **Step 2: Rewrite update_where**

```rust
pub fn update_where(
    &self,
    table: &str,
    filters: &[Filter],
    fields: &[(&str, Value)],
) -> Result<u64> {
    let table_state = {
        let tables = self.tables.read().unwrap();
        tables.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone()
    };
    let mut state = table_state.write().unwrap();

    Self::enforce_index_types(&state.meta, fields)?;

    let durability = self.durability();
    let mut guard = self.file.begin_write();

    // Build predicate
    let pred = |row_bytes: &[u8]| -> bool {
        filters.iter().all(|f| {
            if let Some(col_id) = state.meta.col_id(&f.column) {
                if let Ok(Some(raw)) = row::extract_column_raw(row_bytes, col_id) {
                    if let Some(result) = crate::filter::eval_filter_raw(raw, &f.op, &f.value) {
                        return result;
                    }
                }
                let col_val = row::extract_column(row_bytes, col_id).ok().flatten();
                let actual = col_val.as_ref().unwrap_or(&Value::Null);
                f.matches(actual)
            } else {
                f.matches(&Value::Null)
            }
        })
    };

    // Build updater closure
    let updater = |old_bytes: &[u8]| -> Vec<u8> {
        let decoded = row::decode_row(old_bytes).unwrap();
        let mut col_map: std::collections::HashMap<u16, Value> =
            decoded.columns.into_iter().collect();
        for (name, val) in fields {
            if let Some(col_id) = state.meta.col_id(name) {
                col_map.insert(col_id, val.clone());
            }
        }
        let col_values: Vec<(u16, &Value)> = col_map.iter().map(|(k, v)| (*k, v)).collect();
        row::encode_row(decoded.id, &col_values)
    };

    // Batch update via leaf-chain walk
    let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
    let (in_place, overflow) = tree.update_matching(pred, updater)?;

    // Handle overflow rows via standard delete+reinsert
    for (rowid, _, new_bytes) in &overflow {
        tree.delete(*rowid)?;
        tree.insert(*rowid, new_bytes)?;
    }
    state.meta.root_page = tree.root_page();

    let count = (in_place.len() + overflow.len()) as u64;

    // Index maintenance for all updated rows
    if !state.meta.indexes.is_empty() {
        for (rowid, old_bytes, new_bytes) in in_place.iter().chain(overflow.iter()) {
            Self::index_update_row(&mut guard, &mut state.meta, *rowid, old_bytes, true)?;
            Self::index_update_row(&mut guard, &mut state.meta, *rowid, new_bytes, false)?;
        }
    }

    Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
    Ok(count)
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test`
Expected: All tests pass (the existing delete_where and update_where tests verify correctness).

- [ ] **Step 4: Commit**

```bash
git add src/db.rs
git commit -m "perf: delete_where and update_where use batch leaf-chain walk"
```

---

## Task 4: Run Benchmarks and Verify

- [ ] **Step 1: Run bulk_ops benchmark**

Run: `cargo bench --bench bulk_ops`

Targets:
- Bulk update (~1,000 rows): >800K r/s (was 274K, SQLite 1.04M)
- Bulk delete (~1,000 rows): >1.5M r/s (was 670K, SQLite 2.2M)

- [ ] **Step 2: Run full test suite to verify no regression**

Run: `cargo test`

- [ ] **Step 3: Run other benchmarks to verify no regression**

Run: `cargo bench --bench sqlite_comparison`
Run: `cargo bench --bench point_ops`

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "perf: verified bulk ops optimization — benchmark results"
```
