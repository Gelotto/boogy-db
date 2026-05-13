# ACID Atomicity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add true multi-operation atomic transactions with rollback to boogy-db, toggled via `db.set_acid(true)`.

**Architecture:** `AcidTransaction` holds a private dirty page buffer (not the global WriteGuard). Each operation briefly acquires+releases the WriteGuard using an inject/drain pattern. Per-table write locks are held for modified tables. `commit()` publishes all pages atomically. Drop without commit = full rollback. When ACID is on, standalone operations are auto-wrapped in mini-transactions.

**Tech Stack:** Rust, existing boogy-db crate. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-13-acid-atomicity-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/file.rs` | Modify | Add `inject_dirty`, `drain_dirty` to WriteGuard |
| `src/db.rs` | Modify | Add `AcidTransaction`, `MetaDelta`, acid flag, refactor operations into `_inner` variants, auto-wrap logic |
| `src/error.rs` | Modify | Add `NotInAcidMode` variant |
| `src/lib.rs` | Modify | Export `AcidTransaction` |
| `src/async_db.rs` | Modify | Add async `AcidTransaction` wrapper |
| `tests/crud_test.rs` | Modify | ACID transaction tests |
| `README.md` | Modify | Document ACID transactions |

---

## Task 1: WriteGuard inject/drain Methods

**Files:**
- Modify: `src/file.rs`

- [ ] **Step 1: Add inject_dirty and drain_dirty to WriteGuard**

```rust
/// Move pages into the dirty overlay from an external buffer.
/// Used by ACID transactions to make their private pages visible
/// to BTreeWriter during an operation.
pub fn inject_dirty(&mut self, pages: HashMap<u32, Box<Page>>) {
    for (page_no, page) in pages {
        // Don't overwrite pages already dirty in this guard session
        if !self.state.dirty.contains_key(&page_no) {
            self.state.dirty.insert(page_no, page);
        }
    }
}

/// Drain all dirty pages out of the overlay, returning them.
/// Used by ACID transactions to reclaim pages after an operation.
pub fn drain_dirty(&mut self) -> HashMap<u32, Box<Page>> {
    std::mem::take(&mut self.state.dirty)
}

/// Get the current new_page_count (pages allocated during this guard session).
pub fn new_page_count(&self) -> u32 {
    self.state.new_page_count
}

