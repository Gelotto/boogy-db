# Concurrent Reads — Reader/Writer Split

## Problem

All operations go through `Mutex<PageFile>`, serializing everything. At 4 threads without indexes, boogy-db drops to 17K ops/s while SQLite scales to 19K. At 8 threads: 12K vs 21K. Reads don't need exclusive access but currently take it because `read_page(&mut self)` may populate the cache.

## Design

### Page Cache: Arc Pages + RwLock

Change the page cache to store `Arc<Page>` behind a `RwLock`. Readers clone `Arc<Page>` with a shared read lock. Writers modify pages in a separate overlay and publish via brief write lock.

```rust
pub struct PageFile {
    file: Mutex<File>,
    num_pages: AtomicU32,
    // Read path: shared access via RwLock
    pages: RwLock<Vec<Option<Arc<Page>>>>,
    // Write path: exclusive access via Mutex
    write_state: Mutex<WriteState>,
}

struct WriteState {
    dirty: HashMap<u32, Page>,
    dirty_flags: Vec<bool>,
    before_images: Vec<Option<Box<[u8; PAGE_SIZE]>>>,
    capture_before_images: bool,
}
```

### Read Path (concurrent)

`read_page(&self, page_no) -> Result<Arc<Page>>`: Takes `&self`, not `&mut self`. Acquires `pages` read lock, clones Arc if cached. On cache miss, reads from disk under `file` mutex, then acquires `pages` write lock briefly to insert.

Multiple threads calling `read_page` concurrently do not block each other (shared RwLock). Cache misses briefly serialize on the `file` mutex and `pages` write lock, but once a page is cached, all subsequent reads are lock-free (just RwLock read + Arc clone).

### Write Path (exclusive per table)

Writers already hold a per-table write lock (`table_state.write()`). Within that scope, they acquire `write_state` mutex to modify pages. The write path:

1. `begin_write(&self) -> WriteGuard` — acquires `write_state` mutex
2. `WriteGuard::read_page(page_no)` — checks dirty overlay first, then falls through to `pages` RwLock read
3. `WriteGuard::write_page(page_no)` — copies page from committed into dirty overlay, captures before-image
4. `WriteGuard::allocate_page()` — adds to dirty overlay
5. `WriteGuard::put_page(page_no, page)` — puts into dirty overlay
6. `WriteGuard::commit()` — acquires `pages` write lock, publishes dirty pages as new Arc entries, clears dirty overlay. Also handles WAL via `file` mutex.

The `pages` write lock is held only during commit — O(dirty_page_count) Arc pointer swaps, typically <1µs.

### BTree Reader/Writer Split

Currently:
```rust
pub struct BTree<'a> {
    file: &'a mut PageFile,
    root: u32,
}
```

Split into:
```rust
/// Read-only B+ tree operations. Works with shared &PageFile.
pub struct BTreeReader<'a> {
    file: &'a PageFile,
    root: u32,
}

/// Read-write B+ tree operations. Works with exclusive WriteGuard.
pub struct BTreeWriter<'a, 'b> {
    guard: &'a mut WriteGuard<'b>,
    root: u32,
}
```

**BTreeReader** gets: `search`, `scan_all`, `scan_filtered`, `count_filtered`, `multi_get_sorted`. These only call `file.read_page()`.

**BTreeWriter** gets: `insert`, `delete`. These call `guard.read_page()`, `guard.write_page()`, `guard.allocate_page()`, `guard.put_page()`. Also has `root_page()` so callers can track root changes after splits.

The writer's read_page checks the dirty overlay first, falling back to the committed cache. This means writers always see their own uncommitted changes.

### IndexTree Reader/Writer Split

Same pattern:
```rust
pub struct IndexTreeReader<'a> {
    file: &'a PageFile,
    root: u32,
}

pub struct IndexTreeWriter<'a, 'b> {
    guard: &'a mut WriteGuard<'b>,
    root: u32,
}
```

**IndexTreeReader**: `scan_prefix`, `scan_prefix_limit`, `count_prefix`
**IndexTreeWriter**: `insert`, `delete`, plus `root_page()`

### db.rs Changes

Read operations acquire only the per-table read lock + use `&self.file` directly:

```rust
pub fn get(&self, table: &str, id: u64) -> Result<Option<Row>> {
    let state = table_state.read().unwrap();
    // No file mutex! BTreeReader uses &self.file (shared)
    let reader = BTreeReader::new(&self.file, state.meta.root_page);
    let result = reader.search(id)?;
    ...
}

pub fn find(&self, table: &str, opts: FindOptions) -> Result<FindResult> {
    let state = table_state.read().unwrap();
    // Concurrent readers — no blocking
    let reader = BTreeReader::new(&self.file, state.meta.root_page);
    ...
}
```

Write operations acquire the per-table write lock + a WriteGuard:

```rust
pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
    let mut state = table_state.write().unwrap();
    let mut guard = self.file.begin_write();
    let mut writer = BTreeWriter::new(&mut guard, state.meta.root_page);
    let new_root = writer.insert(rowid, &row_bytes)?;
    // Index maintenance via IndexTreeWriter
    ...
    guard.commit(&self.wal, durability, state.meta.table_id)?;
    ...
}
```

### WAL Integration

The `commit()` on WriteGuard handles WAL:
1. Collect before-images from write_state
2. Acquire `wal` mutex, write before-images
3. Acquire `pages` write lock, publish dirty pages
4. Release locks

For `Durability::None`, skip before-images and WAL entirely. Just publish dirty pages.

### BoogyDb Struct Changes

```rust
pub struct BoogyDb {
    file: PageFile,                    // was Mutex<PageFile> — now has internal locking
    wal: Mutex<Wal>,
    tables: RwLock<HashMap<String, Arc<RwLock<TableState>>>>,
    next_table_id: Mutex<u32>,
    durability: AtomicU8,
    path: PathBuf,
}
```

`file` is no longer behind a Mutex — PageFile handles its own concurrency internally.

## Files Changed

- `src/file.rs` — Restructure PageFile with RwLock pages + Mutex WriteState + WriteGuard
- `src/btree.rs` — Split into BTreeReader and BTreeWriter
- `src/index.rs` — Split into IndexTreeReader and IndexTreeWriter
- `src/db.rs` — Read ops use BTreeReader/IndexTreeReader with &self.file, write ops use WriteGuard
- `src/lib.rs` — Update exports if needed

## Performance Targets

Concurrent mixed workload (no index):
- 4 threads: >25K ops/s (currently 17K, SQLite 19K)
- 8 threads: >30K ops/s (currently 12K, SQLite 21K)

Concurrent mixed workload (with index):
- 4 threads: >100K ops/s (currently 72K, SQLite 46K)
- 8 threads: >120K ops/s (currently 51K, SQLite 48K)

Single-thread performance: no regression.

## Scope

This spec covers concurrent reads via reader/writer split. Not in scope:
- Full MVCC snapshot isolation (readers see writes that commit during their operation)
- Multi-table transactions with isolation
- Group commit (batching WAL writes across concurrent writers)
