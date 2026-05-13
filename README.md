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
- **Write-ahead log** with configurable durability (immediate fsync, deferred, or none)
- **Crash recovery** via WAL before-image replay on open
- **Lazy row decoding** that defers column extraction until access, avoiding allocation on the query hot path
- **Zero-copy filter evaluation** that compares raw bytes on disk pages without decoding values

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
- **Normal** — WAL before-images written on every commit, but not fsynced. Survives process crashes; may lose the last few milliseconds of writes on power loss. This is the production-realistic setting, comparable to SQLite's `synchronous=NORMAL`.

SQLite runs at `PRAGMA synchronous=NORMAL` (WAL mode) in all tests.

### Point Operations

Isolated insert and get operations at various table sizes. Each row has 3 columns (two short text fields and an integer).

| Table Size | Insert (None) | Insert (Normal) | Get |
|-----------|--------------|----------------|-----|
| 100 rows | 494K/s (2.0 us) | 182K/s (5.5 us) | 5.2M/s (0.2 us) |
| 1,000 rows | 515K/s (1.9 us) | 183K/s (5.5 us) | 5.3M/s (0.2 us) |
| 5,000 rows | 506K/s (2.0 us) | 179K/s (5.6 us) | 5.0M/s (0.2 us) |
| 10,000 rows | 503K/s (2.0 us) | 178K/s (5.6 us) | 4.1M/s (0.2 us) |