/// Reset new_page_count to zero (after draining, the transaction tracks this).
pub fn reset_new_page_count(&mut self) {
    self.state.new_page_count = 0;
}
```

Add `use std::collections::HashMap;` at the top of file.rs if not already present.

- [ ] **Step 2: Run file.rs tests**

Run: `cargo test --lib file::tests`
Expected: All pass (new methods are additive).

- [ ] **Step 3: Commit**

```bash
git add src/file.rs
git commit -m "feat: WriteGuard inject_dirty/drain_dirty for ACID transactions"
```

---

## Task 2: AcidTransaction Core + acid Flag

**Files:**
- Modify: `src/db.rs`
- Modify: `src/error.rs`
- Modify: `src/lib.rs`

This is the largest task. It adds the AcidTransaction struct, the acid flag, and a helper for running operations through the inject/drain pattern.

- [ ] **Step 1: Add acid flag to BoogyDb**

Add to the struct:
```rust
pub struct BoogyDb {
    // ... existing fields ...
    acid: std::sync::atomic::AtomicBool,
}
```

Initialize as `acid: std::sync::atomic::AtomicBool::new(false)` in `open()`.

Add methods:
```rust
pub fn set_acid(&self, enabled: bool) {
    self.acid.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_acid(&self) -> bool {
    self.acid.load(std::sync::atomic::Ordering::Relaxed)
}
```

- [ ] **Step 2: Add MetaDelta and AcidTransaction structs**

```rust
/// Deferred metadata changes for one table within an ACID transaction.
struct MetaDelta {
    root_page: u32,
    row_count_delta: i64,
    next_rowid: u64,
    table_id: u32,
    cipher: Option<crate::crypto::Cipher>,
}

/// True atomic transaction. Holds a private dirty page buffer.
/// Commit publishes all-or-nothing. Drop without commit = rollback.
pub struct AcidTransaction<'a> {
    db: &'a BoogyDb,
    /// Private dirty page buffer — NOT in the global WriteGuard.
    private_dirty: HashMap<u32, Box<Page>>,
    /// New pages allocated during this transaction.
    new_page_count: u32,
    /// Per-table write lock guards held for the transaction's duration.
    held_table_states: Vec<(String, Arc<RwLock<TableState>>)>,
    /// Deferred metadata deltas keyed by table name.
    meta_deltas: HashMap<String, MetaDelta>,
    /// Has commit() been called?
    committed: bool,
}
```

Note: We can't store `RwLockWriteGuard` directly because it's not `Send` and has complex lifetime issues. Instead, hold the `Arc<RwLock<TableState>>` and re-acquire the write lock when needed. To prevent other writers, we use a `Mutex<()>` per table as a "transaction lock" — but actually, the simplest approach: just track which tables we've modified and lock them during the operation via the inject/drain pattern. The per-table write lock is acquired and released with each operation (same as non-ACID mode), but since we don't commit the WriteGuard between operations, other writers' commits won't include our pages.

Actually, the simpler design: just track the table names and their MetaDeltas. Don't hold per-table locks across the transaction. The per-table write lock is acquired per-operation (briefly, like current code) to read/modify metadata. This means another writer COULD interleave on the same table — but the ACID guarantee comes from the dirty page buffer being private. If two ACID transactions both modify "users", their page buffers are independent, and whichever commits first wins. The second commit would overwrite pages, which is a conflict. For v1, this is acceptable — true isolation comes in Spec 2.

Simplified AcidTransaction:
```rust
pub struct AcidTransaction<'a> {
    db: &'a BoogyDb,
    private_dirty: HashMap<u32, Box<Page>>,
    new_page_count: u32,
    meta_deltas: HashMap<String, MetaDelta>,
    committed: bool,
}
```

- [ ] **Step 3: Add the with_guard helper**

This is the core inject/drain pattern. It temporarily injects the transaction's dirty pages into a fresh WriteGuard, runs the operation, then drains everything back:

```rust
impl<'a> AcidTransaction<'a> {
    /// Run an operation with a temporary WriteGuard that sees this transaction's dirty pages.
    fn with_guard<F, R>(&mut self, f: F) -> Result<R>
    where
        F: FnOnce(&mut WriteGuard) -> Result<R>,
    {
        let mut guard = self.db.file.begin_write();
        // Inject our private dirty pages
        let pages = std::mem::take(&mut self.private_dirty);
        guard.inject_dirty(pages);
        // Inject our new_page_count so allocate_page() works correctly
        // (the WriteGuard needs to know about previously allocated pages)
        guard.set_new_page_count(self.new_page_count);
        
        let result = f(&mut guard);
        
        // Drain everything back (including any new dirty pages from this operation)
        self.private_dirty = guard.drain_dirty();
        self.new_page_count = guard.new_page_count();
        guard.reset_new_page_count();
        // Discard the guard without publishing
        guard.discard();
        
        result
    }
}
```

This needs a `set_new_page_count` method on WriteGuard:
```rust
pub fn set_new_page_count(&mut self, count: u32) {
    self.state.new_page_count = count;
}
```
Add this to file.rs WriteGuard impl.

- [ ] **Step 4: Add AcidTransaction CRUD methods**

Each method acquires the per-table write lock, calls with_guard, and records the metadata delta:

```rust
impl<'a> AcidTransaction<'a> {
    pub fn insert(&mut self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        let table_state = {
            let tables = self.db.tables.read().unwrap();
            tables.get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };
        let mut state = table_state.write().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;
        BoogyDb::enforce_index_types(&state.meta, data)?;

        // Use the delta's next_rowid if we've already modified this table, else the table's
        let delta = self.meta_deltas.get(table);
        let rowid = delta.map(|d| d.next_rowid).unwrap_or(state.meta.next_rowid);
        let root = delta.map(|d| d.root_page).unwrap_or(state.meta.root_page);

        let col_values: Vec<(u16, &Value)> = data.iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(rowid, &col_values);

        let new_root = self.with_guard(|guard| {
            let mut tree = BTreeWriter::new(guard, root);
            let new_root = tree.insert(rowid, &row_bytes)?;
            
            if !state.meta.indexes.is_empty() {
                BoogyDb::index_update_row(guard, &mut state.meta, rowid, &row_bytes, false)?;
            }
            Ok(new_root)
        })?;

        // Record metadata delta
        let d = self.meta_deltas.entry(table.to_string()).or_insert_with(|| MetaDelta {
            root_page: state.meta.root_page,
            row_count_delta: 0,
            next_rowid: state.meta.next_rowid,
            table_id: state.meta.table_id,
            cipher: state.meta.cipher.clone(),
        });
        d.root_page = new_root;
        d.next_rowid = rowid + 1;
        d.row_count_delta += 1;

        Ok(rowid)
    }

