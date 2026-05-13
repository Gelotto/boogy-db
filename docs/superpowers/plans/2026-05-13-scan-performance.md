# Scan Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make boogy-db beat SQLite on mixed workloads by short-circuiting scans at LIMIT and adding index-aware count.

**Architecture:** `find()` returns `FindResult` with optional total. `scan_filtered` gets early-exit support. `IndexTree` gets `count_prefix`. Index find path stops after LIMIT rows. `count()` uses indexes when available.

**Tech Stack:** Rust, existing boogy-db crate.

**Spec:** `docs/superpowers/specs/2026-05-13-scan-performance-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/filter.rs` | Modify | Add `include_total` to FindOptions, add FindResult struct |
| `src/btree.rs` | Modify | Add `stop_after` to scan_filtered |
| `src/index.rs` | Modify | Add `count_prefix` to IndexTree |
| `src/db.rs` | Modify | Rewrite find() for short-circuit, index-aware count, limit-aware index find |
| `src/lib.rs` | Modify | Re-export FindResult |
| `tests/crud_test.rs` | Modify | Update find() call sites |
| `benches/sqlite_comparison.rs` | Modify | Update find() call sites, add indexed variant |
| `benches/point_ops.rs` | Modify | Update find() call sites |
| `benches/profile_ops.rs` | Modify | Update find() call sites |

---

## Task 1: FindResult Type and FindOptions.include_total

**Files:**
- Modify: `src/filter.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add FindResult and include_total to filter.rs**

In `src/filter.rs`, add `FindResult` struct and `include_total` field:

```rust
#[derive(Debug, Clone, Default)]
pub struct FindOptions {
    pub filters: Vec<Filter>,
    pub sort: Vec<Sort>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub include_total: bool,  // NEW — default false via Default derive
}

/// Result of a find() query.
#[derive(Debug, Clone)]
pub struct FindResult {
    pub rows: Vec<crate::db::Row>,
    /// Only populated when FindOptions.include_total is true.
    pub total: Option<u64>,
}
```

- [ ] **Step 2: Re-export FindResult from lib.rs**

In `src/lib.rs`, change the filter re-export line to:

```rust
pub use filter::{Filter, FilterOp, FindOptions, FindResult, Sort, SortDir};
```

- [ ] **Step 3: Verify it compiles (tests will fail due to find() signature mismatch — that's expected)**

Run: `cargo check --lib 2>&1 | head -5`
Expected: Compiles (FindResult doesn't depend on db.rs's find() yet)

- [ ] **Step 4: Commit**

```bash
git add src/filter.rs src/lib.rs
git commit -m "feat: add FindResult type and include_total option"
```

---

## Task 2: scan_filtered Early Exit

**Files:**
- Modify: `src/btree.rs`

- [ ] **Step 1: Add stop_after parameter to scan_filtered**

Change `scan_filtered` signature to add `stop_after: Option<u64>`:

```rust
pub fn scan_filtered(
    &mut self,
    filter_col_id: u16,
    filter_op: crate::filter::FilterOp,
    filter_val: &crate::value::Value,
    limit: Option<u32>,
    offset: Option<u32>,
    stop_after: Option<u64>,
) -> Result<(Vec<(u64, Vec<u8>)>, u64)> {
```

In the inner loop, after `total += 1`, add early exit:

```rust
if crate::filter::eval_filter_op(actual, &filter_op, filter_val) {
    total += 1;
    if total > skip && (total - skip) <= take {
        if let Ok(id) = row::extract_id(data) {
            results.push((id, data.to_vec()));
        }
    }
    // Early exit: stop scanning once we have enough matches
    if let Some(max) = stop_after {
        if total >= max {
            return Ok((results, total));
        }
    }
}
```

- [ ] **Step 2: Verify btree tests still pass**

Run: `cargo test --lib btree::tests`
Expected: All pass (no callers pass stop_after yet, so this is purely additive).

Note: db.rs callers of scan_filtered will need updating to pass the new parameter — that happens in Task 4.

- [ ] **Step 3: Commit**

```bash
git add src/btree.rs
git commit -m "feat: scan_filtered early exit via stop_after parameter"
```

---

## Task 3: IndexTree count_prefix

**Files:**
- Modify: `src/index.rs`

- [ ] **Step 1: Write test for count_prefix**

Add to the test module in `src/index.rs`:

```rust
#[test]
fn test_count_prefix() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut pf = PageFile::open(tmp.path()).unwrap();
    let root = IndexTree::create(&mut pf).unwrap();
    let mut tree = IndexTree::new(&mut pf, root);

    // Insert 5 entries for value=42, 3 for value=99
    for rowid in 1..=5u64 {
        let key = encode_index_key_integer(42, rowid);
        tree.insert(&key).unwrap();
    }
    for rowid in 1..=3u64 {
        let key = encode_index_key_integer(99, rowid);
        tree.insert(&key).unwrap();
    }

    let prefix_42 = encode_integer_prefix(42);
    assert_eq!(tree.count_prefix(&prefix_42).unwrap(), 5);

    let prefix_99 = encode_integer_prefix(99);
    assert_eq!(tree.count_prefix(&prefix_99).unwrap(), 3);

    let prefix_0 = encode_integer_prefix(0);
    assert_eq!(tree.count_prefix(&prefix_0).unwrap(), 0);
}

