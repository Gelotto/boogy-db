# Vector Search Design

Feature-gated (`vector`) HNSW-based approximate nearest neighbor search for boogy-db. Enables mixed CRUD + vector workloads in the same embedded database.

**Status:** Implemented. See `src/vector/` and the `vector` feature flag.

## Architecture

`src/vector/` module behind a `vector` feature flag (`Cargo.toml`: `vector = ["dep:memmap2"]`). Two storage layers:

1. **Vector file** — custom mmap'd file per collection owning the HNSW graph and raw vector data. Dense layout optimized for traversal locality. Its own WAL for crash recovery.
2. **BoogyDb** — stores collection metadata via internal mapping tables (`__vec_{table}_{collection}`) for rowid-to-node-id persistence. Collection parameters (dims, metric, m, ef_construction) live in the vec file header.

One vector file per collection: `{db_path}.{table}.{collection}.vec` alongside the existing `.boogy` file. Independent sizing, no cross-collection fragmentation.

**Concurrency:** `RwLock<HashMap<(String, String), VectorCollection>>` on BoogyDb. Read operations (search) take a read lock; write operations take a write lock. This serializes writes at the map level, not per-collection — a write to collection A blocks searches on collection B. Acceptable for per-API workloads; per-collection RwLock is a future optimization.

**Dependency direction:** `src/vector/` calls `BoogyDb`'s public API for metadata. Core boogy-db modules know nothing about vectors. The feature flag gates the entire `vector` module and its optional `memmap2` dependency.

**Vectors relate to rows via rowid.** Each vector collection is scoped to a boogy-db table. The collection's vectors are keyed by rowid into that table. This keeps vector storage separate (own mmap file, optimized for traversal) while giving users natural metadata filtering through existing boogy-db secondary indexes and filters.

**Persistence:** On `create_vector_collection`, an internal boogy-db table `__vec_{table}_{collection}` is created with a `node_id` column. Each row's boogy-db rowid IS the user's rowid; the `node_id` column stores the internal HNSW node ID. On `BoogyDb::open()`, tables with the `__vec_` prefix are discovered, parsed for (table, collection) names, and the corresponding `.vec` files are reopened with mappings rebuilt from the internal table.

## Distance Metrics

Three metrics, chosen per-collection at creation time:

- **Cosine distance** — `1 - cosine_similarity`. Range [0, 2]. Zero vector returns 1.0.
- **Euclidean (L2)** — squared L2 distance (no sqrt on hot path). Range [0, inf).
- **Dot product** — negative dot product (lower = more similar). Range (-inf, inf).

Implemented as standalone functions with signature `fn(&[f32], &[f32]) -> f32` in `src/vector/distance.rs`. `distance_fn(metric)` returns a function pointer. Scalar implementations; structured for future SIMD (`#[target_feature(enable = "avx2")]` variants).

NaN inputs propagate through IEEE 754 arithmetic (garbage in, garbage out). Callers must validate vectors before insertion.

## Vector File Layout

The mmap'd file (`VecFile` in `src/vector/mmap.rs`) stores vectors and HNSW graph data in contiguous regions.

### File structure

```
[Header: 4096 bytes]
[Vector Data Region: node_capacity × dims × 4 bytes]
[Node Index: node_capacity × 8 bytes]
[Graph Data Area: pre-allocated, variable neighbor records]
[Deleted Flags: node_capacity × 1 byte]
```

### Header (4096 bytes, fields packed at offset 0)

| Offset | Field | Type |
|--------|-------|------|
| 0 | magic "BVEC" | [u8; 4] |
| 4 | version (1) | u32 |
| 8 | dimensions | u32 |
| 12 | metric tag | u8 |
| 13 | m | u32 |
| 17 | ef_construction | u32 |
| 21 | entry_point (0xFFFFFFFF = none) | u32 |
| 25 | node_count | u32 |
| 29 | max_layer | u32 |
| 33 | node_capacity | u32 |
| 37 | free_list_head (0xFFFFFFFF = empty) | u32 |
| 41 | free_list_len | u32 |
| 45 | graph_data_len | u64 |

### Vector data region

