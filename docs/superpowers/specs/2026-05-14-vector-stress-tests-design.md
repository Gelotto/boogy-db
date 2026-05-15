# Vector Stress Tests Design

Stress tests validating vector search correctness under concurrent access, large scale, and churn. Located in `tests/vector_stress_test.rs`, gated by `#[cfg(feature = "vector")]`, marked `#[ignore]` for explicit invocation.

## Tests

### 1. Concurrent readers during writes

- Spawn 4 reader threads doing `vector_search` continuously via `Arc<BoogyDb>`
- Spawn 1 writer thread doing `vector_insert` continuously
- Run for 3 seconds
- Assert: no panics, no torn reads, all search results have valid rowids
- Uses `Durability::None`, 32 dims, Euclidean metric

### 2. Large collection recall (50K vectors)

- Insert 50K deterministic pseudo-random vectors, 64 dims, Euclidean metric
- Run 100 queries comparing HNSW results (k=10, ef_search=100) to brute-force
- Assert: average recall >= 95%
- Uses deterministic xorshift64 RNG for reproducibility

### 3. Insert/delete churn

- Insert 5K vectors initially
- Loop 10 cycles: delete 500 random vectors + insert 500 new vectors
- After each cycle: verify search returns valid results (no deleted rowids)
- After all cycles: verify final vector count matches expected, file size hasn't grown unboundedly (free list reuse working)

## Conventions

- All tests use `tempfile::TempDir`, `Durability::None`
- Deterministic pseudo-random vectors via xorshift64
- `#[ignore]` — run with `cargo test --features vector --test vector_stress_test -- --ignored`
- No new dependencies
