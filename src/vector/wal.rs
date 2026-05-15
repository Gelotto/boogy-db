use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::crypto::Cipher;
use crate::error::Result;

/// A single entry in the vector WAL.
#[derive(Debug, PartialEq)]
pub enum WalEntry {
    InsertVector { node_id: u32, layer: u32, vector: Vec<f32> },
    SetNeighbors { node_id: u32, layer: u32, neighbors: Vec<u32> },
    DeleteNode { node_id: u32 },
    UpdateHeader { entry_point: u32, node_count: u32, max_layer: u32 },
    Commit,
}

const TAG_INSERT_VECTOR: u8 = 1;
const TAG_SET_NEIGHBORS: u8 = 2;
const TAG_DELETE_NODE: u8 = 3;
const TAG_UPDATE_HEADER: u8 = 4;
const TAG_COMMIT: u8 = 255;

impl WalEntry {
    /// Encode this entry into bytes (tag byte + fields, all little-endian).
    fn encode(&self) -> Vec<u8> {
        match self {
            WalEntry::InsertVector { node_id, layer, vector } => {
                let mut buf = Vec::with_capacity(1 + 4 + 4 + 4 + vector.len() * 4);
                buf.push(TAG_INSERT_VECTOR);
                buf.extend_from_slice(&node_id.to_le_bytes());
                buf.extend_from_slice(&layer.to_le_bytes());
                buf.extend_from_slice(&(vector.len() as u32).to_le_bytes());
                for &f in vector {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
                buf
            }
            WalEntry::SetNeighbors { node_id, layer, neighbors } => {
                let mut buf = Vec::with_capacity(1 + 4 + 4 + 4 + neighbors.len() * 4);
                buf.push(TAG_SET_NEIGHBORS);
                buf.extend_from_slice(&node_id.to_le_bytes());
                buf.extend_from_slice(&layer.to_le_bytes());
                buf.extend_from_slice(&(neighbors.len() as u32).to_le_bytes());
                for &n in neighbors {
                    buf.extend_from_slice(&n.to_le_bytes());
                }
                buf
            }
            WalEntry::DeleteNode { node_id } => {
                let mut buf = Vec::with_capacity(1 + 4);
                buf.push(TAG_DELETE_NODE);
                buf.extend_from_slice(&node_id.to_le_bytes());
                buf
            }
            WalEntry::UpdateHeader { entry_point, node_count, max_layer } => {
                let mut buf = Vec::with_capacity(1 + 4 + 4 + 4);
                buf.push(TAG_UPDATE_HEADER);
                buf.extend_from_slice(&entry_point.to_le_bytes());
                buf.extend_from_slice(&node_count.to_le_bytes());
                buf.extend_from_slice(&max_layer.to_le_bytes());
                buf
            }
            WalEntry::Commit => vec![TAG_COMMIT],
        }
    }

    /// Decode one entry from the reader. Returns None on EOF.
    fn decode(reader: &mut BufReader<&File>) -> std::io::Result<Option<WalEntry>> {
        let mut tag_buf = [0u8; 1];
        match reader.read_exact(&mut tag_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }

        let entry = match tag_buf[0] {
            TAG_INSERT_VECTOR => {
                let node_id = read_u32(reader)?;
                let layer = read_u32(reader)?;
                let vec_len = read_u32(reader)? as usize;
                let mut vector = Vec::with_capacity(vec_len);
                for _ in 0..vec_len {
                    let mut fb = [0u8; 4];
                    reader.read_exact(&mut fb)?;
                    vector.push(f32::from_le_bytes(fb));
                }
                WalEntry::InsertVector { node_id, layer, vector }
            }
            TAG_SET_NEIGHBORS => {
                let node_id = read_u32(reader)?;
                let layer = read_u32(reader)?;
                let count = read_u32(reader)? as usize;
                let mut neighbors = Vec::with_capacity(count);
                for _ in 0..count {
                    neighbors.push(read_u32(reader)?);
                }
                WalEntry::SetNeighbors { node_id, layer, neighbors }
            }
            TAG_DELETE_NODE => {
                let node_id = read_u32(reader)?;
                WalEntry::DeleteNode { node_id }
            }
            TAG_UPDATE_HEADER => {
                let entry_point = read_u32(reader)?;
                let node_count = read_u32(reader)?;
                let max_layer = read_u32(reader)?;
                WalEntry::UpdateHeader { entry_point, node_count, max_layer }
            }
            TAG_COMMIT => WalEntry::Commit,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unknown WAL entry tag",
                ));
            }
        };

        Ok(Some(entry))
    }
}

