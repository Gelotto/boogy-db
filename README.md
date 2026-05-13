# boogy-db

A fast embedded storage engine for Rust, purpose-built for concurrent API workloads. In-place B+ tree with WAL, per-table concurrency, secondary indexes, and a column-aware page format that avoids encode/decode overhead.

## Table of Contents

- [Features](#features)
- [Quick Start](#quick-start)
- [API](#api)
- [Benchmarks](#benchmarks)
  - [Durability Modes](#durability-modes)
  - [Point Operations](#point-operations)
  - [Mixed Workload (Single Thread)](#mixed-workload-single-thread)
  - [Mixed Workload (Concurrent)](#mixed-workload-concurrent)
  - [Join Simulation (User + Posts)](#join-simulation-user--posts)
  - [Bulk Operations](#bulk-operations)
- [Encryption](#encryption)
- [Async API](#async-api)
- [Architecture](#architecture)
- [License](#license)

## Features

- **Integer-keyed B+ tree** with auto-increment row IDs and fixed 12-byte branch entries for high fanout
- **Secondary indexes** via composite-key B+ trees with O(log n) lookup and insert
- **Concurrent readers** that never block each other or writers (shared `RwLock` read on `Arc<Page>` cache — clone pointer and release)
- **Per-table write locks** so writes to different tables are fully concurrent
- **Redo-log WAL** with configurable durability — commits write after-images to the WAL only, data file flushed on checkpoint
- **Crash recovery** via forward WAL replay (redo) on open
- **Lazy row decoding** — `Row` stores raw bytes; `get(column)` decodes only the requested column via binary search on the offset directory
- **Zero-copy filter evaluation** — `extract_column_raw` returns a slice into the page; `eval_filter_raw` compares raw bytes without allocating a `Value`
- **In-place row patching** — `patch_row` splices raw bytes for single-column updates without full decode/encode
- **Batch bulk operations** — `delete_matching`/`update_matching` walk the leaf chain once, rebuilding each page in a single pass instead of per-row tree surgery
- **Per-table encryption** — opt-in AES-256-GCM at the page level. Plaintext in memory, ciphertext on disk. Zero overhead on unencrypted tables
- **Async API** — optional `tokio` feature provides `AsyncBoogyDb` with zero-overhead async methods

## Quick Start

```toml
[dependencies]
boogy-db = { path = "." }
```

```rust
use boogy_db::*;

let db = BoogyDb::open("my.boogy")?;

db.create_table("users", &[
    ColumnDef::new("name", Type::Text),
    ColumnDef::new("email", Type::Text),
])?;

// Insert (auto-increment ID)
let id = db.insert("users", &[
    ("name", Value::Text("Alice".into())),
    ("email", Value::Text("alice@example.com".into())),
])?;

// Get by ID
let row = db.get("users", id)?.unwrap();
println!("{}", row.get("name").unwrap()); // Text("Alice")

// Query with filters, sort, pagination
let result = db.find("users", FindOptions {
    filters: vec![Filter::eq("name", "Alice")],
    limit: Some(10),
    ..Default::default()
})?;

// Secondary indexes
db.create_index("users", "idx_email", "email")?;

// Count (uses index when available)
let n = db.count("users", &[Filter::eq("email", "alice@example.com")])?;
```

## API

| Method | Description |
|--------|-------------|
| `insert(table, data)` | Insert a row, returns auto-increment `u64` ID |
| `insert_with_id(table, id, data)` | Insert with caller-supplied ID |
| `get(table, id)` | Get a row by ID |
| `update(table, id, fields)` | Update specific columns |
| `delete(table, id)` | Delete a row |
| `find(table, opts)` | Query with filters, sort, limit/offset |
| `count(table, filters)` | Count matching rows |
| `insert_many(table, rows)` | Batch insert |
| `update_where(table, filters, fields)` | Bulk update |
| `delete_where(table, filters)` | Bulk delete |
| `create_index(table, name, column)` | Create a secondary index |
| `drop_index(table, name)` | Drop an index |
| `create_table(table, columns)` | Create a table |
| `create_table_encrypted(table, columns, key)` | Create an encrypted table (AES-256-GCM) |
| `unlock_table(table, key)` | Provide the key for an encrypted table after reopen |
| `drop_table(table)` | Drop a table |
| `transaction(fn)` | Multi-table transaction |

## Benchmarks

All benchmarks run on:

| | |
|-|-|
| **CPU** | Intel Core i7-10875H @ 2.30 GHz (8 cores / 16 threads, boost to 5.1 GHz) |
| **RAM** | 32 GB DDR4 |
| **OS** | Manjaro Linux 6.18.4 |
| **Rust** | Edition 2024, compiled with `--release` |
| **SQLite** | 3.x via rusqlite with WAL mode, `synchronous=NORMAL` |

### Durability Modes

Each benchmark shows boogy-db at two durability settings:

- **None** — No WAL writes. Fastest mode. Data survives process crashes (OS page cache) but not power loss.
- **Normal** — Redo-log WAL: after-images written on every commit, not fsynced. Data file updated on checkpoint (clean shutdown). Survives process crashes; may lose the last few milliseconds of writes on power loss. This is the production-realistic setting, comparable to SQLite's `synchronous=NORMAL`.

SQLite runs at `PRAGMA synchronous=NORMAL` (WAL mode) in all tests.

### Point Operations

Isolated insert and get operations at various table sizes. Each row has 3 columns (two short text fields and an integer).

| Table Size | Insert (None) | Insert (Normal) | Get |
|-----------|--------------|----------------|-----|
| 100 rows | 428K/s (2.3 us) | 203K/s (4.9 us) | 5.2M/s (0.2 us) |
| 1,000 rows | 440K/s (2.3 us) | 199K/s (5.0 us) | 5.2M/s (0.2 us) |
| 5,000 rows | 441K/s (2.3 us) | 207K/s (4.8 us) | 4.8M/s (0.2 us) |
| 10,000 rows | 433K/s (2.3 us) | 207K/s (4.8 us) | 3.8M/s (0.3 us) |

Get performance is identical across durability modes (reads don't touch the WAL).

### Mixed Workload (Single Thread)

A realistic API workload: 30% inserts, 30% point reads, 25% filtered queries (equality filter with LIMIT 20), and 15% filtered counts. The table starts with 1,000 rows (3 text/integer columns) and grows over 5 seconds. Filtered operations search on one of 10 distinct values.

**Without secondary index** (both engines do full table scans for filtered queries):

| | boogy (None) | boogy (Normal) | SQLite | ratio (Normal vs SQLite) |
|-|-------------|---------------|--------|---|
| **Total ops/sec** | **21,118** | **21,112** | 11,798 | **1.79x** |
| p50 latency | 2 us | 5 us | 8 us | |
| p99 latency | 530 us | 521 us | 913 us | |
| Insert | 6,292/s | 6,290/s | 3,551/s | |
| Get | 6,346/s | 6,344/s | 3,527/s | |
| Find (limit 20) | 5,270/s | 5,269/s | 2,936/s | |
| Count | 3,210/s | 3,210/s | 1,784/s | |

Without indexes, reads dominate the workload and WAL overhead is negligible. boogy-db is ~1.79x faster at both durability levels.

**With secondary index** on the filter column:

| | boogy (None) | boogy (Normal) | SQLite | ratio |
|-|-------------|---------------|--------|---|
| **Total ops/sec** | **90,640** | **82,228** | 39,359 | **2.09x** |
| p50 latency | 6 us | 6 us | 11 us | |
| p99 latency | 86 us | 81 us | 201 us | |
| Insert | 27,049/s | 24,544/s | 11,712/s | |
| Get | 27,291/s | 24,715/s | 11,815/s | |
| Find (limit 20) | 22,604/s | 20,507/s | 9,841/s | |
| Count | 13,696/s | 12,462/s | 5,991/s | |

With indexes, Normal durability is 2.09x faster than SQLite. The redo-log WAL keeps the None-to-Normal gap small (~10%) by writing only to the WAL, never to the data file during commits.

### Mixed Workload (Concurrent)

Same workload distributed across multiple threads hitting the same table simultaneously.

**Without secondary index:**

| Threads | boogy (None) | boogy (Normal) | SQLite | ratio |
|---------|-------------|---------------|--------|---|
| 1 | **21,206** | **21,072** | 11,580 | 1.82x |
| 2 | **22,029** | **21,855** | 15,301 | 1.43x |
| 4 | **23,630** | **23,749** | 19,198 | 1.24x |
| 8 | **26,152** | **25,918** | 21,217 | 1.22x |

**With secondary index:**

| Threads | boogy (None) | boogy (Normal) | SQLite | ratio |
|---------|-------------|---------------|--------|---|
| 1 | **93,815** | **85,013** | 39,157 | 2.17x |
| 2 | **94,612** | **84,870** | 42,288 | 2.01x |
| 4 | **101,765** | **88,480** | 46,639 | 1.90x |
| 8 | **109,207** | **89,601** | 51,073 | 1.75x |

Readers operate on a shared page cache without blocking each other or writers. The ratio column compares Normal (the production-realistic mode) against SQLite.

### Join Simulation (User + Posts)

Simulates a social media app fetching a user profile and their latest 5 posts. The database has 500 users with 50 posts each (25,000 posts total). Each query picks a random user, fetches their profile row, then fetches their 5 most recent posts sorted by timestamp.

boogy-db performs this as two separate calls (`get` for the user + `find` with filter/sort/limit for the posts). SQLite performs it as a single `JOIN` query with `ORDER BY`. Both engines have an index on the posts' author column where noted.

| Configuration | boogy (None) | boogy (Normal) | SQLite | ratio |
|---|---|---|---|---|
| No index, 1 thread | **2,419 q/s** | **2,397 q/s** | 743 q/s | 3.23x |
| With index, 1 thread | **66,317 q/s** | **66,682 q/s** | 44,619 q/s | 1.49x |
| With index, no sort | **467,255 q/s** | **462,829 q/s** | 161,346 q/s | 2.87x |
| With index, 4 threads | **175,424 q/s** | **160,466 q/s** | 120,091 q/s | 1.34x |
| With index, 8 threads | **210,254 q/s** | **185,018 q/s** | 120,673 q/s | 1.53x |

The "no sort" row shows performance when fetching any 5 posts without ordering, isolating the cost of application-side sorting. The sorted rows are the realistic case — boogy-db is 1.49x faster despite doing two separate queries and application-side sorting while SQLite uses its native query planner with a single JOIN.

### Bulk Operations

Batch insert, update, and delete operations. Bulk insert uses `insert_many` (boogy-db) vs a single `BEGIN/INSERT.../COMMIT` transaction (SQLite). Bulk update and delete operate on a 10,000-row table.

**Bulk Insert** (single batch):

| Batch Size | boogy (None) | boogy (Normal) | SQLite | ratio |
|-----------|-------------|---------------|--------|---|
| 100 | **848K r/s** | **888K r/s** | 493K r/s | 1.80x |
| 1,000 | **859K r/s** | **839K r/s** | 544K r/s | 1.54x |
| 10,000 | **781K r/s** | **809K r/s** | 569K r/s | 1.42x |
| 50,000 | **729K r/s** | **753K r/s** | 547K r/s | 1.38x |

**Bulk Insert with Index** on one column:

| Batch Size | boogy (None) | boogy (Normal) | SQLite | ratio |
|-----------|-------------|---------------|--------|---|
| 100 | **485K r/s** | **485K r/s** | 444K r/s | 1.09x |
| 1,000 | 386K r/s | 354K r/s | **435K r/s** | 0.81x |
| 10,000 | 319K r/s | 309K r/s | **404K r/s** | 0.77x |
| 50,000 | 260K r/s | 261K r/s | **388K r/s** | 0.67x |

**Bulk Update** (`update_where` vs `UPDATE ... WHERE`, 10K-row table):

| Rows Affected | boogy (None) | boogy (Normal) | SQLite | ratio |
|---|---|---|---|---|
| ~1,000 | **1.02M r/s** | 654K r/s | **1.02M r/s** | 0.64x |
| ~2,000 | **1.12M r/s** | 620K r/s | **836K r/s** | 0.74x |

**Bulk Delete** (`delete_where` vs `DELETE ... WHERE`, 10K-row table):

| Rows Deleted | boogy (None) | boogy (Normal) | SQLite | ratio |
|---|---|---|---|---|
| ~1,000 | **2.9M r/s** | **2.7M r/s** | 2.2M r/s | 1.22x |
| ~5,000 | **7.1M r/s** | 5.3M r/s | **6.1M r/s** | 0.87x |
| ~9,000 | **7.9M r/s** | 5.7M r/s | **7.7M r/s** | 0.75x |

boogy-db now beats SQLite on bulk inserts at ALL batch sizes (without index). Bulk delete wins at small batches. Bulk update and indexed bulk insert remain areas where SQLite leads.

## Encryption

boogy-db supports opt-in AES-256-GCM encryption at the table level. Encrypted tables store ciphertext on disk and in the WAL, while the in-memory page cache always holds plaintext. Unencrypted tables have zero encryption overhead.

### Creating an Encrypted Table

```rust
let key: [u8; 32] = /* your 256-bit key */;

db.create_table_encrypted("secrets", &[
    ColumnDef::new("token", Type::Text),
    ColumnDef::new("data", Type::Blob),
], &key)?;

// All operations work identically — encryption is transparent
let id = db.insert("secrets", &[
    ("token", Value::Text("sk_live_abc123".into())),
    ("data", Value::Blob(sensitive_bytes)),
])?;

let row = db.get("secrets", id)?.unwrap();
```

### Reopening an Encrypted Database

Keys are never stored on disk. On reopen, call `unlock_table` before accessing encrypted tables:

```rust
let db = BoogyDb::open("my.boogy")?;

// Unencrypted tables work immediately
let _ = db.get("public_table", 1)?;

// Encrypted tables require the key
db.unlock_table("secrets", &key)?;
let _ = db.get("secrets", 1)?;

// Without unlocking, operations return BoogyError::TableLocked
```

### How It Works

- **Algorithm**: AES-256-GCM with random 12-byte nonces per page write. The GCM auth tag provides both confidentiality and integrity (stronger than CRC32).
- **Encrypted page layout**: `[nonce:12][ciphertext:4068][auth_tag:16]` = 4096 bytes (same page size).
- **Key management**: Caller provides a raw `[u8; 32]` key. Key derivation (Argon2, HKDF, etc.) is the caller's responsibility. Different tables can use different keys.
- **Encryption points**: Pages are encrypted before writing to the WAL and data file, decrypted on cache miss when reading from disk. The page cache always holds plaintext for fast access.
- **Wrong key detection**: `unlock_table` verifies the key by attempting to decrypt the table's root page. If the GCM auth tag doesn't match, it returns `BoogyError::InvalidKey`.
- **Index encryption**: Secondary indexes on encrypted tables are encrypted with the same key.
- **Performance impact**: ~1.5µs per page for AES-256-GCM with AES-NI. Only affects cache misses and WAL writes — cached reads have zero overhead.

## Async API

Enable the `tokio` feature for async support:

```toml
[dependencies]
boogy-db = { path = ".", features = ["tokio"] }
```

`AsyncBoogyDb` wraps the synchronous core with zero overhead — methods call the sync implementation directly without `spawn_blocking` or thread dispatch. This works because boogy-db operations are fast (microsecond-scale for cached reads) and rarely block on disk I/O in steady state.

```rust
use boogy_db::AsyncBoogyDb;

#[tokio::main]
async fn main() -> boogy_db::Result<()> {
    let db = AsyncBoogyDb::open("my.boogy").await?;

    db.create_table("users", &[
        ColumnDef::new("name", Type::Text),
    ]).await?;

    let id = db.insert("users", &[
        ("name", Value::Text("Alice".into())),
    ]).await?;

    let row = db.get("users", id).await?.unwrap();
    println!("{:?}", row.get("name"));

    Ok(())
}
```

`AsyncBoogyDb` is `Clone` (backed by `Arc<BoogyDb>`), so it can be shared across tasks cheaply. All methods from the sync API are available. The full sync `BoogyDb` is also accessible via `db.inner()`.

## Architecture

- **Storage**: Single file per database, 4 KB page-aligned. Page 0 is the system page (table registry). Each table is a separate B+ tree.
- **Row format**: `[rowid:8][num_cols:2][offset_directory: num_cols × 4 bytes][column_data]`. Each offset directory entry is `[col_id:2][data_offset:2]`, sorted by `col_id` for binary-search column access. `patch_row` splices raw bytes to replace a single column without full decode/encode; `patch_row_multi` chains patches for multi-column updates.
- **B+ tree**: `BTreeReader` (takes `&PageFile`, read-only) and `BTreeWriter` (takes `&mut WriteGuard`, exclusive). u64 integer keys with fixed 12-byte branch entries (`[child:4][key:8]`). Leaf pages store rows inline with a row-offset array. `scan_filtered` evaluates filters on raw page bytes via `extract_column_raw` + `eval_filter_raw`, falling back to decode only when the raw path doesn't cover the type/op. `delete_matching`/`update_matching` walk the leaf chain once for batch page rebuilds. `multi_get_sorted` batch-fetches clustered rowids via a single leaf-chain walk.
- **Indexes**: Each secondary index is a separate B+ tree (`IndexTreeReader`/`IndexTreeWriter`) keyed by composite `(encoded_value, rowid)` bytes. Values are encoded for correct byte-order sorting (integers: big-endian with sign-flip; floats: IEEE 754 with sign normalization; text: null-terminated UTF-8). Index lookups use `scan_prefix` to find all rowids for a value, then `multi_get_sorted` to batch-fetch the matching rows.
- **Concurrency**: Per-table `RwLock` for table metadata. Page cache is `RwLock<Vec<Option<Arc<Page>>>>` — readers take a shared lock, clone the `Arc` pointer, and release immediately. Writers get exclusive access to a `Mutex<WriteState>` dirty-page overlay via `WriteGuard`; `peek_dirty` provides zero-copy reads of dirty pages during tree traversal. `BTreeReader`/`IndexTreeReader` take `&PageFile` and never hold any lock during tree traversal.
- **Lazy Row**: The public `Row` type stores raw bytes (`Vec<u8>`) and column names (`Arc<Vec<String>>`). `row.get("name")` decodes only the requested column via `extract_column` (binary search on the offset directory). `row.columns()` does full decode only when all columns are needed.
- **Encryption**: Per-table AES-256-GCM via `Cipher` in `crypto.rs`. `TableMeta` stores `encrypted: bool` (persisted in system page) and `cipher: Option<Cipher>` (in-memory only). `commit_write` encrypts after-images before WAL append. `sync_all` encrypts pages before disk flush. `unlock_table` decrypts and preloads all table pages into cache. The system page is never encrypted (schema metadata stays plaintext).
- **WAL**: Redo-log (after-image) design. `WriteGuard::commit()` publishes dirty pages to the shared cache and returns after-images. The commit path writes these after-images to the WAL — the data file is never modified during commits. On clean shutdown (`Drop`), all cached pages are flushed to the data file and the WAL is truncated. On crash recovery, the WAL is replayed forward to apply committed pages. Configurable durability: `Immediate` (fsync WAL every commit), `Normal` (WAL writes without fsync), `None` (no WAL writes).

## License

Apache-2.0