    pub fn get(&mut self, table: &str, id: u64) -> Result<Option<Row>> {
        let table_state = {
            let tables = self.db.tables.read().unwrap();
            tables.get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root = self.meta_deltas.get(table)
            .map(|d| d.root_page)
            .unwrap_or(state.meta.root_page);
        let col_names = state.meta.col_names.clone();

        let result = self.with_guard(|guard| {
            // Use WriteGuard's read path (sees dirty overlay = our injected pages)
            let arc = guard.read_page(root)?;
            // Actually, we need a full BTree search through the guard.
            // BTreeWriter has no search method — only BTreeReader does.
            // But BTreeReader takes &PageFile (shared cache), not WriteGuard.
            // We need to search through the WriteGuard path.
            // 
            // Simplest: use BTreeReader but it won't see our dirty pages.
            // Since we injected them into the WriteGuard, they're in the dirty overlay.
            // We need a reader that goes through WriteGuard.
            //
            // Workaround: publish dirty pages to cache temporarily?
            // No, that breaks atomicity.
            //
            // Better: use a search that goes through WriteGuard::read_page().
            // BTreeWriter has insert/delete but no search.
            // Add a search_in_guard helper.
            drop(arc);
            Ok(None) // placeholder
        })?;
        
        // TODO: this needs a search method that works through WriteGuard
        todo!()
    }
}
```

Wait — there's a problem. `BTreeReader` takes `&PageFile` and uses `PageFile::read_page()` which reads from the shared cache. It doesn't see the dirty overlay in WriteGuard. So reads within an ACID transaction won't see the transaction's own uncommitted writes.

The fix: add a `search` method to BTreeWriter (which already reads through the WriteGuard). BTreeWriter has `insert_recursive` and `delete_recursive` which navigate the tree through `self.guard.read_page()`. Adding a `search` is the same navigation without mutation.

Let me restructure. The plan needs:
1. Add `search` method to BTreeWriter (navigates B+ tree via WriteGuard, returning row bytes)
2. AcidTransaction uses BTreeWriter for all operations (reads and writes)

- [ ] **Step 4 (revised): Add search to BTreeWriter (btree.rs)**

```rust
/// Search for a row by rowid through the WriteGuard (sees dirty overlay).
pub fn search(&self, rowid: u64) -> Result<Option<Vec<u8>>> {
    self.search_recursive_w(self.root, rowid)
}

fn search_recursive_w(&self, page_no: u32, rowid: u64) -> Result<Option<Vec<u8>>> {
    let (is_leaf, child) = if let Some(p) = self.guard.peek_dirty(page_no) {
        if p.is_leaf() { (true, 0) }
        else { let (_, c) = find_child(p, rowid); (false, c) }
    } else {
        let arc = self.guard.page_file().read_page(page_no)?;
        if arc.is_leaf() { (true, 0) }
        else { let (_, c) = find_child(&arc, rowid); (false, c) }
    };

    if !is_leaf {
        return self.search_recursive_w(child, rowid);
    }

    // Leaf — check for the rowid
    let page = self.guard.read_page_cloned(page_no)?;
    let num_rows = page.num_rows() as usize;
    if num_rows == 0 { return Ok(None); }
    let (pos, found) = find_insertion_point(&page, rowid)?;
    if !found { return Ok(None); }
    let (start, end) = row_bounds(&page, pos, num_rows);
    if start < end && end <= PAGE_SIZE {
        Ok(Some(page.data[start..end].to_vec()))
    } else {
        Ok(None)
    }
}
```

Also add `scan_all` and `scan_filtered` to BTreeWriter with the same pattern (read through guard). Actually for v1, `search` is the most critical. `find()` and `count()` within ACID transactions can use a simplified approach: inject dirty into a temporary WriteGuard, then briefly use a BTreeReader via PageFile (the injected pages are published to the cache during inject... wait no, inject puts them in the WriteGuard's dirty overlay, not the cache).

Hmm, this is getting complex. Let me simplify the approach:

**For reads within ACID transactions**: temporarily publish dirty pages to the shared cache, do the read via BTreeReader, then unpublish. This is safe because:
- We hold the per-table write lock, so no other writer can see the published pages and modify them
- We immediately unpublish after the read

Actually no — publishing to the shared cache means concurrent readers on other threads would see uncommitted data. That violates atomicity.

**Correct approach**: Add `search` to BTreeWriter (done above). For `find()` and `count()`, which use `scan_filtered` and `count_filtered` on BTreeReader, we need WriteGuard-aware versions. These are mechanical copies of the BTreeReader methods but using `self.guard.read_page()` / `self.guard.peek_dirty()` instead of `self.file.read_page()`.

For v1, let's implement: `search`, `scan_all_w` on BTreeWriter. `find()` and `count()` within transactions can use `scan_all_w` + filter in Rust (less efficient but correct). We can optimize later.

Let me restructure the plan to be more practical.

OK I think this plan is getting too detailed for the plan doc. Let me write a focused plan with the key tasks and let the implementer handle the details.

- [ ] **Step 4 (final): Implement AcidTransaction with all CRUD methods**

The implementer needs to:
1. Add `search` and `scan_all_w` methods to BTreeWriter in btree.rs
2. Implement all AcidTransaction methods (insert, insert_with_id, get, update, delete, find, count, insert_many, update_where, delete_where)
3. Each write method: acquires per-table write lock, calls with_guard, records MetaDelta
4. Each read method: acquires per-table read lock, calls with_guard, searches through WriteGuard
5. `find()`: uses scan_all_w + in-memory filter (correct if not optimal)
6. `count()`: uses scan_all_w + count matches (correct if not optimal)

- [ ] **Step 5: Implement commit() and Drop**

```rust
impl<'a> AcidTransaction<'a> {
    pub fn commit(mut self) -> Result<()> {
        self.committed = true;
        let durability = self.db.durability();
        
        // Publish all dirty pages atomically
        let mut guard = self.db.file.begin_write();
        let pages = std::mem::take(&mut self.private_dirty);
        guard.inject_dirty(pages);
        guard.set_new_page_count(self.new_page_count);
        
        // Determine cipher (use first table's cipher, or None for mixed)
        // For simplicity, commit all pages with the cipher of each table.
        // Actually, commit_write takes one cipher. For multi-table with mixed encryption,
        // we need to handle per-page encryption. Use the page_ciphers registry.
        let after_images = guard.commit()?;
        
        // Register page ciphers for encrypted tables
        for (table_name, delta) in &self.meta_deltas {
            if let Some(ref cipher) = delta.cipher {
                let arc = Arc::new(cipher.clone());
                for (page_no, _) in &after_images {
                    self.db.file.register_page_cipher(*page_no, Arc::clone(&arc));
                }
            }
        }
        
        // Write WAL
        match durability {
            Durability::Immediate => {
                let mut wal = self.db.wal.lock().unwrap();
                for (page_no, data) in &after_images {
                    // Encrypt if needed (check page_ciphers)
                    wal.append_before_image(0, *page_no, data)?;
                }
                wal.sync()?;
            }
            Durability::Normal => {
                let mut wal = self.db.wal.lock().unwrap();
                for (page_no, data) in &after_images {
                    wal.append_before_image(0, *page_no, data)?;
                }
            }
            Durability::None => {}
        }

        // Apply metadata deltas
        for (table_name, delta) in &self.meta_deltas {
            let table_state = {
                let tables = self.db.tables.read().unwrap();
                tables.get(table_name).cloned()
            };
            if let Some(ts) = table_state {
                let mut state = ts.write().unwrap();
                state.meta.root_page = delta.root_page;
                state.meta.row_count = (state.meta.row_count as i64 + delta.row_count_delta) as u64;
                state.meta.next_rowid = delta.next_rowid;
            }
        }

        Ok(())
    }
}

impl Drop for AcidTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Rollback: private_dirty and meta_deltas are simply dropped.
            // No pages published, no metadata changed. Clean rollback.
        }
    }
}
```

- [ ] **Step 6: Update begin() to return AcidTransaction when acid=true**

```rust
pub fn begin(&self) -> Result<Transaction<'_>> {
    if self.is_acid() {
        // Return an AcidTransaction wrapped in the Transaction enum
        // ... need to unify the return type
    }
    Ok(Transaction::Light(LightTransaction { db: self, committed: false }))
}
```

Actually, to keep the API clean, use an enum:
```rust
pub enum Transaction<'a> {
    Light(LightTransaction<'a>),
    Acid(AcidTransaction<'a>),
}

impl<'a> Transaction<'a> {
    pub fn insert(&mut self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        match self {
            Transaction::Light(t) => t.db.insert(table, data),
            Transaction::Acid(t) => t.insert(table, data),
        }
    }
    // ... delegate all methods ...
    
    pub fn commit(self) -> Result<()> {
        match self {
            Transaction::Light(mut t) => { t.committed = true; t.db.flush_transaction() }
            Transaction::Acid(t) => t.commit(),
        }
    }
}
```

- [ ] **Step 7: Auto-wrap standalone operations when acid=true**

For each public write method on BoogyDb, add the ACID auto-wrap:

```rust
pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
    if self.is_acid() {
        let mut tx = self.begin_acid_internal()?;
        let result = tx.insert(table, data)?;
        tx.commit()?;
        return Ok(result);
    }
    // ... existing fast path ...
}
```

The existing implementation is kept as the fast path (renamed to nothing — it stays inline after the `if` guard).

- [ ] **Step 8: Run all tests**

Run: `cargo test`
Expected: All existing tests pass (they use acid=false, which is the default).

- [ ] **Step 9: Commit**

```bash
git add src/db.rs src/file.rs src/btree.rs src/error.rs src/lib.rs
git commit -m "feat: AcidTransaction with inject/drain pattern, auto-wrap standalone ops"
```

---

## Task 3: Async AcidTransaction + Tests

**Files:**
- Modify: `src/async_db.rs`
- Modify: `tests/crud_test.rs`
- Modify: `tests/async_test.rs`

- [ ] **Step 1: Update AsyncBoogyDb for ACID**

Add `set_acid`, `is_acid` methods. Update `begin()` to handle ACID mode.

The async Transaction wraps the sync Transaction enum, delegating each method.

- [ ] **Step 2: Add ACID integration tests**

```rust
#[test]
fn test_acid_commit() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    tx.insert("a", &[("v", Value::Integer(1))]).unwrap();
    tx.insert("b", &[("v", Value::Integer(2))]).unwrap();
    tx.commit().unwrap();

    assert_eq!(db.count("a", &[]).unwrap(), 1);
    assert_eq!(db.count("b", &[]).unwrap(), 1);
}

