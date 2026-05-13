# Lazy Row — Zero-Decode find() Hot Path

## Problem

Indexed find at 3K rows takes 18.8µs vs SQLite's 12.8µs. Approximately 8µs is spent decoding 20 rows: `decode_row` allocates Value structs (including String for Text), `decoded_to_row` clones column name Strings. This decode happens eagerly even when callers only access a subset of columns or discard results.

## Design

### Row Struct

Replace the current eagerly-decoded Row:

```rust
// Current
pub struct Row {
    pub id: u64,
    pub columns: Vec<(String, Value)>,
}
```

With a lazy wrapper around raw bytes:

```rust
// New
pub struct Row {
    pub id: u64,
    data: Vec<u8>,                  // raw encoded row bytes
    col_names: Arc<Vec<String>>,    // shared per-table schema, allocated once
}

impl Row {
    /// Get a single column value by name. Decodes only that column.
    pub fn get(&self, column: &str) -> Option<Value> {
        let col_id = self.col_names.iter().position(|n| n == column)? as u16;
        row::extract_column(&self.data, col_id).ok().flatten()
    }

    /// Decode all columns. Use when you need the full row.
    pub fn columns(&self) -> Vec<(String, Value)> {
        let decoded = row::decode_row(&self.data).unwrap();
        decoded.columns.into_iter().filter_map(|(col_id, val)| {
            self.col_names.get(col_id as usize)
                .map(|name| (name.clone(), val))
        }).collect()
    }
}
```

### Schema Sharing

`TableMeta` gets a new field: `col_names: Arc<Vec<String>>`. Built once in `TableMeta::new` from the column definitions. All Rows from the same table share this Arc — cloning it is one atomic increment (~1ns) vs cloning N Strings.

### find() Hot Path

Current (per matching row):
```
decode_row(bytes)           → allocates DecodedRow with Values
decoded_to_row(decoded)     → allocates Row with String column names
```

New (per matching row):
```
extract_id(bytes)           → read 8 bytes, no alloc
Row { id, data: bytes.to_vec(), col_names: arc.clone() }
```

Cost: one Vec<u8> allocation (raw bytes copy) + one Arc increment. No Value decode, no String clone.

### Filter Evaluation

The indexed find path applies remaining filters after index lookup. Currently this decodes the row and checks Value fields. With lazy Row, use raw-byte comparison instead:

```rust
// For each row in index results, before building Row:
if has_extra_filters {
    for filter in &extra_filters {
        let col_id = meta.col_id(&filter.column);
        let raw = row::extract_column_raw(&bytes, col_id);
        if !eval_filter_raw(raw, &filter.op, &filter.value) {
            skip this row
        }
    }
}
```

This reuses the existing `extract_column_raw` + `eval_filter_raw` zero-alloc path from scan_filtered.

### get() Hot Path

`BoogyDb::get()` currently returns `Option<Row>`. With the new Row, it just wraps the raw bytes — same as find. Callers access columns via `row.get("name")`.

### Internal Callers

Methods that need decoded column data for internal logic (update, delete with index maintenance, update_where, delete_where) continue using `row::decode_row` directly on raw bytes. They don't build Row structs for internal use.

### decoded_to_row Removal

The `decoded_to_row` helper function is removed entirely. All paths that return Row to callers use the new lazy constructor instead.

## Files Changed

- `src/db.rs` — Row struct changes, remove decoded_to_row, update find/get/count paths, add col_names to schema sharing
- `src/table.rs` — Add `col_names: Arc<Vec<String>>` to TableMeta
- `src/lib.rs` — No changes (Row is already re-exported from db)
- `tests/crud_test.rs` — Update from `row.columns[i]` to `row.get("name")` / `row.columns()`
- `benches/*.rs` — Update any Row field access

## Performance Target

Indexed find at 3K rows: <13µs (currently 18.8µs, SQLite 12.8µs).
Mixed indexed workload: close to or surpass SQLite's 40K ops/s.
