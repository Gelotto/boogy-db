# Concurrent Reads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the global `Mutex<PageFile>` bottleneck so read operations (get, find, count) run concurrently across threads without blocking each other or blocking writers.

**Architecture:** PageFile is restructured with `RwLock<Vec<Option<Arc<Page>>>>` for the shared read cache and a separate `Mutex<WriteState>` for dirty pages. BTree and IndexTree are each split into Reader (takes `&PageFile`, concurrent) and Writer (takes `&mut WriteGuard`, exclusive) variants. db.rs read operations use readers directly; write operations acquire a WriteGuard.

**Tech Stack:** Rust, `std::sync::{Arc, RwLock, Mutex}`, existing boogy-db crate.

**Spec:** `docs/superpowers/specs/2026-05-13-concurrent-reads-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/file.rs` | Rewrite | PageFile with RwLock read cache + Mutex WriteState + WriteGuard |
| `src/btree.rs` | Rewrite | Split into BTreeReader (`&PageFile`) and BTreeWriter (`&mut WriteGuard`) |
| `src/index.rs` | Modify | Split into IndexTreeReader (`&PageFile`) and IndexTreeWriter (`&mut WriteGuard`) |
| `src/db.rs` | Modify | Read ops use readers, write ops use WriteGuard |
| `src/lib.rs` | Minor | No changes expected |

---

## Task 1: Restructure PageFile with Read/Write Split

**Files:**
- Modify: `src/file.rs`

This is the foundation. PageFile gets internal concurrency: a shared read cache (`RwLock<Vec<Option<Arc<Page>>>>`) and an exclusive write overlay (`Mutex<WriteState>`).

- [ ] **Step 1: Replace struct definition and imports**

```rust
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use crate::error::{BoogyError, Result};
use crate::page::{Page, PAGE_SIZE};

/// Write-side state: dirty pages, before-images, new allocations.
pub struct WriteState {
    dirty: HashMap<u32, Box<Page>>,
    before_images: Vec<Option<Box<[u8; PAGE_SIZE]>>>,
    pub(crate) capture_before_images: bool,
    new_page_count: u32,  // pages allocated during this write
}

/// Guard for exclusive write access. Drop publishes dirty pages.
pub struct WriteGuard<'a> {
    file: &'a PageFile,
    state: std::sync::MutexGuard<'a, WriteState>,
}

/// Page-aligned file I/O with concurrent read access.
///
/// Readers use `read_page(&self)` with the shared RwLock cache.
/// Writers acquire a `WriteGuard` via `begin_write()`.
pub struct PageFile {
    disk: Mutex<File>,
    num_pages: std::sync::atomic::AtomicU32,
    /// Shared read cache. Readers hold read lock, commit holds write lock briefly.
    pages: RwLock<Vec<Option<Arc<Page>>>>,
    /// Writer-only state (dirty overlay, before-images).
    write_state: Mutex<WriteState>,
}
```

- [ ] **Step 2: Implement PageFile::open**

```rust
impl PageFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true).write(true).create(true).truncate(false)
            .open(path)?;
        let file_len = file.metadata()?.len();
        let num_pages = (file_len / PAGE_SIZE as u64) as u32;
        let n = num_pages as usize;

        Ok(Self {
            disk: Mutex::new(file),
            num_pages: std::sync::atomic::AtomicU32::new(num_pages),
            pages: RwLock::new(vec![None; n]),
            write_state: Mutex::new(WriteState {
                dirty: HashMap::new(),
                before_images: vec![None; n],
                capture_before_images: true,
                new_page_count: 0,
            }),
        })
    }

    pub fn page_count(&self) -> u32 {
        self.num_pages.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_capture_before_images(&self, capture: bool) {
        self.write_state.lock().unwrap().capture_before_images = capture;
    }
}
```

Note: `set_capture_before_images` takes `&self` now (uses internal mutex).

- [ ] **Step 3: Implement read_page(&self) — the concurrent read path**

