use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::error::{BoogyError, Result};
use crate::page::{Page, PAGE_SIZE};

/// Mutable write-side state, protected by its own Mutex.
pub struct WriteState {
    dirty: HashMap<u32, Box<Page>>,
    new_page_count: u32,
}

/// RAII guard returned by [`PageFile::begin_write`].
///
/// Reads see the dirty overlay first, then fall through to the shared cache.
/// On [`commit`](WriteGuard::commit) dirty pages are published to the shared
/// cache (and optionally flushed to disk). On [`discard`](WriteGuard::discard)
/// all mutations are thrown away.
pub struct WriteGuard<'a> {
    file: &'a PageFile,
    state: std::sync::MutexGuard<'a, WriteState>,
}

/// Page-aligned file I/O with a shared read cache and exclusive write path.
///
/// The page cache is an `RwLock<Vec<Option<Arc<Page>>>>` so concurrent readers
/// only need a shared lock. All mutations go through a [`WriteGuard`] which
/// holds a separate `Mutex<WriteState>`.
pub struct PageFile {
    disk: Mutex<File>,
    num_pages: AtomicU32,
    pages: RwLock<Vec<Option<Arc<Page>>>>,
    write_state: Mutex<WriteState>,
    /// Page-level cipher map. Pages in this map are encrypted before writing to disk.
    page_ciphers: RwLock<HashMap<u32, Arc<crate::crypto::Cipher>>>,
}