Dense array of `f32` vectors. Node ID is an index into this array — reading vector N is a single offset calculation: `4096 + (N × dims × 4)`. No indirection, no decode step. Zero-copy reads via `std::slice::from_raw_parts` pointer cast into the mmap.

### Graph region

Two-level layout:

1. **Node index** — one `u64` per node, containing the byte offset into the graph data area. `u64::MAX` = no record.
2. **Graph data area** — variable-length neighbor records appended as nodes are inserted.

Each neighbor record:
```
[max_layer: u32]
Layer 0: [count: u32][neighbor_0: u32]...[neighbor_{2M-1}: u32]
Layer i>0: [count: u32][neighbor_0: u32]...[neighbor_{M-1}: u32]
```

Layer 0 has `2×M` fixed slots; higher layers have `M` slots. Each slot is a `u32` node ID (`0xFFFFFFFF` = empty). Fixed-size blocks for offset-based random access.

### Deleted flags

One byte per node (0 = active, 1 = deleted). Separate from the free list — the free list tracks which slots to reuse on insert, the deleted flags are checked during search traversal.

### Free list

Singly linked via vector slot reuse. When freeing node N, store current `free_list_head` as `u32` in the first 4 bytes of N's vector slot. Set `free_list_head = N`.

### Growth

When `node_count` reaches `node_capacity`, `grow()` doubles capacity: snapshots node index, graph data, and deleted flags into memory; resizes file; remaps; writes data to new positions (regions shift because vector region grew); initializes new index entries to `u64::MAX`.

## HNSW Algorithm

Standard HNSW per Malkov & Yashunin (2018). Implemented in `src/vector/hnsw.rs` as pure algorithm — no I/O. All data access via closures (`&dyn Fn`), returning mutation descriptions (`InsertResult`, `SearchResult`) for the caller to persist.

### Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `M` | 16 | Max connections per node per layer. Layer 0 gets `2×M`. |
| `ef_construction` | 200 | Beam width during insert. Higher = better recall, slower build. |
| `ef_search` | 10 | Beam width during query (adjustable per query). Higher = better recall, slower search. |
| `m_l` | `1/ln(M)` | Level generation factor. Controls probability a node appears in higher layers. |

### Insert

1. Assign a random layer `l` for the new node (exponential decay via `m_l`, xorshift64 RNG).
2. From the entry point, greedily descend through layers above `l`, finding the closest node at each layer.
3. From layer `l` down to layer 0, beam search with `ef_construction` to find the `M` nearest neighbors. Connect bidirectionally.
4. If any neighbor now exceeds `M` connections, prune to keep the `M` closest (simple heuristic — sort by distance, take closest).
5. If `l` is higher than the current max layer, update the entry point.

Returns `InsertResult { node_id, level, connections: Vec<(node_id, layer, neighbor_list)>, new_entry_point, new_max_layer }`.

### Search (k-NN)

1. From the entry point, greedily descend through layers above layer 0.
2. At layer 0, beam search with `ef_search` using a min-heap for candidates (closest first) and a max-heap for results (furthest first, for eviction).
3. Return top `k` sorted by distance ascending.

Uses `HashSet<u32>` visited set to avoid revisiting nodes. Deleted nodes are traversed through for connectivity but excluded from results.

### Delete

Lazy deletion — mark the node as deleted via the deleted flags byte, skip during search, reclaim the slot on next insert via the free list. Neighbor lists of other nodes that reference the deleted node are not repaired — deleted nodes are filtered during search traversal.

If the deleted node is the entry point, a replacement is selected from its non-deleted neighbors.

### Filtered search

Post-filtering. When `VectorSearchOptions.filter` is set, the search inflates both `k` and `ef_search` by 4×, runs the HNSW search, loads each result row from boogy-db, checks `filter.matches()`, and truncates to the original `k`. Pre-filtering (constraining traversal to matching nodes) is a future optimization.

## WAL and Crash Recovery

The vector WAL (`src/vector/wal.rs`) is a separate file (`{collection}.vec.wal`) that logs mutations before they're applied to the mmap'd file.

### WAL entry types

