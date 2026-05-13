# Page Cache Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace HashMap-based page cache with Vec-based flat arrays to eliminate hashing overhead on every page access, closing the ~30% performance gap vs SQLite on indexed workloads.

**Architecture:** PageFile's three HashMaps (cache, dirty, before_images) become three Vecs indexed by page number. A `capture_before_images` flag skips WAL before-image capture for Durability::None. Scan loops use a `get_cached_page` method to avoid `&mut self` borrow on cache hits.

**Tech Stack:** Rust, existing boogy-db crate.

**Spec:** `docs/superpowers/specs/2026-05-13-page-cache-optimization-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/file.rs` | Modify | Replace HashMap fields with Vec fields, update all methods |
| `src/db.rs` | Modify | Propagate capture_before_images flag based on durability |
| `src/btree.rs` | Modify | Use get_cached_page in scan loops |

---

## Task 1: Replace HashMap Page Cache with Vec Arrays

**Files:**
- Modify: `src/file.rs`

This is the core change. Replace all three HashMaps with Vecs indexed by page_no.

- [ ] **Step 1: Replace struct fields**

Change the `PageFile` struct from:

```rust
pub struct PageFile {
    file: File,
    num_pages: u32,
    cache: HashMap<u32, Page>,
    dirty: HashMap<u32, Page>,
    before_images: HashMap<u32, [u8; PAGE_SIZE]>,
}
```

To:

```rust
pub struct PageFile {
    file: File,
    num_pages: u32,
    pages: Vec<Option<Page>>,
    dirty_flags: Vec<bool>,
    before_images: Vec<Option<[u8; PAGE_SIZE]>>,
    capture_before_images: bool,
}
```

Remove `use std::collections::HashMap;` from imports.

- [ ] **Step 2: Update open()**

```rust
pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    let file_len = file.metadata()?.len();
    let num_pages = (file_len / PAGE_SIZE as u64) as u32;
    let n = num_pages as usize;

    Ok(Self {
        file,
        num_pages,
        pages: vec![None; n],
        dirty_flags: vec![false; n],
        before_images: vec![None; n],
        capture_before_images: true,
    })
}
```

- [ ] **Step 3: Update read_page()**

```rust
pub fn read_page(&mut self, page_no: u32) -> Result<&Page> {
    let idx = page_no as usize;
    if page_no >= self.num_pages {
        return Err(BoogyError::Corruption(format!(
            "page {page_no} out of range (have {} pages)",
            self.num_pages
        )));
    }

    if self.pages[idx].is_none() {
        let page = self.read_page_from_disk(page_no)?;
        self.pages[idx] = Some(page);
    }
    Ok(self.pages[idx].as_ref().unwrap())
}
```

- [ ] **Step 4: Update write_page()**

```rust
pub fn write_page(&mut self, page_no: u32) -> Result<&mut Page> {
    let idx = page_no as usize;
    if page_no >= self.num_pages {
        return Err(BoogyError::Corruption(format!(
            "page {page_no} out of range"
        )));
    }

    if self.pages[idx].is_none() {
        let page = self.read_page_from_disk(page_no)?;
        self.pages[idx] = Some(page);
    }

    if !self.dirty_flags[idx] {
        // Capture before-image for WAL (only for pages that were on disk)
        if self.capture_before_images && self.before_images[idx].is_none() {
            self.before_images[idx] = Some(self.pages[idx].as_ref().unwrap().data);
        }
        self.dirty_flags[idx] = true;
    }

    Ok(self.pages[idx].as_mut().unwrap())
}
```

- [ ] **Step 5: Update allocate_page()**

```rust
pub fn allocate_page(&mut self) -> Result<u32> {
    let page_no = self.num_pages;
    self.num_pages += 1;
    self.pages.push(Some(Page::default()));
    self.dirty_flags.push(true);
    self.before_images.push(None);
    Ok(page_no)
}
```

- [ ] **Step 6: Update put_page()**

```rust
pub fn put_page(&mut self, page_no: u32, page: Page) {
    let idx = page_no as usize;

    // Capture before-image if this is an existing page being overwritten
    if (page_no as usize) < self.pages.len()
        && self.capture_before_images
        && !self.dirty_flags.get(idx).copied().unwrap_or(false)
    {
        if let Some(Some(existing)) = self.pages.get(idx) {
            if self.before_images.get(idx).map_or(true, |bi| bi.is_none()) {
                // Ensure vecs are big enough
                if idx < self.before_images.len() {
                    self.before_images[idx] = Some(existing.data);
                }
            }
        }
    }

    // Grow vecs if needed (for pages beyond current num_pages)
    while self.pages.len() <= idx {
        self.pages.push(None);
        self.dirty_flags.push(false);
        self.before_images.push(None);
    }

    if page_no >= self.num_pages {
        self.num_pages = page_no + 1;
    }
    self.pages[idx] = Some(page);
    self.dirty_flags[idx] = true;
}
```

- [ ] **Step 7: Update flush()**

```rust
pub fn flush(&mut self) -> Result<()> {
    for idx in 0..self.num_pages as usize {
        if self.dirty_flags[idx] {
            if let Some(ref page) = self.pages[idx] {
                self.write_page_to_disk(idx as u32, &page.data)?;
            }
            self.dirty_flags[idx] = false;
        }
    }
    // Clear before-images after successful flush
    for bi in &mut self.before_images {
        *bi = None;
    }
    Ok(())
}
```

- [ ] **Step 8: Update sync(), discard_dirty(), take_before_images()**

