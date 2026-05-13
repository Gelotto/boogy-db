# Page Cache Optimization — Beat SQLite on Indexed Workloads

## Problem

boogy-db loses to SQLite by ~30-40% on indexed mixed workloads (28K vs 40K ops/s). The gap is uniform across all operation types, indicating per-operation fixed overhead rather than algorithmic issues.

Root cause: every page access goes through `HashMap<u32, Page>` lookups. A single B+ tree search reads 2-3 pages, each requiring 2-3 HashMap operations (check dirty, check cache, get). With indexed inserts touching two trees, that's 12+ HashMap lookups per insert. SQLite uses a flat array page cache with O(1) access.

Secondary causes:
- Before-image capture (4KB memcpy) happens even for Durability::None, then is immediately discarded
- Scan loops clone entire 4KB pages to release the `&mut self` borrow on PageFile

## Changes

### 1. Vec-Based Page Cache

Replace the three HashMaps in `PageFile` with flat arrays indexed by page number:

```rust
// Current
pub struct PageFile {
    file: File,
    num_pages: u32,
    cache: HashMap<u32, Page>,           // clean cached pages
    dirty: HashMap<u32, Page>,           // modified pages
    before_images: HashMap<u32, [u8; PAGE_SIZE]>,  // WAL before-images
}

// New
pub struct PageFile {
    file: File,
    num_pages: u32,
    pages: Vec<Option<Page>>,            // unified cache (clean + dirty)
    dirty_flags: Vec<bool>,              // true = needs flush to disk
    before_images: Vec<Option<[u8; PAGE_SIZE]>>,  // WAL before-images
    capture_before_images: bool,         // false for Durability::None
}
```

**read_page(page_no):**
```
if pages[page_no] is Some → return reference (one array index)
else → read from disk, store in pages[page_no], return reference
```

**write_page(page_no):**
```
if pages[page_no] is None → load from disk
if not dirty and capture_before_images → save before_images[page_no]
set dirty_flags[page_no] = true
return mutable reference
```

**put_page(page_no, page):**
```
if page_no < num_pages and not dirty and capture_before_images → save before_images
pages[page_no] = Some(page)
dirty_flags[page_no] = true
```

**allocate_page():**
```
pages.push(Some(Page::default()))
dirty_flags.push(true)
before_images.push(None)
num_pages += 1
return num_pages - 1
```

**flush():**
```
for i in 0..num_pages where dirty_flags[i]:
    write pages[i] to disk
    dirty_flags[i] = false
before_images = vec![None; num_pages]  // clear all
```

**take_before_images():**
```
collect all Some entries from before_images, replace with None
```

**discard_dirty():**
```
for i where dirty_flags[i]:
    pages[i] = None  // discard dirty version, will re-read from disk
    dirty_flags[i] = false
before_images = vec![None; num_pages]
```

Vec capacity: pages are lazily loaded (None until accessed). At 4KB per page, 1000 cached pages = 4MB. The Vec<Option<Page>> itself uses 8 bytes per slot for None entries (discriminant + padding), so 10000 slots = 80KB of overhead for unloaded pages.

### 2. Skip Before-Image Capture for Durability::None

`PageFile` gets a `capture_before_images: bool` field. When false:
- `write_page` skips the 4KB memcpy into before_images
- `put_page` skips the before-image capture
- `take_before_images` returns empty

Set via a new `PageFile::set_capture_before_images(bool)` method. `BoogyDb` calls this based on durability level: false for `Durability::None`, true otherwise.

This saves one 4KB memcpy per dirty page on every write operation.

### 3. Split-Borrow Page Access for Scan Loops

Current scan loops clone the page to release the `&mut self` borrow:
```rust
let page = self.file.read_page(current)?.clone(); // 4KB copy
```

With Vec-based cache, add a method that checks if a page is cached without taking `&mut self`:

```rust
/// Check if page is in cache. If so, return a clone without disk I/O.
/// If not, return None (caller must use read_page for cache-miss path).
pub fn get_cached_page(&self, page_no: u32) -> Option<Page> {
    self.pages.get(page_no as usize)
        .and_then(|opt| opt.as_ref())
        .cloned()
}
```

Scan loops become:
```rust
let page = self.file.get_cached_page(current)
    .map(Ok)
    .unwrap_or_else(|| self.file.read_page(current).map(|p| p.clone()))?;
```

On cache hit (the common case during scans — pages stay cached), this avoids taking `&mut self` entirely. The clone is still needed (can't hold reference across loop iterations), but the page data is already in CPU cache from the Vec access.

## Files Changed

- `src/file.rs` — Replace HashMap fields with Vec fields, update all methods
- `src/db.rs` — Call `set_capture_before_images` based on durability, update `set_durability` to propagate
- `src/btree.rs` — Use `get_cached_page` in scan loops where beneficial

## Performance Targets

With index (mixed workload): surpass SQLite's ~40K ops/s.

Per-operation targets at 3K rows:
- insert with index: <2.5µs (currently 3.1µs, SQLite 2.9µs)
- get: <0.4µs (currently 0.5µs, SQLite 1.7µs)
- find_eq with index LIMIT 20: <15µs (currently 21.7µs, SQLite 12.8µs)
- count_eq with index: <8µs (currently 10.9µs, SQLite 12.0µs)

## Scope

This spec covers page cache optimization only. Not in scope:
- B+ tree algorithm changes
- Row format changes
- New index features
- MVCC
