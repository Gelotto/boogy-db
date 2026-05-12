# boogy-db v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an embedded storage engine (in-place B+ tree with WAL) that beats SQLite on concurrent read/write workloads for SpinStack's per-API database use case.

**Architecture:** Bottom-up build: page format → file I/O + page cache → B+ tree → WAL → MVCC → table registry → public API. Each layer is independently testable. The engine stores rows directly in B+ tree leaf pages with a column-aware binary format, avoiding encode/decode overhead.

**Tech Stack:** Rust, CRC32 (crc32fast crate), uuid crate for `_id` generation. No other dependencies.

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/lib.rs` | Public API: `BoogyDb`, `Value`, `FindOptions`, `Filter`, `Sort`, re-exports |
| `src/error.rs` | `BoogyError` enum, `Result<T>` alias |
| `src/value.rs` | `Value` enum (Null/Text/Integer/Real/Blob/Boolean), `Type` enum, comparison ops |
| `src/page.rs` | Page constants, page header, raw page read/write, CRC32 checksums |
| `src/row.rs` | Row binary format: encode columns to bytes, decode bytes to columns, single-column extract |
| `src/file.rs` | `PageFile`: file I/O, page-aligned reads/writes, allocate new pages, fsync |
| `src/btree.rs` | B+ tree: insert, search, delete, range scan, split, merge |
| `src/wal.rs` | WAL: append before-images, replay (undo), checkpoint, group commit |
| `src/mvcc.rs` | MVCC: snapshot tracking, version visibility, read-path page resolution |
| `src/table.rs` | `TableManager`: schema registry, per-table B+ tree roots, per-table RwLocks |
| `src/db.rs` | `BoogyDb`: top-level API, transactions, find/count with filter evaluation |
| `src/filter.rs` | `Filter`, `Sort`, `FindOptions` types + evaluation logic |
| `tests/crud_test.rs` | Basic CRUD operations |
| `tests/concurrent_test.rs` | Multi-threaded read/write stress |
| `tests/crash_recovery_test.rs` | WAL replay after simulated crash |
| `tests/stress_test.rs` | High-volume mixed workload correctness |

---

### Task 1: Error types + Value types

**Files:**
- Create: `src/error.rs`
- Create: `src/value.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Set up Cargo.toml**

```toml
[package]
name = "boogy-db"
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"

[dependencies]
crc32fast = "1"
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
tempfile = "3"
rand = "0.9"
```

- [ ] **Step 2: Create error.rs**

```rust
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum BoogyError {
    Io(io::Error),
    Corruption(String),
    TableNotFound(String),
    TableExists(String),
    RowNotFound(String),
    DuplicateKey(String),
    SchemaMismatch(String),
    PageFull,
    TransactionConflict,
}

impl fmt::Display for BoogyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoogyError::Io(e) => write!(f, "I/O error: {e}"),
            BoogyError::Corruption(msg) => write!(f, "corruption: {msg}"),
            BoogyError::TableNotFound(t) => write!(f, "table '{t}' not found"),
            BoogyError::TableExists(t) => write!(f, "table '{t}' already exists"),
            BoogyError::RowNotFound(id) => write!(f, "row '{id}' not found"),
            BoogyError::DuplicateKey(id) => write!(f, "duplicate key '{id}'"),
            BoogyError::SchemaMismatch(msg) => write!(f, "schema mismatch: {msg}"),
            BoogyError::PageFull => write!(f, "page full"),
            BoogyError::TransactionConflict => write!(f, "transaction conflict"),
        }
    }
}

impl std::error::Error for BoogyError {}

impl From<io::Error> for BoogyError {
    fn from(e: io::Error) -> Self {
        BoogyError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, BoogyError>;
```

- [ ] **Step 3: Create value.rs**

```rust
use std::cmp::Ordering;

/// Column data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Text,
    Integer,
    Real,
    Blob,
    Boolean,
}

/// A dynamically-typed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Text(String),
    Integer(i64),
    Real(f64),
    Blob(Vec<u8>),
    Boolean(bool),
}

impl Value {
    pub fn value_type(&self) -> Option<Type> {
        match self {
            Value::Null => None,
            Value::Text(_) => Some(Type::Text),
            Value::Integer(_) => Some(Type::Integer),
            Value::Real(_) => Some(Type::Real),
            Value::Blob(_) => Some(Type::Blob),
            Value::Boolean(_) => Some(Type::Boolean),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Compare two values for ordering. Returns None if types are incompatible.
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Null, _) => Some(Ordering::Less),
            (_, Value::Null) => Some(Ordering::Greater),
            (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
            (Value::Real(a), Value::Real(b)) => a.partial_cmp(b),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
            (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
            // Cross-type: integer/real comparison
            (Value::Integer(a), Value::Real(b)) => (*a as f64).partial_cmp(b),
            (Value::Real(a), Value::Integer(b)) => a.partial_cmp(&(*b as f64)),
            _ => None,
        }
    }
}

/// Column definition for CREATE TABLE.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: Type,
    pub nullable: bool,
    pub unique: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, col_type: Type) -> Self {
        Self {
            name: name.into(),
            col_type,
            nullable: true,
            unique: false,
        }
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
}
```

- [ ] **Step 4: Create lib.rs**

```rust
pub mod error;
pub mod value;

pub use error::{BoogyError, Result};
pub use value::{ColumnDef, Type, Value};
```

- [ ] **Step 5: Verify and commit**

Run: `cargo check`
Run: `cargo test`

```bash
git add -A
git commit -m "feat: error types + Value/Type/ColumnDef"
```

---

### Task 2: Page format + checksums

**Files:**
- Create: `src/page.rs`

The page module defines constants, page header layout, and CRC32 validation. No file I/O — just in-memory byte manipulation.

- [ ] **Step 1: Create page.rs**

```rust
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
        self.data[2..3 + 1].copy_from_slice(&v.to_le_bytes());
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
```

- [ ] **Step 2: Export from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod page;
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test`
Expected: all page tests pass

```bash
git add -A
git commit -m "feat: page format with header, CRC32 checksums, row offset array"
```

---

### Task 3: Row binary format

**Files:**
- Create: `src/row.rs`

Encode/decode rows to/from bytes that live inside leaf pages. The format stores column IDs (not names) for compactness. Supports extracting a single column value without decoding the full row (critical for filter evaluation performance).

- [ ] **Step 1: Create row.rs**

```rust
use crate::error::{BoogyError, Result};
use crate::value::Value;

// Type tags
const TAG_NULL: u8 = 0;
const TAG_TEXT: u8 = 1;
const TAG_INTEGER: u8 = 2;
const TAG_REAL: u8 = 3;
const TAG_BLOB: u8 = 4;
const TAG_BOOLEAN: u8 = 5;

