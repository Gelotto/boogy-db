# Bulk Operations Optimization — Leaf-Chain Walk

## Problem

`update_where` and `delete_where` perform per-row B+ tree surgery: for each matching row, navigate root→leaf (2-3 page reads), rebuild the leaf page (1 page write). For 1,000 deletes in a 10,000-row table, that's ~3,000-4,000 page operations. SQLite modifies rows in-place with a single pass, achieving 3-8x higher throughput.

## Design

### Batch Delete: `BTreeWriter::delete_matching`

```rust
pub fn delete_matching<F>(&mut self, pred: F) -> Result<u64>
where F: Fn(&[u8]) -> bool
```

Walk all leaf pages via the leaf chain (leftmost leaf → next_leaf pointers). For each page:
1. Scan rows, evaluate predicate on raw row bytes
2. If no matches on this page, skip (no write)
3. If matches found, rebuild the page once excluding deleted rows

Page rebuild: same as the existing `write_leaf_without` logic but handling multiple deletions at once. Build a new page with only the surviving rows packed contiguously. Update num_rows and free_space_offset.

Empty pages after deletion: leave them in the tree with num_rows=0. They're harmless and will be skipped by scans. Removing them would require tree rebalancing which adds complexity for no meaningful benefit.

Cost: one traversal to leftmost leaf (2-3 reads) + one read per leaf page + one write per modified leaf page. For 1,000 deletes across ~25 leaf pages (40 rows/page): ~28 page reads + 25 page writes = 53 operations. vs ~3,500 currently. **~66x fewer page operations.**

### Batch Update: `BTreeWriter::update_matching`

```rust
pub fn update_matching<F, U>(&mut self, pred: F, updater: U) -> Result<(u64, Vec<(u64, Vec<u8>)>)>
where
    F: Fn(&[u8]) -> bool,
    U: Fn(&[u8]) -> Vec<u8>,
```

Walk all leaf pages via the leaf chain. For each page:
1. Scan rows, evaluate predicate on raw row bytes
2. If no matches, skip
3. For each match, call `updater(old_row_bytes)` to get new row bytes
4. Check if replacing all matched rows still fits in the page
5. If yes: rebuild the page once with updated rows swapped in
6. If no (page would overflow): add the overflow rows to a return list

Returns `(count_updated_in_place, overflow_rows)`. Overflow rows are `(rowid, new_row_bytes)` pairs that the caller handles via standard delete+reinsert.

For the benchmark workload (updating "active" → "archived", same byte length), zero overflows are expected. All updates happen in a single leaf-chain walk.

### db.rs Changes

**delete_where:**

```
1. Walk leaf chain to collect matching (rowid, row_bytes) for index removal
2. Call tree.delete_matching(pred) to do the actual B+ tree deletions
3. For each collected match: remove index entries (if indexes exist)
4. Commit
```

Wait — we can't collect matches AND delete them in the same walk because the page data changes. Two options:

**Option A (simpler):** Two passes. First pass: walk leaves, collect matching rowids + bytes for index removal. Second pass: `delete_matching` walks leaves again and deletes. Two walks but no data consistency issues.

**Option B (single pass):** `delete_matching` returns the deleted row data alongside the count:

```rust
pub fn delete_matching<F>(&mut self, pred: F) -> Result<Vec<(u64, Vec<u8>)>>
where F: Fn(&[u8]) -> bool
```

Returns `Vec<(rowid, old_row_bytes)>` for each deleted row. The caller uses these for index removal. Single walk, slightly more memory (stores all deleted row bytes).

**Choose Option B** — single pass is faster, and the memory cost is bounded by the number of deleted rows (already stored in the current implementation).

**update_where:**

```
1. Build updater closure that merges fields into existing row bytes
2. Call tree.update_matching(pred, updater)
3. Handle overflows via standard delete+reinsert
4. For each updated row: remove old index entries, add new index entries
5. Commit
```

The `update_matching` return needs to include both old and new bytes for index maintenance:

```rust
pub fn update_matching<F, U>(&mut self, pred: F, updater: U) -> Result<UpdateResult>
where
    F: Fn(&[u8]) -> bool,
    U: Fn(&[u8]) -> Vec<u8>,

pub struct UpdateResult {
    /// Rows updated in-place: (rowid, old_bytes, new_bytes)
    pub updated: Vec<(u64, Vec<u8>, Vec<u8>)>,
    /// Rows that didn't fit — need slow-path delete+reinsert: (rowid, old_bytes, new_bytes)
    pub overflow: Vec<(u64, Vec<u8>, Vec<u8>)>,
}
```

### Updater Closure

The `updater` closure in `update_matching` takes raw row bytes and returns new raw row bytes. In db.rs, this closure:
1. Decodes the old row
2. Merges the update fields
3. Encodes the new row

```rust
let updater = |old_bytes: &[u8]| -> Vec<u8> {
    let decoded = row::decode_row(old_bytes).unwrap();
    let mut col_map: HashMap<u16, Value> = decoded.columns.into_iter().collect();
    for (name, val) in fields {
        if let Some(col_id) = meta.col_id(name) {
            col_map.insert(col_id, val.clone());
        }
    }
    let col_values: Vec<(u16, &Value)> = col_map.iter().map(|(k, v)| (*k, v)).collect();
    row::encode_row(decoded.id, &col_values)
};
```

### Page Rebuild Helpers

Two new helper functions in btree.rs:

**write_leaf_without_multiple:** Rebuild a leaf page excluding rows at given indices.
```rust
fn write_leaf_without_multiple(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    skip_indices: &[usize],  // sorted
)
```

**write_leaf_with_replacements:** Rebuild a leaf page with some rows replaced.
```rust
fn write_leaf_with_replacements(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    replacements: &[(usize, &[u8])],  // (row_index, new_row_bytes), sorted by index
) -> bool  // false if page would overflow
```

Returns false if the total size exceeds PAGE_SIZE, in which case the caller falls back to the slow path.

## Files Changed

- `src/btree.rs` — Add `delete_matching`, `update_matching`, `UpdateResult`, and leaf rebuild helpers to BTreeWriter
- `src/db.rs` — Rewrite `delete_where` and `update_where` to use the new batch methods

## Performance Targets

Bulk update (~1,000 rows in 10K table): >800K r/s (currently 274K, SQLite 1.04M)
Bulk delete (~1,000 rows in 10K table): >1.5M r/s (currently 670K, SQLite 2.2M)

The goal is to close the gap with SQLite, not necessarily surpass it — SQLite's C-level page manipulation is inherently lower overhead. Getting within 1.5x would be a strong result.
