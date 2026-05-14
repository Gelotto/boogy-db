# boogy-db

An embedded storage engine for Rust that consistently outperforms SQLite on concurrent read/write workloads — typically 1.5-2.5x faster in mixed benchmarks, scaling to 100K+ operations/second with secondary indexes.

Built from scratch around a B+ tree with per-table concurrency, a redo-log WAL, lazy row decoding, and zero-copy filter evaluation. Supports ACID transactions, per-table AES-256-GCM encryption, overflow pages for large blobs, and an optional async API.

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
  - [ACID Transactions](#acid-transaction-benchmarks)
- [Encryption](#encryption)
- [Async API](#async-api)
- [ACID Transactions](#acid-transactions)
- [Skills & Guides](#skills--guides)
- [Architecture](#architecture)
- [License](#license)

## Features

**Storage**
- Integer-keyed B+ tree with auto-increment row IDs
- Secondary indexes with O(log n) lookup and insert
- Overflow pages for large rows — blobs up to 10MB (configurable), zero overhead on normal rows
- Redo-log WAL with configurable durability (immediate fsync, deferred, or none)
- Crash recovery via forward WAL replay

**Performance**
- Concurrent readers never block each other or writers
- Per-table write locks — different tables are fully concurrent
- Lazy row decoding — `row.get("column")` decodes only the requested column
- Zero-copy filter evaluation on raw page bytes
- Batch bulk operations via single-pass leaf-chain walks

**Security & Reliability**
- Per-table AES-256-GCM encryption (opt-in, zero overhead when off)
- ACID transactions with rollback (opt-in, zero overhead when off)

**Integration**
- Async API via optional `tokio` feature

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

// Transactions (guard-based)
let tx = db.begin()?;
tx.insert("users", &[("name", Value::Text("Bob".into()))])?;
tx.commit()?;
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
| `transaction(fn)` | Multi-table transaction (callback-based) |
| `begin()` | Begin a guard-based transaction (alternative to `transaction(fn)`) |
| `set_acid(enabled)` | Enable/disable ACID transaction mode |
| `set_max_row_size(bytes)` | Set maximum row size in bytes (default 10MB) |

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

### ACID Transaction Benchmarks

Compares boogy-db's ACID transactions (`set_acid(true)`) against SQLite's `BEGIN`/`COMMIT`. boogy-db uses `Durability::None`; SQLite uses WAL + `synchronous=NORMAL`. "fast" column shows boogy-db with ACID off (current non-atomic `begin()`/`commit()`).

**Transaction Insert** (N rows per transaction, 10K total rows):

| Rows/Tx | boogy (ACID) | boogy (fast) | SQLite | ratio |
|---------|-------------|-------------|--------|-------|
| 1 | **384K r/s** | 434K r/s | 130K r/s | 2.95x |
| 10 | **616K r/s** | 442K r/s | 436K r/s | 1.41x |
| 50 | **651K r/s** | 447K r/s | 609K r/s | 1.07x |
| 100 | **649K r/s** | 441K r/s | 635K r/s | 1.02x |
| 500 | 565K r/s | 448K r/s | **653K r/s** | 0.86x |

ACID transactions amortize commit overhead across rows. At 10+ rows/tx, boogy-db ACID is faster than the non-ACID fast path because dirty pages accumulate and are published in one batch.

**Mixed Transaction** (1 insert + 2 gets + 1 update per tx, 1K-row table):

| boogy (ACID) | boogy (fast) | SQLite | ratio |
|-------------|-------------|--------|-------|
| **110K tx/s** | 166K tx/s | 59K tx/s | **1.87x** |

**Single-Insert Transaction Throughput:**

| boogy (ACID) | boogy (fast) | SQLite | ratio |
|-------------|-------------|--------|-------|
| **391K tx/s** | 411K tx/s | 143K tx/s | **2.73x** |

**Rollback Cost** (begin + 10 inserts + drop, no commit):

| boogy (ACID) | boogy (fast) |
|-------------|-------------|
| 110K rb/s | 42K rb/s |

ACID rollback is faster than the fast path's "rollback" because ACID discards the private page buffer (cheap), while the fast path's individual commits can't be undone.

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

## ACID Transactions

boogy-db supports opt-in ACID-compliant transactions. When enabled, multi-operation transactions are truly atomic (all-or-nothing) with full rollback on failure or drop. When disabled (the default), operations commit individually for maximum throughput.

### Enabling ACID Mode

```rust
let db = BoogyDb::open("my.boogy")?;
db.set_acid(true);
db.set_durability(Durability::Immediate); // fsync for full durability
```

For full ACID compliance, combine `set_acid(true)` (atomicity + consistency) with `Durability::Immediate` (durability). `Durability::Normal` provides durability against process crashes but not power loss.

### Transactions

```rust
// Multi-operation transaction — all-or-nothing
let mut tx = db.begin()?;
tx.insert("users", &[("name", Value::Text("Alice".into()))])?;
tx.insert("posts", &[("title", Value::Text("Hello".into()))])?;
tx.commit()?;  // atomic publish — both rows visible at once

// Drop without commit = full rollback
{
    let mut tx = db.begin()?;
    tx.insert("users", &[("name", Value::Text("Bob".into()))])?;
    // dropped here — nothing is written, database unchanged
}

// Reads within a transaction see uncommitted writes
let mut tx = db.begin()?;
let id = tx.insert("users", &[("name", Value::Text("Carol".into()))])?;
let row = tx.get("users", id)?.unwrap(); // sees Carol
tx.commit()?;
```

When ACID mode is on, standalone operations outside `begin()` are automatically wrapped in mini-transactions. When off, operations commit individually with zero overhead — one `AtomicBool` check per operation.

### How It Works

ACID transactions hold a private dirty page buffer. Each operation briefly acquires the global write lock (microseconds), does the B+ tree mutation in the private buffer, and releases. Tables not touched by the transaction are completely unblocked. `commit()` publishes all pages atomically in one batch. Drop without commit discards the buffer — a zero-cost rollback.

## Skills & Guides

The `skills/` directory contains step-by-step guides for working with boogy-db:

**For application developers** (`skills/consumer/`):
- [Schema Design](skills/consumer/schema-design.md) — table layout, column types, indexes, migration patterns
- [Configuration](skills/consumer/configure-database.md) — durability, ACID mode, encryption setup
- [Query Patterns](skills/consumer/query-patterns.md) — filters, pagination, joins, bulk ops, transactions
- [Async Usage](skills/consumer/async-usage.md) — tokio integration, sharing across tasks, axum/actix patterns
- [Error Handling](skills/consumer/error-handling.md) — error variants, matching, recovery patterns

**For boogy-db contributors** (`skills/internal/`):
- [Adding a Table Method](skills/internal/add-table-method.md) — locking protocol, WriteGuard, index maintenance
- [Adding a Benchmark](skills/internal/add-benchmark.md) — benchmark structure, SQLite comparison
- [Optimizing Hot Paths](skills/internal/optimize-hot-path.md) — zero-copy, peek_dirty, leaf-chain walks
- [Modifying Page Format](skills/internal/modify-page-format.md) — page layout, header, compatibility

## Architecture

### Storage

Single file per database, 4 KB page-aligned. Each table is a separate B+ tree with u64 integer keys and fixed 12-byte branch entries. Rows are stored inline on leaf pages with an offset directory for O(1) column access. Rows exceeding page capacity (~4KB) spill into linked overflow pages transparently. The system page (table registry) is limited to 4KB, which constrains the total metadata size (table names, column definitions, index names). This accommodates approximately 20-30 tables with typical schemas.

### Concurrency

Readers and writers never block each other. The page cache (`Arc<Page>` pointers behind an `RwLock`) allows concurrent reads with a brief shared lock. Writers get exclusive access to a dirty-page overlay via `WriteGuard`, holding it only for the duration of a single B+ tree mutation. Per-table `RwLock`s ensure different tables are fully independent.

### WAL

Redo-log design. Commits write after-images to the WAL — the data file is never modified during normal operation. On clean shutdown, cached pages flush to the data file and the WAL is truncated. On crash, the WAL is replayed forward to restore committed state. Three durability levels: `Immediate` (fsync per commit), `Normal` (WAL write, no fsync), `None` (no WAL).

### Indexes

Each secondary index is a separate B+ tree keyed by composite `(encoded_value, rowid)` bytes, sorted for correct byte-order comparison. Lookups do a prefix scan on the index tree, then batch-fetch matching rows from the data tree in a single leaf-chain walk.

### Encryption

Per-table AES-256-GCM. Plaintext lives in the page cache; encryption happens only at I/O boundaries (WAL writes, disk flushes). Keys are never stored on disk. The system page (schema metadata) is always plaintext.

### ACID Transactions

When enabled, transactions hold a private dirty-page buffer. Each operation briefly acquires the global write lock, mutates pages in the buffer, and releases. `commit()` publishes all pages atomically. Drop without commit discards the buffer — zero-cost rollback. Tables not touched by a transaction are unblocked.

## License

Apache-2.0