Get performance is identical across durability modes (reads don't touch the WAL).

### Mixed Workload (Single Thread)

A realistic API workload: 30% inserts, 30% point reads, 25% filtered queries (equality filter with LIMIT 20), and 15% filtered counts. The table starts with 1,000 rows (3 text/integer columns) and grows over 5 seconds. Filtered operations search on one of 10 distinct values.

**Without secondary index** (both engines do full table scans for filtered queries):

| | boogy (None) | boogy (Normal) | SQLite | |
|-|-------------|---------------|--------|-|
| **Total ops/sec** | **20,972** | **20,898** | 11,732 | **1.78x** |
| p50 latency | 2 us | 6 us | 8 us | |
| p99 latency | 536 us | 519 us | 908 us | |
| Insert | 6,248/s | 6,228/s | 3,530/s | |
| Get | 6,301/s | 6,279/s | 3,506/s | |
| Find (limit 20) | 5,233/s | 5,215/s | 2,921/s | |
| Count | 3,190/s | 3,176/s | 1,776/s | |

Without indexes, the read-heavy workload dominates and WAL overhead has minimal impact. boogy-db is ~1.78x faster at both durability levels.

**With secondary index** on the filter column:

| | boogy (None) | boogy (Normal) | SQLite | |
|-|-------------|---------------|--------|-|
| **Total ops/sec** | **95,205** | **79,626** | 39,982 | **1.99x** |
| p50 latency | 5 us | 6 us | 11 us | |
| p99 latency | 84 us | 71 us | 203 us | |
| Insert | 28,399/s | 23,774/s | 11,901/s | |
| Get | 28,644/s | 23,933/s | 12,007/s | |
| Find (limit 20) | 23,763/s | 19,864/s | 9,995/s | |
| Count | 14,400/s | 12,055/s | 6,078/s | |

With indexes, Normal durability introduces WAL overhead on the higher insert throughput, reducing from 2.38x to 1.99x vs SQLite. Still nearly 2x faster.

### Mixed Workload (Concurrent)

Same workload distributed across multiple threads hitting the same table simultaneously.

**Without secondary index:**

| Threads | boogy (None) | boogy (Normal) | SQLite | |
|---------|-------------|---------------|--------|-|
| 1 | **21,246** | **21,184** | 11,580 | 1.83x |
| 2 | **22,112** | **21,380** | 15,340 | 1.39x |
| 4 | **23,967** | **23,731** | 19,707 | 1.20x |
| 8 | **26,247** | **26,290** | 21,878 | 1.20x |

**With secondary index:**

| Threads | boogy (None) | boogy (Normal) | SQLite | |
|---------|-------------|---------------|--------|-|
| 1 | **94,397** | **78,832** | 39,302 | 2.01x |
| 2 | **98,571** | **78,821** | 42,058 | 1.87x |
| 4 | **104,463** | **80,636** | 47,269 | 1.71x |
| 8 | **114,585** | **80,496** | 47,729 | 1.69x |

Readers operate on a shared page cache without blocking each other or writers. The ratio column compares Normal (the production-realistic mode) against SQLite.

### Join Simulation (User + Posts)

Simulates a social media app fetching a user profile and their latest 5 posts. The database has 500 users with 50 posts each (25,000 posts total). Each query picks a random user, fetches their profile row, then fetches their 5 most recent posts sorted by timestamp.

boogy-db performs this as two separate calls (`get` for the user + `find` with filter/sort/limit for the posts). SQLite performs it as a single `JOIN` query with `ORDER BY`. Both engines have an index on the posts' author column where noted.

| Configuration | boogy (None) | boogy (Normal) | SQLite | |
|---|---|---|---|---|
| No index, 1 thread | **2,402 q/s** | **2,339 q/s** | 741 q/s | 3.16x |
| With index, 1 thread | **66,418 q/s** | **66,283 q/s** | 42,874 q/s | 1.55x |
| With index, no sort | **473,413 q/s** | **463,180 q/s** | 157,134 q/s | 2.95x |
| With index, 4 threads | **157,812 q/s** | **161,155 q/s** | 120,416 q/s | 1.34x |
| With index, 8 threads | **167,231 q/s** | **201,428 q/s** | 122,736 q/s | 1.64x |

The "no sort" row shows performance when fetching any 5 posts without ordering, isolating the cost of application-side sorting. The sorted rows are the realistic case — boogy-db is 1.55x faster despite doing two separate queries and application-side sorting while SQLite uses its native query planner with a single JOIN.

### Bulk Operations

Batch insert, update, and delete operations. Bulk insert uses `insert_many` (boogy-db) vs a single `BEGIN/INSERT.../COMMIT` transaction (SQLite). Bulk update and delete operate on a 10,000-row table.

**Bulk Insert** (single batch):

| Batch Size | boogy (None) | boogy (Normal) | SQLite | |
|-----------|-------------|---------------|--------|-|
| 100 | **844K r/s** | **921K r/s** | 547K r/s | 1.68x |
| 1,000 | **717K r/s** | **741K r/s** | 578K r/s | 1.28x |
| 10,000 | **666K r/s** | **674K r/s** | 584K r/s | 1.15x |
| 50,000 | 553K r/s | 552K r/s | **598K r/s** | 0.92x |

**Bulk Insert with Index** on one column:

| Batch Size | boogy (None) | boogy (Normal) | SQLite | |
|-----------|-------------|---------------|--------|-|
| 100 | **486K r/s** | **464K r/s** | 445K r/s | 1.04x |
| 1,000 | 337K r/s | 334K r/s | **447K r/s** | 0.75x |
| 10,000 | 276K r/s | 268K r/s | **409K r/s** | 0.66x |
| 50,000 | 214K r/s | 211K r/s | **401K r/s** | 0.53x |

**Bulk Update** (`update_where` vs `UPDATE ... WHERE`, 10K-row table):

| Rows Affected | boogy (None) | boogy (Normal) | SQLite | |
|---|---|---|---|---|
| ~1,000 | 274K r/s | 219K r/s | **1.04M r/s** | 0.21x |
| ~2,000 | 269K r/s | 207K r/s | **1.06M r/s** | 0.20x |

**Bulk Delete** (`delete_where` vs `DELETE ... WHERE`, 10K-row table):

| Rows Deleted | boogy (None) | boogy (Normal) | SQLite | |
|---|---|---|---|---|
| ~1,000 | 670K r/s | 612K r/s | **2.2M r/s** | 0.28x |
| ~5,000 | 980K r/s | 867K r/s | **6.0M r/s** | 0.14x |
| ~9,000 | 1.0M r/s | 916K r/s | **7.6M r/s** | 0.12x |

boogy-db wins on small-to-medium bulk inserts. SQLite is significantly faster at bulk update and delete because it modifies rows in-place, while boogy-db performs individual B+ tree delete-and-reinsert operations per row. Indexed bulk inserts also favor SQLite at scale due to tighter C-level index maintenance. These are known areas for future optimization.

## Architecture

- **Storage**: Single file per database, 4 KB page-aligned. Page 0 is the system page (table registry). Each table is a separate B+ tree.
- **Row format**: `[rowid:8][num_cols:2][offset_directory][column_data]`. The offset directory enables O(1) column access by ID via binary search.
- **B+ tree**: u64 integer keys with fixed 12-byte branch entries (`[child:4][key:8]`). Leaf pages store rows inline with a row-offset array.
- **Indexes**: Each secondary index is a separate B+ tree keyed by composite `(encoded_value, rowid)` bytes. Values are encoded for correct byte-order sorting (integers: big-endian with sign-flip; floats: IEEE 754 with sign normalization; text: null-terminated UTF-8).
- **Concurrency**: Per-table `RwLock` for table metadata. Page cache uses `RwLock<Vec<Option<Arc<Page>>>>` so readers clone `Arc` pointers without blocking. Writers get exclusive access to a dirty-page overlay via `WriteGuard`.
- **WAL**: Before-image undo log. On crash, original pages are restored from the WAL. Configurable durability: `Immediate` (fsync every commit), `Normal` (WAL writes without fsync), `None` (no WAL writes).

## License

Apache-2.0