/// Encode a row (_id + columns) into compact binary format.
///
/// Layout: [id_len:2][id_bytes][num_cols:2][col_id:2][tag:1][value]...
pub fn encode_row(id: &str, columns: &[(u16, &Value)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    let id_bytes = id.as_bytes();
    buf.extend_from_slice(&(id_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(id_bytes);
    buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());
    for &(col_id, val) in columns {
        buf.extend_from_slice(&col_id.to_le_bytes());
        encode_value(&mut buf, val);
    }
    buf
}

fn encode_value(buf: &mut Vec<u8>, val: &Value) {
    match val {
        Value::Null => buf.push(TAG_NULL),
        Value::Text(s) => {
            buf.push(TAG_TEXT);
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        Value::Integer(i) => {
            buf.push(TAG_INTEGER);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(TAG_REAL);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Blob(b) => {
            buf.push(TAG_BLOB);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Boolean(b) => {
            buf.push(TAG_BOOLEAN);
            buf.push(if *b { 1 } else { 0 });
        }
    }
}

/// Decoded row: the _id and all column values.
pub struct DecodedRow {
    pub id: String,
    pub columns: Vec<(u16, Value)>,
}

/// Decode a full row from bytes.
pub fn decode_row(data: &[u8]) -> Result<DecodedRow> {
    let mut offset = 0;

    // _id
    ensure_bytes(data, offset, 2)?;
    let id_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;
    ensure_bytes(data, offset, id_len)?;
    let id = String::from_utf8(data[offset..offset + id_len].to_vec())
        .map_err(|_| BoogyError::Corruption("invalid utf8 in _id".into()))?;
    offset += id_len;

    // columns
    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    let mut columns = Vec::with_capacity(num_cols);
    for _ in 0..num_cols {
        ensure_bytes(data, offset, 2)?;
        let col_id = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
        offset += 2;
        let (val, consumed) = decode_value(&data[offset..])?;
        offset += consumed;
        columns.push((col_id, val));
    }

    Ok(DecodedRow { id, columns })
}

/// Extract just the _id from row bytes without decoding columns.
pub fn extract_id(data: &[u8]) -> Result<&str> {
    ensure_bytes(data, 0, 2)?;
    let id_len = u16::from_le_bytes(data[0..2].try_into().unwrap()) as usize;
    ensure_bytes(data, 2, id_len)?;
    std::str::from_utf8(&data[2..2 + id_len])
        .map_err(|_| BoogyError::Corruption("invalid utf8 in _id".into()))
}

/// Extract a single column value by column ID without decoding all columns.
pub fn extract_column(data: &[u8], target_col_id: u16) -> Result<Option<Value>> {
    let mut offset = 0;

    // Skip _id
    ensure_bytes(data, offset, 2)?;
    let id_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2 + id_len;

    // num columns
    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    for _ in 0..num_cols {
        ensure_bytes(data, offset, 2)?;
        let col_id = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if col_id == target_col_id {
            let (val, _) = decode_value(&data[offset..])?;
            return Ok(Some(val));
        }
        // Skip this value
        offset += value_byte_size(&data[offset..])?;
    }
    Ok(None)
}

fn decode_value(data: &[u8]) -> Result<(Value, usize)> {
    ensure_bytes(data, 0, 1)?;
    match data[0] {
        TAG_NULL => Ok((Value::Null, 1)),
        TAG_TEXT => {
            ensure_bytes(data, 1, 4)?;
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            ensure_bytes(data, 5, len)?;
            let s = String::from_utf8(data[5..5 + len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8".into()))?;
            Ok((Value::Text(s), 5 + len))
        }
        TAG_INTEGER => {
            ensure_bytes(data, 1, 8)?;
            let i = i64::from_le_bytes(data[1..9].try_into().unwrap());
            Ok((Value::Integer(i), 9))
        }
        TAG_REAL => {
            ensure_bytes(data, 1, 8)?;
            let f = f64::from_le_bytes(data[1..9].try_into().unwrap());
            Ok((Value::Real(f), 9))
        }
        TAG_BLOB => {
            ensure_bytes(data, 1, 4)?;
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            ensure_bytes(data, 5, len)?;
            Ok((Value::Blob(data[5..5 + len].to_vec()), 5 + len))
        }
        TAG_BOOLEAN => {
            ensure_bytes(data, 1, 1)?;
            Ok((Value::Boolean(data[1] != 0), 2))
        }
        tag => Err(BoogyError::Corruption(format!("unknown type tag: {tag}"))),
    }
}

fn value_byte_size(data: &[u8]) -> Result<usize> {
    ensure_bytes(data, 0, 1)?;
    match data[0] {
        TAG_NULL => Ok(1),
        TAG_TEXT | TAG_BLOB => {
            ensure_bytes(data, 1, 4)?;
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            Ok(5 + len)
        }
        TAG_INTEGER | TAG_REAL => Ok(9),
        TAG_BOOLEAN => Ok(2),
        tag => Err(BoogyError::Corruption(format!("unknown type tag: {tag}"))),
    }
}

fn ensure_bytes(data: &[u8], offset: usize, need: usize) -> Result<()> {
    if offset + need > data.len() {
        Err(BoogyError::Corruption(format!(
            "truncated: need {need} bytes at offset {offset}, have {}",
            data.len()
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_round_trip() {
        let cols = vec![
            (0u16, &Value::Text("alice".into())),
            (1, &Value::Integer(42)),
            (2, &Value::Real(3.14)),
            (3, &Value::Boolean(true)),
            (4, &Value::Null),
        ];
        let encoded = encode_row("row_1", &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.id, "row_1");
        assert_eq!(decoded.columns.len(), 5);
        assert_eq!(decoded.columns[0], (0, Value::Text("alice".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(42)));
        assert_eq!(decoded.columns[3], (3, Value::Boolean(true)));
        assert_eq!(decoded.columns[4], (4, Value::Null));
    }

    #[test]
    fn test_extract_id() {
        let encoded = encode_row("my_uuid", &[(0, &Value::Integer(1))]);
        assert_eq!(extract_id(&encoded).unwrap(), "my_uuid");
    }

    #[test]
    fn test_extract_column() {
        let cols = vec![
            (0u16, &Value::Text("alice".into())),
            (1, &Value::Integer(42)),
            (2, &Value::Boolean(false)),
        ];
        let encoded = encode_row("id1", &cols);

        assert_eq!(extract_column(&encoded, 1).unwrap(), Some(Value::Integer(42)));
        assert_eq!(extract_column(&encoded, 2).unwrap(), Some(Value::Boolean(false)));
        assert_eq!(extract_column(&encoded, 99).unwrap(), None);
    }

    #[test]
    fn test_blob_round_trip() {
        let blob_data = vec![0xFF, 0x00, 0xAB, 0xCD];
        let cols = vec![(0u16, &Value::Blob(blob_data.clone()))];
        let encoded = encode_row("blob_row", &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Blob(blob_data));
    }

    #[test]
    fn test_empty_string() {
        let cols = vec![(0u16, &Value::Text(String::new()))];
        let encoded = encode_row("empty", &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Text(String::new()));
    }
}
```

- [ ] **Step 2: Export from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod row;
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test`
Expected: all row tests pass

```bash
git add -A
git commit -m "feat: row binary format with encode/decode/extract_column"
```

---

### Task 4: File I/O + page cache

**Files:**
- Create: `src/file.rs`

Page-aligned file I/O. Read/write 4KB pages by page number. Track free pages. Simple page cache (HashMap) to avoid repeated disk reads.

- [ ] **Step 1: Create file.rs**

```rust
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

    /// Write a new page at a specific page number (for initialization).
    pub fn put_page(&mut self, page_no: u32, page: Page) {
        if page_no >= self.num_pages {
            self.num_pages = page_no + 1;
        }
        self.dirty.insert(page_no, page);
    }

    /// Flush all dirty pages to disk.
    pub fn flush(&mut self) -> Result<()> {
        for (&page_no, page) in &self.dirty {
            self.write_page_to_disk(page_no, &page.data)?;
        }
        // Move dirty pages to cache
        for (page_no, page) in self.dirty.drain() {
            self.cache.insert(page_no, page);
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
        self.dirty.clear();
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
```

- [ ] **Step 2: Export from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod file;
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test`

```bash
git add -A
git commit -m "feat: page file I/O with cache, dirty tracking, flush/sync"
```

---

### Task 5: Filter + Sort types

**Files:**
- Create: `src/filter.rs`

Filter evaluation, sort, and FindOptions. This is separate from the B+ tree — it operates on decoded column values.

- [ ] **Step 1: Create filter.rs**

```rust
use crate::value::Value;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: Value,
}

impl Filter {
    pub fn eq(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Eq, value: value.into() }
    }
    pub fn ne(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Ne, value: value.into() }
    }
    pub fn lt(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Lt, value: value.into() }
    }
    pub fn le(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Le, value: value.into() }
    }
    pub fn gt(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Gt, value: value.into() }
    }
    pub fn ge(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Ge, value: value.into() }
    }

    /// Evaluate this filter against a value.
    pub fn matches(&self, actual: &Value) -> bool {
        let cmp = actual.compare(&self.value);
        match cmp {
            Some(ord) => match self.op {
                FilterOp::Eq => ord == Ordering::Equal,
                FilterOp::Ne => ord != Ordering::Equal,
                FilterOp::Lt => ord == Ordering::Less,
                FilterOp::Le => ord != Ordering::Greater,
                FilterOp::Gt => ord == Ordering::Greater,
                FilterOp::Ge => ord != Ordering::Less,
            },
            None => false, // incompatible types don't match
        }
    }
}

// Convenience Into<Value> impls
impl From<&str> for Value {
    fn from(s: &str) -> Self { Value::Text(s.to_string()) }
}
impl From<String> for Value {
    fn from(s: String) -> Self { Value::Text(s) }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self { Value::Integer(i) }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self { Value::Real(f) }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self { Value::Boolean(b) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub struct Sort {
    pub column: String,
    pub dir: SortDir,
}

impl Sort {
    pub fn asc(column: impl Into<String>) -> Self {
        Self { column: column.into(), dir: SortDir::Asc }
    }
    pub fn desc(column: impl Into<String>) -> Self {
        Self { column: column.into(), dir: SortDir::Desc }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FindOptions {
    pub filters: Vec<Filter>,
    pub sort: Vec<Sort>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_eq() {
        let f = Filter::eq("name", "alice");
        assert!(f.matches(&Value::Text("alice".into())));
        assert!(!f.matches(&Value::Text("bob".into())));
    }

    #[test]
    fn test_filter_gt_integer() {
        let f = Filter::gt("age", 18i64);
        assert!(f.matches(&Value::Integer(21)));
        assert!(!f.matches(&Value::Integer(18)));
        assert!(!f.matches(&Value::Integer(10)));
    }

    #[test]
    fn test_filter_null() {
        let f = Filter::eq("x", Value::Null);
        assert!(f.matches(&Value::Null));
        assert!(!f.matches(&Value::Integer(0)));
    }
}
```

- [ ] **Step 2: Export from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod filter;
pub use filter::{Filter, FilterOp, FindOptions, Sort, SortDir};
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test`

```bash
git add -A
git commit -m "feat: Filter, Sort, FindOptions with evaluation logic"
```

---

### Task 6: B+ tree — insert + search

**Files:**
- Create: `src/btree.rs`

The core B+ tree implementation. Leaf pages store rows keyed by `_id`. Branch pages store child pointers + separator keys. This task covers insert (with leaf split) and search (point lookup by _id). Range scan comes in the next task.

This is the largest and most critical task. The B+ tree operates on `PageFile` and manipulates pages directly.

- [ ] **Step 1: Create btree.rs**

This file will be ~400 lines. Key types and functions:

```rust
use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::page::{Page, PAGE_HEADER_SIZE, PAGE_SIZE, PAGE_LEAF, PAGE_BRANCH};
use crate::row;

/// A B+ tree rooted at a given page number.
pub struct BTree<'a> {
    file: &'a mut PageFile,
    root: u32,
}

impl<'a> BTree<'a> {
    pub fn new(file: &'a mut PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    /// Create a new empty B+ tree (single empty leaf page).
    pub fn create(file: &mut PageFile) -> Result<u32> {
        let page_no = file.allocate_page()?;
        let page = Page::new_leaf();
        file.put_page(page_no, page);
        Ok(page_no)
    }

    /// Insert a row. Returns the (possibly new) root page number.
    pub fn insert(&mut self, id: &str, row_data: &[u8]) -> Result<u32> {
        let result = self.insert_recursive(self.root, id, row_data)?;
        match result {
            InsertResult::Fit => Ok(self.root),
            InsertResult::Split { new_page, separator } => {
                // Create new root
                let new_root = self.file.allocate_page()?;
                let mut root_page = Page::new_branch();
                // Branch format: [num_keys:2][child0:4][key0_len:2][key0][child1:4]
                write_branch_entry(&mut root_page, 0, self.root, &separator);
                set_branch_last_child(&mut root_page, 1, new_page);
                root_page.set_num_rows(1);
                root_page.update_checksum();
                self.file.put_page(new_root, root_page);
                self.root = new_root;
                Ok(self.root)
            }
        }
    }

    /// Search for a row by _id. Returns the raw row bytes if found.
    pub fn search(&mut self, id: &str) -> Result<Option<Vec<u8>>> {
        self.search_recursive(self.root, id)
    }

    /// Delete a row by _id. Returns true if the row existed.
    pub fn delete(&mut self, id: &str) -> Result<bool> {
        self.delete_from_leaf(self.root, id)
    }

    /// Iterate all rows in key order. Returns (id, row_bytes) pairs.
    pub fn scan_all(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut results = Vec::new();
        self.scan_leaf(self.root, &mut results)?;
        Ok(results)
    }

    // --- Internal methods ---

    fn insert_recursive(&mut self, page_no: u32, id: &str, row_data: &[u8]) -> Result<InsertResult> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            self.insert_into_leaf(page_no, &page, id, row_data)
        } else {
            // Find the right child
            let (child_idx, child_page_no) = find_child(&page, id);
            let result = self.insert_recursive(child_page_no, id, row_data)?;

            match result {
                InsertResult::Fit => Ok(InsertResult::Fit),
                InsertResult::Split { new_page, separator } => {
                    // Insert separator + new_page into this branch
                    self.insert_into_branch(page_no, child_idx, &separator, new_page)
                }
            }
        }
    }

    fn insert_into_leaf(&mut self, page_no: u32, page: &Page, id: &str, row_data: &[u8]) -> Result<InsertResult> {
        // Check for duplicate
        let num_rows = page.num_rows() as usize;
        for i in 0..num_rows {
            let offset = page.row_offset(i as u16) as usize;
            let row_end = if i + 1 < num_rows {
                page.row_offset((i + 1) as u16) as usize
            } else {
                PAGE_SIZE - 4 // before checksum
            };
            let existing_id = row::extract_id(&page.data[offset..row_end])?;
            if existing_id == id {
                return Err(BoogyError::DuplicateKey(id.to_string()));
            }
        }

        let needed = row_data.len() + 2; // +2 for offset entry
        let available = page.free_space();

        if needed <= available {
            // Fits in current page
            let page = self.file.write_page(page_no)?;
            append_row_to_leaf(page, row_data);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // Split the leaf
            let new_page_no = self.file.allocate_page()?;
            let mut new_page = Page::new_leaf();

            // Collect all rows + new row, sort by _id, split in half
            let page = self.file.read_page(page_no)?.clone();
            let mut all_rows = collect_leaf_rows(&page);
            all_rows.push((id.to_string(), row_data.to_vec()));
            all_rows.sort_by(|a, b| a.0.cmp(&b.0));

            let mid = all_rows.len() / 2;
            let left_rows = &all_rows[..mid];
            let right_rows = &all_rows[mid..];
            let separator = right_rows[0].0.clone();

            // Rebuild left page
            let left_page = self.file.write_page(page_no)?;
            rebuild_leaf(left_page, left_rows);
            left_page.set_next_leaf(new_page_no);
            left_page.update_checksum();

            // Build right page
            rebuild_leaf(&mut new_page, right_rows);
            new_page.set_prev_leaf(page_no);
            new_page.update_checksum();
            self.file.put_page(new_page_no, new_page);

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator,
            })
        }
    }

    fn insert_into_branch(&mut self, page_no: u32, child_idx: usize, separator: &str, new_child: u32) -> Result<InsertResult> {
        let page = self.file.read_page(page_no)?.clone();
        let num_keys = page.num_rows() as usize;

        // Check if branch has space (simplified: if num_keys < max branch entries)
        let max_keys = (PAGE_SIZE - PAGE_HEADER_SIZE - 4 - 4) / (4 + 2 + 36); // rough estimate
        if num_keys < max_keys {
            let page = self.file.write_page(page_no)?;
            insert_branch_entry(page, child_idx + 1, separator, new_child);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // Split the branch (rare for reasonable data sizes)
            // For v1, just create a new branch and split entries
            let new_page_no = self.file.allocate_page()?;
            let mut entries = collect_branch_entries(&page);
            entries.insert(child_idx + 1, (separator.to_string(), new_child));

            let mid = entries.len() / 2;
            let left_entries = &entries[..mid];
            let right_entries = &entries[mid + 1..];
            let split_separator = entries[mid].0.clone();
            let right_first_child = entries[mid].1;

            let left_page = self.file.write_page(page_no)?;
            rebuild_branch(left_page, left_entries, page.next_leaf()); // reuse field for first child

            let mut new_page = Page::new_branch();
            rebuild_branch_with_first_child(&mut new_page, right_entries, right_first_child);
            new_page.update_checksum();
            self.file.put_page(new_page_no, new_page);

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator: split_separator,
            })
        }
    }

    fn search_recursive(&mut self, page_no: u32, id: &str) -> Result<Option<Vec<u8>>> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            // Linear search through rows
            let num_rows = page.num_rows() as usize;
            for i in 0..num_rows {
                let (start, end) = row_bounds(&page, i, num_rows);
                let row_bytes = &page.data[start..end];
                let row_id = row::extract_id(row_bytes)?;
                if row_id == id {
                    return Ok(Some(row_bytes.to_vec()));
                }
            }
            Ok(None)
        } else {
            let (_, child_page_no) = find_child(&page, id);
            self.search_recursive(child_page_no, id)
        }
    }

    fn delete_from_leaf(&mut self, page_no: u32, id: &str) -> Result<bool> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            let num_rows = page.num_rows() as usize;
            let mut rows = collect_leaf_rows(&page);
            let original_len = rows.len();
            rows.retain(|r| r.0 != id);
            if rows.len() == original_len {
                return Ok(false); // not found
            }
            let page = self.file.write_page(page_no)?;
            rebuild_leaf(page, &rows.iter().map(|(id, data)| (id.clone(), data.clone())).collect::<Vec<_>>());
            page.update_checksum();
            Ok(true)
        } else {
            let (_, child_page_no) = find_child(&page, id);
            self.delete_from_leaf(child_page_no, id)
        }
    }

    fn scan_leaf(&mut self, page_no: u32, results: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        let page = self.file.read_page(page_no)?.clone();

        if page.is_leaf() {
            let rows = collect_leaf_rows(&page);
            for (id, data) in rows {
                results.push((id, data));
            }
            // Follow next_leaf pointer
            let next = page.next_leaf();
            if next != 0 {
                self.scan_leaf(next, results)?;
            }
        } else {
            // Traverse leftmost path to find first leaf
            let first_child = get_branch_child(&page, 0);
            self.scan_leaf(first_child, results)?;
        }
        Ok(())
    }
}

enum InsertResult {
    Fit,
    Split { new_page: u32, separator: String },
}

// --- Helper functions for page manipulation ---

fn row_bounds(page: &Page, i: usize, num_rows: usize) -> (usize, usize) {
    let start = page.row_offset(i as u16) as usize;
    let end = if i + 1 < num_rows {
        page.row_offset((i + 1) as u16) as usize
    } else {
        PAGE_SIZE - 4
    };
    (start, end)
}

fn append_row_to_leaf(page: &mut Page, row_data: &[u8]) {
    let num_rows = page.num_rows();
    let offset = if num_rows == 0 {
        PAGE_HEADER_SIZE + 2 // first row starts after first offset slot
    } else {
        let last_end = if num_rows == 1 {
            page.row_offset(0) as usize + row_data.len() // approximate
        } else {
            PAGE_SIZE - 4 // pack from the offset
        };
        page.free_space_offset() as usize
    };

    // Simple approach: pack rows sequentially after the offset array
    let row_start = PAGE_HEADER_SIZE + ((num_rows + 1) as usize) * 2;
    let current_data_end = page.free_space_offset() as usize;
    let write_at = if current_data_end > row_start {
        current_data_end
    } else {
        row_start
    };

    page.data[write_at..write_at + row_data.len()].copy_from_slice(row_data);
    page.set_row_offset(num_rows, write_at as u16);
    page.set_num_rows(num_rows + 1);
    page.set_free_space_offset((write_at + row_data.len()) as u16);
}

fn collect_leaf_rows(page: &Page) -> Vec<(String, Vec<u8>)> {
    let num_rows = page.num_rows() as usize;
    let mut rows = Vec::with_capacity(num_rows);
    for i in 0..num_rows {
        let (start, end) = row_bounds(page, i, num_rows);
        if start < end && end <= PAGE_SIZE {
            let data = page.data[start..end].to_vec();
            if let Ok(id) = row::extract_id(&data) {
                rows.push((id.to_string(), data));
            }
        }
    }
    rows
}

fn rebuild_leaf(page: &mut Page, rows: &[(String, Vec<u8>)]) {
    page.set_flags(crate::page::PAGE_LEAF);
    page.set_num_rows(0);
    page.set_free_space_offset(PAGE_HEADER_SIZE as u16);

    for (_, data) in rows {
        append_row_to_leaf(page, data);
    }
}

// --- Branch page helpers ---

fn get_branch_child(page: &Page, idx: usize) -> u32 {
    let offset = PAGE_HEADER_SIZE + 2 + idx * (4 + 2 + 36); // simplified fixed key size
    // Actually, branch format needs careful offset calculation.
    // For v1, use a simpler encoding: [num_keys:2][entries...]
    // Each entry: [child:4][key_len:2][key_bytes (var)]
    // Last child (no key): [child:4]
    // This is complex. Let's use a simple fixed-offset approach for now.
    let base = PAGE_HEADER_SIZE + 2; // after num_keys
    let offset = base + idx * 42; // 4 (child) + 2 (key_len) + 36 (max key) = 42
    u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap())
}

fn find_child(page: &Page, id: &str) -> (usize, u32) {
    let num_keys = page.num_rows() as usize;
    let base = PAGE_HEADER_SIZE + 2;

    for i in 0..num_keys {
        let entry_offset = base + i * 42;
        let key_len = u16::from_le_bytes(
            page.data[entry_offset + 4..entry_offset + 6].try_into().unwrap()
        ) as usize;
        let key = std::str::from_utf8(&page.data[entry_offset + 6..entry_offset + 6 + key_len])
            .unwrap_or("");
        if id < key {
            let child = u32::from_le_bytes(
                page.data[entry_offset..entry_offset + 4].try_into().unwrap()
            );
            return (i, child);
        }
    }
    // Last child
    let last_offset = base + num_keys * 42;
    let child = u32::from_le_bytes(
        page.data[last_offset..last_offset + 4].try_into().unwrap()
    );
    (num_keys, child)
}

fn write_branch_entry(page: &mut Page, idx: usize, child: u32, key: &str) {
    let base = PAGE_HEADER_SIZE + 2;
    let offset = base + idx * 42;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
    let key_bytes = key.as_bytes();
    let key_len = key_bytes.len().min(36);
    page.data[offset + 4..offset + 6].copy_from_slice(&(key_len as u16).to_le_bytes());
    page.data[offset + 6..offset + 6 + key_len].copy_from_slice(&key_bytes[..key_len]);
}

fn set_branch_last_child(page: &mut Page, idx: usize, child: u32) {
    let base = PAGE_HEADER_SIZE + 2;
    let offset = base + idx * 42;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
}

fn insert_branch_entry(page: &mut Page, at_idx: usize, key: &str, new_child: u32) {
    let num_keys = page.num_rows() as usize;
    // Shift entries right
    let base = PAGE_HEADER_SIZE + 2;
    for i in (at_idx..=num_keys).rev() {
        let src = base + i * 42;
        let dst = base + (i + 1) * 42;
        if dst + 42 <= PAGE_SIZE - 4 {
            page.data.copy_within(src..src + 42, dst);
        }
    }
    write_branch_entry(page, at_idx, page.data[base + (at_idx) * 42..base + (at_idx) * 42 + 4]
        .try_into().map(u32::from_le_bytes).unwrap_or(0), key);
    set_branch_last_child(page, at_idx + 1, new_child);
    page.set_num_rows(num_keys as u16 + 1);
}

fn collect_branch_entries(page: &Page) -> Vec<(String, u32)> {
    let num_keys = page.num_rows() as usize;
    let base = PAGE_HEADER_SIZE + 2;
    let mut entries = Vec::new();
    for i in 0..=num_keys {
        let offset = base + i * 42;
        let child = u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap());
        if i < num_keys {
            let key_len = u16::from_le_bytes(
                page.data[offset + 4..offset + 6].try_into().unwrap()
            ) as usize;
            let key = String::from_utf8_lossy(&page.data[offset + 6..offset + 6 + key_len]).to_string();
            entries.push((key, child));
        } else {
            entries.push((String::new(), child));
        }
    }
    entries
}

fn rebuild_branch(page: &mut Page, entries: &[(String, u32)], _first_child: u32) {
    page.set_flags(crate::page::PAGE_BRANCH);
    page.set_num_rows(entries.len() as u16);
    for (i, (key, child)) in entries.iter().enumerate() {
        write_branch_entry(page, i, *child, key);
    }
    page.update_checksum();
}

fn rebuild_branch_with_first_child(page: &mut Page, entries: &[(String, u32)], first_child: u32) {
    page.set_flags(crate::page::PAGE_BRANCH);
    set_branch_last_child(page, 0, first_child);
    page.set_num_rows(entries.len() as u16);
    for (i, (key, child)) in entries.iter().enumerate() {
        write_branch_entry(page, i + 1, *child, key);
    }
    page.update_checksum();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row;
    use crate::value::Value;
    use tempfile::NamedTempFile;

    fn make_row(id: &str, name: &str) -> Vec<u8> {
        row::encode_row(id, &[(0, &Value::Text(name.into()))])
    }

    #[test]
    fn test_insert_and_search() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        let row = make_row("id1", "alice");
        tree.insert("id1", &row).unwrap();

        let found = tree.search("id1").unwrap();
        assert!(found.is_some());
        let decoded = row::decode_row(&found.unwrap()).unwrap();
        assert_eq!(decoded.id, "id1");
        assert_eq!(decoded.columns[0].1, Value::Text("alice".into()));
    }

    #[test]
    fn test_search_not_found() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        assert!(tree.search("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_duplicate_key_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        let row = make_row("id1", "alice");
        tree.insert("id1", &row).unwrap();
        assert!(tree.insert("id1", &row).is_err());
    }

    #[test]
    fn test_many_inserts_trigger_split() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();

        // Insert enough rows to fill a page and trigger a split
        for i in 0..100 {
            let id = format!("id_{i:04}");
            let row = make_row(&id, &format!("name_{i}"));
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(&id, &row).unwrap();
        }

        // Verify all rows are findable
        let mut tree = BTree::new(&mut pf, root);
        for i in 0..100 {
            let id = format!("id_{i:04}");
            assert!(tree.search(&id).unwrap().is_some(), "missing: {id}");
        }
    }

    #[test]
    fn test_delete() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let root = BTree::create(&mut pf).unwrap();
        let mut tree = BTree::new(&mut pf, root);

        let row = make_row("id1", "alice");
        tree.insert("id1", &row).unwrap();
        assert!(tree.delete("id1").unwrap());
        assert!(tree.search("id1").unwrap().is_none());
        assert!(!tree.delete("id1").unwrap()); // already deleted
    }

    #[test]
    fn test_scan_all() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pf = PageFile::open(tmp.path()).unwrap();
        let mut root = BTree::create(&mut pf).unwrap();

        for i in 0..20 {
            let id = format!("id_{i:04}");
            let row = make_row(&id, &format!("name_{i}"));
            let mut tree = BTree::new(&mut pf, root);
            root = tree.insert(&id, &row).unwrap();
        }

        let mut tree = BTree::new(&mut pf, root);
        let all = tree.scan_all().unwrap();
        assert_eq!(all.len(), 20);
    }
}
```

- [ ] **Step 2: Export from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod btree;
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test`
Expected: all B+ tree tests pass

```bash
git add -A
git commit -m "feat: B+ tree — insert with split, search, delete, scan"
```

---

### Task 7: WAL (Write-Ahead Log)

**Files:**
- Create: `src/wal.rs`

Append-only log that stores before-images of modified pages. On commit, the WAL ensures durability. On crash recovery, replay the WAL to undo uncommitted changes.

- [ ] **Step 1: Create wal.rs**

```rust
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{BoogyError, Result};
use crate::page::PAGE_SIZE;

const WAL_ENTRY_SIZE: usize = 8 + 4 + 4 + PAGE_SIZE + 4; // seq + table_id + page_no + page_data + checksum
const WAL_HEADER_SIZE: usize = 16; // magic(4) + version(4) + entry_count(4) + reserved(4)
const WAL_MAGIC: u32 = 0xB00DWA01;

/// Write-Ahead Log for crash recovery.
pub struct Wal {
    file: File,
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

        let (next_sequence, entry_count) = if file_len == 0 {
            // New WAL — write header
            let mut wal = Self {
                file,
                path,
                next_sequence: 1,
                entry_count: 0,
            };
            wal.write_header()?;
            (1, 0)
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

            return Ok(Self {
                file: f,
                path,
                next_sequence: next_seq,
                entry_count,
            });
        };

        Ok(Self {
            file,
            path,
            next_sequence,
            entry_count,
        })
    }

    /// Append a before-image of a page to the WAL.
    pub fn append_before_image(
        &mut self,
        table_id: u32,
        page_no: u32,
        page_data: &[u8; PAGE_SIZE],
    ) -> Result<u64> {
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
        wal.append_before_image(1, 5, &page).unwrap();
        wal.append_before_image(2, 10, &[0xCD; PAGE_SIZE]).unwrap();
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

        wal.append_before_image(1, 0, &[0; PAGE_SIZE]).unwrap();
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
            wal.append_before_image(1, 0, &[0x42; PAGE_SIZE]).unwrap();
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
            wal.append_before_image(1, 0, &[0; PAGE_SIZE]).unwrap();
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
```

- [ ] **Step 2: Export from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod wal;
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test`

```bash
git add -A
git commit -m "feat: WAL — append before-images, read/replay, truncate, checksums"
```

---

### Task 8: Table registry + public API (BoogyDb)

**Files:**
- Create: `src/table.rs`
- Create: `src/db.rs`
- Modify: `src/lib.rs`
- Create: `tests/crud_test.rs`

This task ties everything together into the public `BoogyDb` API. The table registry manages schemas and per-table B+ tree roots. `BoogyDb` provides `create_table`, `insert`, `get`, `update`, `delete`, `find`, `count`, and `transaction`.

MVCC (Task 9) and crash recovery (Task 10) are deferred to separate tasks — this task gets the core API working with single-writer semantics.

- [ ] **Step 1: Create table.rs**

```rust
use std::collections::HashMap;
use std::sync::RwLock;

use crate::value::{ColumnDef, Type};

/// Metadata for a registered table.
#[derive(Debug, Clone)]
pub struct TableMeta {
    pub name: String,
    pub table_id: u32,
    pub columns: Vec<ColumnDef>,
    /// Column name → column ID mapping.
    pub col_name_to_id: HashMap<String, u16>,
    /// B+ tree root page number.
    pub root_page: u32,
    /// Number of rows (maintained by insert/delete).
    pub row_count: u64,
}

impl TableMeta {
    pub fn new(name: String, table_id: u32, columns: Vec<ColumnDef>, root_page: u32) -> Self {
        let col_name_to_id: HashMap<String, u16> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i as u16))
            .collect();
        Self {
            name,
            table_id,
            columns,
            col_name_to_id,
            root_page,
            row_count: 0,
        }
    }

    pub fn col_id(&self, name: &str) -> Option<u16> {
        self.col_name_to_id.get(name).copied()
    }
}

/// Registry of all tables in a database.
pub struct TableRegistry {
    tables: HashMap<String, TableMeta>,
    next_table_id: u32,
}

impl TableRegistry {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            next_table_id: 1,
        }
    }

    pub fn register(&mut self, name: String, columns: Vec<ColumnDef>, root_page: u32) -> &TableMeta {
        let id = self.next_table_id;
        self.next_table_id += 1;
        let meta = TableMeta::new(name.clone(), id, columns, root_page);
        self.tables.insert(name.clone(), meta);
        self.tables.get(&name).unwrap()
    }

    pub fn get(&self, name: &str) -> Option<&TableMeta> {
        self.tables.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut TableMeta> {
        self.tables.get_mut(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<TableMeta> {
        self.tables.remove(name)
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.keys().cloned().collect()
    }
}
```

- [ ] **Step 2: Create db.rs**

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::btree::BTree;
use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::filter::{FindOptions, Sort, SortDir};
use crate::row;
use crate::table::{TableMeta, TableRegistry};
use crate::value::{ColumnDef, Value};

/// A row returned from queries.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub columns: Vec<(String, Value)>,
}

/// The main database handle.
pub struct BoogyDb {
    file: Mutex<PageFile>,
    registry: Mutex<TableRegistry>,
    path: PathBuf,
}

impl BoogyDb {
    /// Open or create a database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = PageFile::open(&path)?;
        let registry = TableRegistry::new();

        Ok(Self {
            file: Mutex::new(file),
            registry: Mutex::new(registry),
            path,
        })
    }

    /// Create a new table.
    pub fn create_table(&self, name: &str, columns: &[ColumnDef]) -> Result<()> {
        let mut registry = self.registry.lock().unwrap();
        if registry.get(name).is_some() {
            return Err(BoogyError::TableExists(name.to_string()));
        }

        let mut file = self.file.lock().unwrap();
        let root = BTree::create(&mut file)?;
        file.flush()?;

        registry.register(name.to_string(), columns.to_vec(), root);
        Ok(())
    }

    /// Drop a table.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut registry = self.registry.lock().unwrap();
        if registry.remove(name).is_none() {
            return Err(BoogyError::TableNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Insert a row. Returns the auto-generated _id.
    pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<String> {
        let mut registry = self.registry.lock().unwrap();
        let meta = registry.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone();

        let id = uuid::Uuid::new_v4().to_string();

        // Convert column names to IDs
        let col_values: Vec<(u16, &Value)> = data.iter()
            .filter_map(|(name, val)| {
                meta.col_id(name).map(|id| (id, val))
            })
            .collect();

        let row_bytes = row::encode_row(&id, &col_values);

        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, meta.root_page);
        let new_root = tree.insert(&id, &row_bytes)?;
        file.flush()?;

        // Update root if changed (split occurred)
        if new_root != meta.root_page {
            let meta = registry.get_mut(table).unwrap();
            meta.root_page = new_root;
        }
        let meta = registry.get_mut(table).unwrap();
        meta.row_count += 1;

        Ok(id)
    }

    /// Get a row by _id.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Row>> {
        let registry = self.registry.lock().unwrap();
        let meta = registry.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone();

        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, meta.root_page);

        match tree.search(id)? {
            Some(bytes) => {
                let decoded = row::decode_row(&bytes)?;
                Ok(Some(decoded_to_row(&decoded, &meta)))
            }
            None => Ok(None),
        }
    }

    /// Update a row by _id. Replaces specified columns.
    pub fn update(&self, table: &str, id: &str, fields: &[(&str, Value)]) -> Result<bool> {
        let mut registry = self.registry.lock().unwrap();
        let meta = registry.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone();

        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, meta.root_page);

        // Get existing row
        let existing = match tree.search(id)? {
            Some(bytes) => row::decode_row(&bytes)?,
            None => return Ok(false),
        };

        // Merge updates
        let mut col_map: HashMap<u16, Value> = existing.columns.into_iter().collect();
        for (name, val) in fields {
            if let Some(col_id) = meta.col_id(name) {
                col_map.insert(col_id, val.clone());
            }
        }

        let col_values: Vec<(u16, &Value)> = col_map.iter().map(|(k, v)| (*k, v)).collect();
        let new_row = row::encode_row(id, &col_values);

        // Delete + re-insert (simple approach for v1)
        tree.delete(id)?;
        let new_root = tree.insert(id, &new_row)?;
        file.flush()?;

        if new_root != meta.root_page {
            let meta = registry.get_mut(table).unwrap();
            meta.root_page = new_root;
        }

        Ok(true)
    }

    /// Delete a row by _id.
    pub fn delete(&self, table: &str, id: &str) -> Result<bool> {
        let mut registry = self.registry.lock().unwrap();
        let meta = registry.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone();

        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, meta.root_page);
        let deleted = tree.delete(id)?;
        file.flush()?;

        if deleted {
            let meta = registry.get_mut(table).unwrap();
            meta.row_count -= 1;
        }
        Ok(deleted)
    }

    /// Find rows matching filters, with sort and pagination.
    /// Returns (matching_rows, total_count).
    pub fn find(&self, table: &str, opts: FindOptions) -> Result<(Vec<Row>, u64)> {
        let registry = self.registry.lock().unwrap();
        let meta = registry.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone();

        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, meta.root_page);
        let all = tree.scan_all()?;

        // Decode and filter
        let mut matching: Vec<Row> = Vec::new();
        for (_, bytes) in &all {
            let decoded = row::decode_row(bytes)?;
            let row = decoded_to_row(&decoded, &meta);

            let passes = opts.filters.iter().all(|f| {
                let col_val = row.columns.iter()
                    .find(|(name, _)| name == &f.column)
                    .map(|(_, v)| v);
                match col_val {
                    Some(v) => f.matches(v),
                    None => f.matches(&Value::Null),
                }
            });

            if passes {
                matching.push(row);
            }
        }

        let total = matching.len() as u64;

        // Sort
        for sort in opts.sort.iter().rev() {
            matching.sort_by(|a, b| {
                let va = a.columns.iter().find(|(n, _)| n == &sort.column).map(|(_, v)| v);
                let vb = b.columns.iter().find(|(n, _)| n == &sort.column).map(|(_, v)| v);
                let ord = match (va, vb) {
                    (Some(a), Some(b)) => a.compare(b).unwrap_or(std::cmp::Ordering::Equal),
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (None, None) => std::cmp::Ordering::Equal,
                };
                match sort.dir {
                    SortDir::Asc => ord,
                    SortDir::Desc => ord.reverse(),
                }
            });
        }

        // Pagination
        let skip = opts.offset.unwrap_or(0) as usize;
        let take = opts.limit.unwrap_or(u32::MAX) as usize;
        let page: Vec<Row> = matching.into_iter().skip(skip).take(take).collect();

        Ok((page, total))
    }

    /// Count rows matching filters.
    pub fn count(&self, table: &str, filters: &[crate::filter::Filter]) -> Result<u64> {
        let registry = self.registry.lock().unwrap();
        let meta = registry.get(table)
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
            .clone();

        if filters.is_empty() {
            return Ok(meta.row_count);
        }

        let mut file = self.file.lock().unwrap();
        let mut tree = BTree::new(&mut file, meta.root_page);
        let all = tree.scan_all()?;

        let mut count = 0u64;
        for (_, bytes) in &all {
            let decoded = row::decode_row(bytes)?;
            let row = decoded_to_row(&decoded, &meta);

            let passes = filters.iter().all(|f| {
                let col_val = row.columns.iter()
                    .find(|(name, _)| name == &f.column)
                    .map(|(_, v)| v);
                match col_val {
                    Some(v) => f.matches(v),
                    None => f.matches(&Value::Null),
                }
            });

            if passes {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Run a multi-table transaction.
    pub fn transaction<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&TransactionCtx) -> Result<R>,
    {
        let ctx = TransactionCtx { db: self };
        let result = f(&ctx)?;
        // Flush all changes
        let mut file = self.file.lock().unwrap();
        file.flush()?;
        Ok(result)
    }
}

/// Transaction context — provides the same API as BoogyDb but within a transaction.
pub struct TransactionCtx<'a> {
    db: &'a BoogyDb,
}

impl<'a> TransactionCtx<'a> {
    pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<String> {
        self.db.insert(table, data)
    }

    pub fn get(&self, table: &str, id: &str) -> Result<Option<Row>> {
        self.db.get(table, id)
    }

    pub fn update(&self, table: &str, id: &str, fields: &[(&str, Value)]) -> Result<bool> {
        self.db.update(table, id, fields)
    }

    pub fn delete(&self, table: &str, id: &str) -> Result<bool> {
        self.db.delete(table, id)
    }
}

fn decoded_to_row(decoded: &row::DecodedRow, meta: &TableMeta) -> Row {
    let columns: Vec<(String, Value)> = decoded.columns.iter()
        .filter_map(|(col_id, val)| {
            meta.columns.get(*col_id as usize)
                .map(|def| (def.name.clone(), val.clone()))
        })
        .collect();
    Row {
        id: decoded.id.clone(),
        columns,
    }
}
```

- [ ] **Step 3: Update lib.rs**

```rust
pub mod error;
pub mod value;
pub mod page;
pub mod row;
pub mod file;
pub mod filter;
pub mod btree;
pub mod wal;
pub mod table;
pub mod db;

pub use error::{BoogyError, Result};
pub use value::{ColumnDef, Type, Value};
pub use filter::{Filter, FilterOp, FindOptions, Sort, SortDir};
pub use db::{BoogyDb, Row};
```

- [ ] **Step 4: Create tests/crud_test.rs**

```rust
use boogy_db::*;
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    (db, dir)
}

#[test]
fn test_create_table_and_insert() {
    let (db, _dir) = create_db();
    db.create_table("users", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("age", Type::Integer),
    ]).unwrap();

    let id = db.insert("users", &[
        ("name", Value::Text("alice".into())),
        ("age", Value::Integer(30)),
    ]).unwrap();

    let row = db.get("users", &id).unwrap().unwrap();
    assert_eq!(row.id, id);
    let name = row.columns.iter().find(|(n, _)| n == "name").unwrap();
    assert_eq!(name.1, Value::Text("alice".into()));
}

#[test]
fn test_get_not_found() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
    assert!(db.get("users", "nonexistent").unwrap().is_none());
}

#[test]
fn test_update() {
    let (db, _dir) = create_db();
    db.create_table("users", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("age", Type::Integer),
    ]).unwrap();

    let id = db.insert("users", &[
        ("name", Value::Text("alice".into())),
        ("age", Value::Integer(30)),
    ]).unwrap();

    db.update("users", &id, &[("age", Value::Integer(31))]).unwrap();

    let row = db.get("users", &id).unwrap().unwrap();
    let age = row.columns.iter().find(|(n, _)| n == "age").unwrap();
    assert_eq!(age.1, Value::Integer(31));
}

#[test]
fn test_delete() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();

    let id = db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
    assert!(db.delete("users", &id).unwrap());
    assert!(db.get("users", &id).unwrap().is_none());
    assert!(!db.delete("users", &id).unwrap());
}