```rust
impl PageFile {
    /// Read a page from cache or disk. Takes &self — concurrent readers OK.
    pub fn read_page(&self, page_no: u32) -> Result<Arc<Page>> {
        let np = self.num_pages.load(std::sync::atomic::Ordering::Relaxed);
        if page_no >= np {
            return Err(BoogyError::Corruption(format!(
                "page {page_no} out of range (have {np} pages)"
            )));
        }

        // Fast path: page already cached (shared read lock)
        {
            let cache = self.pages.read().unwrap();
            if let Some(Some(arc)) = cache.get(page_no as usize) {
                return Ok(Arc::clone(arc));
            }
        }

        // Slow path: cache miss — read from disk, populate cache
        let page = {
            let mut disk = self.disk.lock().unwrap();
            let offset = page_no as u64 * PAGE_SIZE as u64;
            disk.seek(SeekFrom::Start(offset))?;
            let mut buf = [0u8; PAGE_SIZE];
            disk.read_exact(&mut buf)?;
            Page::from_bytes(buf)?
        };
        let arc = Arc::new(page);

        // Insert into cache (brief write lock)
        {
            let mut cache = self.pages.write().unwrap();
            // Double-check: another thread may have populated it
            if cache[page_no as usize].is_none() {
                cache[page_no as usize] = Some(Arc::clone(&arc));
            }
        }

        Ok(arc)
    }
}
```

- [ ] **Step 4: Implement begin_write() and WriteGuard**

```rust
impl PageFile {
    /// Begin a write transaction. Only one writer at a time.
    pub fn begin_write(&self) -> WriteGuard<'_> {
        let state = self.write_state.lock().unwrap();
        WriteGuard { file: self, state }
    }
}

impl<'a> WriteGuard<'a> {
    /// Read a page — checks dirty overlay first, then shared cache.
    pub fn read_page(&self, page_no: u32) -> Result<Arc<Page>> {
        // Check dirty overlay first (writer's uncommitted changes)
        if let Some(page) = self.state.dirty.get(&page_no) {
            return Ok(Arc::new((**page).clone()));
        }
        // Fall through to shared cache
        self.file.read_page(page_no)
    }

    /// Get a mutable page. Copies from cache into dirty overlay on first access.
    pub fn write_page(&mut self, page_no: u32) -> Result<&mut Page> {
        let np = self.file.num_pages.load(std::sync::atomic::Ordering::Relaxed)
            + self.state.new_page_count;
        if page_no >= np {
            return Err(BoogyError::Corruption(format!("page {page_no} out of range")));
        }

        if !self.state.dirty.contains_key(&page_no) {
            // Read current version and copy into dirty overlay
            let arc = self.file.read_page(page_no)?;
            let page = (*arc).clone();

            // Capture before-image
            if self.state.capture_before_images {
                let idx = page_no as usize;
                // Grow before_images if needed
                while idx >= self.state.before_images.len() {
                    self.state.before_images.push(None);
                }
                if self.state.before_images[idx].is_none() {
                    self.state.before_images[idx] = Some(Box::new(page.data));
                }
            }

            self.state.dirty.insert(page_no, Box::new(page));
        }

        Ok(self.state.dirty.get_mut(&page_no).unwrap())
    }

    /// Allocate a new page.
    pub fn allocate_page(&mut self) -> Result<u32> {
        let base = self.file.num_pages.load(std::sync::atomic::Ordering::Relaxed);
        let page_no = base + self.state.new_page_count;
        self.state.new_page_count += 1;
        self.state.dirty.insert(page_no, Box::new(Page::default()));
        Ok(page_no)
    }

    /// Put a page (e.g., for new leaf/branch pages during splits).
    pub fn put_page(&mut self, page_no: u32, page: Page) {
        let base = self.file.num_pages.load(std::sync::atomic::Ordering::Relaxed);
        // Capture before-image if overwriting existing page
        if page_no < base && self.state.capture_before_images {
            if !self.state.dirty.contains_key(&page_no) {
                // Try to read current version for before-image
                if let Ok(arc) = self.file.read_page(page_no) {
                    let idx = page_no as usize;
                    while idx >= self.state.before_images.len() {
                        self.state.before_images.push(None);
                    }
                    if self.state.before_images[idx].is_none() {
                        self.state.before_images[idx] = Some(Box::new(arc.data));
                    }
                }
            }
        }
        self.state.dirty.insert(page_no, Box::new(page));
    }

    /// Take before-images (for WAL).
    pub fn take_before_images(&mut self) -> Vec<(u32, [u8; PAGE_SIZE])> {
        let mut result = Vec::new();
        for (i, bi) in self.state.before_images.iter_mut().enumerate() {
            if let Some(data) = bi.take() {
                result.push((i as u32, *data));
            }
        }
        result
    }

    /// Publish dirty pages to shared cache, flush to disk, handle WAL.
    pub fn commit(
        mut self,
        wal: &Mutex<crate::wal::Wal>,
        durability: Durability,
        table_id: u32,
    ) -> Result<()> {
        // 1. WAL: write before-images
        match durability {
            Durability::Immediate => {
                let before_images = self.take_before_images();
                let mut wal = wal.lock().unwrap();
                for (page_no, data) in &before_images {
                    wal.append_before_image(table_id, *page_no, data)?;
                }
                wal.sync()?;
            }
            Durability::Normal => {
                let before_images = self.take_before_images();
                let mut wal = wal.lock().unwrap();
                for (page_no, data) in &before_images {
                    wal.append_before_image(table_id, *page_no, data)?;
                }
            }
            Durability::None => {
                // No WAL
            }
        }

        // 2. Write dirty pages to disk
        if !matches!(durability, Durability::None) {
            let mut disk = self.file.disk.lock().unwrap();
            for (&page_no, page) in &self.state.dirty {
                let offset = page_no as u64 * PAGE_SIZE as u64;
                disk.seek(SeekFrom::Start(offset))?;
                disk.write_all(&page.data)?;
            }
            if matches!(durability, Durability::Immediate) {
                disk.sync_data()?;
            }
        }

        // 3. Publish to shared cache (brief write lock)
        {
            let mut cache = self.file.pages.write().unwrap();
            // Grow cache if new pages were allocated
            let new_total = self.file.num_pages.load(std::sync::atomic::Ordering::Relaxed)
                + self.state.new_page_count;
            while cache.len() < new_total as usize {
                cache.push(None);
            }
            for (page_no, page) in self.state.dirty.drain() {
                cache[page_no as usize] = Some(Arc::new(*page));
            }
        }

        // 4. Update page count
        if self.state.new_page_count > 0 {
            self.file.num_pages.fetch_add(
                self.state.new_page_count,
                std::sync::atomic::Ordering::Relaxed,
            );
            self.state.new_page_count = 0;
        }

        // 5. Truncate WAL for Immediate durability
        if matches!(durability, Durability::Immediate) {
            let mut wal = wal.lock().unwrap();
            wal.truncate()?;
        }

        Ok(())
    }

    /// Discard dirty pages without committing (rollback).
    pub fn discard(mut self) {
        self.state.dirty.clear();
        for bi in self.state.before_images.iter_mut() {
            *bi = None;
        }
        self.state.new_page_count = 0;
    }
}
```