```rust
pub fn sync(&mut self) -> Result<()> {
    self.flush()?;
    self.file.sync_data()?;
    Ok(())
}

pub fn discard_dirty(&mut self) {
    for idx in 0..self.pages.len() {
        if self.dirty_flags[idx] {
            self.pages[idx] = None; // discard, will re-read from disk
            self.dirty_flags[idx] = false;
        }
    }
    for bi in &mut self.before_images {
        *bi = None;
    }
}

pub fn take_before_images(&mut self) -> Vec<(u32, [u8; PAGE_SIZE])> {
    let mut result = Vec::new();
    for (idx, bi) in self.before_images.iter_mut().enumerate() {
        if let Some(data) = bi.take() {
            result.push((idx as u32, data));
        }
    }
    result
}
```

- [ ] **Step 9: Add set_capture_before_images()**

```rust
/// Enable or disable before-image capture.
/// Disable for Durability::None to avoid unnecessary 4KB memcpys.
pub fn set_capture_before_images(&mut self, capture: bool) {
    self.capture_before_images = capture;
}
```

- [ ] **Step 10: Add get_cached_page() for scan optimization**

```rust
/// Return a clone of a cached page without requiring &mut self.
/// Returns None if the page is not in cache (caller should use read_page).
pub fn get_cached_page(&self, page_no: u32) -> Option<Page> {
    self.pages
        .get(page_no as usize)
        .and_then(|opt| opt.as_ref())
        .cloned()
}
```

- [ ] **Step 11: Run file.rs tests**

Run: `cargo test --lib file::tests`
Expected: All 3 tests pass (test_allocate_and_read, test_persist_across_reopen, test_discard_dirty).

- [ ] **Step 12: Commit**

```bash
git add src/file.rs
git commit -m "perf: replace HashMap page cache with Vec-based flat arrays"
```

---

## Task 2: Propagate Durability Flag to PageFile

**Files:**
- Modify: `src/db.rs`

Wire the durability setting to PageFile's `capture_before_images` flag.

- [ ] **Step 1: Update set_durability() to propagate to PageFile**

In `BoogyDb::set_durability`:

```rust
pub fn set_durability(&self, d: Durability) {
    self.durability.store(d as u8, std::sync::atomic::Ordering::Relaxed);
    // Propagate to PageFile: skip before-image capture for Durability::None
    if let Ok(mut file) = self.file.lock() {
        file.set_capture_before_images(!matches!(d, Durability::None));
    }
}
```

- [ ] **Step 2: Update open() to set initial capture flag**

In `BoogyDb::open`, after creating the PageFile, the default durability is Normal, so `capture_before_images` defaults to true (already set in PageFile::open). No change needed here.

- [ ] **Step 3: Run all tests**

Run: `cargo test --lib`
Expected: All tests pass. The Durability::None tests should still work (they call set_durability which now propagates).

- [ ] **Step 4: Commit**

```bash
git add src/db.rs
git commit -m "perf: propagate durability to PageFile, skip before-images for None"
```

---

## Task 3: Use get_cached_page in Scan Loops

**Files:**
- Modify: `src/btree.rs`

The scan methods (scan_all, scan_filtered, count_filtered, multi_get_sorted) clone pages to release the `&mut self` borrow. Use `get_cached_page` on cache hits to avoid taking `&mut self` — still clones, but avoids the mutable borrow overhead and potential cache-miss path.

- [ ] **Step 1: Update scan_all**

```rust
pub fn scan_all(&mut self) -> Result<Vec<(u64, Vec<u8>)>> {
    let first_leaf = self.find_leftmost_leaf(self.root)?;
    let mut results = Vec::new();
    let mut current = first_leaf;
    loop {
        let page = self.file.get_cached_page(current)
            .unwrap_or(self.file.read_page(current)?.clone());
        let num_rows = page.num_rows() as usize;
        // ... rest unchanged ...
```

Apply the same pattern to the page-read in the loop body. The `get_cached_page` call is `&self` (no mutable borrow), so if the page was already read by `find_leftmost_leaf`, it's a cache hit.

- [ ] **Step 2: Update scan_filtered**

Same pattern in the loop:

```rust
let (page_data, num_rows, next) = {
    let page = self.file.get_cached_page(current)
        .unwrap_or(self.file.read_page(current)?.clone());
    (page.data, page.num_rows() as usize, page.next_leaf())
};
```

- [ ] **Step 3: Update count_filtered**

Same pattern.

- [ ] **Step 4: Update multi_get_sorted**

Same pattern in the leaf-chain walk:

```rust
let page = self.file.get_cached_page(current)
    .unwrap_or(self.file.read_page(current)?.clone());
```

- [ ] **Step 5: Run all tests**

Run: `cargo test`
Expected: All 115 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/btree.rs
git commit -m "perf: use get_cached_page in scan loops to avoid mutable borrow overhead"
```

---

## Task 4: Run Benchmarks and Verify

- [ ] **Step 1: Run profile_ops benchmark**

Run: `cargo bench --bench profile_ops`

Check per-operation costs at 3K rows with index:
- insert: target <2.5µs (was 3.1µs)
- get: target <0.4µs (was 0.5µs)
- find_eq: target <15µs (was 21.7µs)
- count_eq: target <8µs (was 10.9µs)

- [ ] **Step 2: Run sqlite_comparison benchmark**

Run: `cargo bench --bench sqlite_comparison`

Target: indexed mixed workload ops/sec > SQLite's ~40K ops/sec.

- [ ] **Step 3: Run point_ops to verify no regression**

Run: `cargo bench --bench point_ops`

Target: insert >700K/s, get >2.5M/s (same as before).

- [ ] **Step 4: Commit results**

```bash
git add -A
git commit -m "perf: verified page cache optimization — benchmark results"
```