| Tag | Entry | Fields |
|-----|-------|--------|
| 1 | `InsertVector` | `node_id: u32, layer: u32, vector: Vec<f32>` |
| 2 | `SetNeighbors` | `node_id: u32, layer: u32, neighbors: Vec<u32>` |
| 3 | `DeleteNode` | `node_id: u32` |
| 4 | `UpdateHeader` | `entry_point: u32, node_count: u32, max_layer: u32` |
| 255 | `Commit` | (no payload) |

Entries are length-prefixed: `[len: u32 LE][tag: u8][fields...]`. All integers little-endian.

### Write path

1. Build all mutation WAL entries
2. `append_committed(entries, fsync)` — writes entries + Commit marker
3. `fsync` the WAL if durability is Immediate
4. Apply mutations to the mmap'd file (write_neighbors, update header)
5. `flush()` the mmap (msync + write header)
6. Truncate the WAL

**Critical: WAL is committed BEFORE mutations are applied to the mmap.** If the process crashes between WAL commit and mmap flush, recovery replays the WAL to restore the mutations. If the process crashes before WAL commit, the mmap has only pre-mutation state (vector data and graph record are allocated but not connected).

### Recovery

On open, if the WAL is non-empty, `read_committed()` returns all committed transactions (entries up to each `Commit` marker). Incomplete transactions (entries after the last Commit) are discarded. Each committed transaction is replayed onto the `VecFile`, then the file is flushed and the WAL truncated.

### Coordination with boogy-db

An insert involves two stores — the vector file (via WAL) and boogy-db (internal mapping table). The sequence is:

1. Write vector WAL + commit → apply to mmap → flush → truncate
2. Write boogy-db mapping row (`insert_with_id` into `__vec_{table}_{collection}`)
3. If step 2 fails, the vector exists in the mmap but has no mapping row — it's an orphan, unreachable during search because `node_to_rowid` won't contain it on next open.

Two independent WALs, no distributed transaction protocol. Orphan cleanup is implicit.

## Public API

