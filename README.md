# boogy-db

A fast embedded storage engine for Rust, purpose-built for concurrent API workloads. In-place B+ tree with WAL, per-table concurrency, secondary indexes, and a column-aware page format that avoids encode/decode overhead.

## Table of Contents

- [Features](#features)
- [Quick Start](#quick-start)
- [API](#api)
- [Benchmarks](#benchmarks)
  - [Point Operations](#point-operations)
  - [Mixed Workload (Single Thread)](#mixed-workload-single-thread)
  - [Mixed Workload (Concurrent)](#mixed-workload-concurrent)
  - [Join Simulation (User + Posts)](#join-simulation-user--posts)
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

Both engines use their fastest safe durability settings (boogy-db: `Durability::None`, SQLite: WAL + `synchronous=NORMAL`). The SQLite comparison uses integer primary keys for a fair comparison on the primary key path.

### Point Operations

Isolated insert and get operations at various table sizes. Each operation is a single row with 3 columns (two short text fields and an integer).

| Table Size | Insert | Get |
|-----------|--------|-----|
| 100 rows | 500K/s (2.0 us) | 5.3M/s (0.2 us) |
| 1,000 rows | 511K/s (2.0 us) | 5.2M/s (0.2 us) |
| 5,000 rows | 488K/s (2.0 us) | 3.4M/s (0.3 us) |
| 10,000 rows | 456K/s (2.2 us) | 3.9M/s (0.3 us) |

### Mixed Workload (Single Thread)

A realistic API workload: 30% inserts, 30% point reads, 25% filtered queries (equality filter with LIMIT 20), and 15% filtered counts. The table starts with 1,000 rows (3 text/integer columns) and grows continuously over 5 seconds. Filtered operations search on one of 10 distinct values.

**Without secondary index** (both engines do full table scans for filtered queries):

| | boogy-db | SQLite | |
|-|----------|--------|-|
| **Total ops/sec** | **21,059** | 11,727 | **1.80x faster** |
| p50 latency | 2 us | 8 us | |
| p99 latency | 531 us | 897 us | |
| Insert | 6,273/s | 3,528/s | |
| Get | 6,328/s | 3,504/s | |
| Find (limit 20) | 5,256/s | 2,919/s | |
| Count | 3,202/s | 1,776/s | |

**With secondary index** on the filter column:

| | boogy-db | SQLite | |
|-|----------|--------|-|
| **Total ops/sec** | **95,646** | 39,583 | **2.42x faster** |
| p50 latency | 5 us | 11 us | |
| p99 latency | 84 us | 205 us | |
| Insert | 28,530/s | 11,776/s | |
| Get | 28,769/s | 11,881/s | |
| Find (limit 20) | 23,881/s | 9,902/s | |
| Count | 14,465/s | 6,025/s | |

### Mixed Workload (Concurrent)

Same workload as above, distributed across multiple threads hitting the same database and table simultaneously.

**Without secondary index:**

| Threads | boogy-db | SQLite | |
|---------|----------|--------|-|
| 1 | **21,184** | 11,585 | 1.83x |
| 2 | **22,133** | 15,385 | 1.44x |
| 4 | **23,911** | 19,295 | 1.24x |
| 8 | **26,049** | 21,654 | 1.20x |

**With secondary index:**

| Threads | boogy-db | SQLite | |
|---------|----------|--------|-|
| 1 | **95,480** | 38,980 | 2.45x |
| 2 | **96,607** | 41,765 | 2.31x |
| 4 | **105,205** | 46,862 | 2.24x |
| 8 | **112,352** | 50,515 | 2.22x |

boogy-db scales with concurrency because readers operate on a shared page cache (`Arc<Page>` behind an `RwLock`) without blocking each other or writers. Writers acquire a short-lived exclusive guard only for the pages they modify.

### Join Simulation (User + Posts)

Simulates a social media app fetching a user profile and their latest 5 posts. The database has 500 users with 50 posts each (25,000 posts total). Each query picks a random user, fetches their profile row, then fetches their 5 most recent posts.

boogy-db performs this as two separate calls (`get` for the user + `find` with filter/sort/limit for the posts). SQLite performs it as a single `JOIN` query. Both engines have an index on the posts' author column.

| Configuration | boogy-db | SQLite | |
|---|---|---|---|
| No index, 1 thread | **2,375 q/s** | 726 q/s | 3.27x |
| With index, 1 thread | **66,039 q/s** | 43,090 q/s | 1.53x |
| With index, no sort, 1 thread | **478,814 q/s** | 159,919 q/s | 2.99x |
| With index, 4 threads | **155,931 q/s** | 123,895 q/s | 1.26x |
| With index, 8 threads | **252,044 q/s** | 121,191 q/s | 2.08x |

The "no sort" row shows performance when fetching any 5 posts (no ORDER BY), isolating the sort overhead. The sorted case is the realistic one — boogy-db is 1.53x faster despite doing application-side sorting while SQLite uses its native query planner.

## Architecture

- **Storage**: Single file per database, 4 KB page-aligned. Page 0 is the system page (table registry). Each table is a separate B+ tree.
- **Row format**: `[rowid:8][num_cols:2][offset_directory][column_data]`. The offset directory enables O(1) column access by ID via binary search.
- **B+ tree**: u64 integer keys with fixed 12-byte branch entries (`[child:4][key:8]`). Leaf pages store rows inline with a row-offset array.
- **Indexes**: Each secondary index is a separate B+ tree keyed by composite `(encoded_value, rowid)` bytes. Values are encoded for correct byte-order sorting (integers: big-endian with sign-flip; floats: IEEE 754 with sign normalization; text: null-terminated UTF-8).
- **Concurrency**: Per-table `RwLock` for table metadata. Page cache uses `RwLock<Vec<Option<Arc<Page>>>>` so readers clone `Arc` pointers without blocking. Writers get exclusive access to a dirty-page overlay via `WriteGuard`.
- **WAL**: Before-image undo log. On crash, original pages are restored from the WAL. Configurable durability: `Immediate` (fsync every commit), `Normal` (fsync periodically), `None` (no WAL writes).

## License

Apache-2.0