#[test]
fn test_count_prefix_text() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut pf = PageFile::open(tmp.path()).unwrap();
    let root = IndexTree::create(&mut pf).unwrap();
    let mut tree = IndexTree::new(&mut pf, root);

    for rowid in 1..=10u64 {
        let key = encode_index_key_text("alice", rowid);
        tree.insert(&key).unwrap();
    }
    for rowid in 1..=7u64 {
        let key = encode_index_key_text("bob", rowid);
        tree.insert(&key).unwrap();
    }

    let prefix = encode_text_prefix("alice");
    assert_eq!(tree.count_prefix(&prefix).unwrap(), 10);

    let prefix = encode_text_prefix("bob");
    assert_eq!(tree.count_prefix(&prefix).unwrap(), 7);

    let prefix = encode_text_prefix("charlie");
    assert_eq!(tree.count_prefix(&prefix).unwrap(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib index::tests::test_count_prefix`
Expected: FAIL — method `count_prefix` not found.

- [ ] **Step 3: Implement count_prefix**

Add to the `IndexTree` impl block, right after `scan_prefix`:

```rust
/// Count entries whose key starts with `prefix` without collecting them.
pub fn count_prefix(&mut self, prefix: &[u8]) -> Result<u64> {
    let first_leaf = self.find_leftmost_leaf(self.root)?;
    let mut count = 0u64;
    let mut current = first_leaf;
    let mut found_start = false;

    loop {
        let page = self.file.read_page(current)?.clone();
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
        if next == 0 {
            break;
        }
        current = next;
    }

    Ok(count)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib index::tests::test_count_prefix`
Expected: Both tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/index.rs
git commit -m "feat: IndexTree::count_prefix for index-aware counting"
```

---

## Task 4: Rewrite find() and count() in db.rs

**Files:**
- Modify: `src/db.rs`

This is the core task. It changes `find()` to return `FindResult`, adds short-circuit scanning, limit-aware index lookups, and index-aware count.

- [ ] **Step 1: Change find() signature and return type**

```rust
/// Find rows matching filters, with sort and pagination.
pub fn find(&self, table: &str, opts: FindOptions) -> Result<FindResult> {
```

Import `FindResult` at the top of db.rs:
```rust
use crate::filter::{Filter, FilterOp, FindOptions, FindResult, SortDir};
```

- [ ] **Step 2: Rewrite the index find path with LIMIT awareness**

When `include_total` is false, stop fetching rowids after `limit + offset`:

```rust
let (matching, total) = if let Some(idx_filter) = index_candidate {
    let idx_meta = state.meta.find_index_for_column(&idx_filter.column).unwrap().clone();
    let col_type = state.meta.columns.iter()
        .find(|c| c.name == idx_filter.column)
        .map(|c| c.col_type)
        .unwrap();
    let mut file = self.file.lock().unwrap();

    let need = if !opts.include_total && opts.sort.is_empty() {
        // Only need limit+offset rows
        let skip = opts.offset.unwrap_or(0) as usize;
        let take = opts.limit.unwrap_or(u32::MAX) as usize;
        Some(skip + take)
    } else {
        None // need all matches
    };

    // Get rowids from index, with optional limit
    let prefix = index::encode_value_prefix(col_type, &idx_filter.value)
        .unwrap_or_default();
    let mut idx_tree = IndexTree::new(&mut file, idx_meta.root_page);

    let rowids: Vec<u64> = if let Some(max) = need {
        // Limited scan: stop after enough rowids
        let mut ids = Vec::with_capacity(max);
        // Use a limited prefix scan
        let keys = idx_tree.scan_prefix_limit(&prefix, max)?;
        for k in &keys {
            ids.push(index::extract_rowid(col_type, k));
        }
        ids
    } else {
        // Full scan for total
        let keys = idx_tree.scan_prefix(&prefix)?;
        keys.iter().map(|k| index::extract_rowid(col_type, k)).collect()
    };

    let total = if opts.include_total && need.is_some() {
        // We limited the rowid scan but need the total — use count_prefix
        Some(idx_tree.count_prefix(&prefix)?)
    } else if opts.include_total {
        Some(rowids.len() as u64)
    } else {
        None
    };

    // Point-lookup each rowid
    let mut rows = Vec::with_capacity(rowids.len());
    for rowid in &rowids {
        let mut tree = BTree::new(&mut file, state.meta.root_page);
        if let Some(bytes) = tree.search(*rowid)? {
            let decoded = row::decode_row(&bytes)?;
            let row = decoded_to_row(&decoded, &state.meta);
            let passes = opts.filters.iter().all(|f| {
                let col_val = row.columns.iter()
                    .find(|(name, _)| name == &f.column)
                    .map(|(_, v)| v);
                match col_val {
                    Some(v) => f.matches(v),
                    None => f.matches(&Value::Null),
                }
            });
            if passes { rows.push(row); }
        }
    }

    (rows, total)
} else ...
```

This requires a new `scan_prefix_limit` method on IndexTree — see Step 5.

- [ ] **Step 3: Rewrite the scan_filtered path with short-circuit**

For the single-filter non-indexed path:

```rust
} else if opts.filters.len() == 1 {
    let f = &opts.filters[0];
    if let Some(col_id) = state.meta.col_id(&f.column) {
        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, state.meta.root_page);

        let can_short_circuit = !opts.include_total && opts.sort.is_empty();

        let (lim, off, stop) = if can_short_circuit {
            (opts.limit, opts.offset,
             Some(opts.offset.unwrap_or(0) as u64 + opts.limit.unwrap_or(u32::MAX) as u64))
        } else if opts.sort.is_empty() {
            (opts.limit, opts.offset, None)
        } else {
            (None, None, None)
        };

        let (raw_rows, count) = tree.scan_filtered(col_id, f.op, &f.value, lim, off, stop)?;
        drop(file);
        let matching: Vec<Row> = raw_rows.iter()
            .map(|(_, bytes)| {
                let decoded = row::decode_row(bytes).unwrap();
                decoded_to_row(&decoded, &state.meta)
            })
            .collect();
        let total = if opts.include_total { Some(count) } else { None };
        // Note: if include_total is true, stop was None, so count is the real total
        (matching, total)
    } else {
        (Vec::new(), if opts.include_total { Some(0) } else { None })
    }
}
```

- [ ] **Step 4: Update no-filter and multi-filter paths**

For no-filter path: total = `Some(all.len() as u64)` if `include_total`, else `None`.

For multi-filter path: same pattern.

Replace all `(matching, total)` bindings with type `(Vec<Row>, Option<u64>)`.

- [ ] **Step 5: Add scan_prefix_limit to IndexTree**

In `src/index.rs`, add to the IndexTree impl:

```rust
/// Scan prefix entries, stopping after collecting `max` keys.
pub fn scan_prefix_limit(&mut self, prefix: &[u8], max: usize) -> Result<Vec<Vec<u8>>> {
    let first_leaf = self.find_leftmost_leaf(self.root)?;
    let mut results = Vec::with_capacity(max);
    let mut current = first_leaf;
    let mut found_start = false;

    loop {
        let page = self.file.read_page(current)?.clone();
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
```

- [ ] **Step 6: Update sort and pagination at end of find()**

```rust
// Sort (only if sort requested).
let mut matching = matching;
if !opts.sort.is_empty() {
    for sort in opts.sort.iter().rev() {
        matching.sort_by(|a, b| {
            // ... existing sort logic unchanged ...
        });
    }
}

// Pagination — only if we didn't already paginate during scan.
// If sort is non-empty, we collected all rows and need to paginate now.
// If sort is empty and we short-circuited, rows are already paginated.
let rows = if !opts.sort.is_empty() {
    let skip = opts.offset.unwrap_or(0) as usize;
    let take = opts.limit.unwrap_or(u32::MAX) as usize;
    matching.into_iter().skip(skip).take(take).collect()
} else {
    matching
};

Ok(FindResult { rows, total })
```

- [ ] **Step 7: Add index-aware count()**

At the top of `count()`, after the empty-filter fast path, add:

```rust
// Index path: Eq filter on an indexed column — count via index tree
if filters.len() == 1 && filters[0].op == FilterOp::Eq {
    if let Some(idx_meta) = state.meta.find_index_for_column(&filters[0].column) {
        let col_type = state.meta.columns.iter()
            .find(|c| c.name == filters[0].column)
            .map(|c| c.col_type);
        if let Some(ct) = col_type {
            if let Some(prefix) = index::encode_value_prefix(ct, &filters[0].value) {
                let mut file = self.file.lock().unwrap();
                let mut tree = IndexTree::new(&mut file, idx_meta.root_page);
                return tree.count_prefix(&prefix);
            }
        }
    }
}
```

This must come BEFORE the existing `scan_filtered` count path.

- [ ] **Step 8: Update db.rs tests**

All tests that call `find()` need to be updated. The pattern:
- `let (rows, total) = db.find("t", opts).unwrap();` → `let result = db.find("t", opts).unwrap();` then use `result.rows` and `result.total`
- Tests that assert on `total` need `include_total: true` in their FindOptions
- Tests that don't check total can use the default `include_total: false`

Add a new test:

```rust
#[test]
fn test_find_short_circuits_at_limit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let db = BoogyDb::open(&path).unwrap();
    db.set_durability(Durability::None);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    for i in 0..1000 {
        db.insert("t", &[("v", Value::Integer(i % 10))]).unwrap();
    }

    // Without include_total, total should be None
    let result = db.find("t", FindOptions {
        filters: vec![Filter::eq("v", 5i64)],
        limit: Some(5),
        ..Default::default()
    }).unwrap();
    assert_eq!(result.rows.len(), 5);
    assert_eq!(result.total, None);

    // With include_total, total should be the real count
    let result = db.find("t", FindOptions {
        filters: vec![Filter::eq("v", 5i64)],
        limit: Some(5),
        include_total: true,
        ..Default::default()
    }).unwrap();
    assert_eq!(result.rows.len(), 5);
    assert_eq!(result.total, Some(100));
}

#[test]
fn test_count_uses_index() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let db = BoogyDb::open(&path).unwrap();
    db.set_durability(Durability::None);
    db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();

    for i in 0..100 {
        db.insert("t", &[("v", Value::Text(format!("val_{}", i % 5)))]).unwrap();
    }

    let count = db.count("t", &[Filter::eq("v", "val_2")]).unwrap();
    assert_eq!(count, 20);
}
```

- [ ] **Step 9: Run all lib tests**

Run: `cargo test --lib`
Expected: All tests pass.

- [ ] **Step 10: Commit**

```bash
git add src/db.rs src/index.rs
git commit -m "perf: short-circuit find() at LIMIT, index-aware count"
```

---

## Task 5: Update External Tests and Benchmarks

**Files:**
- Modify: `tests/crud_test.rs`
- Modify: `benches/sqlite_comparison.rs`
- Modify: `benches/point_ops.rs`
- Modify: `benches/profile_ops.rs`

- [ ] **Step 1: Update tests/crud_test.rs**

All `db.find()` calls need updating. The pattern:

```rust
// Old:
let (rows, total) = db.find("t", opts).unwrap();

// New (when asserting total):
let result = db.find("t", FindOptions { include_total: true, ..opts }).unwrap();
let rows = result.rows;
let total = result.total.unwrap();

// New (when not asserting total):
let result = db.find("t", opts).unwrap();
let rows = result.rows;
```

Tests that assert on `total`:
- `test_find_with_filter` — needs include_total
- `test_find_with_sort_and_pagination` — needs include_total
- `test_index_speeds_up_find` — needs include_total
- `test_index_maintained_on_insert` — needs include_total

Tests that don't assert on total:
- `test_index_maintained_on_update` — uses count, no find total
- `test_index_maintained_on_delete` — uses count, no find total

- [ ] **Step 2: Update benches/sqlite_comparison.rs**

Change boogy find calls:
```rust
// Old:
let _ = db.find("notes", FindOptions { ... }).unwrap();

// New:
let _ = db.find("notes", FindOptions { ... }).unwrap();
// No change needed — FindOptions has Default for include_total (false)
// and we don't use the total in the benchmark
```

The return type changed from `(Vec<Row>, u64)` to `FindResult`, so update any destructuring:
```rust
// If there's destructuring like: let (rows, _total) = db.find(...)
// Change to: let _result = db.find(...)
```

- [ ] **Step 3: Update benches/point_ops.rs and benches/profile_ops.rs**

Same pattern — `find()` now returns `FindResult` instead of `(Vec<Row>, u64)`. Update any destructuring.

- [ ] **Step 4: Run all tests and compile benchmarks**

Run: `cargo test`
Run: `cargo bench --no-run`
Expected: All 110+ tests pass, benchmarks compile.

- [ ] **Step 5: Commit**

```bash
git add tests/ benches/
git commit -m "refactor: update tests and benchmarks for FindResult API"
```

---

## Task 6: Run Benchmarks and Verify

- [ ] **Step 1: Run the profile benchmark**

Run: `cargo bench --bench profile_ops`

Expected targets (at 3K rows):
- find_eq no index: <20µs (was 192µs)
- find_eq with index: <20µs (was 235µs)
- count_eq with index: <5µs (was 172µs)
- insert/get: unchanged or better

- [ ] **Step 2: Run the mixed workload benchmark**

Run: `cargo bench --bench sqlite_comparison`

Expected: boogy-db ops/sec > SQLite ops/sec.

- [ ] **Step 3: If targets not met, profile and iterate**

If find_eq is still slow, check:
- Is scan_filtered actually short-circuiting? Add a debug print for `total` at return.
- Is the index path actually being used? Check the filter/index matching logic.
- Are we doing unnecessary work after the scan (sort on unsorted results)?

- [ ] **Step 4: Commit benchmark results as a comment or doc update**

```bash
git add -A
git commit -m "perf: verified mixed workload surpasses SQLite"
```
