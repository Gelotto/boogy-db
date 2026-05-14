use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{BoogyError, Result};
use crate::page::PAGE_SIZE;

const WAL_ENTRY_SIZE: usize = 8 + 4 + 4 + PAGE_SIZE + 4; // seq + table_id + page_no + page_data + checksum
const WAL_HEADER_SIZE: usize = 16; // magic(4) + version(4) + entry_count(4) + reserved(4)
const WAL_MAGIC: u32 = 0xB00D_0A01;

/// Write-Ahead Log for crash recovery.
pub struct Wal {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
    next_sequence: u64,
    entry_count: u32,
}

impl Wal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let file_len = file.metadata()?.len();

        if file_len == 0 {
            // New WAL — write header
            let mut wal = Self {
                file,
                path,
                next_sequence: 1,
                entry_count: 0,
            };
            wal.write_header()?;
            Ok(wal)
        } else {
            // Existing WAL — read header
            let mut f = file;
            let mut header = [0u8; WAL_HEADER_SIZE];
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut header)?;

            let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
            if magic != WAL_MAGIC {
                return Err(BoogyError::Corruption("bad WAL magic".into()));
            }

            let entry_count = u32::from_le_bytes(header[8..12].try_into().unwrap());
            let next_seq = entry_count as u64 + 1;

            Ok(Self {
                file: f,
                path,
                next_sequence: next_seq,
                entry_count,
            })
        }
    }

    /// Append an after-image (redo-log entry) of a page to the WAL.
    pub fn append_page_image(
        &mut self,
        table_id: u32,
        page_no: u32,
        page_data: &[u8; PAGE_SIZE],
    ) -> Result<u64> {
        if self.entry_count == u32::MAX {
            return Err(BoogyError::Corruption("WAL entry count overflow".into()));
        }

        let seq = self.next_sequence;
        self.next_sequence += 1;

        let mut entry = Vec::with_capacity(WAL_ENTRY_SIZE);
        entry.extend_from_slice(&seq.to_le_bytes());
        entry.extend_from_slice(&table_id.to_le_bytes());
        entry.extend_from_slice(&page_no.to_le_bytes());
        entry.extend_from_slice(page_data);
        let checksum = crc32fast::hash(&entry);
        entry.extend_from_slice(&checksum.to_le_bytes());

        let offset = WAL_HEADER_SIZE as u64 + self.entry_count as u64 * WAL_ENTRY_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&entry)?;

        self.entry_count += 1;
        self.update_entry_count()?;

        Ok(seq)
    }

    /// Fsync the WAL file.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Read all WAL entries (for recovery/replay).
    pub fn read_entries(&mut self) -> Result<Vec<WalEntry>> {
        let mut entries = Vec::with_capacity(self.entry_count as usize);
        for i in 0..self.entry_count {
            let offset = WAL_HEADER_SIZE as u64 + i as u64 * WAL_ENTRY_SIZE as u64;
            self.file.seek(SeekFrom::Start(offset))?;

            let mut buf = vec![0u8; WAL_ENTRY_SIZE];
            self.file.read_exact(&mut buf)?;

            let seq = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            let table_id = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            let page_no = u32::from_le_bytes(buf[12..16].try_into().unwrap());
            let mut page_data = [0u8; PAGE_SIZE];
            page_data.copy_from_slice(&buf[16..16 + PAGE_SIZE]);
            let stored_checksum = u32::from_le_bytes(buf[16 + PAGE_SIZE..].try_into().unwrap());

            // Verify checksum
            let computed = crc32fast::hash(&buf[..16 + PAGE_SIZE]);
            if stored_checksum != computed {
                return Err(BoogyError::Corruption(format!(
                    "WAL entry {seq} checksum mismatch"
                )));
            }

            entries.push(WalEntry {
                sequence: seq,
                table_id,
                page_no,
                page_data,
            });
        }
        Ok(entries)
    }

    /// Truncate the WAL (called after checkpoint).
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(WAL_HEADER_SIZE as u64)?;
        self.entry_count = 0;
        self.next_sequence = 1;
        self.update_entry_count()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Current sequence number (for MVCC snapshot tracking).
    pub fn current_sequence(&self) -> u64 {
        self.next_sequence - 1
    }

    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    fn write_header(&mut self) -> Result<()> {
        let mut header = [0u8; WAL_HEADER_SIZE];
        header[0..4].copy_from_slice(&WAL_MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&1u32.to_le_bytes()); // version
        header[8..12].copy_from_slice(&0u32.to_le_bytes()); // entry_count
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        Ok(())
    }

    fn update_entry_count(&mut self) -> Result<()> {
        self.file.seek(SeekFrom::Start(8))?;
        self.file.write_all(&self.entry_count.to_le_bytes())?;
        Ok(())
    }
}

pub struct WalEntry {
    pub sequence: u64,
    pub table_id: u32,
    pub page_no: u32,
    pub page_data: [u8; PAGE_SIZE],
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_wal_append_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();

        let page = [0xAB; PAGE_SIZE];
        wal.append_page_image(1, 5, &page).unwrap();
        wal.append_page_image(2, 10, &[0xCD; PAGE_SIZE]).unwrap();
        wal.sync().unwrap();

        let entries = wal.read_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].table_id, 1);
        assert_eq!(entries[0].page_no, 5);
        assert_eq!(entries[0].page_data[0], 0xAB);
        assert_eq!(entries[1].table_id, 2);
        assert_eq!(entries[1].page_no, 10);
    }

    #[test]
    fn test_wal_truncate() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();

        wal.append_page_image(1, 0, &[0; PAGE_SIZE]).unwrap();
        assert_eq!(wal.entry_count(), 1);

        wal.truncate().unwrap();
        assert_eq!(wal.entry_count(), 0);

        let entries = wal.read_entries().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_wal_persist_across_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append_page_image(1, 0, &[0x42; PAGE_SIZE]).unwrap();
            wal.sync().unwrap();
        }

        {
            let mut wal = Wal::open(&path).unwrap();
            assert_eq!(wal.entry_count(), 1);
            let entries = wal.read_entries().unwrap();
            assert_eq!(entries[0].page_data[0], 0x42);
        }
    }

    #[test]
    fn test_wal_detects_corruption() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append_page_image(1, 0, &[0; PAGE_SIZE]).unwrap();
            wal.sync().unwrap();
        }

        // Corrupt the entry
        {
            let mut f = OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64 + 20)).unwrap();
            f.write_all(&[0xFF]).unwrap();
        }

        {
            let mut wal = Wal::open(&path).unwrap();
            assert!(wal.read_entries().is_err());
        }
    }
}
