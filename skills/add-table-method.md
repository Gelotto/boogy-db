# Adding a New Public Method to BoogyDb

All public API methods live in `src/db.rs` on the `BoogyDb` struct. Follow the existing locking protocol exactly. Naming pattern: verb + noun (e.g., `insert`, `find`, `count`, `delete_where`).

### 2. Follow the locking protocol

```rust
pub fn my_method(&self, table: &str, ...) -> Result<...> {
    // 1. Read-lock the table registry, clone the Arc to the table state.
    let table_state = {
        let tables = self.tables.read().unwrap();
        tables.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone()
    };

    // 2. Lock the specific table (read for queries, write for mutations).
    let state = table_state.read().unwrap();  // or .write() for mutations

    // 3. Do your work...
}
```

**Read operations**: Use `table_state.read().unwrap()` and `BTreeReader::new(&self.file, state.meta.root_page)`.

**Write operations**: Use `table_state.write().unwrap()` and work through a `WriteGuard`:

```rust
let durability = self.durability();
let mut guard = self.file.begin_write();
let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
// ... tree operations ...
state.meta.root_page = tree.root_page(); // or the return value of insert()
Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
```

### 3. Handle index maintenance (write ops only)

If your write operation modifies rows that have indexes:

```rust
if !state.meta.indexes.is_empty() {
    // Remove old index entries
    Self::index_update_row(&mut guard, &mut state.meta, rowid, &old_bytes, true)?;
    // Insert new index entries
    Self::index_update_row(&mut guard, &mut state.meta, rowid, &new_bytes, false)?;
}
```

### 4. Handle encryption (if applicable)

The page cache holds plaintext. Encryption is handled at I/O boundaries by `PageFile::sync_all()`. You do not need to encrypt/decrypt in your method -- just read and write plaintext pages.

### 5. Update the public exports

If your method returns a new type, export it from `src/lib.rs`:

```rust
pub use db::MyNewType;
```

### 6. Add tests

Add unit tests in the `mod tests` block at the bottom of `src/db.rs`. Use `tempfile::TempDir`:

```rust
#[test]
fn test_my_method() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let db = BoogyDb::open(&path).unwrap();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    // ... test your method ...
}
```

Add integration tests in `tests/crud_test.rs` for end-to-end scenarios.

### 7. Persist registry changes (if you modify table metadata)

If your method changes `TableMeta` (e.g., adds an index, changes row count for persistence):

```rust
let (metas, next_id) = self.snapshot_table_metas();
let durability = self.durability();
Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;
```

## Checklist

- [ ] Method follows the 3-step locking protocol (registry read -> table lock -> work)
- [ ] Write operations go through `WriteGuard` and call `Self::commit_write`
- [ ] Index maintenance handled for any row modifications
- [ ] Unit test in `db.rs`, integration test in `tests/`
- [ ] New public types exported from `lib.rs`
- [ ] `cargo test` passes