All behind `#[cfg(feature = "vector")]`. Methods take `&self` (not `&mut self`) using interior mutability via `RwLock` on the collections map, consistent with existing BoogyDb methods.

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
        &self, table: &str, name: &str, options: &VectorCollectionOptions,
    ) -> Result<()>;

    pub fn drop_vector_collection(
        &self, table: &str, name: &str,
    ) -> Result<()>;

    pub fn vector_insert(
        &self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()>;

    pub fn vector_insert_batch(
        &self, table: &str, collection: &str, entries: &[(u64, Vec<f32>)],
    ) -> Result<()>;

    pub fn vector_update(
        &self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()>;

    pub fn vector_delete(
        &self, table: &str, collection: &str, rowid: u64,
    ) -> Result<()>;

    pub fn vector_search(
        &self, table: &str, collection: &str, query: &[f32], options: &VectorSearchOptions,
    ) -> Result<Vec<VectorResult>>;

    pub fn open_vector_collection(
        &self, table: &str, name: &str,
    ) -> Result<()>;

    pub fn vector_rebuild_mappings(
        &self, table: &str, collection: &str, mappings: Vec<(u64, u32)>,
    ) -> Result<()>;
}
```

Collections are scoped to a table (`table` + `collection` name). `vector_insert` verifies the rowid exists in the table before inserting. `vector_search` returns `VectorResult` with rowid + distance; callers use the rowid to fetch the full row via existing `get()`. `&[f32]` for input vectors to avoid forcing allocations on callers. Options passed by reference (`&VectorCollectionOptions`, `&VectorSearchOptions`).

`VectorSearchOptions::new(k)` sets `ef_search = k.max(10)` — for k > 10, ef_search automatically scales up.

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

`collection.rs` is the coordinator — owns the `VecFile`, `VectorWal`, distance function pointer, and bidirectional `HashMap<u64, u32>` / `HashMap<u32, u64>` rowid-to-node mappings. `BoogyDb` holds a `RwLock<HashMap<(String, String), VectorCollection>>` behind the feature flag. The `vector_*` methods on `BoogyDb` delegate to the appropriate `VectorCollection`, then maintain the internal mapping table.

`hnsw.rs` is pure algorithm — takes `&dyn Fn` closures for data access (read_vector, read_neighbors, is_deleted), returns mutation descriptions without doing I/O. Testable in isolation with in-memory `TestGraph`.

`distance.rs` — each metric is a standalone function returning `f32`. `distance_fn(metric)` returns a function pointer (not a trait object). Scalar for-loop implementations.

## Performance

Benchmarks on Intel i7-10875H, 32GB DDR4, Manjaro Linux, `--release`, 128 dimensions, `Durability::None`:

### Insert Throughput

| Vectors | Single | Batch(100) |
|---------|--------|------------|
| 1K | 2,153 v/s | 1,931 v/s |
| 10K | 843 v/s | 839 v/s |

Throughput drops at scale as HNSW needs to search the growing graph per insert.

### Search Latency (k=10, ef_search=50)

| Collection Size | Avg | p50 | p99 | Throughput |
|-----------------|-----|-----|-----|------------|
| 1K | 188 µs | 186 µs | 238 µs | 5,308/s |
| 10K | 349 µs | 331 µs | 701 µs | 2,861/s |
| 50K | 477 µs | 464 µs | 756 µs | 2,093/s |

Sub-millisecond at all sizes. p99 under 1ms even at 50K.

### HNSW vs Brute Force (10K vectors)

| Method | Avg | Throughput | Speedup |
|--------|-----|------------|---------|
| HNSW | 396 µs | 2,519/s | **9.5×** |
| Brute force | 3.8 ms | 265/s | baseline |

### Regression (vector feature enabled, no collections)

| Op | Latency | Throughput |
|----|---------|------------|
| Insert | 5.1 µs | 198K/s |
| Get | 0.3 µs | 3.8M/s |
| Find | 5.4 µs | 186K/s |

Zero overhead when the feature is enabled but no vector collections exist.

## Testing

### Unit tests (per module)

- `distance.rs` (15 tests) — cosine/euclidean/dot product correctness against known values, zero vectors, NaN propagation, dispatch, cosine-normalized-equals-dot equivalence
- `hnsw.rs` (5 tests) — layer assignment, neighbor selection, beam search, first-node insert, 20-node insert+search, deleted nodes skipped
- `mmap.rs` (5 tests) — create/reopen header persistence, vector read/write roundtrip, free list reuse, graph record neighbors, grow preserves data
- `wal.rs` (5 tests) — append+read committed, uncommitted transaction discarded, truncate, empty WAL, multiple transactions
- `collection.rs` (6 tests) — create/insert/search, delete+search, update, dimension mismatch, persistence+WAL replay, empty search

### Integration tests (12 tests in `tests/vector_test.rs`)

- Full lifecycle: create, insert, search, delete, drop
- Batch insert (50 vectors)
- Filtered search with category filter
- Vector update
- Multiple collections (different dims/metrics)
- Error cases: collection not found, duplicate name, dimension mismatch
- Existing CRUD ops unaffected
- Recall against brute force (1000 vectors, 32 dims, ≥90% recall)
- Crash recovery persistence (create → insert → drop → reopen → search)
- Rowid linkage after delete

### Benchmarks (`benches/vector_ops.rs`)

- Insert throughput at 1K, 10K
- Search latency at 1K, 10K, 50K (avg/p50/p99)
- HNSW vs brute force comparison
- Existing ops regression check

## Out of Scope (Future Work)

- **Encryption** for the vector file (follow boogy-db's per-table AES-256-GCM pattern)
- **WIT interface** exposure to Wasm components (immediate follow-on)
- **SIMD-optimized distance functions** (`#[target_feature(enable = "avx2")]`)
- **Pre-filtering** for filtered search (constrain HNSW traversal to matching nodes)
- **Diversity-based neighbor selection** (upgrade from simple closest-M pruning)
- **Product quantization** or other compression for very large collections
- **Per-collection RwLock** for finer-grained concurrency (currently map-level)
- **Stress tests** — concurrent readers during writes, 50K+ recall benchmarks, insert/delete churn
- **100K benchmark** — search latency at 100K vectors