- [ ] **Step 5: Add helper methods for crash recovery and shutdown**

PageFile needs methods for crash recovery (called during open) and clean shutdown:

```rust
impl PageFile {
    /// Direct page write for crash recovery (no WAL, no cache).
    /// Called during open() to replay WAL before cache is populated.
    pub fn put_page_direct(&self, page_no: u32, page: Page) {
        let mut cache = self.pages.write().unwrap();
        while cache.len() <= page_no as usize {
            cache.push(None);
        }
        cache[page_no as usize] = Some(Arc::new(page));
        // Also write to disk
        let mut disk = self.disk.lock().unwrap();
        let offset = page_no as u64 * PAGE_SIZE as u64;
        let _ = disk.seek(SeekFrom::Start(offset));
        let _ = disk.write_all(&page.data);
    }

    /// Sync all cached pages to disk (for clean shutdown).
    pub fn sync_all(&self) -> Result<()> {
        let cache = self.pages.read().unwrap();
        let mut disk = self.disk.lock().unwrap();
        for (i, slot) in cache.iter().enumerate() {
            if let Some(arc) = slot {
                let offset = i as u64 * PAGE_SIZE as u64;
                disk.seek(SeekFrom::Start(offset))?;
                disk.write_all(&arc.data)?;
            }
        }
        disk.sync_data()?;
        Ok(())
    }
}
```

- [ ] **Step 6: Update file.rs tests**