fn read_u32(reader: &mut BufReader<&File>) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u32_from<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Decode a WalEntry from a byte slice (used for decrypted WAL entries).
fn decode_from_slice(data: &[u8]) -> crate::error::Result<Option<WalEntry>> {
    use std::io::Cursor;
    if data.is_empty() {
        return Ok(None);
    }
    let mut cursor = Cursor::new(data);
    let mut tag_buf = [0u8; 1];
    match cursor.read_exact(&mut tag_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let entry = match tag_buf[0] {
        TAG_INSERT_VECTOR => {
            let node_id = read_u32_from(&mut cursor)?;
            let layer = read_u32_from(&mut cursor)?;
            let vec_len = read_u32_from(&mut cursor)? as usize;
            let mut vector = Vec::with_capacity(vec_len);
            for _ in 0..vec_len {
                let mut fb = [0u8; 4];
                cursor.read_exact(&mut fb)?;
                vector.push(f32::from_le_bytes(fb));
            }
            WalEntry::InsertVector { node_id, layer, vector }
        }
        TAG_SET_NEIGHBORS => {
            let node_id = read_u32_from(&mut cursor)?;
            let layer = read_u32_from(&mut cursor)?;
            let count = read_u32_from(&mut cursor)? as usize;
            let mut neighbors = Vec::with_capacity(count);
            for _ in 0..count {
                neighbors.push(read_u32_from(&mut cursor)?);
            }
            WalEntry::SetNeighbors { node_id, layer, neighbors }
        }
        TAG_DELETE_NODE => {
            let node_id = read_u32_from(&mut cursor)?;
            WalEntry::DeleteNode { node_id }
        }
        TAG_UPDATE_HEADER => {
            let entry_point = read_u32_from(&mut cursor)?;
            let node_count = read_u32_from(&mut cursor)?;
            let max_layer = read_u32_from(&mut cursor)?;
            WalEntry::UpdateHeader { entry_point, node_count, max_layer }
        }
        TAG_COMMIT => WalEntry::Commit,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown WAL entry tag",
            ).into());
        }
    };

    Ok(Some(entry))
}

/// Write-ahead log for vector mutations.
///
/// Each entry is written as `[len: u32 LE][encoded_data]`.
/// Transactions are committed by appending a `Commit` entry.
/// On recovery, only complete transactions (those ending with a Commit) are returned.
pub struct VectorWal {
    pub path: PathBuf,
    pub file: File,
    pub cipher: Option<Cipher>,
}

impl VectorWal {
    /// Open or create the WAL file at the given path.
    pub fn open(path: impl AsRef<Path>, key: Option<&[u8; 32]>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let cipher = key.map(Cipher::new);
        Ok(Self { path, file, cipher })
    }

    /// Append a single entry as `[len: u32][payload]`.
    /// When encrypted, payload is `[nonce:12][ciphertext+tag]` of the encoded data.
    /// When unencrypted, payload is the raw encoded data.
    pub fn append(&mut self, entry: &WalEntry) -> Result<()> {
        let data = entry.encode();
        let payload = if let Some(ref cipher) = self.cipher {
            cipher.encrypt_bytes(&data)?
        } else {
            data
        };
        let len = payload.len() as u32;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(&payload)?;
        Ok(())
    }

