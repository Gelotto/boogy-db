# boogy-db

Embedded storage engine for SpinStack. In-place B+ tree with WAL, per-table concurrency, secondary indexes, lazy row decoding, and zero-copy filter evaluation. Faster than SQLite for concurrent API workloads.

## Architecture

| Module | Responsibility |
|--------|---------------|
| `db.rs` | Public API (`BoogyDb`). Table registry, system page (de)serialization, locking protocol, WAL commit path, ACID transaction support (`AcidTransaction` with private dirty page buffer). All public methods go here. |
| `file.rs` | `PageFile` (shared read cache + disk I/O) and `WriteGuard` (exclusive dirty overlay). The concurrency boundary. |
| `btree.rs` | `BTreeReader` (read-only, takes `&PageFile`) and `BTreeWriter` (takes `&mut WriteGuard`). u64-keyed B+ tree with leaf-chain walks. |
| `index.rs` | `IndexTreeReader`/`IndexTreeWriter`. Composite-key B+ tree for secondary indexes. Key encoding (sortable integers, floats, null-terminated text). |
| `page.rs` | `Page` struct (4096-byte buffer). Header layout, row offset array, checksum. Page type flags (leaf/branch/system/free). |
| `row.rs` | Row binary format: `[rowid:8][num_cols:2][offset_directory][column_data]`. Encode, decode, `extract_column` (binary search), `extract_column_raw` (zero-copy), `patch_row`/`patch_row_multi` (in-place splice). |
| `filter.rs` | `Filter`, `FilterOp`, `FindOptions` (incl. `or_groups: Vec<Vec<Filter>>` for OR-of-AND — `ALL(filters) AND ANY(group)`), `FindResult`. `eval_filter_raw` for zero-alloc comparison on raw column bytes. |
| `wal.rs` | `Wal` struct. Redo-log (after-image) entries with checksums. Append, read, truncate, fsync. |
| `table.rs` | `TableMeta`, `IndexMeta`, `TableRegistry`. Column name-to-ID mapping, index lookup. |
| `crypto.rs` | `Cipher` (AES-256-GCM). Page-level encryption at I/O boundaries. `encrypt_page`/`decrypt_page`. |
| `value.rs` | `Value` enum (Null/Text/Integer/Real/Blob/Boolean), `Type`, `ColumnDef`. |
| `error.rs` | `BoogyError` enum, `Result<T>` alias. |
| `async_db.rs` | `AsyncBoogyDb` -- zero-cost async wrapper behind `tokio` feature flag. Delegates to sync `BoogyDb`. |

## Key Types

- **`BoogyDb`** -- Main handle. Owns `PageFile`, `Wal`, per-table `RwLock<TableState>` map. All public CRUD methods.
- **`PageFile`** -- Page cache (`RwLock<Vec<Option<Arc<Page>>>>`) + disk file (`Mutex<File>`). `read_page(&self)` for concurrent reads.
- **`WriteGuard`** -- RAII guard from `PageFile::begin_write()`. Holds `MutexGuard<WriteState>` with dirty overlay. `commit()` publishes dirty pages to cache and returns after-images. `peek_dirty()` for zero-copy reads during tree traversal.
- **`BTreeReader`/`BTreeWriter`** -- Read path takes `&PageFile`, write path takes `&mut WriteGuard`. Methods: `search`, `scan_all`, `multi_get_sorted`, `scan_filtered`, `insert`, `delete`, `delete_matching`, `update_matching`.
- **`IndexTreeReader`/`IndexTreeWriter`** -- Same split. Methods: `scan_prefix`, `scan_prefix_limit`, `count_prefix`, `insert`, `delete`.
- **`Row`** -- Public query result. Stores raw bytes + `Arc<Vec<String>>` column names. `get(column)` decodes one column. `columns()` decodes all.
- **`Page`** -- `[u8; 4096]` buffer. 16-byte header, CRC32 checksum in last 4 bytes.
- **`Cipher`** -- AES-256-GCM. Plaintext lives in the page cache; encryption only at disk I/O.
- **`AcidTransaction`** -- ACID transaction guard. Holds a private `HashMap<u32, Box<Page>>` dirty buffer. Uses `inject_dirty`/`drain_dirty` on `WriteGuard` to run B+ tree operations against private pages. `commit()` publishes atomically; `Drop` without commit = rollback.
- **`TableMeta`** -- Per-table metadata: columns, indexes, root page, row count, next rowid.