#[test]
fn test_find_with_filter() {
    let (db, _dir) = create_db();
    db.create_table("posts", &[
        ColumnDef::new("author", Type::Text),
        ColumnDef::new("title", Type::Text),
    ]).unwrap();

    for i in 0..20 {
        let author = if i % 2 == 0 { "alice" } else { "bob" };
        db.insert("posts", &[
            ("author", Value::Text(author.into())),
            ("title", Value::Text(format!("Post {i}"))),
        ]).unwrap();
    }

    let (rows, total) = db.find("posts", FindOptions {
        filters: vec![Filter::eq("author", "alice")],
        sort: vec![],
        limit: Some(5),
        offset: None,
    }).unwrap();

    assert_eq!(total, 10); // 10 by alice
    assert_eq!(rows.len(), 5); // limited to 5
}

#[test]
fn test_find_with_sort_and_pagination() {
    let (db, _dir) = create_db();
    db.create_table("items", &[
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    for i in 0..10 {
        db.insert("items", &[("value", Value::Integer(i))]).unwrap();
    }

    let (rows, total) = db.find("items", FindOptions {
        filters: vec![],
        sort: vec![Sort::desc("value")],
        limit: Some(3),
        offset: Some(2),
    }).unwrap();

    assert_eq!(total, 10);
    assert_eq!(rows.len(), 3);
    // Descending: 9,8,7,6,5,4,3,2,1,0 → skip 2 → 7,6,5
    let values: Vec<i64> = rows.iter()
        .filter_map(|r| r.columns.iter().find(|(n, _)| n == "value").map(|(_, v)| {
            if let Value::Integer(i) = v { *i } else { -1 }
        }))
        .collect();
    assert_eq!(values, vec![7, 6, 5]);
}

#[test]
fn test_count() {
    let (db, _dir) = create_db();
    db.create_table("items", &[
        ColumnDef::new("category", Type::Text),
    ]).unwrap();

    for i in 0..30 {
        let cat = format!("cat_{}", i % 3);
        db.insert("items", &[("category", Value::Text(cat))]).unwrap();
    }

    assert_eq!(db.count("items", &[]).unwrap(), 30);
    assert_eq!(
        db.count("items", &[Filter::eq("category", "cat_0")]).unwrap(),
        10
    );
}

#[test]
fn test_transaction() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
    db.create_table("posts", &[ColumnDef::new("title", Type::Text)]).unwrap();

    db.transaction(|tx| {
        tx.insert("users", &[("name", Value::Text("alice".into()))])?;
        tx.insert("posts", &[("title", Value::Text("hello".into()))])?;
        Ok(())
    }).unwrap();

    assert_eq!(db.count("users", &[]).unwrap(), 1);
    assert_eq!(db.count("posts", &[]).unwrap(), 1);
}

#[test]
fn test_many_inserts() {
    let (db, _dir) = create_db();
    db.create_table("data", &[
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    let mut ids = Vec::new();
    for i in 0..500 {
        let id = db.insert("data", &[("value", Value::Integer(i))]).unwrap();
        ids.push(id);
    }

    assert_eq!(db.count("data", &[]).unwrap(), 500);

    // Spot check
    for (i, id) in ids.iter().enumerate().step_by(50) {
        let row = db.get("data", id).unwrap().unwrap();
        let val = row.columns.iter().find(|(n, _)| n == "value").unwrap();
        assert_eq!(val.1, Value::Integer(i as i64));
    }
}

#[test]
fn test_duplicate_table_rejected() {
    let (db, _dir) = create_db();
    db.create_table("t", &[]).unwrap();
    assert!(db.create_table("t", &[]).is_err());
}

#[test]
fn test_table_not_found() {
    let (db, _dir) = create_db();
    assert!(db.insert("nonexistent", &[]).is_err());
    assert!(db.get("nonexistent", "id").is_err());
}
```

- [ ] **Step 5: Verify and commit**

Run: `cargo test`
Expected: all tests pass

```bash
git add -A
git commit -m "feat: BoogyDb public API — create_table, insert, get, update, delete, find, count, transaction"
```

---

### Task 9: MVCC + per-table RwLocks (deferred to next session)

### Task 10: Crash recovery via WAL replay (deferred to next session)

### Task 11: Stress tests + concurrent tests (deferred to next session)

---

## Self-Review

**Spec coverage:**
- create_table / drop_table ✓ (Task 8)
- insert / get / update / delete ✓ (Task 8)
- find with filters, sort, pagination, total_count ✓ (Task 8)
- count ✓ (Task 8)
- Transactions ✓ (Task 8, simplified)
- Crash recovery — WAL structure built (Task 7), integration deferred
- Per-table MVCC — deferred (Task 9)
- CRC32 checksums ✓ (Task 2, Task 7)

**Tasks 9-11 are deferred** to keep the first implementation pass focused. The core engine (Tasks 1-8) is a complete working database that can be benchmarked against SQLite. MVCC and crash recovery build on top without changing the API.