The tests need to use the new API (begin_write, WriteGuard, commit):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;
    use crate::wal::Wal;
    use tempfile::NamedTempFile;

    #[test]
    fn test_allocate_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();
        let wal_tmp = NamedTempFile::new().unwrap();
        let wal = Mutex::new(Wal::open(wal_tmp.path()).unwrap());

        let pg0 = {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            assert_eq!(pg0, 0);
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(5);
            page.update_checksum();
            guard.commit(&wal, Durability::Normal, 0).unwrap();
            pg0
        };

        let page = pf.read_page(pg0).unwrap();
        assert!(page.is_leaf());
        assert_eq!(page.num_rows(), 5);
    }

    #[test]
    fn test_persist_across_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let wal_tmp = NamedTempFile::new().unwrap();
        let wal_path = wal_tmp.path().to_path_buf();

        {
            let pf = PageFile::open(&path).unwrap();
            let wal = Mutex::new(Wal::open(&wal_path).unwrap());
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(42);
            page.update_checksum();
            guard.commit(&wal, Durability::Immediate, 0).unwrap();
        }

        {
            let pf = PageFile::open(&path).unwrap();
            assert_eq!(pf.page_count(), 1);
            let page = pf.read_page(0).unwrap();
            assert_eq!(page.num_rows(), 42);
        }
    }

    #[test]
    fn test_discard_dirty() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(99);
            page.update_checksum();
            guard.discard(); // rollback
        }

        assert_eq!(pf.page_count(), 0); // not committed
    }
}
```

- [ ] **Step 7: Run file.rs tests**

Run: `cargo test --lib file::tests`
Expected: All 3 tests pass. Other modules won't compile yet (they still use old API).

- [ ] **Step 8: Commit**

```bash
git add src/file.rs
git commit -m "refactor: PageFile with RwLock read cache + Mutex WriteState for concurrent reads"
```

---

## Task 2: Split BTree into Reader and Writer

**Files:**
- Modify: `src/btree.rs`

Split the BTree struct into BTreeReader (read operations with `&PageFile`) and BTreeWriter (write operations with `&mut WriteGuard`).

- [ ] **Step 1: Create BTreeReader**

Add at the top of btree.rs, alongside the existing BTree:

```rust
use crate::file::{PageFile, WriteGuard};

/// Read-only B+ tree. Uses shared &PageFile for concurrent reads.
pub struct BTreeReader<'a> {
    file: &'a PageFile,
    root: u32,
}

impl<'a> BTreeReader<'a> {
    pub fn new(file: &'a PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn search(&self, rowid: u64) -> Result<Option<Vec<u8>>> {
        self.search_recursive(self.root, rowid)
    }

    pub fn scan_all(&self) -> Result<Vec<(u64, Vec<u8>)>> { ... }
    pub fn scan_filtered(&self, ...) -> Result<(Vec<(u64, Vec<u8>)>, u64)> { ... }
    pub fn count_filtered(&self, ...) -> Result<u64> { ... }
    pub fn multi_get_sorted(&self, rowids: &[u64]) -> Result<Vec<Vec<u8>>> { ... }

    // Internal methods — same algorithms as current BTree, but using
    // self.file.read_page(page_no)? which returns Arc<Page>.
    // Clone the Arc'd page for local use in each method.
}
```

The key difference from the current BTree: `self.file.read_page()` returns `Arc<Page>` instead of `&Page`. The reader dereferences the Arc for page data access. The algorithms are identical.

- [ ] **Step 2: Create BTreeWriter**

```rust
/// Read-write B+ tree. Uses exclusive WriteGuard.
pub struct BTreeWriter<'a, 'b> {
    guard: &'a mut WriteGuard<'b>,
    root: u32,
}

