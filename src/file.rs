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
    before_images: Vec<Option<Box<[u8; PAGE_SIZE]>>>,
    pub(crate) capture_before_images: bool,
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
                before_images: vec![None; n],
                capture_before_images: true,
                new_page_count: 0,
            }),
        })
    }

    /// Number of pages currently known to the file.
    pub fn page_count(&self) -> u32 {
        self.num_pages.load(Ordering::Relaxed)
    }

    /// Toggle before-image capture (disabled for `Durability::None`).
    pub fn set_capture_before_images(&self, capture: bool) {
        let mut ws = self.write_state.lock().unwrap();
        ws.capture_before_images = capture;
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
    pub fn put_page_direct(&self, page_no: u32, page: Page) {
        let np = self.num_pages.load(Ordering::Relaxed);

        // Write to disk.
        {
            let mut disk = self.disk.lock().unwrap();
            let offset = page_no as u64 * PAGE_SIZE as u64;
            let _ = disk.seek(SeekFrom::Start(offset));
            let _ = disk.write_all(&page.data);
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
    }

    /// Flush ALL cached pages to disk and fsync. For clean shutdown.
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
    /// on first access and captures a before-image for WAL use.
    pub fn write_page(&mut self, page_no: u32) -> Result<&mut Page> {
        if !self.state.dirty.contains_key(&page_no) {
            // Load the current version of the page.
            let current = self.read_page_inner(page_no)?;

            // Capture before-image of the on-disk page before we mutate.
            if self.state.capture_before_images {
                let idx = page_no as usize;
                while self.state.before_images.len() <= idx {
                    self.state.before_images.push(None);
                }
                if self.state.before_images[idx].is_none() {
                    self.state.before_images[idx] = Some(Box::new(current.data));
                }
            }

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

    /// Overwrite a page slot. Captures a before-image if the page already
    /// existed on disk.
    pub fn put_page(&mut self, page_no: u32, page: Page) {
        let np = self.file.num_pages.load(Ordering::Relaxed);

        // Capture before-image for existing, non-dirty pages.
        if page_no < np
            && self.state.capture_before_images
            && !self.state.dirty.contains_key(&page_no)
        {
            let idx = page_no as usize;
            while self.state.before_images.len() <= idx {
                self.state.before_images.push(None);
            }
            if self.state.before_images[idx].is_none() {
                // Try to get current data from cache.
                if let Ok(existing) = self.file.read_page(page_no) {
                    self.state.before_images[idx] = Some(Box::new(existing.data));
                }
            }
        }

        self.state.dirty.insert(page_no, Box::new(page));
    }

    /// Drain all captured before-images. Returns `(page_no, original_data)` pairs.
    pub fn take_before_images(&mut self) -> Vec<(u32, [u8; PAGE_SIZE])> {
        let mut result = Vec::new();
        for (i, bi) in self.state.before_images.iter_mut().enumerate() {
            if let Some(data) = bi.take() {
                result.push((i as u32, *data));
            }
        }
        result
    }

    /// Publish dirty pages to the shared cache and optionally flush to disk.
    ///
    /// Returns before-images so the caller can write them to the WAL.
    /// This keeps `file.rs` independent of the WAL module.
    pub fn commit(mut self, flush_to_disk: bool) -> Result<Vec<(u32, [u8; PAGE_SIZE])>> {
        // Collect before-images before we drain dirty.
        let before_images = self.take_before_images_inner();

        if flush_to_disk {
            let mut disk = self.file.disk.lock().unwrap();
            for (&page_no, page) in &self.state.dirty {
                let offset = page_no as u64 * PAGE_SIZE as u64;
                disk.seek(SeekFrom::Start(offset))?;
                disk.write_all(&page.data)?;
            }
        }

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
        self.state.before_images.iter_mut().for_each(|bi| *bi = None);
        self.state.new_page_count = 0;

        Ok(before_images)
    }

    /// Discard all dirty pages without publishing. Rollback.
    pub fn discard(mut self) {
        self.state.dirty.clear();
        self.state.before_images.iter_mut().for_each(|bi| *bi = None);
        self.state.new_page_count = 0;
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

    /// Drain before-images (private, for commit).
    fn take_before_images_inner(&mut self) -> Vec<(u32, [u8; PAGE_SIZE])> {
        let mut result = Vec::new();
        for (i, bi) in self.state.before_images.iter_mut().enumerate() {
            if let Some(data) = bi.take() {
                result.push((i as u32, *data));
            }
        }
        result
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
            guard.commit(false).unwrap(); // don't flush to disk
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
            guard.commit(true).unwrap(); // flush to disk
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
            guard.commit(false).unwrap();
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
}