impl PageFile {
    /// Open or create a page file.
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
            disk: Mutex::new(file),
            num_pages: AtomicU32::new(num_pages),
            pages: RwLock::new(vec![None; n]),
            write_state: Mutex::new(WriteState {
                dirty: HashMap::new(),
                new_page_count: 0,
            }),
            page_ciphers: RwLock::new(HashMap::new()),
        })
    }

    /// Number of pages currently known to the file.
    pub fn page_count(&self) -> u32 {
        self.num_pages.load(Ordering::Relaxed)
    }

    /// Read a page from the shared cache, falling back to disk on a miss.
    ///
    /// This is the key concurrency primitive: it takes `&self`, so multiple
    /// threads can call it concurrently while holding only a shared reference.
    pub fn read_page(&self, page_no: u32) -> Result<Arc<Page>> {
        let np = self.num_pages.load(Ordering::Relaxed);
        if page_no >= np {
            return Err(BoogyError::Corruption(format!(
                "page {page_no} out of range (have {np} pages)"
            )));
        }

        // Fast path: shared lock on cache.
        {
            let cache = self.pages.read().unwrap();
            if let Some(Some(arc)) = cache.get(page_no as usize) {
                return Ok(Arc::clone(arc));
            }
        }

        // Slow path: cache miss -- read from disk, then insert.
        let page = {
            let mut disk = self.disk.lock().unwrap();
            Self::read_page_from_disk(&mut disk, page_no)?
        };

        let arc = Arc::new(page);

        {
            let mut cache = self.pages.write().unwrap();
            // Double-check: another thread may have populated it.
            if let Some(existing) = cache.get(page_no as usize) {
                if let Some(existing_arc) = existing {
                    return Ok(Arc::clone(existing_arc));
                }
            }
            // Grow if needed (shouldn't normally happen, but be safe).
            while cache.len() <= page_no as usize {
                cache.push(None);
            }
            cache[page_no as usize] = Some(Arc::clone(&arc));
        }

        Ok(arc)
    }

    /// Begin a write transaction. Only one writer at a time.
    pub fn begin_write(&self) -> WriteGuard<'_> {
        let state = self.write_state.lock().unwrap();
        WriteGuard { file: self, state }
    }

    /// Write a page directly to cache and disk. Used during crash recovery
    /// in `open()` to restore WAL before-images without going through the
    /// normal write path.
    pub fn put_page_direct(&self, page_no: u32, page: Page) -> Result<()> {
        let np = self.num_pages.load(Ordering::Relaxed);

        // Write to disk.
        {
            let mut disk = self.disk.lock().unwrap();
            let offset = page_no as u64 * PAGE_SIZE as u64;
            disk.seek(SeekFrom::Start(offset))?;
            disk.write_all(&page.data)?;
        }

        // Update page count if this extends the file.
        if page_no >= np {
            self.num_pages.store(page_no + 1, Ordering::Relaxed);
        }

        // Insert into shared cache.
        let arc = Arc::new(page);
        let mut cache = self.pages.write().unwrap();
        while cache.len() <= page_no as usize {
            cache.push(None);
        }
        cache[page_no as usize] = Some(arc);
        Ok(())
    }

    /// Read raw page bytes from disk without caching or parsing.
    pub fn read_page_raw(&self, page_no: u32) -> Result<[u8; PAGE_SIZE]> {
        let mut disk = self.disk.lock().unwrap();
        let offset = page_no as u64 * PAGE_SIZE as u64;
        disk.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; PAGE_SIZE];
        disk.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Insert a plaintext page directly into the shared cache.
    pub fn put_cached_page(&self, page_no: u32, page: Page) {
        let mut cache = self.pages.write().unwrap();
        while cache.len() <= page_no as usize {
            cache.push(None);
        }
        cache[page_no as usize] = Some(Arc::new(page));
    }

    /// Check whether a page is already in the shared cache.
    pub fn is_cached(&self, page_no: u32) -> bool {
        let cache = self.pages.read().unwrap();
        cache.get(page_no as usize).map_or(false, |slot| slot.is_some())
    }

    /// Register a cipher for a page so it is encrypted before writing to disk.
    pub fn register_page_cipher(&self, page_no: u32, cipher: Arc<crate::crypto::Cipher>) {
        let mut map = self.page_ciphers.write().unwrap();
        map.insert(page_no, cipher);
    }

    /// Flush ALL cached pages to disk and fsync. For clean shutdown.
    pub fn sync_all(&self) -> Result<()> {
        let cache = self.pages.read().unwrap();
        let page_ciphers = self.page_ciphers.read().unwrap();
        let mut disk = self.disk.lock().unwrap();

        for (i, slot) in cache.iter().enumerate() {
            if let Some(arc) = slot {
                let page_no = i as u32;
                let offset = page_no as u64 * PAGE_SIZE as u64;
                disk.seek(SeekFrom::Start(offset))?;

                if let Some(cipher) = page_ciphers.get(&page_no) {
                    // Encrypt before writing
                    let encrypted = cipher.encrypt_page(
                        &arc.data[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE],
                    )?;
                    disk.write_all(&encrypted)?;
                } else {
                    disk.write_all(&arc.data)?;
                }
            }
        }

        disk.sync_data()?;
        Ok(())
    }

    /// Read a single page from the underlying file.
    fn read_page_from_disk(disk: &mut File, page_no: u32) -> Result<Page> {
        let offset = page_no as u64 * PAGE_SIZE as u64;
        disk.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; PAGE_SIZE];
        disk.read_exact(&mut buf)?;
        Page::from_bytes(buf)
    }
}

// ---------------------------------------------------------------------------
// WriteGuard
// ---------------------------------------------------------------------------

impl<'a> WriteGuard<'a> {
    /// Read a page, checking the dirty overlay first.
    pub fn read_page(&self, page_no: u32) -> Result<Arc<Page>> {
        // Check dirty overlay.
        if let Some(dirty_page) = self.state.dirty.get(&page_no) {
            return Ok(Arc::new((**dirty_page).clone()));
        }
        // Fall through to shared cache / disk.
        self.file.read_page(page_no)
    }

    /// Get a mutable reference to a page. Copies it into the dirty overlay
    /// on first access.
    pub fn write_page(&mut self, page_no: u32) -> Result<&mut Page> {
        if !self.state.dirty.contains_key(&page_no) {
            // Load the current version of the page.
            let current = self.read_page_inner(page_no)?;
            self.state.dirty.insert(page_no, Box::new(current));
        }
        Ok(self.state.dirty.get_mut(&page_no).unwrap())
    }

    /// Allocate a new page at the logical end of the file.
    pub fn allocate_page(&mut self) -> Result<u32> {
        let base = self.file.num_pages.load(Ordering::Relaxed);
        let page_no = base + self.state.new_page_count;
        self.state.new_page_count += 1;
        self.state.dirty.insert(page_no, Box::new(Page::default()));
        Ok(page_no)
    }

