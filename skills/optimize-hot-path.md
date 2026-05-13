# Optimizing Performance-Critical Code

Profile with `cargo bench --bench profile_ops` or `perf record cargo bench --bench profile_ops`.

## Zero-Copy Page Access

In `BTreeWriter`, check the dirty overlay first (zero-copy), then the shared cache (Arc deref):

```rust
// GOOD: zero-copy check, no allocation
if let Some(p) = self.guard.peek_dirty(page_no) {
    if p.is_leaf() { ... }
} else {
    let arc = self.guard.page_file().read_page(page_no)?;
    // arc deref is free -- just pointer chase
}

// BAD: always clones the page
let page = self.guard.read_page_cloned(page_no)?; // allocates 4KB
```

Only use `read_page_cloned` at leaf pages where you need owned data for page rebuild (insert/delete/split). In `BTreeReader`, deref the `Arc<Page>` from `file.read_page()` -- don't clone the Page.

## Zero-Alloc Filter Evaluation

The hot path for filter evaluation avoids allocating `Value` objects:

```rust
// 1. Get raw column bytes (slice into page data, no allocation)
if let Ok(Some(raw)) = row::extract_column_raw(data, col_id) {
    // 2. Compare raw bytes directly
    if let Some(result) = filter::eval_filter_raw(raw, &f.op, &f.value) {
        return result; // fast path: no Value allocation
    }
}
// 3. Fallback: decode column to Value (allocates for Text/Blob)
let col_val = row::extract_column(data, col_id)?;
```

`eval_filter_raw` currently handles:
- Integer: all ops (Eq, Ne, Lt, Le, Gt, Ge) -- reads i64 directly from raw bytes
- Text Eq: compares raw UTF-8 bytes without String allocation

To add a new raw comparison, extend the match in `filter::eval_filter_raw`.

## Row Patching vs Full Decode/Encode

For single-column updates, `patch_row` splices raw bytes directly:

```rust
// GOOD: O(1) splice, no decode
let new_bytes = row::patch_row(old_bytes, col_id, &new_value)?;

// BAD: full decode + re-encode
let decoded = row::decode_row(old_bytes)?;
let mut cols = decoded.columns;
cols.push((col_id, new_value));
let new_bytes = row::encode_row(decoded.id, &cols);
```

For multi-column updates, `patch_row_multi` chains patches sequentially.

## Batch Operations

Use leaf-chain walks instead of per-row tree traversal:

- **`BTreeWriter::delete_matching`**: Walks the leaf chain once, rebuilds each modified page in a single pass. Much faster than N individual `delete()` calls.
- **`BTreeWriter::update_matching`**: Same pattern. Updates in-place when new row fits, collects overflow for re-insertion.
- **`BTreeReader::multi_get_sorted`**: Given sorted rowids, finds the first leaf via tree traversal, then walks the chain collecting matches. Faster than N individual `search()` calls.

## When to Use What

| Operation | Use | Avoid |
|-----------|-----|-------|
| Read page during branch traversal | `peek_dirty` / `Arc` deref | `read_page_cloned` |
| Read page for leaf rebuild | `read_page_cloned` | multiple `peek_dirty` copies |
| Filter single column | `extract_column_raw` + `eval_filter_raw` | `extract_column` (allocates) |
| Update one column | `patch_row` | decode + re-encode |
| Delete N rows by filter | `delete_matching` (one leaf walk) | N calls to `delete()` |
| Fetch N rows by sorted ids | `multi_get_sorted` (one leaf walk) | N calls to `search()` |
| Count with index | `IndexTreeReader::count_prefix` | `scan_prefix` + `.len()` |

## Checklist Before Submitting a Perf Change

- [ ] Profiled before and after with `profile_ops` or a focused benchmark
- [ ] No new allocations on the hot path (check for `.to_vec()`, `.clone()`, `String::from`)
- [ ] No new lock acquisitions in inner loops
- [ ] `cargo test` passes
- [ ] Benchmark shows measurable improvement (>5%)
