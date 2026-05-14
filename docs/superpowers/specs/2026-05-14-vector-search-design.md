# Vector Search Design

Feature-gated (`vector`) HNSW-based approximate nearest neighbor search for boogy-db. Enables mixed CRUD + vector workloads in the same embedded database.

## Architecture

`src/vector/` module behind a `vector` feature flag. Two storage layers:

1. **Vector file** — custom mmap'd file per collection owning the HNSW graph and raw vector data. Dense, page-aligned layout optimized for traversal locality. Its own WAL for crash recovery.
2. **BoogyDb** — stores collection metadata (name, dimensions, metric, HNSW parameters) and the rowid-to-vector-id mapping table.

One vector file per collection: `{db_path}.{collection_name}.vec` alongside the existing `.db` file. Independent sizing, no cross-collection fragmentation.

**Concurrency:** Multiple concurrent readers via shared mmap. Writes serialized behind a single writer lock. Readers see a consistent snapshot — graph mutations committed atomically via the vector WAL.

**Dependency direction:** `src/vector/` calls `BoogyDb`'s public API for metadata. Core boogy-db modules know nothing about vectors. The feature flag gates the entire `vector` module and its optional dependencies.

**Vectors relate to rows via rowid.** Each vector collection is scoped to a boogy-db table. The collection's vectors are keyed by rowid into that table. This keeps vector storage separate (own mmap file, optimized for traversal) while giving users natural metadata filtering through existing boogy-db secondary indexes and filters.

## Distance Metrics

Three metrics, chosen per-collection at creation time:

- **Cosine similarity** — angle between vectors. Most common for text embeddings.
- **Euclidean (L2)** — straight-line distance. Common for image embeddings and spatial data.
- **Dot product (inner product)** — fast when vectors are pre-normalized.

Implemented as standalone functions with signature `fn(&[f32], &[f32]) -> f32`. Scalar implementations first, structured for future SIMD (`#[target_feature(enable = "avx2")]` variants).

## Vector File Layout

The mmap'd file stores vectors and HNSW graph data in a contiguous, page-aligned format.

### File structure

```
[Header Page]           — magic bytes, version, dimensions, metric, HNSW params,
                          entry point node ID, node count, free list head
[Vector Data Region]    — packed f32 arrays, one slot per node,
                          fixed-size (dims x 4 bytes per slot)
[Graph Region]          — per-node neighbor lists for each HNSW layer
[WAL]                   — separate file: {collection}.vec.wal
```

### Vector data region

Dense array of `f32` vectors. Node ID is an index into this array, so reading vector N is a single offset calculation: `header_size + (N x dims x 4)`. No indirection, no decode step. Deleted nodes are tracked in a free list (in the header) and reused on next insert.

### Graph region

Each node has a fixed-size record in the graph region containing: a `max_layer` field (the highest layer this node appears in) and a neighbor block for each layer from 0 to `max_layer`. Layer 0 has `M x 2` neighbor slots; higher layers have `M` slots. `M` is the HNSW connectivity parameter (default 16). Each slot is a `u32` node ID (0xFFFFFFFF = empty).

Since nodes have varying numbers of layers, the graph region uses a two-level layout: a fixed-size **node index** (one entry per node, containing the offset into the graph data area) followed by the **graph data area** (variable-length neighbor records, appended as nodes are inserted). Node ID to graph record lookup is: read offset from index slot N, then read the neighbor record at that offset.

Fixed-size neighbor blocks within each record (not variable-length) because HNSW traversal is random-access heavy. The trade-off is some wasted space for nodes with fewer neighbors than the max, but at 16 neighbors x 4 bytes = 64 bytes per layer, that's negligible.

## HNSW Algorithm

Standard HNSW per Malkov & Yashunin (2018).

### Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `M` | 16 | Max connections per node per layer. Layer 0 gets `2*M`. |
| `ef_construction` | 200 | Beam width during insert. Higher = better recall, slower build. |
| `ef_search` | 10 | Beam width during query (adjustable per query). Higher = better recall, slower search. |
| `m_l` | `1/ln(M)` | Level generation factor. Controls probability a node appears in higher layers. |

### Insert

1. Assign a random layer `l` for the new node (exponential decay via `m_l`).
2. From the entry point, greedily descend through layers above `l`, finding the closest node at each layer.
3. From layer `l` down to layer 0, search with beam width `ef_construction` to find the `M` nearest neighbors. Connect bidirectionally.
4. If any neighbor now exceeds `M` connections, prune to keep the `M` closest (simple heuristic).
5. If `l` is higher than the current max layer, update the entry point.

### Search (k-NN)

1. From the entry point, greedily descend through layers above layer 0.
2. At layer 0, search with beam width `ef_search`, maintaining a max-heap of candidates and a min-heap of results.
3. Return top `k` from results.

### Delete

Lazy deletion — mark the node as deleted, skip during search, reclaim the slot on next insert. Neighbor lists of other nodes that reference the deleted node are cleaned up lazily during searches. Avoids expensive graph repair on delete.

### Filtered search

Post-filtering. Run the HNSW search with an inflated `ef_search` (e.g., `ef_search x 4`), apply the boogy-db metadata filter to results, return top `k` that pass. Pre-filtering (constraining traversal to matching nodes) is a future optimization.

## WAL and Crash Recovery

The vector WAL is a separate file (`{collection}.vec.wal`) that logs mutations before they're applied to the mmap'd file.

### WAL entry types

- `InsertVector { node_id, layer, vector_data }` — new vector slot written
- `SetNeighbors { node_id, layer, neighbors }` — neighbor list update
- `DeleteNode { node_id }` — marks node as deleted, adds to free list
- `UpdateHeader { entry_point, node_count, max_layer }` — global state change
- `Commit` — transaction boundary marker

### Write path