    /// Append all entries then a `Commit` marker. Optionally fsync.
    pub fn append_committed(&mut self, entries: &[WalEntry], fsync: bool) -> Result<()> {
        for entry in entries {
            self.append(entry)?;
        }
        self.append(&WalEntry::Commit)?;
        if fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    /// Read all fully-committed transactions from the WAL.
    ///
    /// Seeks to the start, scans all entries, and returns a `Vec<Vec<WalEntry>>` where
    /// each inner `Vec` is one committed transaction (without the trailing Commit entry).
    /// Incomplete transactions (entries after the last Commit) are silently discarded.
    pub fn read_committed(&mut self) -> Result<Vec<Vec<WalEntry>>> {
        self.file.seek(SeekFrom::Start(0))?;

        let mut transactions: Vec<Vec<WalEntry>> = Vec::new();
        let mut current_tx: Vec<WalEntry> = Vec::new();

        if self.cipher.is_some() {
            // Encrypted path: read each [len:4][encrypted_payload] and decrypt
            // before decoding.
            let mut reader = BufReader::new(&self.file);
            loop {
                let mut len_buf = [0u8; 4];
                match reader.read_exact(&mut len_buf) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
                let len = u32::from_le_bytes(len_buf) as usize;
                let mut encrypted = vec![0u8; len];
                match reader.read_exact(&mut encrypted) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
                let plaintext = self.cipher.as_ref().unwrap().decrypt_bytes(&encrypted)?;
                let entry = decode_from_slice(&plaintext)?;
                match entry {
                    Some(WalEntry::Commit) => {
                        transactions.push(std::mem::take(&mut current_tx));
                    }
                    Some(e) => {
                        current_tx.push(e);
                    }
                    None => break,
                }
            }
        } else {
            // Unencrypted path: original logic.
            let mut reader = BufReader::new(&self.file);
            loop {
                let mut len_buf = [0u8; 4];
                match reader.read_exact(&mut len_buf) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
                let _len = u32::from_le_bytes(len_buf);

                match WalEntry::decode(&mut reader)? {
                    None => break,
                    Some(WalEntry::Commit) => {
                        transactions.push(std::mem::take(&mut current_tx));
                    }
                    Some(entry) => {
                        current_tx.push(entry);
                    }
                }
            }
        }

        // Discard any incomplete trailing transaction.
        Ok(transactions)
    }

    /// Set the file length to 0 and seek to the start (clears the WAL).
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    /// Returns true if the WAL file has no content.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.file.metadata()?.len() == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_append_and_read_committed() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = VectorWal::open(tmp.path(), None).unwrap();

        let entries = vec![
            WalEntry::InsertVector { node_id: 1, layer: 0, vector: vec![0.1, 0.2, 0.3] },
            WalEntry::SetNeighbors { node_id: 1, layer: 0, neighbors: vec![2, 3, 4] },
            WalEntry::UpdateHeader { entry_point: 1, node_count: 1, max_layer: 0 },
        ];
        wal.append_committed(&entries, false).unwrap();

        let txns = wal.read_committed().unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].len(), 3);

        assert_eq!(
            txns[0][0],
            WalEntry::InsertVector { node_id: 1, layer: 0, vector: vec![0.1, 0.2, 0.3] }
        );
        assert_eq!(
            txns[0][1],
            WalEntry::SetNeighbors { node_id: 1, layer: 0, neighbors: vec![2, 3, 4] }
        );
        assert_eq!(
            txns[0][2],
            WalEntry::UpdateHeader { entry_point: 1, node_count: 1, max_layer: 0 }
        );
    }

    #[test]
    fn test_uncommitted_transaction_discarded() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = VectorWal::open(tmp.path(), None).unwrap();

        // First transaction: committed.
        wal.append_committed(
            &[WalEntry::DeleteNode { node_id: 42 }],
            false,
        )
        .unwrap();

        // Second transaction: not committed (no Commit appended).
        wal.append(&WalEntry::InsertVector {
            node_id: 99,
            layer: 0,
            vector: vec![1.0, 2.0],
        })
        .unwrap();

        let txns = wal.read_committed().unwrap();
        assert_eq!(txns.len(), 1, "only the committed transaction should be returned");
        assert_eq!(txns[0].len(), 1);
        assert_eq!(txns[0][0], WalEntry::DeleteNode { node_id: 42 });
    }

    #[test]
    fn test_truncate() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = VectorWal::open(tmp.path(), None).unwrap();

        wal.append_committed(
            &[WalEntry::DeleteNode { node_id: 1 }],
            false,
        )
        .unwrap();
        assert!(!wal.is_empty().unwrap());

        wal.truncate().unwrap();
        assert!(wal.is_empty().unwrap());

        let txns = wal.read_committed().unwrap();
        assert!(txns.is_empty());
    }

    #[test]
    fn test_empty_wal() {
        let tmp = NamedTempFile::new().unwrap();
        let wal = VectorWal::open(tmp.path(), None).unwrap();

        assert!(wal.is_empty().unwrap());

        // read_committed on a mutable borrow — need mut
        let mut wal = wal;
        let txns = wal.read_committed().unwrap();
        assert!(txns.is_empty());
    }

    #[test]
    fn test_multiple_transactions() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = VectorWal::open(tmp.path(), None).unwrap();

        // Transaction 1.
        wal.append_committed(
            &[
                WalEntry::InsertVector { node_id: 10, layer: 0, vector: vec![1.0, 0.0] },
                WalEntry::UpdateHeader { entry_point: 10, node_count: 1, max_layer: 0 },
            ],
            false,
        )
        .unwrap();

        // Transaction 2.
        wal.append_committed(
            &[
                WalEntry::InsertVector { node_id: 20, layer: 1, vector: vec![0.0, 1.0] },
                WalEntry::SetNeighbors { node_id: 20, layer: 0, neighbors: vec![10] },
                WalEntry::UpdateHeader { entry_point: 10, node_count: 2, max_layer: 1 },
            ],
            false,
        )
        .unwrap();

        let txns = wal.read_committed().unwrap();
        assert_eq!(txns.len(), 2);

        assert_eq!(txns[0].len(), 2);
        assert_eq!(
            txns[0][0],
            WalEntry::InsertVector { node_id: 10, layer: 0, vector: vec![1.0, 0.0] }
        );
        assert_eq!(
            txns[0][1],
            WalEntry::UpdateHeader { entry_point: 10, node_count: 1, max_layer: 0 }
        );

        assert_eq!(txns[1].len(), 3);
        assert_eq!(
            txns[1][0],
            WalEntry::InsertVector { node_id: 20, layer: 1, vector: vec![0.0, 1.0] }
        );
        assert_eq!(
            txns[1][1],
            WalEntry::SetNeighbors { node_id: 20, layer: 0, neighbors: vec![10] }
        );
        assert_eq!(
            txns[1][2],
            WalEntry::UpdateHeader { entry_point: 10, node_count: 2, max_layer: 1 }
        );
    }
}