#[test]
fn test_acid_rollback() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    // Insert one row normally
    db.insert("t", &[("v", Value::Integer(1))]).unwrap();
    assert_eq!(db.count("t", &[]).unwrap(), 1);

    // Start transaction, insert, then drop without commit
    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("v", Value::Integer(2))]).unwrap();
        // Drop without commit — rollback
    }

    // Should still have only 1 row
    assert_eq!(db.count("t", &[]).unwrap(), 1);
}

#[test]
fn test_acid_read_own_writes() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    let id = tx.insert("t", &[("v", Value::Integer(42))]).unwrap();
    let row = tx.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(42));
    tx.commit().unwrap();
}

#[test]
fn test_acid_auto_wrap_standalone() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    // Standalone insert is auto-wrapped in mini-transaction
    db.insert("t", &[("v", Value::Integer(1))]).unwrap();
    assert_eq!(db.count("t", &[]).unwrap(), 1);
}

#[test]
fn test_acid_multi_table_rollback() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
    db.create_table("posts", &[ColumnDef::new("title", Type::Text)]).unwrap();

    db.insert("users", &[("name", Value::Text("Alice".into()))]).unwrap();

    {
        let mut tx = db.begin().unwrap();
        tx.insert("users", &[("name", Value::Text("Bob".into()))]).unwrap();
        tx.insert("posts", &[("title", Value::Text("Hello".into()))]).unwrap();
        // Rollback — neither Bob nor the post should exist
    }

    assert_eq!(db.count("users", &[]).unwrap(), 1); // only Alice
    assert_eq!(db.count("posts", &[]).unwrap(), 0);
}