1. Append all mutation entries to the WAL
2. Write `Commit` marker
3. `fsync` the WAL (configurable — can match boogy-db's durability setting)
4. Apply mutations to the mmap'd file
5. `msync` the mmap'd region
6. Truncate the WAL

### Recovery

On open, if the WAL is non-empty, replay all entries up to the last `Commit` marker, discard any partial transaction after that. Same forward-replay model as boogy-db's redo log.

### Coordination with boogy-db

An insert involves two stores — the vector file and boogy-db (rowid-to-vector-id mapping). The sequence is:

1. Write vector WAL + commit
2. Write boogy-db mapping
3. If step 2 fails, the vector WAL replay creates an orphaned vector — cleaned up lazily (no mapping = unreachable during search)

Two independent WALs, no distributed transaction protocol. Orphan cleanup is cheap because unmapped vectors are never found.

## Public API

All behind `#[cfg(feature = "vector")]`.

```rust
pub enum DistanceMetric {
    Cosine,
    Euclidean,
    DotProduct,
}

pub struct VectorCollectionOptions {
    pub dimensions: u32,
    pub metric: DistanceMetric,
    pub m: u32,                // default 16
    pub ef_construction: u32,  // default 200
}

pub struct VectorResult {
    pub rowid: u64,
    pub distance: f32,
}

pub struct VectorSearchOptions {
    pub k: u32,
    pub ef_search: u32,        // default 10
    pub filter: Option<Filter>,
}

impl BoogyDb {
    pub fn create_vector_collection(
        &mut self, table: &str, name: &str, options: VectorCollectionOptions,
    ) -> Result<()>;

    pub fn drop_vector_collection(
        &mut self, table: &str, name: &str,
    ) -> Result<()>;

    pub fn vector_insert(
        &mut self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()>;

    pub fn vector_insert_batch(
        &mut self, table: &str, collection: &str, entries: &[(u64, Vec<f32>)],
    ) -> Result<()>;

    pub fn vector_update(
        &mut self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()>;

    pub fn vector_delete(
        &mut self, table: &str, collection: &str, rowid: u64,
    ) -> Result<()>;

    pub fn vector_search(
        &self, table: &str, collection: &str, query: &[f32], options: VectorSearchOptions,
    ) -> Result<Vec<VectorResult>>;
}
```

Collections are scoped to a table (`table` + `collection` name). The collection's rowids refer to that table's rows. `vector_search` returns `VectorResult` with rowid + distance; callers use the rowid to fetch the full row via existing `get()`. `vector_insert_batch` takes the whole batch so HNSW can optimize insertion order. `&[f32]` for input vectors to avoid forcing allocations on callers.

## Module Structure

```
src/vector/
    mod.rs          — public re-exports, feature gate
    collection.rs   — VectorCollection struct, manages one mmap'd file + graph
    hnsw.rs         — HNSW algorithm: insert, search, delete, layer assignment, pruning
    mmap.rs         — memory-mapped file management, region layout, slot read/write
    wal.rs          — vector WAL: entry types, append, fsync, replay, truncate
    distance.rs     — cosine, euclidean, dot product (SIMD-ready function signatures)
    types.rs        — DistanceMetric, VectorCollectionOptions, VectorResult, VectorSearchOptions
```

`collection.rs` is the coordinator — owns the mmap handle, WAL, and HNSW graph state. `BoogyDb` holds a `HashMap<(table, collection_name), VectorCollection>` behind the feature flag. The `vector_*` methods on `BoogyDb` delegate to the appropriate `VectorCollection`.

`hnsw.rs` is pure algorithm — takes references to the mmap'd data and graph regions, computes distances via function pointers from `distance.rs`, returns mutations without doing I/O. Testable in isolation.

`distance.rs` is structured for future SIMD — each metric is a standalone function. Scalar implementations first, layout ready for `#[target_feature(enable = "avx2")]` variants.

## Testing Strategy

### Unit tests (per module)

- `distance.rs` — correctness against known values, edge cases (zero vectors, identical vectors, NaN handling), equivalence between cosine and normalized dot product
- `hnsw.rs` — graph construction properties (connectivity, layer distribution), search recall against brute-force ground truth, delete + search correctness, pruning behavior at max neighbors
- `mmap.rs` — slot read/write roundtrips, free list reuse, file growth
- `wal.rs` — write + replay roundtrip, partial transaction discard, empty WAL on clean open, fsync behavior

### Integration tests

- Full lifecycle: create collection, insert vectors, search, verify recall, delete, verify removal
- Batch insert vs sequential insert produce comparable recall
- Filtered search: insert rows with metadata, vector search with filter, verify results respect both ANN ranking and filter predicates
- Crash recovery: write vectors, simulate crash (kill before msync), reopen, verify data integrity via WAL replay
- Multiple collections on the same database, independent operation
- Rowid linkage: delete a row from boogy-db, delete its vector, confirm search no longer returns it

### Stress tests

- Concurrent readers during writes — verify no torn reads
- Large collection (50K+ vectors) recall benchmarks against brute-force (target >95% recall at default parameters)
- Insert/delete churn — free list reuse, no file size explosion

### Benchmarks

- Search latency at various collection sizes (1K, 10K, 50K, 100K)
- Insert throughput (single and batch)
- Comparison against brute-force to quantify the HNSW speedup
- Zero performance regression on existing boogy-db operations when `vector` feature is enabled but no collections exist

## Out of Scope (Future Work)

- Encryption for the vector file (follow boogy-db's per-table AES-256-GCM pattern)
- WIT interface exposure to Wasm components (immediate follow-on after Rust API is solid)
- SIMD-optimized distance functions
- Pre-filtering for filtered search (constraining HNSW traversal to matching nodes)
- Diversity-based neighbor selection heuristic (upgrade from simple closest-M pruning)
- Product quantization or other compression for very large collections
