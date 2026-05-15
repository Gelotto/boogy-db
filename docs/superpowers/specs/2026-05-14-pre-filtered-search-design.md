# Pre-Filtered HNSW Search Design

Replace the post-filter approach (4x inflate k/ef_search, filter results after search) with inline pre-filtering during HNSW graph traversal. Nodes that don't match the metadata filter are traversed through for connectivity but excluded from results — same pattern as deleted nodes.

## Changes

### hnsw.rs

Add `is_allowed: &dyn Fn(u32) -> bool` parameter to `search_layer` and `search`. During traversal, a node is excluded from results if `is_deleted(node_id) || !is_allowed(node_id)`. The node is still expanded (its neighbors are visited) to maintain graph connectivity.

When no filter is present, the caller passes `&|_| true`.

### collection.rs

`VectorCollection::search` gains an optional filter parameter. When present, the collection constructs the `is_allowed` closure:

1. Map node_id → rowid via `self.node_to_rowid`
2. Load the row from boogy-db via a provided callback (since collection.rs doesn't hold a BoogyDb reference)
3. Check `filter.matches()` on the relevant column
4. Cache results in a `HashMap<u32, bool>` inside the closure to avoid re-loading rows (a node may be visited as a neighbor of multiple candidates)

The callback signature: `&dyn Fn(u64) -> Option<crate::db::Row>` — takes a rowid, returns the row if it exists. Provided by db.rs when calling collection.search.

### db.rs

`BoogyDb::vector_search` passes the filter down to the collection with a row-loader callback (`|rowid| self.get(table, rowid).ok().flatten()`). Removes the 4x inflate + post-filter path entirely.

## No API Changes

`VectorSearchOptions` already has `filter: Option<Filter>`. The public interface is unchanged. Behavior improves: tighter results, no over-fetching, no inflation heuristic.

## Testing

- Existing `test_filtered_search` integration test validates correctness (all results match filter)
- Add a test comparing pre-filter results to brute-force filtered results for recall verification
- Add a test with a highly selective filter (matches <5% of rows) to verify pre-filter finds enough results without inflation

## Performance

Pre-filtering adds a boogy-db row lookup per visited node during HNSW traversal (~200-400 nodes for a typical search). With the page cache warm, each lookup is <1µs. Total overhead: ~200-400µs. But it eliminates the 4x inflation (which was searching 4x more nodes), so net search time should decrease for selective filters.