    /// Overwrite a page slot.
    pub fn put_page(&mut self, page_no: u32, page: Page) {
        self.state.dirty.insert(page_no, Box::new(page));
    }

    /// Publish dirty pages to the shared cache.
    ///
    /// Returns after-images so the caller can write them to the WAL.
    /// This keeps `file.rs` independent of the WAL module.
    pub fn commit(mut self) -> Result<Vec<(u32, [u8; PAGE_SIZE])>> {
        // Collect after-images from dirty pages.
        let after_images: Vec<(u32, [u8; PAGE_SIZE])> = self
            .state
            .dirty
            .iter()
            .map(|(&page_no, page)| (page_no, page.data))
            .collect();

        // Publish to shared cache.
        let new_page_count = self.state.new_page_count;
        let np = self.file.num_pages.load(Ordering::Relaxed);
        let new_total = np + new_page_count;

        {
            let mut cache = self.file.pages.write().unwrap();
            while cache.len() < new_total as usize {
                cache.push(None);
            }
            for (&page_no, page) in &self.state.dirty {
                cache[page_no as usize] = Some(Arc::new((**page).clone()));
            }
        }

        if new_page_count > 0 {
            self.file
                .num_pages
                .fetch_add(new_page_count, Ordering::Relaxed);
        }

        // Clear write state for next transaction.
        self.state.dirty.clear();
        self.state.new_page_count = 0;

        Ok(after_images)
    }

    /// Discard all dirty pages without publishing. Rollback.
    pub fn discard(mut self) {
        self.state.dirty.clear();
        self.state.new_page_count = 0;
    }

    /// Access the underlying PageFile for read-path fallthrough.
    pub fn page_file(&self) -> &PageFile {
        self.file
    }

    /// Peek at a dirty page by reference (zero-copy). Returns None if not dirty.
    pub fn peek_dirty(&self, page_no: u32) -> Option<&Page> {
        self.state.dirty.get(&page_no).map(|p| &**p)
    }

    /// Read a page as an owned clone without Arc wrapper.
    /// More efficient than read_page() because it avoids Arc allocation for dirty pages.
    pub fn read_page_cloned(&self, page_no: u32) -> Result<Page> {
        if let Some(page) = self.state.dirty.get(&page_no) {
            return Ok((**page).clone());
        }
        Ok((*self.file.read_page(page_no)?).clone())
    }

    /// Move pages into the dirty overlay from an external buffer.
    /// Pages already in the overlay are NOT overwritten.
    pub fn inject_dirty(&mut self, pages: HashMap<u32, Box<Page>>) {
        for (page_no, page) in pages {
            if !self.state.dirty.contains_key(&page_no) {
                self.state.dirty.insert(page_no, page);
            }
        }
    }

    /// Drain all dirty pages out of the overlay, returning them.
    pub fn drain_dirty(&mut self) -> HashMap<u32, Box<Page>> {
        std::mem::take(&mut self.state.dirty)
    }

    /// Get the current new_page_count.
    pub fn new_page_count(&self) -> u32 {
        self.state.new_page_count
    }

    /// Set new_page_count (used by ACID transactions to restore state).
    pub fn set_new_page_count(&mut self, count: u32) {
        self.state.new_page_count = count;
    }

    // -- private helpers --

    /// Read a page (without going through the public API that returns Arc).
    /// Checks dirty overlay first, then shared cache / disk.
    fn read_page_inner(&self, page_no: u32) -> Result<Page> {
        if let Some(dirty_page) = self.state.dirty.get(&page_no) {
            return Ok((**dirty_page).clone());
        }
        let arc = self.file.read_page(page_no)?;
        Ok((*arc).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_allocate_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            assert_eq!(pg0, 0);
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(5);
            page.update_checksum();
            guard.commit().unwrap();
        }

        let page = pf.read_page(0).unwrap();
        assert!(page.is_leaf());
        assert_eq!(page.num_rows(), 5);
    }

