# boogy-db Design Spec

## Purpose

An embedded storage engine for SpinStack, purpose-built to be faster than SQLite for concurrent API workloads. In-place B+ tree with WAL, per-table concurrency, MVCC readers, and a column-aware page format that avoids encode/decode overhead.

## Target Use Case

SpinStack APIs: many concurrent HTTP requests reading and writing to multiple tables in the same database. The engine runs on the host side (native Rust library). Wasm modules access it via WIT host functions through SpinStack's existing `AnyStore` abstraction.

## Storage Architecture

In-place B+ tree with WAL. Pages are 4KB, stored in a single file per database. The B+ tree stores rows directly in leaf pages — no separate encoding/decoding step. Internal pages store only keys + child pointers for maximum fanout.

Each table gets its own B+ tree root (stored in a system page at offset 0). Writes modify pages in-place. Before any page modification, the original page image is written to the WAL. On crash, the WAL is replayed to restore original pages (undo log). On checkpoint, the WAL is discarded.

### Per-Table Write Locks

Each table has an independent `RwLock`. Readers acquire a shared lock; writers acquire exclusive. Different tables are fully concurrent — writing to "posts" never blocks reading "comments".

### MVCC via WAL Snapshots

When a read transaction starts, it records the current WAL position. Any pages modified after that position are read from the WAL (original version) instead of the data file. This gives readers a consistent snapshot without blocking writers.

## Page Layout

### Leaf Page (4KB)

```
[page_header: 16 bytes]
  magic(2) | flags(2) | num_rows(2) | free_space_offset(2)
  | next_leaf(4) | prev_leaf(4)
[row_offset_array: 2 bytes x num_rows]  -- fixed at top, grows down
[free space]
[row_data]  -- packed at bottom, grows up
```

Each row is stored inline as:

```
[_id_len: 2][_id_bytes][num_cols: 2]
for each column: [col_id: 2][type_tag: 1][value_bytes]
```

Type tags: 0=null (0 bytes), 1=text (u32 len + utf8), 2=integer (i64 LE, 8 bytes), 3=real (f64 LE, 8 bytes), 4=blob (u32 len + raw), 5=boolean (1 byte).

Column IDs (u16, not names) are stored in rows. The schema table maps column IDs to names. This keeps rows compact.

### Branch Page

```
[page_header: 16 bytes]
[num_keys: 2]
[child_0: 4][key_0_len: 2][key_0_bytes]
[child_1: 4][key_1_len: 2][key_1_bytes]
...
[child_N: 4]
```

## WAL Design

Append-only file. Each entry:

```
[sequence: 8][table_id: 4][page_no: 4][page_data: 4096][checksum: 4]
```

On commit: append all dirty page images to WAL, then optionally fsync (configurable durability). On checkpoint: verify all committed pages are in the data file, then truncate WAL.

Group commit: multiple concurrent writers' WAL entries are batched into a single fsync call.

## Durability Model

Configurable per-database:

- `Durability::Immediate` — fsync WAL on every commit. Survives power loss.
- `Durability::Normal` — fsync WAL periodically / on group commit. Survives process crash; may lose last few ms on power loss.
- `Durability::None` — no fsync. Fastest. Data may be lost on any crash.

## Public API

```rust
let db = BoogyDb::open("path/to/db")?;

// Schema
db.create_table("posts", &[
    ColumnDef::new("author_id", Type::Text),
    ColumnDef::new("content", Type::Text),
    ColumnDef::new("created_at", Type::Integer),
])?;

db.drop_table("posts")?;

// Write (auto-commit)
let id = db.insert("posts", &[
    ("author_id", Value::Text("user_42")),
    ("content", Value::Text("hello world")),
    ("created_at", Value::Integer(1234567890)),
])?;

// Read
let row = db.get("posts", &id)?;

// Update
db.update("posts", &id, &[
    ("content", Value::Text("updated content")),
])?;

// Delete
db.delete("posts", &id)?;

// Query with filters, sort, pagination
let (rows, total) = db.find("posts", FindOptions {
    filters: &[Filter::eq("author_id", "user_42")],
    sort: &[Sort::desc("created_at")],
    limit: 20,
    offset: 0,
})?;

// Count
let n = db.count("posts", &[Filter::eq("author_id", "user_42")])?;

// OR-of-AND: `filters` is a mandatory AND-prefix; `or_groups: Vec<Vec<Filter>>`
// adds an OR clause — a row matches when ALL(filters) AND ANY(group: ALL(group)).
// Empty or_groups = filters-only (back-compat). count_with(table, filters, or_groups)
// counts with an OR clause.

// Multi-table transaction
db.transaction(|tx| {
    tx.insert("posts", &[...])?;
    tx.update("users", &id, &[("post_count", Value::Integer(5))])?;
    Ok(())
})?;
```

