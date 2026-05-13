# Scan Performance — Beat SQLite on Mixed Workloads

## Problem

boogy-db loses to SQLite on mixed workloads (30% insert / 30% get / 25% find / 15% count). Point ops already win (insert 1.4µs vs 2.2µs, get 0.6µs vs 1.7µs), but find and count drag overall throughput below SQLite.

Root causes:
1. `find()` always computes `total_count`, scanning all rows even with LIMIT
2. Index find path fetches ALL matching rowids before applying LIMIT
3. `count()` never uses indexes
4. `scan_filtered` copies entire 4KB page arrays to release borrows

## Changes

### 1. find() API Change

Current:
```rust
pub fn find(&self, table: &str, opts: FindOptions) -> Result<(Vec<Row>, u64)>
```

New:
```rust
pub struct FindOptions {
    pub filters: Vec<Filter>,
    pub sort: Vec<Sort>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub include_total: bool,  // NEW — default false
}

pub struct FindResult {
    pub rows: Vec<Row>,
    pub total: Option<u64>,  // Some only when include_total=true
}

pub fn find(&self, table: &str, opts: FindOptions) -> Result<FindResult>
```

When `include_total` is false AND sort is empty, scanning stops after `limit + offset` matches. When `include_total` is true or sort is non-empty, full scan is required (sort needs all rows, total needs full count).

### 2. scan_filtered Early Exit

Add a `stop_after` parameter to `BTree::scan_filtered`:

```rust
pub fn scan_filtered(
    &mut self,
    filter_col_id: u16,
    filter_op: FilterOp,
    filter_val: &Value,
    limit: Option<u32>,
    offset: Option<u32>,
    stop_after: Option<u64>,  // NEW — stop once this many matches found
) -> Result<(Vec<(u64, Vec<u8>)>, u64)>
```

When `stop_after` is `Some(n)`, the scan returns as soon as `n` matching rows are found. The returned count equals the number of matches found (which may be less than the true total). When `None`, scans all rows (existing behavior).

`count_filtered` is unchanged — it always needs the full count.

### 3. Index-Aware count()

When `count()` has a single Eq filter on an indexed column, use the index tree to count matches directly:

```rust
// In count():
if filters.len() == 1 && filters[0].op == FilterOp::Eq {
    if let Some(idx_meta) = state.meta.find_index_for_column(&filters[0].column) {
        // Scan index tree for prefix, count entries — no data tree access
        let col_type = ...;
        let prefix = index::encode_value_prefix(col_type, &filters[0].value);
        let mut tree = IndexTree::new(&mut file, idx_meta.root_page);
        let count = tree.count_prefix(&prefix)?;
        return Ok(count);
    }
}
```

Add `count_prefix(&self, prefix: &[u8]) -> Result<u64>` to IndexTree — same as scan_prefix but only counts entries instead of collecting them.

### 4. Index find() Respects LIMIT

Currently:
1. Scan ALL index entries matching prefix → Vec of all rowids
2. Point-lookup every rowid → Vec of all rows
3. Apply LIMIT

New:
1. Scan index entries matching prefix, for each:
   a. Extract rowid
   b. Point-lookup in data tree
   c. Add to results
   d. If results.len() == limit + offset → stop scanning index
2. Apply offset (skip first `offset` results)

This makes indexed find O(limit * log n) instead of O(k * log n) where k = total matching rows.

When `include_total` is true, we still need all matches. In that case, use `count_prefix` for the total and the limited scan for the rows.

### 5. Avoid Page Copy in scan_filtered

Current code copies the entire page array:
```rust
let (page_data, num_rows, next) = {
    let page = self.file.read_page(current)?;
    (page.data, page.num_rows() as usize, page.next_leaf())
};
```

New approach — process rows while holding the page reference:
```rust
let page = self.file.read_page(current)?.clone();
// Process all rows in page...
```

Actually, the issue is that `read_page` returns `&Page` borrowing `self.file`, so we can't call `read_page` again for the next page. The current approach copies `page.data` (a `[u8; 4096]` array) to release the borrow.

The fix: clone the `Page` struct (which contains the same `[u8; 4096]` — same cost) but use a reference to avoid re-copying. Actually the real fix is to extract `num_rows`, `next_leaf`, and all row data in one pass while the borrow is held, avoiding the need to copy the page at all.

Alternative: change `read_page` to return a cloned Page (by value) instead of a reference. This is the same cost as the current copy but makes the API cleaner.

For scan_filtered specifically: read the page, extract num_rows and next_leaf from the header, then process each row's filter column — all within the same borrow scope. Only clone the row bytes for matching rows that are within the limit/offset window.

## Files Changed

- `src/filter.rs` — add `include_total: bool` to FindOptions, add FindResult struct
- `src/btree.rs` — add `stop_after` parameter to scan_filtered
- `src/index.rs` — add `count_prefix` method to IndexTree
- `src/db.rs` — update find() return type, wire up short-circuit scan, index-aware count, limit-aware index find
- `src/lib.rs` — re-export FindResult
- `tests/crud_test.rs` — update find() call sites for new return type
- `benches/*.rs` — update find() call sites

## Performance Targets

At 3K rows, no index:
- find_eq LIMIT 20: <15µs (currently 192µs, SQLite 17.6µs)
- count_eq: <160µs (currently 172µs, SQLite 152µs — close enough, full scan required)

At 3K rows, with index:
- find_eq LIMIT 20: <15µs (currently 235µs, SQLite 12.8µs)
- count_eq: <5µs (currently 172µs, SQLite 12.4µs)

Mixed workload benchmark: surpass SQLite's 11.5K ops/sec.

## Scope

This spec covers scan performance only. Not in scope:
- Sort optimization (full materialization still required for sorted results)
- Multi-column index support
- Range query index support (gt/lt/gte/lte on indexed columns)
- MVCC
