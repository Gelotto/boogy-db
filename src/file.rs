use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{BoogyError, Result};
use crate::page::{Page, PAGE_SIZE};

/// Page-aligned file I/O with an in-memory page cache.
///
/// Pages and before-images are Box-allocated so Vec growth only copies
/// pointers (8 bytes each) instead of 4KB page data.
pub struct PageFile {
    file: File,
    /// Total number of pages in the file.
    num_pages: u32,
    /// Unified cache (clean + dirty pages), indexed by page number.
    pages: Vec<Option<Box<Page>>>,
    /// Which pages need flushing to disk.
    dirty_flags: Vec<bool>,
    /// WAL before-images, indexed by page number.
    before_images: Vec<Option<Box<[u8; PAGE_SIZE]>>>,
    /// When false, skip 4KB memcpy for before-images (Durability::None).
    capture_before_images: bool,
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
            file,
            num_pages,
            pages: vec![None; n],
            dirty_flags: vec![false; n],
            before_images: vec![None; n],
            capture_before_images: true,
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

        let idx = page_no as usize;
        if self.pages[idx].is_none() {
            let page = self.read_page_from_disk(page_no)?;
            self.pages[idx] = Some(Box::new(page));
        }
        Ok(self.pages[idx].as_ref().unwrap())
    }

    /// Get a mutable reference to a page. Marks it dirty.
    /// Captures a before-image of the original page data for WAL use.
    pub fn write_page(&mut self, page_no: u32) -> Result<&mut Page> {
        let idx = page_no as usize;

        if self.pages[idx].is_none() {
            if page_no < self.num_pages {
                let page = self.read_page_from_disk(page_no)?;
                self.pages[idx] = Some(Box::new(page));
            } else {
                return Err(BoogyError::Corruption(format!(
                    "page {page_no} out of range"
                )));
            }
        }

        if !self.dirty_flags[idx] {
            // Capture before-image before first mutation.
            if self.capture_before_images && self.before_images[idx].is_none() {
                self.before_images[idx] = Some(Box::new(self.pages[idx].as_ref().unwrap().data));
            }
            self.dirty_flags[idx] = true;
        }

        Ok(self.pages[idx].as_mut().unwrap())
    }

    /// Allocate a new page at the end of the file.
    pub fn allocate_page(&mut self) -> Result<u32> {
        let page_no = self.num_pages;
        self.num_pages += 1;
        self.pages.push(Some(Box::new(Page::default())));
        self.dirty_flags.push(true);
        self.before_images.push(None);
        Ok(page_no)
    }

    /// Write a page at a specific page number.
    /// If the page already exists, captures a before-image for WAL use.
    pub fn put_page(&mut self, page_no: u32, page: Page) {
        let idx = page_no as usize;

        // Grow vecs if needed.
        while idx >= self.pages.len() {
            self.pages.push(None);
            self.dirty_flags.push(false);
            self.before_images.push(None);
        }

        // Capture before-image if this is an existing page being overwritten.
        if page_no < self.num_pages
            && self.capture_before_images
            && self.before_images[idx].is_none()
        {
            if !self.dirty_flags[idx] {
                // Not yet dirty -- capture from cached page if present.
                if let Some(ref existing) = self.pages[idx] {
                    self.before_images[idx] = Some(Box::new(existing.data));
                }
            }
            // If already dirty, the original before-image was captured on first mutation.
        }

        if page_no >= self.num_pages {
            self.num_pages = page_no + 1;
        }
        self.pages[idx] = Some(Box::new(page));
        self.dirty_flags[idx] = true;
    }

    /// Flush all dirty pages to disk.
    pub fn flush(&mut self) -> Result<()> {
        for i in 0..self.num_pages as usize {
            if self.dirty_flags[i] {
                let data = self.pages[i].as_ref().unwrap().data;
                self.write_page_to_disk(i as u32, &data)?;
                self.dirty_flags[i] = false;
            }
        }
        // Clear before-images after successful flush.
        for bi in self.before_images.iter_mut() {
            *bi = None;
        }
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
        for i in 0..self.dirty_flags.len() {
            if self.dirty_flags[i] {
                self.pages[i] = None; // will re-read from disk on next access
                self.dirty_flags[i] = false;
            }
        }
        for bi in self.before_images.iter_mut() {
            *bi = None;
        }
    }

    /// Take all captured before-images, clearing the internal buffer.
    /// Returns (page_no, original_page_data) pairs.
    pub fn take_before_images(&mut self) -> Vec<(u32, [u8; PAGE_SIZE])> {
        let mut result = Vec::new();
        for (i, bi) in self.before_images.iter_mut().enumerate() {
            if let Some(data) = bi.take() {
                result.push((i as u32, *data));
            }
        }
        result
    }

    /// Number of pages in the file.
    pub fn page_count(&self) -> u32 {
        self.num_pages
    }

    /// Set whether to capture before-images on page mutation.
    pub fn set_capture_before_images(&mut self, capture: bool) {
        self.capture_before_images = capture;
    }

    /// Get a cached page without requiring `&mut self`.
    /// Returns a clone of the page if it is in cache, None otherwise.
    pub fn get_cached_page(&self, page_no: u32) -> Option<Page> {
        self.pages
            .get(page_no as usize)
            .and_then(|opt| opt.as_deref())
            .cloned()
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