## Crash Safety

1. Before modifying any data page, write its original image to the WAL.
2. Modify page in-place in the data file.
3. On commit: WAL entries are durable (per durability setting).
4. On crash: replay WAL to restore all modified pages to pre-transaction state (undo).
5. On clean shutdown or checkpoint: WAL is truncated.

No data loss for committed transactions with `Durability::Immediate`. Uncommitted transactions are rolled back via WAL replay.

## File Layout

```
data.boogy        -- main data file (page-aligned, 4KB pages)
  page 0: system page (table registry, free page list head)
  page 1+: B+ tree pages for tables
data.boogy.wal    -- write-ahead log (append-only)
```

## Security

- CRC32 checksum per page header; validated on every read.
- Path traversal prevention on database file paths.
- No SQL injection surface (no SQL).
- Bounds checks on all page offset arithmetic.
- Integer overflow checks on page/row size calculations.

## Concurrency Model

- Per-table `RwLock`: readers share, writer exclusive. Cross-table operations are fully concurrent.
- MVCC: readers see a consistent snapshot as of transaction start. Writers don't block readers on the same table (readers use WAL for pre-modification page images).
- Group commit: concurrent writers across different tables batch their WAL fsyncs.

## Testing Strategy

- **Unit tests**: page layout (serialize/deserialize round-trips), B+ tree operations (insert/search/delete/split/merge), WAL (append/replay/checkpoint), MVCC (snapshot isolation).
- **Stress tests**: N concurrent reader threads + M concurrent writer threads on overlapping tables. Verify no data corruption, no deadlocks, correct counts.
- **Crash recovery tests**: inject crashes at every WAL state (before WAL write, after WAL write but before data write, after data write but before checkpoint). Verify recovery produces consistent state.
- **Fuzz tests**: random sequences of insert/update/delete/get/find operations. Verify results match an in-memory reference model.
- **Benchmark suite**: direct comparison against SQLite using SpinStack's `store-bench` workload.

## Crate Structure

```
boogy-db/
  src/
    lib.rs          -- public API (BoogyDb, Value, FindOptions, etc.)
    page.rs         -- page layout, read/write, checksums
    btree.rs        -- B+ tree insert/delete/search/range scan
    wal.rs          -- WAL append/replay/checkpoint/group commit
    mvcc.rs         -- snapshot management, version visibility
    table.rs        -- table registry, schema, per-table locks
    file.rs         -- file I/O, page cache, fsync
    error.rs        -- error types
  tests/
    crud_test.rs
    concurrent_test.rs
    crash_recovery_test.rs
    stress_test.rs
```

## Composite Indexes and Bounded-Batch Primitives

Secondary indexes shipped in v1 (single-column `create_index`). Multi-column and unique indexes were added as a follow-on primitive set alongside `scan_batch` and `upsert_increment`.

**Composite and unique indexes** (`create_index_ex(table, name, &[col], unique)`) extend the single-column case to multi-column `(v₁, v₂, …, rowid)` keys encoded with sortable per-type byte representations (big-endian integers, length-prefixed text). Concatenated encodings compose correctly — multi-column ordering is lexicographic over per-column orderings. When `unique = true`, the write path checks for an existing value-prefix entry under the table write lock before any mutation; a rejected `insert` or `upsert_increment` returns `BoogyError::UniqueViolation` with no partial state.

**`scan_batch`** and **`upsert_increment`** are the two bounded-memory streaming primitives. `scan_batch` pages through a table in primary-key or named-index order with a `ScanKey` resume token, applying filters and OR groups per row. The caller loops: fetch a page, process it, pass `last_key` as `after`; `last_key = None` signals exhaustion. `upsert_increment` atomically locates a row by a key tuple, adds an integer or real delta to a counter column, writes any additional `set` columns, and inserts the row if absent — designed to pair with a composite unique index so multi-call accumulation requires no caller-side find-then-insert logic.

## v1 Scope

### Core (this spec)
- create_table / drop_table
- insert_row / get_row / update_row / delete_row
- find_rows (with filters, sort, pagination, total count)
- count_rows
- Transactions (begin / commit / rollback)
- Crash recovery via WAL
- Per-table MVCC concurrency

### Shipped post-v1
- Secondary indexes (`create_index` / `drop_index`)
- Composite + unique indexes (`create_index_ex`)
- Bulk operations (`update_where` / `delete_where` / `insert_many`)
- Streaming cursor (`scan_batch`)
- Atomic keyed counter (`upsert_increment`)

### Deferred
- Foreign key enforcement