impl<'a, 'b> BTreeWriter<'a, 'b> {
    pub fn new(guard: &'a mut WriteGuard<'b>, root: u32) -> Self {
        Self { guard, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    pub fn create(guard: &mut WriteGuard) -> Result<u32> {
        let page_no = guard.allocate_page()?;
        let page = Page::new_leaf();
        guard.put_page(page_no, page);
        Ok(page_no)
    }

    pub fn insert(&mut self, rowid: u64, row_data: &[u8]) -> Result<u32> { ... }
    pub fn delete(&mut self, rowid: u64) -> Result<bool> { ... }

    // Internal methods use self.guard.read_page() and self.guard.write_page()
    // for reads and writes respectively.
}
```

- [ ] **Step 3: Port read methods from current BTree to BTreeReader**

Copy the implementation of `search`, `scan_all`, `scan_filtered`, `count_filtered`, `multi_get_sorted` and their internal helpers (`search_recursive`, `find_leftmost_leaf`, `find_leaf_for_rowid`, etc.) into BTreeReader.

Key change: replace `self.file.read_page(x)?.clone()` with dereferencing the returned `Arc<Page>`:
```rust
// Old (BTree):
let page = self.file.read_page(page_no)?.clone();
// or:
let page = match self.file.get_cached_page(current) {
    Some(p) => p,
    None => self.file.read_page(current)?.clone(),
};

// New (BTreeReader):
let page = (*self.file.read_page(page_no)?).clone();
```

The `get_cached_page` pattern is no longer needed — `read_page(&self)` already handles concurrent access internally. All read paths just use `self.file.read_page(page_no)?` and clone the Arc'd page.

Also: all internal helper functions that take `&Page` or use page data work unchanged since `Arc<Page>` derefs to `Page`.

- [ ] **Step 4: Port write methods from current BTree to BTreeWriter**

Copy `insert`, `delete`, and their internal helpers (`insert_recursive`, `insert_into_leaf`, `insert_into_branch`, `delete_recursive`, `find_child`, `find_insertion_point`, etc.) into BTreeWriter.

Key changes:
- `self.file.read_page(x)` → `self.guard.read_page(x)` (returns `Arc<Page>`, clone it)
- `self.file.write_page(x)` → `self.guard.write_page(x)` (returns `&mut Page`)
- `self.file.allocate_page()` → `self.guard.allocate_page()`
- `self.file.put_page(x, p)` → `self.guard.put_page(x, p)`

The leaf/branch helper functions (row_bounds, write_leaf_with_insert, etc.) are pure functions operating on page data — they don't touch the file at all. Keep them as free functions shared by both Reader and Writer.

- [ ] **Step 5: Remove old BTree struct**

Delete the old `BTree` struct and its impl block entirely. All callers will use BTreeReader or BTreeWriter.

- [ ] **Step 6: Run btree tests**

Update btree tests to use BTreeWriter for inserts and BTreeReader for searches:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::PageFile;
    use crate::row;
    use crate::value::Value;
    use crate::wal::Wal;
    use crate::db::Durability;
    use tempfile::NamedTempFile;
    use std::sync::Mutex;

    fn make_row(rowid: u64, name: &str) -> Vec<u8> {
        row::encode_row(rowid, &[(0, &Value::Text(name.into()))])
    }

    #[test]
    fn test_insert_and_search() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();
        let wal_tmp = NamedTempFile::new().unwrap();
        let wal = Mutex::new(Wal::open(wal_tmp.path()).unwrap());

        let root = {
            let mut guard = pf.begin_write();
            let root = BTreeWriter::create(&mut guard).unwrap();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let row = make_row(1, "alice");
            tree.insert(1, &row).unwrap();
            guard.commit(&wal, Durability::None, 0).unwrap();
            root
        };

        let reader = BTreeReader::new(&pf, root);
        let found = reader.search(1).unwrap();
        assert!(found.is_some());
        let decoded = row::decode_row(&found.unwrap()).unwrap();
        assert_eq!(decoded.id, 1);
    }

    // ... similar updates for all other tests ...
}
```

Run: `cargo test --lib btree::tests`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/btree.rs
git commit -m "refactor: split BTree into BTreeReader (concurrent) and BTreeWriter (exclusive)"
```

---

## Task 3: Split IndexTree into Reader and Writer

**Files:**
- Modify: `src/index.rs`

Same pattern as Task 2 but for IndexTree.

- [ ] **Step 1: Create IndexTreeReader**

```rust
pub struct IndexTreeReader<'a> {
    file: &'a PageFile,
    root: u32,
}

impl<'a> IndexTreeReader<'a> {
    pub fn new(file: &'a PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>> { ... }
    pub fn scan_prefix_limit(&self, prefix: &[u8], max: usize) -> Result<Vec<Vec<u8>>> { ... }
    pub fn count_prefix(&self, prefix: &[u8]) -> Result<u64> { ... }
}
```

Port read methods using `self.file.read_page()` (returns Arc<Page>).

- [ ] **Step 2: Create IndexTreeWriter**

```rust
pub struct IndexTreeWriter<'a, 'b> {
    guard: &'a mut WriteGuard<'b>,
    root: u32,
}

impl<'a, 'b> IndexTreeWriter<'a, 'b> {
    pub fn new(guard: &'a mut WriteGuard<'b>, root: u32) -> Self {
        Self { guard, root }
    }

    pub fn root_page(&self) -> u32 { self.root }

