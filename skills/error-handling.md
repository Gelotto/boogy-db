# Handling boogy-db Errors

How to handle `BoogyError` variants in application code.

## Error Type

All boogy-db operations return `boogy_db::Result<T>`, which is `std::result::Result<T, BoogyError>`. `BoogyError` implements `std::error::Error` and `Display`.

## Error Variants

| Variant | When it occurs |
|---------|---------------|
| `Io(io::Error)` | File system errors (disk full, permissions, file not found) |
| `Corruption(String)` | Checksum mismatch, invalid page data, bad WAL entry |
| `TableNotFound(String)` | Operating on a table that doesn't exist |
| `TableExists(String)` | `create_table` with a name that's already taken |
| `RowNotFound(String)` | Internal -- rarely surfaced to application code |
| `DuplicateKey(u64)` | `insert_with_id` with a rowid that already exists |
| `SchemaMismatch(String)` | Column count/type mismatch during insert |
| `TypeMismatch(String)` | Value type doesn't match an indexed column's type |
| `IndexNotFound(String)` | `drop_index` on an index that doesn't exist |
| `IndexExists(String)` | `create_index` with a name that's already taken |
| `TableLocked(String)` | Operating on an encrypted table before calling `unlock_table` |
| `DecryptionFailed(String)` | Wrong encryption key provided to `unlock_table` |
| `InvalidKey(String)` | Malformed encryption key |
| `PageFull` | Row too large for a page (>4068 bytes) |
| `TransactionConflict` | ACID transaction detected a conflict during commit |

## Propagation with ?

Propagate with `?` and handle at the boundary. Implement `From<BoogyError>` for your app's error type:

```rust
fn create_user(db: &BoogyDb, name: &str, email: &str) -> boogy_db::Result<u64> {
    db.insert("users", &[
        ("name", Value::Text(name.into())),
        ("email", Value::Text(email.into())),
    ])
}
```

## Matching Specific Errors

```rust
match db.insert_with_id("users", rowid, &data) {
    Ok(()) => println!("created user {rowid}"),
    Err(BoogyError::DuplicateKey(id)) => { db.update("users", id, &data)?; }
    Err(e) => return Err(e),
}
```

## Common Patterns

### Upsert (insert or update)
```rust
fn upsert(db: &BoogyDb, id: u64, data: &[(&str, Value)]) -> boogy_db::Result<()> {
    match db.insert_with_id("items", id, data) {
        Ok(()) => Ok(()),
        Err(BoogyError::DuplicateKey(_)) => {
            db.update("items", id, data)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}
```

### Ensure table exists
```rust
fn ensure_table(db: &BoogyDb) -> boogy_db::Result<()> {
    match db.create_table("cache", &[ColumnDef::new("data", Type::Blob)]) {
        Ok(()) => Ok(()),
        Err(BoogyError::TableExists(_)) => Ok(()), // already created
        Err(e) => Err(e),
    }
}
```

### User-facing error messages
```rust
fn handle_db_error(e: BoogyError) -> String {
    match e {
        BoogyError::TableLocked(t) => format!("Table '{t}' is locked. Provide the encryption key."),
        BoogyError::DuplicateKey(id) => format!("Record {id} already exists."),
        BoogyError::TypeMismatch(msg) => format!("Invalid data type: {msg}"),
        BoogyError::PageFull => "Row data is too large. Reduce the size of text/blob fields.".into(),
        BoogyError::Corruption(_) => "Database file is corrupted. Restore from backup.".into(),
        other => format!("Database error: {other}"),
    }
}
```

### Transaction conflict retry
```rust
for _ in 0..3 {
    let mut tx = db.begin()?;
    // ... do work ...
    match tx.commit() {
        Ok(()) => return Ok(()),
        Err(BoogyError::TransactionConflict) => continue,
        Err(e) => return Err(e),
    }
}
```

### Corruption recovery
If you encounter `BoogyError::Corruption`, the database file or WAL is damaged. Options:
1. **Restore from backup** -- the safest path
2. **Delete the WAL file** (`*.boogy.wal`) and reopen -- loses uncommitted data but may recover the last clean checkpoint
3. **Start fresh** -- delete the `.boogy` file and rebuild from your data source
