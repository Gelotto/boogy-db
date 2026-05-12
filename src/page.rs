use crate::error::{BoogyError, Result};

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_HEADER_SIZE: usize = 16;

// Page type flags
pub const PAGE_LEAF: u16 = 0x01;
pub const PAGE_BRANCH: u16 = 0x02;
pub const PAGE_SYSTEM: u16 = 0x04;
pub const PAGE_FREE: u16 = 0x08;

const MAGIC: u16 = 0xB00D;

/// A fixed-size page buffer.
#[derive(Clone)]
pub struct Page {
    pub data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new_leaf() -> Self {
        let mut page = Self { data: [0; PAGE_SIZE] };
        page.set_magic(MAGIC);
        page.set_flags(PAGE_LEAF);
        page.set_num_rows(0);
        page.set_free_space_offset(PAGE_HEADER_SIZE as u16 + 0); // no row offsets yet
        page.set_next_leaf(0);
        page.set_prev_leaf(0);
        page.update_checksum();
        page
    }

    pub fn new_branch() -> Self {
        let mut page = Self { data: [0; PAGE_SIZE] };
        page.set_magic(MAGIC);
        page.set_flags(PAGE_BRANCH);
        page.set_num_rows(0); // num_keys for branch
        page.update_checksum();
        page
    }

    pub fn new_system() -> Self {
        let mut page = Self { data: [0; PAGE_SIZE] };
        page.set_magic(MAGIC);
        page.set_flags(PAGE_SYSTEM);
        page.update_checksum();
        page
    }

    pub fn from_bytes(data: [u8; PAGE_SIZE]) -> Result<Self> {
        let page = Self { data };
        page.validate()?;
        Ok(page)
    }

    fn validate(&self) -> Result<()> {
        let magic = self.magic();
        if magic != MAGIC {
            return Err(BoogyError::Corruption(format!(
                "bad page magic: expected {MAGIC:#06x}, got {magic:#06x}"
            )));
        }
        if !self.verify_checksum() {
            return Err(BoogyError::Corruption("page checksum mismatch".into()));
        }
        Ok(())
    }

    // --- Header accessors ---

    pub fn magic(&self) -> u16 {
        u16::from_le_bytes([self.data[0], self.data[1]])
    }
    fn set_magic(&mut self, v: u16) {
        self.data[0..2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn flags(&self) -> u16 {
        u16::from_le_bytes([self.data[2], self.data[3]])
    }
    pub fn set_flags(&mut self, v: u16) {
        self.data[2..4].copy_from_slice(&v.to_le_bytes());
    }

    pub fn is_leaf(&self) -> bool { self.flags() & PAGE_LEAF != 0 }
    pub fn is_branch(&self) -> bool { self.flags() & PAGE_BRANCH != 0 }

    pub fn num_rows(&self) -> u16 {
        u16::from_le_bytes([self.data[4], self.data[5]])
    }
    pub fn set_num_rows(&mut self, v: u16) {
        self.data[4..6].copy_from_slice(&v.to_le_bytes());
    }

    pub fn free_space_offset(&self) -> u16 {
        u16::from_le_bytes([self.data[6], self.data[7]])
    }
    pub fn set_free_space_offset(&mut self, v: u16) {
        self.data[6..8].copy_from_slice(&v.to_le_bytes());
    }

    pub fn next_leaf(&self) -> u32 {
        u32::from_le_bytes(self.data[8..12].try_into().unwrap())
    }
    pub fn set_next_leaf(&mut self, v: u32) {
        self.data[8..12].copy_from_slice(&v.to_le_bytes());
    }

    pub fn prev_leaf(&self) -> u32 {
        u32::from_le_bytes(self.data[12..16].try_into().unwrap())
    }
    pub fn set_prev_leaf(&mut self, v: u32) {
        self.data[12..16].copy_from_slice(&v.to_le_bytes());
    }

    // --- Checksum ---

    /// CRC32 of the page data, excluding the magic bytes (which hold the checksum
    /// space). We compute over bytes [4..PAGE_SIZE] and store in a reserved spot.
    /// For simplicity, we use the last 4 bytes of the page as the checksum.
    pub fn update_checksum(&mut self) {
        let crc = crc32fast::hash(&self.data[..PAGE_SIZE - 4]);
        self.data[PAGE_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
    }

    pub fn verify_checksum(&self) -> bool {
        let stored = u32::from_le_bytes(self.data[PAGE_SIZE - 4..].try_into().unwrap());
        let computed = crc32fast::hash(&self.data[..PAGE_SIZE - 4]);
        stored == computed
    }

    // --- Row offset array (leaf pages) ---

    /// Get the byte offset within the page where row `i` starts.
    pub fn row_offset(&self, i: u16) -> u16 {
        let base = PAGE_HEADER_SIZE + (i as usize) * 2;
        u16::from_le_bytes([self.data[base], self.data[base + 1]])
    }

    /// Set the byte offset for row `i`.
    pub fn set_row_offset(&mut self, i: u16, offset: u16) {
        let base = PAGE_HEADER_SIZE + (i as usize) * 2;
        self.data[base..base + 2].copy_from_slice(&offset.to_le_bytes());
    }

    /// Available free space in the leaf page (between offset array and row data).
    pub fn free_space(&self) -> usize {
        let offset_array_end = PAGE_HEADER_SIZE + (self.num_rows() as usize) * 2 + 2; // +2 for next slot
        let row_data_start = self.free_space_offset() as usize;
        if row_data_start <= offset_array_end {
            0
        } else {
            // Actually rows grow from end, offsets grow from start.
            // free_space_offset tracks where the next row offset slot ends.
            // Row data packs from the END of the page (before checksum).
            // Let's simplify: rows are appended after the offset array.
            PAGE_SIZE - 4 - row_data_start // -4 for checksum
        }
    }
}

impl Default for Page {
    fn default() -> Self {
        Self { data: [0; PAGE_SIZE] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leaf_page_creation() {
        let page = Page::new_leaf();
        assert!(page.is_leaf());
        assert!(!page.is_branch());
        assert_eq!(page.num_rows(), 0);
        assert!(page.verify_checksum());
    }

    #[test]
    fn test_branch_page_creation() {
        let page = Page::new_branch();
        assert!(page.is_branch());
        assert!(!page.is_leaf());
        assert!(page.verify_checksum());
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let mut page = Page::new_leaf();
        page.update_checksum();
        assert!(page.verify_checksum());
        // Corrupt one byte
        page.data[100] ^= 0xFF;
        assert!(!page.verify_checksum());
    }

    #[test]
    fn test_from_bytes_validates() {
        let page = Page::new_leaf();
        let result = Page::from_bytes(page.data);
        assert!(result.is_ok());

        let mut bad = page.data;
        bad[0] = 0xFF; // corrupt magic
        let result = Page::from_bytes(bad);
        assert!(result.is_err());
    }

    #[test]
    fn test_row_offsets() {
        let mut page = Page::new_leaf();
        page.set_row_offset(0, 500);
        page.set_row_offset(1, 600);
        assert_eq!(page.row_offset(0), 500);
        assert_eq!(page.row_offset(1), 600);
    }
}