    pub fn create(guard: &mut WriteGuard) -> Result<u32> { ... }
    pub fn insert(&mut self, key: &[u8]) -> Result<u32> { ... }
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> { ... }
}
```

Port write methods using `self.guard.read_page()`, `self.guard.write_page()`, etc.

- [ ] **Step 3: Remove old IndexTree struct**

Delete the old `IndexTree` struct.

- [ ] **Step 4: Update index tests**

Update all index tests to use IndexTreeWriter for inserts and IndexTreeReader for queries.

Run: `cargo test --lib index::tests`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/index.rs
git commit -m "refactor: split IndexTree into IndexTreeReader and IndexTreeWriter"
```

---

## Task 4: Update db.rs — Read Operations Use Readers

**Files:**
- Modify: `src/db.rs`

Update all read operations (get, find, count) to use BTreeReader/IndexTreeReader with `&self.file` — no Mutex acquisition.

- [ ] **Step 1: Change BoogyDb struct**

```rust
pub struct BoogyDb {
    file: PageFile,          // was Mutex<PageFile> — now has internal locking
    wal: Mutex<Wal>,
    tables: RwLock<HashMap<String, Arc<RwLock<TableState>>>>,
    next_table_id: Mutex<u32>,
    durability: std::sync::atomic::AtomicU8,
    path: PathBuf,
}
```

- [ ] **Step 2: Update open() — crash recovery + PageFile construction**

The crash recovery during open() needs to work with the new API:

```rust
pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref().to_path_buf();
    validate_path(&path)?;
    let wal_path = path.with_extension("wal");

    // Step 1: Crash recovery
    {
        let mut wal = Wal::open(&wal_path)?;
        if wal.entry_count() > 0 {
            let pf = PageFile::open(&path)?;
            let entries = wal.read_entries()?;
            for entry in entries.iter().rev() {
                let page = Page::from_bytes_unchecked(entry.page_data);
                pf.put_page_direct(entry.page_no, page);
            }
            pf.sync_all()?;
            wal.truncate()?;
        }
    }

    // Step 2: Normal open
    let file = PageFile::open(&path)?;
    let wal = Wal::open(&wal_path)?;

    // Step 3: Load table registry
    let mut tables = HashMap::new();
    let mut next_table_id = 1u32;
    if file.page_count() > 0 {
        let sys_page = file.read_page(0)?;
        if sys_page.flags() & PAGE_SYSTEM != 0 {
            let (metas, next_id) = deserialize_system_page(&sys_page)?;
            next_table_id = next_id;
            for meta in metas {
                let name = meta.name.clone();
                tables.insert(name, Arc::new(RwLock::new(TableState { meta })));
            }
        }
    }

    Ok(Self {
        file,
        wal: Mutex::new(wal),
        tables: RwLock::new(tables),
        next_table_id: Mutex::new(next_table_id),
        durability: std::sync::atomic::AtomicU8::new(Durability::Normal as u8),
        path,
    })
}
```

- [ ] **Step 3: Update set_durability**

```rust
pub fn set_durability(&self, d: Durability) {
    self.durability.store(d as u8, std::sync::atomic::Ordering::Relaxed);
    self.file.set_capture_before_images(!matches!(d, Durability::None));
}
```

- [ ] **Step 4: Update get() — use BTreeReader, no file mutex**

```rust
pub fn get(&self, table: &str, id: u64) -> Result<Option<Row>> {
    let table_state = { ... clone Arc ... };
    let state = table_state.read().unwrap();

    // No file mutex! BTreeReader uses &self.file (shared read lock internally)
    let reader = BTreeReader::new(&self.file, state.meta.root_page);
    let result = reader.search(id)?;

    match result {
        Some(bytes) => Ok(Some(Row::from_raw(&bytes, state.meta.col_names.clone())?)),
        None => Ok(None),
    }
}
```

- [ ] **Step 5: Update find() — use BTreeReader/IndexTreeReader, no file mutex**

Replace all `let mut file = self.file.lock().unwrap();` in find() with direct reader usage:

For the index path:
```rust
let reader = IndexTreeReader::new(&self.file, idx_meta.root_page);
let keys = if let Some(n) = need {
    reader.scan_prefix_limit(&prefix, n)?
} else {
    reader.scan_prefix(&prefix)?
};
// ...
let data_reader = BTreeReader::new(&self.file, state.meta.root_page);
let raw_rows = data_reader.multi_get_sorted(&matching_rowids)?;
```

For the scan_filtered path:
```rust
let reader = BTreeReader::new(&self.file, state.meta.root_page);
let (raw_rows, count) = reader.scan_filtered(col_id, f.op, &f.value, lim, off, stop)?;
```

For the no-filter/multi-filter paths:
```rust
let reader = BTreeReader::new(&self.file, state.meta.root_page);
let all = reader.scan_all()?;
```

- [ ] **Step 6: Update count() — use readers, no file mutex**

```rust
// Index path:
let reader = IndexTreeReader::new(&self.file, idx_meta.root_page);
return reader.count_prefix(&prefix);

// Scan path:
let reader = BTreeReader::new(&self.file, state.meta.root_page);
return reader.count_filtered(col_id, f.op, &f.value);
```

- [ ] **Step 7: Update insert() — use WriteGuard**

```rust
pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
    let table_state = { ... };
    let mut state = table_state.write().unwrap();
    Self::enforce_index_types(&state.meta, data)?;
    let rowid = state.meta.next_rowid;
    state.meta.next_rowid += 1;
    let col_values = ...;
    let row_bytes = row::encode_row(rowid, &col_values);

    let durability = self.durability();
    let mut guard = self.file.begin_write();
    let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
    let new_root = tree.insert(rowid, &row_bytes)?;
    state.meta.root_page = new_root;

    // Index maintenance
    if !state.meta.indexes.is_empty() {
        Self::index_update_row_w(&mut guard, &mut state.meta, rowid, &row_bytes, false)?;
    }

    if matches!(durability, Durability::None) {
        // For Durability::None, still need to publish pages to shared cache
        // but skip WAL
    }
    guard.commit(&self.wal, durability, state.meta.table_id)?;
    state.meta.row_count += 1;
    Ok(rowid)
}
```

- [ ] **Step 8: Update all other write operations similarly**

`insert_with_id`, `update`, `delete`, `create_table`, `drop_table`, `create_index`, `drop_index`, `insert_many`, `update_where`, `delete_where`, `transaction` — all follow the same pattern:
- `self.file.lock().unwrap()` → `self.file.begin_write()`
- `BTree::new(&mut file, root)` → `BTreeWriter::new(&mut guard, root)`
- `IndexTree::new(&mut file, root)` → `IndexTreeWriter::new(&mut guard, root)`
- WAL commit at end via `guard.commit(&self.wal, durability, table_id)`

Also update the internal helpers:
- `index_update_row` needs a version that takes `&mut WriteGuard` instead of `&mut PageFile`
- `commit_with_wal` is replaced by `guard.commit()`
- `persist_registry_with` needs a WriteGuard version
- The `snapshot_table_metas` + persist pattern stays the same but uses WriteGuard

- [ ] **Step 9: Update Drop impl**

```rust
impl Drop for BoogyDb {
    fn drop(&mut self) {
        let (metas, next_id) = self.snapshot_table_metas();
        let mut guard = self.file.begin_write();
        // ... serialize and put system page ...
        let _ = guard.commit(&self.wal, Durability::Normal, 0);
        let _ = self.file.sync_all();
        if let Ok(mut wal) = self.wal.lock() {
            let _ = wal.truncate();
        }
    }
}
```

- [ ] **Step 10: Update db.rs tests**

Tests that use `db.file.lock().unwrap()` for internal state inspection (like `test_scan_all_matches_count`) need updating. Replace with BTreeReader.

- [ ] **Step 11: Run all tests**

Run: `cargo test`
Expected: All 115 tests pass.

- [ ] **Step 12: Commit**

```bash
git add src/db.rs
git commit -m "refactor: read ops use BTreeReader/IndexTreeReader — no file mutex for reads"
```

---

## Task 5: Run Benchmarks and Verify

- [ ] **Step 1: Run concurrent benchmark**

Run: `cargo bench --bench concurrent_ops`

Targets (no index):
- 4 threads: >25K ops/s (was 17K)
- 8 threads: >30K ops/s (was 12K)

Targets (with index):
- 4 threads: >100K ops/s (was 72K)
- 8 threads: >120K ops/s (was 51K)

- [ ] **Step 2: Run single-thread benchmarks**

Run: `cargo bench --bench sqlite_comparison`
Run: `cargo bench --bench point_ops`

Target: no regression from current performance.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "perf: verified concurrent reads — benchmark results"
```
