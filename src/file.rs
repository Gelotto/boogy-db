use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{BoogyError, Result};
use crate::page::{Page, PAGE_SIZE};

/// Page-aligned file I/O with an in-memory page cache.
pub struct PageFile {
    file: File,
    /// Total number of pages in the file.
    num_pages: u32,
    /// In-memory cache of recently accessed pages.
    cache: HashMap<u32, Page>,
    /// Pages modified since last flush.
    dirty: HashMap<u32, Page>,
    /// Before-images of pages captured when they first become dirty.
    /// Keyed by page_no. Only present for pages that existed on disk.
    before_images: HashMap<u32, [u8; PAGE_SIZE]>,
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

        Ok(Self {
            file,
            num_pages,
            cache: HashMap::new(),
            dirty: HashMap::new(),
            before_images: HashMap::new(),
        })
    }

    /// Read a page from cache or disk.
    pub fn read_page(&mut self, page_no: u32) -> Result<&Page> {
        if page_no >= self.num_pages {
            return Err(BoogyError::Corruption(format!(
                "page {page_no} out of range (have {0} pages)",
                self.num_pages
            )));
        }

        // Check dirty pages first (most recent version)
        if self.dirty.contains_key(&page_no) {
            return Ok(self.dirty.get(&page_no).unwrap());
        }

        // Check clean cache
        if !self.cache.contains_key(&page_no) {
            let page = self.read_page_from_disk(page_no)?;
            self.cache.insert(page_no, page);
        }
        Ok(self.cache.get(&page_no).unwrap())
    }

    /// Get a mutable reference to a page. Marks it dirty.
    /// Captures a before-image of the original page data for WAL use.
    pub fn write_page(&mut self, page_no: u32) -> Result<&mut Page> {
        if !self.dirty.contains_key(&page_no) {
            // Copy from cache or disk into dirty set
            let page = if let Some(cached) = self.cache.remove(&page_no) {
                cached
            } else if page_no < self.num_pages {
                self.read_page_from_disk(page_no)?
            } else {
                return Err(BoogyError::Corruption(format!(
                    "page {page_no} out of range"
                )));
            };
            // Capture before-image for existing pages (not newly allocated).
            if !self.before_images.contains_key(&page_no) {
                self.before_images.insert(page_no, page.data);
            }
            self.dirty.insert(page_no, page);
        }
        Ok(self.dirty.get_mut(&page_no).unwrap())
    }

    /// Allocate a new page at the end of the file.
    pub fn allocate_page(&mut self) -> Result<u32> {
        let page_no = self.num_pages;
        self.num_pages += 1;
        // Write a zeroed page to extend the file
        let page = Page::default();
        self.dirty.insert(page_no, page);
        Ok(page_no)
    }

    /// Write a page at a specific page number.
    /// If the page already exists, captures a before-image for WAL use.
    pub fn put_page(&mut self, page_no: u32, page: Page) {
        // Capture before-image if this is an existing page being overwritten.
        if page_no < self.num_pages && !self.before_images.contains_key(&page_no) {
            // Get original from dirty (already modified), cache, or we skip
            // (reading from disk here would require Result return type).
            if let Some(dirty_page) = self.dirty.get(&page_no) {
                // Already dirty -- the original before-image should have been
                // captured when it first became dirty via write_page.
                let _ = dirty_page;
            } else if let Some(cached) = self.cache.get(&page_no) {
                self.before_images.insert(page_no, cached.data);
            }
            // Note: if the page is on disk but not cached, we skip before-image.
            // The caller (db.rs commit_with_wal) will read it from disk if needed.
            // In practice, page 0 will always be cached after first open.
        }
        if page_no >= self.num_pages {
            self.num_pages = page_no + 1;
        }
        self.dirty.insert(page_no, page);
    }

    /// Flush all dirty pages to disk.
    pub fn flush(&mut self) -> Result<()> {
        // Collect page numbers to avoid borrow conflict
        let page_nos: Vec<u32> = self.dirty.keys().copied().collect();
        for page_no in page_nos {
            let data = self.dirty[&page_no].data;
            self.write_page_to_disk(page_no, &data)?;
        }
        // Move dirty pages to cache
        for (page_no, page) in self.dirty.drain() {
            self.cache.insert(page_no, page);
        }
        // Clear before-images after successful flush
        self.before_images.clear();
        Ok(())
    }

    /// Flush + fsync.
    pub fn sync(&mut self) -> Result<()> {
        self.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Discard all dirty pages (rollback).
    pub fn discard_dirty(&mut self) {
        self.dirty.clear();
        self.before_images.clear();
    }

    /// Take all captured before-images, clearing the internal buffer.
    /// Returns (page_no, original_page_data) pairs.
    pub fn take_before_images(&mut self) -> Vec<(u32, [u8; PAGE_SIZE])> {
        self.before_images.drain().collect()
    }

    /// Number of pages in the file.
    pub fn page_count(&self) -> u32 {
        self.num_pages
    }

    fn read_page_from_disk(&mut self, page_no: u32) -> Result<Page> {
        let offset = page_no as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;
        Page::from_bytes(buf)
    }

    fn write_page_to_disk(&mut self, page_no: u32, data: &[u8; PAGE_SIZE]) -> Result<()> {
        let offset = page_no as u64 * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_allocate_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();

        let pg0 = pf.allocate_page().unwrap();
        assert_eq!(pg0, 0);

        {
            let page = pf.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(5);
            page.update_checksum();
        }

        pf.flush().unwrap();

        // Read back
        let page = pf.read_page(pg0).unwrap();
        assert!(page.is_leaf());
        assert_eq!(page.num_rows(), 5);
    }

    #[test]
    fn test_persist_across_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut pf = PageFile::open(&path).unwrap();
            let pg0 = pf.allocate_page().unwrap();
            let page = pf.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(42);
            page.update_checksum();
            pf.sync().unwrap();
        }

        {
            let mut pf = PageFile::open(&path).unwrap();
            assert_eq!(pf.page_count(), 1);
            let page = pf.read_page(0).unwrap();
            assert_eq!(page.num_rows(), 42);
        }
    }

    #[test]
    fn test_discard_dirty() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();

        let pg0 = pf.allocate_page().unwrap();
        {
            let page = pf.write_page(pg0).unwrap();
            *page = Page::new_leaf();
            page.set_num_rows(99);
            page.update_checksum();
        }

        pf.discard_dirty();
        // Page was never flushed, file is empty
        assert_eq!(pf.page_count(), 1); // allocated but not persisted
    }
}
