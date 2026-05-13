# Integer Keys & Index Redesign

Replace string (UUID) keys with u64 integer keys and redesign secondary indexes as composite-key B+ trees.

## Motivation

The B+ tree currently uses 36-byte UUID string keys. Switching to u64 gives:
- Faster comparisons (single integer compare vs 36-byte memcmp)
- Higher branch fanout (fixed 8-byte keys vs variable-length strings)
- 28 fewer bytes per row (8 bytes vs 36+2 length prefix)
- Simpler index architecture (index maps column_value to u64 rowid, point lookup is one integer search)

## Part 1: Integer Keys

### Row Format

Current:
```
[id_len:2][id_bytes:variable]
[num_cols:2][offset_directory][column_data]
```

New:
```
[rowid:8]  (u64 little-endian, fixed size)
[num_cols:2][offset_directory][column_data]
```

### B+ Tree Changes

**Leaf pages:** Row data starts with `[rowid:8]` instead of `[id_len:2][id_bytes]`. Key extraction reads a fixed 8 bytes. Row ordering and binary search use `u64::cmp`.

**Branch pages:** Entries become fixed-size:
```
[child_0:4][key_0:8][child_1:4][key_1:8]...[child_N:4]
```
No `key_len` field needed. This maximizes fanout — a 4KB branch page fits ~330 keys (vs ~90 with UUID strings).

**Method signatures:**
- `insert(id: &str, row_data: &[u8])` becomes `insert(rowid: u64, row_data: &[u8])`
- `search(id: &str)` becomes `search(rowid: u64)`
- `delete(id: &str)` becomes `delete(rowid: u64)`
- `scan_all` returns `Vec<(u64, Vec<u8>)>` instead of `Vec<(String, Vec<u8>)>`
- All internal helpers (`find_insertion_point`, `find_child`, etc.) switch from `&str` to `u64`

### Auto-Increment Row IDs

Each table tracks `next_rowid: u64` in `TableMeta`, starting at 1.

- `insert()` with no explicit ID assigns `next_rowid` and increments it.
- `insert_with_id(rowid)` uses the caller's ID. If `rowid >= next_rowid`, sets `next_rowid = rowid + 1`.
- `next_rowid` is persisted in the system page (page 0) alongside `root_page` and `row_count`.

### Public API Changes

```rust
pub struct Row {
    pub id: u64,  // was String
    pub columns: Vec<(String, Value)>,
}

// Auto-increment (common case)
let id: u64 = db.insert("posts", &[
    ("author_id", Value::Text("user_42".into())),
    ("content", Value::Text("hello world".into())),
])?;

// Caller-supplied ID
db.insert_with_id("posts", 42, &[
    ("author_id", Value::Text("user_42".into())),
])?;

// Read/update/delete take u64
let row = db.get("posts", id)?;
db.update("posts", id, &[("content", Value::Text("edited".into()))])?;
db.delete("posts", id)?;

// Bulk ops unchanged in shape, just use u64 internally
db.insert_many("posts", &[row1_cols, row2_cols])?;  // returns Vec<u64>
db.update_where("posts", &filters, &updates)?;
db.delete_where("posts", &filters)?;
```

### System Page Format

Updated to include `next_rowid`:
```
[magic: 4 bytes = 0xB00D_5150]
[next_table_id: u32]
[num_tables: u16]
for each table:
  [table_id: u32][root_page: u32][row_count: u64][next_rowid: u64]
  [name_len: u16][name_bytes]
  [num_columns: u16]
  for each column: [col_id: u16][type_tag: u8][name_len: u16][name_bytes]
  [num_indexes: u16]
  for each index: [col_id: u16][root_page: u32]
```

## Part 2: Index Redesign

### Architecture

Each secondary index is a separate B+ tree. The key is a composite `(encoded_column_value, rowid)`. The payload is empty — the key alone is sufficient.

Lookup: range-scan the index tree for keys matching the target column value prefix, collect the rowids, then point-lookup each rowid in the data tree (a single u64 B+ tree search per row).