    #[test]
    fn test_persist_across_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let pf = PageFile::open(&path).unwrap();
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(42);
            page.update_checksum();
            guard.commit().unwrap();
            pf.sync_all().unwrap();
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
            let _pg0 = guard.allocate_page().unwrap();
            guard.discard();
        }

        assert_eq!(pf.page_count(), 0);
    }

    #[test]
    fn test_concurrent_reads() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = Arc::new(PageFile::open(tmp.path()).unwrap());

        // Write a page
        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(7);
            page.update_checksum();
            guard.commit().unwrap();
        }

        // Read concurrently from multiple threads
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let pf = Arc::clone(&pf);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        let page = pf.read_page(0).unwrap();
                        assert_eq!(page.num_rows(), 7);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_commit_returns_after_images() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let after_images;
        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(3);
            page.update_checksum();
            after_images = guard.commit().unwrap();
        }

        // commit() should return exactly one after-image for pg0
        assert_eq!(after_images.len(), 1);
        let (page_no, data) = &after_images[0];
        assert_eq!(*page_no, 0);
        // Verify the after-image is a leaf page with 3 rows
        let page = Page::from_bytes_unchecked(*data);
        assert!(page.is_leaf());
        assert_eq!(page.num_rows(), 3);
    }

    #[test]
    fn test_commit_multiple_pages_after_images() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let after_images;
        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let pg1 = guard.allocate_page().unwrap();

            let page0 = guard.write_page(pg0).unwrap();
            *page0 = Page::new_leaf();
            page0.set_num_rows(1);
            page0.update_checksum();

            let page1 = guard.write_page(pg1).unwrap();
            *page1 = Page::new_branch();
            page1.set_num_rows(2);
            page1.update_checksum();

            after_images = guard.commit().unwrap();
        }

        // Should have after-images for both pages
        assert_eq!(after_images.len(), 2);
        let page_nos: Vec<u32> = after_images.iter().map(|(pn, _)| *pn).collect();
        assert!(page_nos.contains(&0));
        assert!(page_nos.contains(&1));
    }

    #[test]
    fn test_discard_produces_no_visible_changes() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        // First create a page with known data
        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(5);
            page.update_checksum();
            guard.commit().unwrap();
        }

        // Now modify it but discard
        {
            let mut guard = pf.begin_write();
            let page = guard.write_page(0).unwrap();
            page.set_num_rows(99);
            page.update_checksum();
            guard.discard();
        }

        // Original value should be unchanged
        let page = pf.read_page(0).unwrap();
        assert_eq!(page.num_rows(), 5);
    }

    #[test]
    fn test_write_guard_reads_dirty_overlay() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        // Create a page
        {
            let mut guard = pf.begin_write();
            let pg0 = guard.allocate_page().unwrap();
            let page = guard.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(1);
            page.update_checksum();
            guard.commit().unwrap();
        }

        // Within a write guard, reads should see the dirty overlay
        {
            let mut guard = pf.begin_write();
            let page = guard.write_page(0).unwrap();
            page.set_num_rows(42);
            page.update_checksum();

            // Reading within the same guard should see 42
            let read = guard.read_page(0).unwrap();
            assert_eq!(read.num_rows(), 42);
            guard.discard();
        }

        // After discard, original is unchanged
        let page = pf.read_page(0).unwrap();
        assert_eq!(page.num_rows(), 1);
    }

    #[test]
    fn test_inject_drain_roundtrip() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        // Create a page via normal write path
        let mut guard = pf.begin_write();
        let pg0 = guard.allocate_page().unwrap();
        let page = guard.write_page(pg0).unwrap();
        *page = Page::new_leaf();
        page.set_num_rows(3);
        page.update_checksum();

        // Drain dirty pages
        let drained = guard.drain_dirty();
        assert!(drained.contains_key(&pg0));
        assert_eq!(drained[&pg0].num_rows(), 3);

        // Guard should have no dirty pages now
        guard.discard();

        // Inject into a new guard
        let mut guard2 = pf.begin_write();
        guard2.inject_dirty(drained);

        // Should be visible via peek_dirty
        let peeked = guard2.peek_dirty(pg0).unwrap();
        assert_eq!(peeked.num_rows(), 3);

        guard2.discard();
    }
}
