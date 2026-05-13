# ACID Atomicity — True Multi-Operation Transactions

## Problem

Current transactions (`begin()` / `transaction()`) execute each operation immediately — each has its own WriteGuard that commits independently. If the process crashes mid-transaction, some operations are committed and others aren't. There's no rollback capability.

## Design

### Toggle

```rust
db.set_acid(true);  // default: false
```

When `acid: false`: current behavior. Each operation creates its own WriteGuard, commits immediately. Zero overhead.

When `acid: true`: every write is wrapped in a mini-transaction. Explicit `begin()` groups operations. `commit()` publishes all-or-nothing. Drop without commit = full rollback.

### AcidTransaction

```rust
pub struct AcidTransaction<'a> {
    db: &'a BoogyDb,
    /// Private dirty page buffer — NOT in the global WriteGuard.
    private_dirty: HashMap<u32, Box<Page>>,
    /// New pages allocated during this transaction.
    new_page_count: u32,
    /// Per-table write locks held for the transaction's duration.
    held_locks: HashMap<String, RwLockWriteGuard<'a, TableState>>,
    /// Deferred metadata changes per table.
    meta_deltas: HashMap<String, MetaDelta>,
    committed: bool,
}

struct MetaDelta {
    new_root: u32,
    row_count_delta: i64,
    new_next_rowid: u64,
}
```

### Per-Operation Flow (inject/drain pattern)

Each write operation within a transaction:

1. Acquire per-table write lock (if not already held) — stored in `held_locks`
2. Acquire global WriteGuard (briefly)
3. `inject_dirty`: move pages from `private_dirty` into WriteGuard's dirty overlay
4. Do the B+ tree operation via BTreeWriter (sees all previous writes from this tx)
5. `drain_dirty`: move ALL dirty pages back into `private_dirty`
6. Release WriteGuard (other writers to other tables can proceed)
7. Record metadata delta (root_page, row_count, next_rowid changes)

The global WriteGuard is held for microseconds per operation, not the transaction duration.

### Commit

```rust
tx.commit()?;
```

1. Acquire global WriteGuard
2. Inject all `private_dirty` pages into the overlay
3. Call `commit()` — publishes to shared cache, returns after-images
4. Write WAL entries (one batch for all pages)
5. Release WriteGuard
6. Apply all `meta_deltas` to their TableMeta (root_page, row_count, next_rowid)
7. Release all per-table write locks (drop `held_locks`)

All dirty pages from all tables are published in one atomic operation. The WAL batch ensures crash recovery can redo the entire transaction.

### Rollback (Drop without commit)

```rust
{
    let tx = db.begin()?;
    tx.insert("users", &[...])?;
    tx.insert("posts", &[...])?;
    // Drop without commit — full rollback
}
```

1. `private_dirty` is dropped (dirty pages discarded)
2. `meta_deltas` is dropped (metadata unchanged)
3. `held_locks` is dropped (per-table write locks released)
4. No WAL entries written, shared cache unchanged
5. Database state is exactly as before the transaction started

### Auto-Wrap for Standalone Operations

When `acid: true`, standalone operations (not inside `begin()`) are auto-wrapped:

```rust
pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
    if self.acid.load(Ordering::Relaxed) {
        let tx = self.begin_acid()?;
        let result = tx.insert(table, data)?;
        tx.commit()?;
        Ok(result)
    } else {
        self.insert_fast(table, data)  // current path, zero overhead
    }
}
```

`insert_fast` is the current implementation renamed. The ACID check is one atomic bool load (~1ns) when ACID is off.

### Reads Within a Transaction

Reads within a transaction must see the transaction's own writes. The inject/drain pattern handles this: when the WriteGuard is briefly acquired, the transaction's dirty pages are injected, so BTreeReader-through-WriteGuard sees them.

For read-only operations within a transaction (get, find, count), the same inject/drain flow is used but through WriteGuard's read_page (which checks dirty overlay first).

### WriteGuard Additions

Two new methods on WriteGuard:

```rust
/// Insert pages into the dirty overlay (for ACID transaction inject).
pub fn inject_dirty(&mut self, pages: HashMap<u32, Box<Page>>) {
    for (page_no, page) in pages {
        if !self.state.dirty.contains_key(&page_no) {
            self.state.dirty.insert(page_no, page);
        }
    }
}

/// Drain all dirty pages out of the overlay (for ACID transaction extract).
pub fn drain_dirty(&mut self) -> HashMap<u32, Box<Page>> {
    std::mem::take(&mut self.state.dirty)
}
```

### Internal Method Refactor

Each write operation on BoogyDb gets split into:
- `insert_fast` — current implementation (creates own WriteGuard, commits immediately)
- `insert_acid` — called by AcidTransaction, takes `&mut WriteGuard`, returns `(u64, MetaDelta)` without committing

The public `insert` dispatches based on the acid flag. The AcidTransaction calls the `_acid` variants directly.

Similarly for `update`, `delete`, `insert_many`, `update_where`, `delete_where`.

Read operations (`get`, `find`, `count`) don't need `_acid` variants — they just need to see the transaction's dirty pages, which the inject/drain pattern provides.

### Concurrency

- Two ACID transactions on DIFFERENT tables: both proceed concurrently. Per-table write locks don't conflict. WriteGuard mutex is held briefly per operation (microseconds).
- Two ACID transactions on the SAME table: second transaction blocks on the per-table write lock until the first commits or rolls back. Correct serialization.
- Non-ACID operations: completely unaffected. They don't use `held_locks` or `private_dirty`.

### BoogyDb Struct Changes

```rust
pub struct BoogyDb {
    // ... existing fields ...
    acid: AtomicBool,  // default false
}
```

New methods:
- `set_acid(&self, enabled: bool)`
- `begin_acid(&self) -> Result<AcidTransaction>` — only valid when acid=true
- `begin(&self)` — returns AcidTransaction when acid=true, lightweight Transaction when acid=false

### Error Handling

- `BoogyError::NotInAcidMode` — `begin_acid()` called when acid=false
- Operations on a committed/dropped transaction: impossible (Rust ownership prevents it)

## Files Changed

- `src/file.rs` — Add `inject_dirty`, `drain_dirty` to WriteGuard
- `src/db.rs` — Add `AcidTransaction`, `MetaDelta`, acid flag, split operations into fast/acid variants, auto-wrap logic
- `src/lib.rs` — Export `AcidTransaction`
- `src/async_db.rs` — Add async AcidTransaction wrapper

## Performance Impact

ACID off: one AtomicBool load per operation (~1ns). Zero other overhead.

ACID on, per operation: ~300ns-1µs for inject/drain (proportional to dirty page count, pointer moves not clones). Total overhead on a 2µs insert: ~25%.

## Scope

This spec covers atomicity only (the A in ACID). Not in scope:
- Isolation / MVCC (readers seeing consistent snapshots during concurrent writes) — separate spec
- Savepoints / nested transactions
- Two-phase commit