### Composite Key Encoding

Keys must sort correctly via byte comparison (`memcmp`). Encoding depends on column type:

**Integer (Type::Integer):**
```
[i64_sortable:8][rowid:8]   — 16 bytes total
```
`i64_sortable`: convert to big-endian, flip the sign bit (XOR top byte with 0x80). This gives correct ascending sort order for signed integers via unsigned byte comparison.

**Real (Type::Real):**
```
[f64_sortable:8][rowid:8]   — 16 bytes total
```
`f64_sortable`: IEEE 754 big-endian. For positive values, flip the sign bit. For negative values, flip all bits. This gives correct ascending sort for all finite values. NaN is not supported — reject at insert time.

**Text (Type::Text):**
```
[utf8_bytes][0x00][rowid:8]   — variable length
```
Null-terminated UTF-8. The null byte separates the text value from the rowid so that byte comparison sorts by text first, then by rowid for duplicates. Constraint: text values in indexed columns must not contain `0x00` bytes (reject at insert time).

**Null values:** Rows with NULL in an indexed column are not inserted into the index tree. Queries filtering on an indexed column with `IS NULL` fall back to a full scan.

### Type Enforcement

At insert and update time, if a row provides a value for an indexed column whose type does not match the column's declared `Type` (from `create_table`), return `BoogyError::TypeMismatch`. This ensures all entries in an index tree use the same encoding, so byte comparison is always valid.

### Index Operations

**create_index(table, column):**
1. Validate the column exists and has a declared type.
2. Allocate a new B+ tree root.
3. Scan all existing rows, encode `(col_value, rowid)` for each, insert into index tree.
4. Record the index in `TableMeta` and persist to system page.

**drop_index(table, column):**
1. Remove the index from `TableMeta`.
2. Free the index tree pages (or just abandon — the space is recoverable on compaction).
3. Persist to system page.

**On insert:**
For each index on the table, encode `(new_value, rowid)` and insert into the index tree.

**On delete:**
For each index on the table, encode `(old_value, rowid)` and delete from the index tree. The old value is obtained by reading the row before deletion.

**On update:**
For each index on the table whose indexed column changed:
1. Delete `(old_value, rowid)` from index tree.
2. Insert `(new_value, rowid)` into index tree.

**find() with indexed filter:**
1. Encode the filter value as a prefix (without rowid suffix).
2. Range-scan the index tree for all keys with that prefix.
3. Extract rowids from matching keys.
4. Point-lookup each rowid in the data tree.
5. Apply any remaining (non-indexed) filters, sort, and paginate.

### Index-Aware Query Planning

`find()` checks if any filter column has an index. If so, it uses the index path. If multiple indexed columns match, use the first one (simple heuristic — no cost-based optimizer).

For `eq` filters: scan for exact prefix match.
For `gt`/`gte`/`lt`/`lte` filters: scan the appropriate range.
For `ne` filters: do not use the index (full scan is simpler).

## Scope

This spec covers integer keys and secondary index redesign only. The following remain unchanged:
- WAL integration, crash recovery, durability config
- Per-table RwLock concurrency
- scan_filtered / count_filtered on non-indexed columns
- All existing filter operations (eq, ne, gt, gte, lt, lte, in, contains)

The following are deferred to future specs:
- MVCC (src/mvcc.rs)
- Group commit
- Multi-column indexes
- Stress tests, crash recovery tests, fuzz tests

## Files Changed

- `src/row.rs` — encode/decode with u64 rowid, extract_id returns u64
- `src/btree.rs` — u64 keys throughout, fixed-size branch entries
- `src/db.rs` — Row.id becomes u64, insert/get/update/delete take u64, auto-increment logic, index-aware find()
- `src/table.rs` — next_rowid in TableMeta, index metadata
- `src/page.rs` — system page format update for next_rowid
- `src/filter.rs` — index-aware query path in find/count
- `src/lib.rs` — re-export updated types
- All tests — update to use u64 IDs
