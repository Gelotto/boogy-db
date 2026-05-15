# WIT Vector Interface Design

Expose boogy-db's vector search capability to Wasm components via WIT, following the established capability pattern (deny-by-default, host-enforced, manifest-gated).

**Workspace:** `/home/daniel/projects/boogy` (not boogy-db — this is a platform-level change)

## WIT Interface

New file `crates/boogy-wit/wit/vector.wit`. Full mirror of the Rust API — 7 operations: create-collection, drop-collection, insert, insert-batch, update, delete, search. Runtime-only (no manifest-level collection declaration).

```wit
interface vector {
    use store.{filter};

    enum distance-metric {
        cosine,
        euclidean,
        dot-product,
    }

    record vector-collection-options {
        dimensions: u32,
        metric: distance-metric,
        m: option<u32>,
        ef-construction: option<u32>,
    }

    record vector-result {
        rowid: u64,
        distance: float32,
    }

    record vector-search-options {
        k: u32,
        ef-search: option<u32>,
        filter: option<filter>,
    }

    create-collection: func(table: string, name: string, options: vector-collection-options) -> result<_, string>;
    drop-collection: func(table: string, name: string) -> result<_, string>;
    insert: func(table: string, collection: string, rowid: u64, vector: list<float32>) -> result<_, string>;
    insert-batch: func(table: string, collection: string, entries: list<tuple<u64, list<float32>>>) -> result<_, string>;
    update: func(table: string, collection: string, rowid: u64, vector: list<float32>) -> result<_, string>;
    delete: func(table: string, collection: string, rowid: u64) -> result<_, string>;
    search: func(table: string, collection: string, query: list<float32>, options: vector-search-options) -> result<list<vector-result>, string>;
}
```

Reuses `filter` from `store.wit` directly — the store's filter record already has column, op, val, and in-values fields, which covers all vector search filter needs. No separate `vector-filter` type needed. `option<u32>` for m/ef_construction/ef_search so defaults are applied host-side. `list<float32>` for vectors (WIT standard). All functions return `result<_, string>` matching existing error pattern.

## Host Implementation

New file `crates/boogy-host/src/capabilities/vector.rs`, following the `store.rs` pattern:

- **Capability gate:** `check_cap!(self, vector)` on every function. Denied if manifest doesn't grant `vector = true`.
- **Value mapping:** Convert WIT `distance-metric` / `vector-collection-options` / `vector-search-options` to boogy-db's Rust types at the boundary. Thin 1:1 mapping. The search filter reuses `store::filter` directly — the same conversion code the store capability already uses.
- **Delegation:** Each WIT function maps directly to the corresponding `BoogyDb::vector_*` method via the `ApiStore` handle.
- **Defaults:** `m` defaults to 16, `ef_construction` to 200, `ef_search` to 10 when the WIT `option` is `None`.

### Manifest Changes

Add `vector: bool` (default `false`) to `Capabilities` struct in `crates/boogy-host/src/manifest.rs`:

```rust
pub struct Capabilities {
    pub store: bool,
    pub auth: bool,
    pub peer: bool,
    pub outbound_http: bool,
    pub background_jobs: bool,
    pub vector: bool,          // new
}
```

### Linker Changes

In `crates/boogy-host/src/linker.rs`, wire the vector interface conditionally based on `manifest.capabilities.vector`, same pattern as peer/outbound_http.

### World Changes

In `crates/boogy-wit/wit/world.wit`, add `import vector;` to both the `api` and `api-with-jobs` worlds.

## SDK Wrapper

New module `crates/boogy-sdk/src/vector.rs` with ergonomic helpers:

```rust
pub fn create_collection(table: &str, name: &str, dims: u32, metric: DistanceMetric) -> Result<()>;
pub fn create_collection_with_options(table: &str, name: &str, options: VectorCollectionOptions) -> Result<()>;
pub fn insert(table: &str, collection: &str, rowid: u64, vector: &[f32]) -> Result<()>;
pub fn insert_batch(table: &str, collection: &str, entries: &[(u64, &[f32])]) -> Result<()>;
pub fn update(table: &str, collection: &str, rowid: u64, vector: &[f32]) -> Result<()>;
pub fn delete(table: &str, collection: &str, rowid: u64) -> Result<()>;
pub fn search(table: &str, collection: &str, query: &[f32], k: u32) -> Result<Vec<VectorResult>>;
pub fn search_filtered(table: &str, collection: &str, query: &[f32], k: u32, filter: Filter) -> Result<Vec<VectorResult>>;
```

- `create_collection` takes bare args (dims + metric). `create_collection_with_options` takes full options for m/ef_construction tuning.
- `search` and `search_filtered` are separate functions for clean call sites.
- `&[f32]` for vectors; glue macro converts to `list<float32>` for WIT.
- SDK re-exports `DistanceMetric`, `VectorResult`, `VectorCollectionOptions`, `Filter`.
- `wit_glue!` macro extended to emit WIT-to-SDK type conversions for vector types.

## boogy-db Dependency

The boogy workspace's `Cargo.toml` needs to add the `vector` feature to the `boogy-db` dependency (or `boogy-host`'s `Cargo.toml` if that's where the dependency lives). The host crate compiles boogy-db with `features = ["vector"]`.

## Testing

### Host-side unit tests (`crates/boogy-host/src/capabilities/vector.rs`)

- Capability denial: call vector ops with `vector = false`, verify error string
- Value mapping: WIT `distance-metric` variants map to correct boogy-db `DistanceMetric` variants
- Option defaults: `None` m/ef_construction resolved to 16/200

### Integration tests (`crates/tests-integration/`)

- Deploy a test Wasm API with `vector = true`, exercise full path: HTTP request → Wasm handler → SDK vector call → WIT → host → boogy-db
- Verify: create collection, insert vectors, search returns correct results, delete works, drop collection works
- Filtered search: insert rows with metadata, search with filter, verify results match

### Capability gating test

- Deploy an API with `vector = false` (or absent), call a vector SDK function, verify it returns a capability-denied error

### Example API (`crates/examples/vector-demo/`)

Minimal API demonstrating vector search:
- One table (`documents`) with `title` and `category` columns
- One vector collection (`embeddings`, cosine, 3 dims for simplicity)
- POST `/insert` — insert a row + embedding
- POST `/search` — search by query vector, return results with metadata
- Manifest: `[capabilities] store = true, vector = true`

## Out of Scope

- Manifest-level collection declaration (collections are created at runtime via WIT)
- Async vector operations (boogy-db's vector ops are synchronous, wrapped in the host's async boundary like store ops)
- Vector-specific telemetry events (can be added later under the existing `UsageEvent` shape)
