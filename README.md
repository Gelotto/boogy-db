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
- [Architecture](#architecture)
- [License](#license)

## Features

- **Integer-keyed B+ tree** with auto-increment row IDs and fixed 12-byte branch entries for high fanout
- **Secondary indexes** via composite-key B+ trees with O(log n) lookup and insert
- **Concurrent readers** that never block each other or writers (lock-free read path via `Arc<Page>` cache)
- **Per-table write locks** so writes to different tables are fully concurrent
- **Redo-log WAL** with configurable durability — commits write only to the WAL (one I/O per page), data file flushed on checkpoint
- **Crash recovery** via WAL redo replay on open
- **Lazy row decoding** that defers column extraction until access, avoiding allocation on the query hot path
- **Zero-copy filter evaluation** that compares raw bytes on disk pages without decoding values
- **Batch bulk operations** that walk the leaf chain once for delete/update instead of per-row tree surgery

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
| 100 rows | 439K/s (2.3 us) | 217K/s (4.6 us) | 5.3M/s (0.2 us) |
| 1,000 rows | 449K/s (2.2 us) | 214K/s (4.7 us) | 5.2M/s (0.2 us) |
| 5,000 rows | 444K/s (2.3 us) | 208K/s (4.8 us) | 4.5M/s (0.2 us) |
| 10,000 rows | 441K/s (2.3 us) | 212K/s (4.7 us) | 4.0M/s (0.3 us) |

Get performance is identical across durability modes (reads don't touch the WAL).

### Mixed Workload (Single Thread)

A realistic API workload: 30% inserts, 30% point reads, 25% filtered queries (equality filter with LIMIT 20), and 15% filtered counts. The table starts with 1,000 rows (3 text/integer columns) and grows over 5 seconds. Filtered operations search on one of 10 distinct values.

**Without secondary index** (both engines do full table scans for filtered queries):

| | boogy (None) | boogy (Normal) | SQLite | |
|-|-------------|---------------|--------|-|
| **Total ops/sec** | **20,397** | **20,856** | 11,716 | **1.78x** |
| p50 latency | 2 us | 5 us | 8 us | |
| p99 latency | 548 us | 524 us | 910 us | |
| Insert | 6,076/s | 6,217/s | 3,526/s | |
| Get | 6,135/s | 6,266/s | 3,500/s | |
| Find (limit 20) | 5,086/s | 5,205/s | 2,917/s | |
| Count | 3,101/s | 3,168/s | 1,773/s | |

Without indexes, reads dominate the workload and WAL overhead is negligible. boogy-db is ~1.78x faster at both durability levels.

**With secondary index** on the filter column:

| | boogy (None) | boogy (Normal) | SQLite | |
|-|-------------|---------------|--------|-|
| **Total ops/sec** | **92,723** | **82,952** | 39,570 | **2.10x** |
| p50 latency | 6 us | 6 us | 10 us | |
| p99 latency | 85 us | 75 us | 203 us | |
| Insert | 27,665/s | 24,759/s | 11,772/s | |
| Get | 27,913/s | 24,935/s | 11,877/s | |
| Find (limit 20) | 23,132/s | 20,683/s | 9,897/s | |
| Count | 14,012/s | 12,576/s | 6,023/s | |

With indexes, Normal durability is 2.10x faster than SQLite. The redo-log WAL keeps the None-to-Normal gap small (~10%) by writing only to the WAL, never to the data file during commits.

### Mixed Workload (Concurrent)

Same workload distributed across multiple threads hitting the same table simultaneously.

**Without secondary index:**

| Threads | boogy (None) | boogy (Normal) | SQLite | |
|---------|-------------|---------------|--------|-|
| 1 | **20,956** | **20,903** | 11,460 | 1.82x |
| 2 | **22,122** | **21,660** | 15,237 | 1.42x |
| 4 | **23,570** | **23,654** | 19,285 | 1.23x |
| 8 | **26,271** | **26,440** | 21,823 | 1.21x |

**With secondary index:**

| Threads | boogy (None) | boogy (Normal) | SQLite | |
|---------|-------------|---------------|--------|-|
| 1 | **95,450** | **85,054** | 39,296 | 2.16x |
| 2 | **96,317** | **85,686** | 42,101 | 2.04x |
| 4 | **103,643** | **89,533** | 46,007 | 1.95x |
| 8 | **109,451** | **90,447** | 48,006 | 1.88x |

Readers operate on a shared page cache without blocking each other or writers. The ratio column compares Normal (the production-realistic mode) against SQLite.

### Join Simulation (User + Posts)

Simulates a social media app fetching a user profile and their latest 5 posts. The database has 500 users with 50 posts each (25,000 posts total). Each query picks a random user, fetches their profile row, then fetches their 5 most recent posts sorted by timestamp.

boogy-db performs this as two separate calls (`get` for the user + `find` with filter/sort/limit for the posts). SQLite performs it as a single `JOIN` query with `ORDER BY`. Both engines have an index on the posts' author column where noted.

| Configuration | boogy (None) | boogy (Normal) | SQLite | |
|---|---|---|---|---|
| No index, 1 thread | **2,384 q/s** | **2,389 q/s** | 715 q/s | 3.34x |
| With index, 1 thread | **65,671 q/s** | **65,543 q/s** | 43,444 q/s | 1.51x |
| With index, no sort | **456,755 q/s** | **460,988 q/s** | 155,505 q/s | 2.96x |
| With index, 4 threads | **157,930 q/s** | **161,514 q/s** | 123,131 q/s | 1.31x |
| With index, 8 threads | **262,223 q/s** | **184,386 q/s** | 124,978 q/s | 1.47x |

The "no sort" row shows performance when fetching any 5 posts without ordering, isolating the cost of application-side sorting. The sorted rows are the realistic case — boogy-db is 1.51x faster despite doing two separate queries and application-side sorting while SQLite uses its native query planner with a single JOIN.

### Bulk Operations

Batch insert, update, and delete operations. Bulk insert uses `insert_many` (boogy-db) vs a single `BEGIN/INSERT.../COMMIT` transaction (SQLite). Bulk update and delete operate on a 10,000-row table.

**Bulk Insert** (single batch):

| Batch Size | boogy (None) | boogy (Normal) | SQLite | |
|-----------|-------------|---------------|--------|-|
| 100 | **820K r/s** | **907K r/s** | 526K r/s | 1.72x |
| 1,000 | **673K r/s** | **676K r/s** | 557K r/s | 1.21x |
| 10,000 | **657K r/s** | **650K r/s** | 569K r/s | 1.14x |
| 50,000 | 547K r/s | 546K r/s | **579K r/s** | 0.94x |

**Bulk Insert with Index** on one column:

| Batch Size | boogy (None) | boogy (Normal) | SQLite | |
|-----------|-------------|---------------|--------|-|
| 100 | 434K r/s | **465K r/s** | 443K r/s | 1.05x |
| 1,000 | 324K r/s | 320K r/s | **437K r/s** | 0.73x |
| 10,000 | 269K r/s | 262K r/s | **406K r/s** | 0.65x |
| 50,000 | 208K r/s | 202K r/s | **391K r/s** | 0.52x |

**Bulk Update** (`update_where` vs `UPDATE ... WHERE`, 10K-row table):

| Rows Affected | boogy (None) | boogy (Normal) | SQLite | |
|---|---|---|---|---|
| ~1,000 | **1.1M r/s** | 705K r/s | **1.03M r/s** | 0.68x |
| ~2,000 | **1.1M r/s** | 679K r/s | **1.00M r/s** | 0.68x |

**Bulk Delete** (`delete_where` vs `DELETE ... WHERE`, 10K-row table):

| Rows Deleted | boogy (None) | boogy (Normal) | SQLite | |
|---|---|---|---|---|
| ~1,000 | **3.4M r/s** | **2.9M r/s** | 2.2M r/s | 1.34x |
| ~5,000 | **7.4M r/s** | **5.5M r/s** | 5.9M r/s | 0.93x |
| ~9,000 | **9.0M r/s** | **6.2M r/s** | 7.4M r/s | 0.84x |

boogy-db wins on bulk inserts up to ~50K rows and on small-batch bulk deletes. Bulk update with Normal durability trails SQLite at ~0.68x — the per-page WAL write overhead compounds across the ~25 leaf pages touched. With None durability, bulk update beats SQLite (1.1M vs 1.03M). Indexed bulk inserts favor SQLite at scale due to tighter C-level index maintenance.

## Architecture

- **Storage**: Single file per database, 4 KB page-aligned. Page 0 is the system page (table registry). Each table is a separate B+ tree.
- **Row format**: `[rowid:8][num_cols:2][offset_directory][column_data]`. The offset directory enables O(1) column access by ID via binary search. Updates use `patch_row` to splice raw bytes without full decode.
- **B+ tree**: u64 integer keys with fixed 12-byte branch entries (`[child:4][key:8]`). Leaf pages store rows inline with a row-offset array. Bulk operations walk the leaf chain for batch page rebuilds.
- **Indexes**: Each secondary index is a separate B+ tree keyed by composite `(encoded_value, rowid)` bytes. Values are encoded for correct byte-order sorting (integers: big-endian with sign-flip; floats: IEEE 754 with sign normalization; text: null-terminated UTF-8).
- **Concurrency**: Per-table `RwLock` for table metadata. Page cache uses `RwLock<Vec<Option<Arc<Page>>>>` so readers clone `Arc` pointers without blocking. Writers get exclusive access to a dirty-page overlay via `WriteGuard`. BTreeReader/IndexTreeReader take `&PageFile` for lock-free reads.
- **WAL**: Redo-log (after-image) design. Commits write new page data to the WAL only — the data file is never modified during commits. On clean shutdown, all cached pages are flushed to the data file and the WAL is truncated. On crash recovery, the WAL is replayed forward to apply committed changes. Configurable durability: `Immediate` (fsync WAL every commit), `Normal` (WAL writes without fsync), `None` (no WAL writes).

## License

Apache-2.0