## Concurrency Model

1. **Per-table RwLock**: Reads take shared lock, writes take exclusive lock. Different tables never block each other.
2. **Page cache**: `RwLock<Vec<Option<Arc<Page>>>>`. Readers clone an `Arc` pointer under a shared lock and release immediately.
3. **WriteGuard**: Single writer at a time (holds `Mutex<WriteState>`). Dirty pages live in a `HashMap<u32, Box<Page>>` overlay.
4. **BTreeReader**: Takes `&PageFile`, traverses the tree without holding any lock during traversal (each `read_page` is a brief shared lock).

## WAL Design

Redo-log (after-images). On commit, `WriteGuard::commit()` publishes dirty pages to the shared cache and returns after-images. The commit path in `db.rs` writes those after-images to the WAL. The data file is never modified during commits -- only on clean shutdown (`Drop`), when cached pages flush to disk and the WAL is truncated. On crash recovery, WAL entries are replayed forward (redo) to restore committed state.

Three durability levels: `Immediate` (fsync WAL per commit), `Normal` (WAL write, no fsync), `None` (no WAL writes).

## Build & Test

```bash
cargo test                    # all unit + integration tests
cargo test --test stress_test # stress tests
cargo bench                   # all benchmarks (uses custom harness)
cargo bench --bench point_ops # single benchmark
```

## Code Conventions

- Rust edition 2024.
- No unnecessary abstractions. Functions over traits when the abstraction has one implementation.
- Performance-critical paths use zero-copy: `extract_column_raw` returns `&[u8]` into page data, `eval_filter_raw` compares raw bytes without allocating a `Value`, `peek_dirty` borrows dirty pages without cloning.
- Benchmarks use custom harness (`harness = false` in Cargo.toml) with `std::time::Instant`. No criterion dependency.
- Tests use `tempfile::TempDir` or `tempfile::NamedTempFile` for isolation.

## Performance Rules

- **Never clone pages unnecessarily.** Use `peek_dirty` for dirty overlay reads, `Arc` deref for cache reads. Only clone at leaves where you need owned data for page rebuild.
- **Never allocate on hot paths.** Use `extract_column_raw` + `eval_filter_raw` for filter evaluation. Fall back to `extract_column` + `eval_filter_op` only when raw path doesn't cover the type/op combination.
- **Page cache holds plaintext.** Encryption/decryption happens only at disk I/O boundaries (`sync_all` and `read_page_from_disk`).
- **Batch operations walk the leaf chain once.** `delete_matching`/`update_matching` rebuild each page in a single pass instead of per-row tree surgery.
- **Use `multi_get_sorted` for clustered rowid fetches.** Single leaf-chain walk instead of N individual tree traversals.

## Things to Avoid

- Don't change `PAGE_SIZE` (4096). The entire page format depends on it.
- Don't add dependencies without justification. Current deps: `crc32fast`, `aes-gcm`, `rand`.
- Don't break the `Row` lazy API. `get(column)` must decode only the requested column. `columns()` is the full-decode path.
- Don't use before-images in the WAL. It is a redo-log now (after-images, forward replay).
- Don't hold locks during tree traversal in read paths. `BTreeReader` takes `&PageFile` specifically so it never holds a lock across multiple page reads.
- Don't write to the data file during commits. All disk writes happen at checkpoint (shutdown).

## Common Tasks

### Adding a new public API method
See `skills/internal/add-table-method.md`.

### Adding a new benchmark
See `skills/internal/add-benchmark.md`.

### Optimizing a hot path
See `skills/internal/optimize-hot-path.md`.

### Modifying the page format
See `skills/internal/modify-page-format.md`.