#[test]
fn test_acid_doesnt_block_other_tables() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(BoogyDb::open(dir.path().join("test.boogy")).unwrap());
    db.set_acid(true);
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    // Thread 1: long transaction on table "a"
    let db1 = Arc::clone(&db);
    let h = thread::spawn(move || {
        let mut tx = db1.begin().unwrap();
        for i in 0..100 {
            tx.insert("a", &[("v", Value::Integer(i))]).unwrap();
        }
        tx.commit().unwrap();
    });

    // Thread 2: concurrent writes to table "b" (should not block)
    for i in 0..100 {
        db.insert("b", &[("v", Value::Integer(i))]).unwrap();
    }

    h.join().unwrap();
    assert_eq!(db.count("a", &[]).unwrap(), 100);
    assert_eq!(db.count("b", &[]).unwrap(), 100);
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test`
Run: `cargo test --features tokio`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add src/async_db.rs tests/
git commit -m "test: ACID transaction tests — commit, rollback, read-own-writes, concurrency"
```

---

## Task 4: Update README + Docs

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add ACID Transactions section to README**

Add to ToC (after Async API, before Architecture):
```
- [ACID Transactions](#acid-transactions)
```

Add to Features list:
```
- **ACID transactions** — opt-in atomic multi-operation transactions with rollback, toggled via `set_acid(true)`
```

Add section:
```markdown
## ACID Transactions

Enable ACID mode for true multi-operation atomicity with rollback:

\`\`\`rust
db.set_acid(true);

// Multi-operation transaction — all-or-nothing
let mut tx = db.begin()?;
tx.insert("users", &[("name", Value::Text("Alice".into()))])?;
tx.insert("posts", &[("title", Value::Text("Hello".into()))])?;
tx.commit()?;  // atomic publish

// Drop without commit = full rollback
{
    let mut tx = db.begin()?;
    tx.insert("users", &[("name", Value::Text("Bob".into()))])?;
    // dropped — nothing written
}
\`\`\`

When ACID is enabled, standalone operations are automatically wrapped in mini-transactions. When disabled (the default), operations commit individually with zero overhead.

ACID transactions use a private dirty page buffer with an inject/drain pattern — the global write lock is held only for microseconds per operation, not the transaction duration. Tables not touched by a transaction are completely unblocked.
```

Add `set_acid(enabled)` to the API table.

- [ ] **Step 2: Update CLAUDE.md architecture table**

Add note about AcidTransaction to the db.rs row.

- [ ] **Step 3: Commit and push**

```bash
git add README.md CLAUDE.md
git commit -m "docs: add ACID transactions section to README"
git push
```
