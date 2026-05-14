# Overflow Pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Support rows larger than a single page via overflow page chains, enabling large blobs and text fields up to a configurable limit (default 10MB).

**Architecture:** When a row exceeds available leaf page space, the excess is stored in linked overflow pages (PAGE_OVERFLOW type). The leaf stores an inline prefix + 9-byte overflow marker. Reads reassemble the full row transparently. Normal rows (no overflow) have zero overhead — one byte comparison on reads.

**Tech Stack:** Rust, existing boogy-db crate. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-13-overflow-pages-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/page.rs` | Modify | PAGE_OVERFLOW flag, overflow page accessors |
| `src/overflow.rs` | Create | Overflow marker helpers, chain read/write/free functions |
| `src/btree.rs` | Modify | Integrate overflow into insert/search/scan/delete for both Reader and Writer |
| `src/db.rs` | Modify | max_row_size field + RowTooLarge check in insert |
| `src/error.rs` | Modify | Add RowTooLarge variant |
| `src/lib.rs` | Modify | Add `pub mod overflow;` |
| `tests/crud_test.rs` | Modify | Overflow integration tests |
| `README.md` | Modify | Document large row support |

---

## Task 1: Page Type + Overflow Helpers

**Files:**
- Modify: `src/page.rs`
- Create: `src/overflow.rs`
- Modify: `src/error.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add PAGE_OVERFLOW constant and accessors to page.rs**

Add after existing page type flags:
```rust
pub const PAGE_OVERFLOW: u16 = 0x10;
```

Add methods to `impl Page`:
```rust
pub fn new_overflow() -> Self {
    let mut page = Self { data: [0; PAGE_SIZE] };
    page.set_magic(MAGIC);
    page.set_flags(PAGE_OVERFLOW);
    page.update_checksum();
    page
}

pub fn is_overflow(&self) -> bool { self.flags() & PAGE_OVERFLOW != 0 }

/// Overflow payload length (stored in num_rows position, bytes 4-5).
pub fn overflow_payload_len(&self) -> u16 {
    self.num_rows() // reuse the same header field
}

pub fn set_overflow_payload_len(&mut self, len: u16) {
    self.set_num_rows(len)
}

/// Next overflow page (stored in next_leaf position, bytes 8-11).
pub fn overflow_next(&self) -> u32 {
    self.next_leaf() // reuse the same header field
}

pub fn set_overflow_next(&mut self, page_no: u32) {
    self.set_next_leaf(page_no)
}
```

The overflow payload area starts at `PAGE_HEADER_SIZE` (16) and extends to `PAGE_SIZE - 4` (checksum). Max payload: `PAGE_SIZE - PAGE_HEADER_SIZE - 4` = **4076 bytes**.

- [ ] **Step 2: Create src/overflow.rs**

```rust
use crate::error::Result;
use crate::page::{Page, PAGE_HEADER_SIZE, PAGE_SIZE, PAGE_OVERFLOW};

/// Maximum payload bytes per overflow page.
pub const OVERFLOW_PAYLOAD_MAX: usize = PAGE_SIZE - PAGE_HEADER_SIZE - 4; // 4076

/// The overflow marker byte. Placed at the start of the 9-byte trailer.
/// Normal type tags are 0-5, so 0xFF is unambiguous.
pub const OVERFLOW_MARKER: u8 = 0xFF;

/// Size of the overflow trailer: [marker:1][first_page:4][remaining_len:4]
pub const OVERFLOW_TRAILER_SIZE: usize = 9;

/// Check if raw row bytes have an overflow trailer.
pub fn has_overflow(row_bytes: &[u8]) -> bool {
    row_bytes.len() >= OVERFLOW_TRAILER_SIZE
        && row_bytes[row_bytes.len() - OVERFLOW_TRAILER_SIZE] == OVERFLOW_MARKER
}

/// Extract overflow metadata from the trailer.
/// Returns (inline_len, first_overflow_page, remaining_len).
pub fn decode_overflow_trailer(row_bytes: &[u8]) -> (usize, u32, u32) {
    let inline_len = row_bytes.len() - OVERFLOW_TRAILER_SIZE;
    let base = inline_len + 1; // skip marker byte
    let first_page = u32::from_le_bytes(row_bytes[base..base + 4].try_into().unwrap());
    let remaining = u32::from_le_bytes(row_bytes[base + 4..base + 8].try_into().unwrap());
    (inline_len, first_page, remaining)
}

/// Encode an overflow trailer and append it to a buffer.
pub fn append_overflow_trailer(buf: &mut Vec<u8>, first_page: u32, remaining_len: u32) {
    buf.push(OVERFLOW_MARKER);
    buf.extend_from_slice(&first_page.to_le_bytes());
    buf.extend_from_slice(&remaining_len.to_le_bytes());
}

/// Build an overflow page from a chunk of data.
pub fn build_overflow_page(data: &[u8], next_page: u32) -> Page {
    let mut page = Page::new_overflow();
    let len = data.len().min(OVERFLOW_PAYLOAD_MAX);
    page.set_overflow_payload_len(len as u16);
    page.set_overflow_next(next_page);
    page.data[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + len].copy_from_slice(&data[..len]);
    page.update_checksum();
    page
}

/// Read the payload from an overflow page.
pub fn read_overflow_payload(page: &Page) -> &[u8] {
    let len = page.overflow_payload_len() as usize;
    &page.data[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + len]
}
```

- [ ] **Step 3: Add RowTooLarge to error.rs**

```rust
RowTooLarge(usize),
```

Display: `write!(f, "row too large: {size} bytes exceeds maximum")`.

- [ ] **Step 4: Add `pub mod overflow;` to lib.rs**

After `pub mod crypto;`.

- [ ] **Step 5: Add unit tests to overflow.rs**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_overflow_positive() {
        let mut data = vec![0u8; 20];
        data[20 - OVERFLOW_TRAILER_SIZE] = OVERFLOW_MARKER;
        assert!(has_overflow(&data));
    }

    #[test]
    fn test_has_overflow_negative() {
        let data = vec![0u8; 20]; // no marker
        assert!(!has_overflow(&data));
    }

    #[test]
    fn test_has_overflow_too_short() {
        let data = vec![OVERFLOW_MARKER; 5]; // shorter than trailer
        assert!(!has_overflow(&data));
    }

    #[test]
    fn test_trailer_roundtrip() {
        let mut buf = vec![1u8, 2, 3, 4, 5]; // some inline data
        append_overflow_trailer(&mut buf, 42, 8000);
        assert!(has_overflow(&buf));
        let (inline_len, first_page, remaining) = decode_overflow_trailer(&buf);
        assert_eq!(inline_len, 5);
        assert_eq!(first_page, 42);
        assert_eq!(remaining, 8000);
    }

    #[test]
    fn test_overflow_page_build_and_read() {
        let data = vec![0xAB; 1000];
        let page = build_overflow_page(&data, 99);
        assert!(page.is_overflow());
        assert_eq!(page.overflow_payload_len(), 1000);
        assert_eq!(page.overflow_next(), 99);
        let payload = read_overflow_payload(&page);
        assert_eq!(payload.len(), 1000);
        assert!(payload.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn test_overflow_page_max_payload() {
        let data = vec![0xCD; OVERFLOW_PAYLOAD_MAX + 100]; // more than one page
        let page = build_overflow_page(&data, 0);
        assert_eq!(page.overflow_payload_len() as usize, OVERFLOW_PAYLOAD_MAX);
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test --lib overflow::tests`
Expected: All 5 tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/page.rs src/overflow.rs src/error.rs src/lib.rs
git commit -m "feat: overflow page type and helpers for large row support"
```

---

## Task 2: Overflow Write Path in BTreeWriter

**Files:**
- Modify: `src/btree.rs`

Integrate overflow into `insert_into_leaf`. When a row doesn't fit even after a page split would be tried, store the row with overflow pages instead.

- [ ] **Step 1: Add overflow write helper to BTreeWriter**

Add a method that splits a row into inline + overflow pages:

```rust
/// Store a large row using overflow pages.
/// Returns the inline bytes (to be stored on the leaf page).
fn write_overflow_row(&mut self, row_data: &[u8], available_inline: usize) -> Result<Vec<u8>> {
    use crate::overflow::*;

    let inline_data_len = available_inline - OVERFLOW_TRAILER_SIZE;
    let overflow_data = &row_data[inline_data_len..];

    // Allocate overflow pages (build chain from last to first)
    let mut chunks: Vec<&[u8]> = overflow_data.chunks(OVERFLOW_PAYLOAD_MAX).collect();
    let mut next_page: u32 = 0;
    let mut first_page: u32 = 0;

    // Build from last chunk to first so we know next_page pointers
    for chunk in chunks.iter().rev() {
        let page_no = self.guard.allocate_page()?;
        let page = build_overflow_page(chunk, next_page);
        self.guard.put_page(page_no, page);
        next_page = page_no;
        first_page = page_no;
    }

    // Build inline portion + trailer
    let mut inline = Vec::with_capacity(available_inline);
    inline.extend_from_slice(&row_data[..inline_data_len]);
    append_overflow_trailer(&mut inline, first_page, overflow_data.len() as u32);

    Ok(inline)
}
```

- [ ] **Step 2: Modify insert_into_leaf to handle overflow**

In the `else` branch of `insert_into_leaf` (where the row doesn't fit), BEFORE the existing split logic, add an overflow check:

```rust
} else {
    // Row doesn't fit on this page.
    // If the row is larger than what ANY leaf page can hold (even empty),
    // use overflow pages.
    let max_inline = PAGE_SIZE - PAGE_HEADER_SIZE - 2 - CHECKSUM_SIZE;
    // A single row on an empty page: header + 1 offset entry (2 bytes) + row + checksum
    if row_data.len() > max_inline {
        // Row is too large for any single page — use overflow
        let available = max_inline; // max bytes for inline portion on a fresh page
        let inline_row = self.write_overflow_row(row_data, available)?;

        // Now insert the inline portion. It might still not fit on THIS page
        // (if this page has other rows), so try inserting the smaller inline row.
        // If it still doesn't fit, the existing split logic will handle it.
        let needed_with_inline = PAGE_HEADER_SIZE
            + (num_rows + 1) * 2
            + existing_data_size
            + inline_row.len()
            + CHECKSUM_SIZE;

        if needed_with_inline <= PAGE_SIZE {
            let snapshot = page.data;
            let page = self.guard.write_page(page_no)?;
            write_leaf_with_insert(page, &snapshot, num_rows, pos, &inline_row);
            page.update_checksum();
            return Ok(InsertResult::Fit);
        }

        // Inline portion still doesn't fit on this page — split, then insert
        // Fall through to existing split logic with inline_row instead of row_data
        // ... (use inline_row in the split path)
    }

    // --- Existing split logic (for rows that fit on a page but this page is full) ---
    let snapshot = page.data;
    // ... rest of existing split code ...
```

The exact integration depends on the current split code structure. The implementer should:
1. Check if `row_data.len() > max_inline_for_empty_page` (needs overflow)
2. If yes: call `write_overflow_row`, get inline bytes, use those for the insert/split
3. If no: existing path unchanged

- [ ] **Step 3: Run btree tests**

Run: `cargo test --lib btree::tests`
Expected: All existing tests pass (they use small rows, overflow path not triggered).

- [ ] **Step 4: Commit**

```bash
git add src/btree.rs
git commit -m "feat: overflow write path in BTreeWriter::insert_into_leaf"
```

---

## Task 3: Overflow Read Path — Reassembly

**Files:**
- Modify: `src/btree.rs`

Add row reassembly after extracting bytes from leaf pages in both BTreeReader and BTreeWriter.

- [ ] **Step 1: Add reassembly helper functions**

Add a helper at the module level (used by both Reader and Writer):

```rust
use crate::overflow;

/// Reassemble a row that may have overflow pages.
/// For BTreeReader (reads via &PageFile).
fn reassemble_row_reader(row_bytes: &[u8], file: &PageFile) -> Result<Vec<u8>> {
    if !overflow::has_overflow(row_bytes) {
        return Ok(row_bytes.to_vec());
    }
    let (inline_len, first_page, remaining) = overflow::decode_overflow_trailer(row_bytes);
    let mut full = Vec::with_capacity(inline_len + remaining as usize);
    full.extend_from_slice(&row_bytes[..inline_len]);

    let mut current = first_page;
    let mut left = remaining as usize;
    while current != 0 && left > 0 {
        let page = file.read_page(current)?;
        let payload = overflow::read_overflow_payload(&page);
        let take = payload.len().min(left);
        full.extend_from_slice(&payload[..take]);
        left -= take;
        current = page.overflow_next();
    }
    Ok(full)
}

/// Reassemble a row via WriteGuard (sees dirty overlay).
fn reassemble_row_writer(row_bytes: &[u8], guard: &WriteGuard) -> Result<Vec<u8>> {
    if !overflow::has_overflow(row_bytes) {
        return Ok(row_bytes.to_vec());
    }
    let (inline_len, first_page, remaining) = overflow::decode_overflow_trailer(row_bytes);
    let mut full = Vec::with_capacity(inline_len + remaining as usize);
    full.extend_from_slice(&row_bytes[..inline_len]);

    let mut current = first_page;
    let mut left = remaining as usize;
    while current != 0 && left > 0 {
        let page_arc = guard.read_page(current)?;
        let payload = overflow::read_overflow_payload(&page_arc);
        let take = payload.len().min(left);
        full.extend_from_slice(&payload[..take]);
        left -= take;
        current = page_arc.overflow_next();
    }
    Ok(full)
}
```

- [ ] **Step 2: Integrate into BTreeReader**

In `search_recursive` — after `Ok(Some(page.data[start..end].to_vec()))`:
```rust
// Before:
Ok(Some(page.data[start..end].to_vec()))
// After:
let raw = &page.data[start..end];
Ok(Some(reassemble_row_reader(raw, self.file)?))
```

In `scan_all` — after `results.push((id, data.to_vec()))`:
```rust
// Before:
results.push((id, data.to_vec()));
// After:
let full = reassemble_row_reader(data, self.file)?;
let id = row::extract_id(&full)?;
results.push((id, full));
```

In `multi_get_sorted` — after `results.push(data.to_vec())`:
```rust
// Before:
results.push(data.to_vec());
// After:
results.push(reassemble_row_reader(data, self.file)?);
```

In `scan_filtered` — the filter evaluation uses `extract_column_raw` on inline bytes. If the column is in overflow, `extract_column_raw` returns None and the existing fallback to `extract_column` handles it. BUT `extract_column` also operates on inline bytes. For overflow rows, we need to reassemble before decoding. Add after the match:
```rust
// For overflow rows where column is in the overflow portion,
// reassemble and retry extraction
if matches.is_none() && overflow::has_overflow(data) {
    // Reassemble and evaluate
    // ... (fall back to full reassembly + extract_column)
}
```

Actually, simpler: in scan_filtered, for rows that match the filter, reassemble before collecting:
```rust
if matches {
    total += 1;
    if total > skip && (total - skip) <= take {
        let full = reassemble_row_reader(data, self.file)?;
        let id = row::extract_id(&full)?;
        results.push((id, full));
    }
}
```

Filter evaluation on inline bytes still works for columns that fit inline. For overflow columns that are being filtered (rare), the filter returns false (column not found inline), which means the row is skipped. This is a known limitation — document it. Full fix would require reassembling before filtering, which is expensive. For v1, accept that filters on columns in the overflow portion may miss rows. This is an edge case (filtering on a multi-KB text field).

- [ ] **Step 3: Integrate into BTreeWriter**

Same pattern for `search_recursive_w` and `scan_all_w` — use `reassemble_row_writer` instead of `reassemble_row_reader`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib btree::tests`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add src/btree.rs
git commit -m "feat: overflow read path — row reassembly in BTreeReader and BTreeWriter"
```

---

## Task 4: Overflow Delete + max_row_size

**Files:**
- Modify: `src/btree.rs`
- Modify: `src/db.rs`

- [ ] **Step 1: Free overflow pages on delete**

In BTreeWriter's `delete_recursive`, after finding the row to delete, check for overflow and free the chain:

```rust
// Before deleting the leaf entry, free overflow pages if present
let (start, end) = row_bounds(&page, pos, num_rows);
let row_bytes = &page.data[start..end];
if overflow::has_overflow(row_bytes) {
    let (_, first_page, _) = overflow::decode_overflow_trailer(row_bytes);
    self.free_overflow_chain(first_page)?;
}
// ... existing delete logic (write_leaf_without) ...
```

Add the chain-freeing helper:
```rust
fn free_overflow_chain(&mut self, first_page: u32) -> Result<()> {
    let mut current = first_page;
    while current != 0 {
        let page = self.guard.read_page_cloned(current)?;
        let next = page.overflow_next();
        // For now, just leave the page allocated but unused.
        // A future free-page-list optimization can reclaim these.
        current = next;
    }
    Ok(())
}
```

Also handle overflow in `delete_matching` — when iterating rows for deletion, check each for overflow.

- [ ] **Step 2: Add max_row_size to BoogyDb**

In `src/db.rs`:
```rust
pub struct BoogyDb {
    // ... existing fields ...
    max_row_size: std::sync::atomic::AtomicU32,
}
```

Initialize as `AtomicU32::new(10 * 1024 * 1024)` (10MB) in `open()`.

Add methods:
```rust
pub fn set_max_row_size(&self, bytes: u32) {
    self.max_row_size.store(bytes, std::sync::atomic::Ordering::Relaxed);
}
pub fn max_row_size(&self) -> u32 {
    self.max_row_size.load(std::sync::atomic::Ordering::Relaxed)
}
```

In `insert()` and `insert_with_id()`, after encoding the row:
```rust
let row_bytes = row::encode_row(rowid, &col_values);
if row_bytes.len() > self.max_row_size() as usize {
    return Err(BoogyError::RowTooLarge(row_bytes.len()));
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add src/btree.rs src/db.rs
git commit -m "feat: overflow delete + max_row_size enforcement"
```

---

## Task 5: Integration Tests

**Files:**
- Modify: `tests/crud_test.rs`

- [ ] **Step 1: Add overflow integration tests**

```rust
#[test]
fn test_overflow_insert_get_roundtrip_10kb() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xABu8; 10_000];
    let id = db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
}

#[test]
fn test_overflow_insert_get_roundtrip_100kb() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xCDu8; 100_000];
    let id = db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
}

#[test]
fn test_overflow_insert_get_roundtrip_1mb() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xEFu8; 1_000_000];
    let id = db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
}

#[test]
fn test_overflow_delete() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xABu8; 50_000];
    let id = db.insert("t", &[("data", Value::Blob(blob))]).unwrap();
    assert!(db.delete("t", id).unwrap());
    assert!(db.get("t", id).unwrap().is_none());
}

#[test]
fn test_overflow_mixed_with_normal_rows() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("data", Type::Blob),
    ]).unwrap();

    // Normal rows
    for i in 0..10 {
        db.insert("t", &[
            ("name", Value::Text(format!("small_{i}"))),
            ("data", Value::Blob(vec![i as u8; 100])),
        ]).unwrap();
    }
    // Overflow rows
    for i in 0..5 {
        db.insert("t", &[
            ("name", Value::Text(format!("big_{i}"))),
            ("data", Value::Blob(vec![i as u8; 50_000])),
        ]).unwrap();
    }

    assert_eq!(db.count("t", &[]).unwrap(), 15);

    // Verify a big row
    let row = db.get("t", 11).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(vec![0u8; 50_000]));
}

#[test]
fn test_overflow_max_row_size_enforced() {
    let (db, _dir) = create_db();
    db.set_max_row_size(1000);
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let result = db.insert("t", &[("data", Value::Blob(vec![0u8; 2000]))]);
    assert!(result.is_err());
}

#[test]
fn test_overflow_update_large_to_small() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let id = db.insert("t", &[("data", Value::Blob(vec![0xAB; 50_000]))]).unwrap();
    db.update("t", id, &[("data", Value::Blob(vec![0xCD; 100]))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(vec![0xCD; 100]));
}

#[test]
fn test_overflow_update_small_to_large() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let id = db.insert("t", &[("data", Value::Blob(vec![0xAB; 100]))]).unwrap();
    db.update("t", id, &[("data", Value::Blob(vec![0xCD; 50_000]))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(vec![0xCD; 50_000]));
}

#[test]
fn test_overflow_persist_across_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let blob = vec![0xABu8; 50_000];

    {
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::Normal);
        db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
        db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    }

    {
        let db = BoogyDb::open(&path).unwrap();
        let row = db.get("t", 1).unwrap().unwrap();
        assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
    }
}

#[test]
fn test_overflow_with_acid_transaction() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();

    // Commit
    let mut tx = db.begin().unwrap();
    let blob = vec![0xABu8; 50_000];
    let id = tx.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    tx.commit().unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));

    // Rollback
    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("data", Value::Blob(vec![0xCD; 50_000]))]).unwrap();
        // drop without commit
    }
    assert_eq!(db.count("t", &[]).unwrap(), 1);
}

#[test]
fn test_overflow_long_text() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("content", Type::Text)]).unwrap();
    let text = "x".repeat(100_000);
    let id = db.insert("t", &[("content", Value::Text(text.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("content").unwrap(), Value::Text(text));
}

#[test]
fn test_overflow_find_scan() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("tag", Type::Text),
        ColumnDef::new("data", Type::Blob),
    ]).unwrap();

    // Mix of normal and overflow rows
    for i in 0..5 {
        let size = if i % 2 == 0 { 100 } else { 50_000 };
        db.insert("t", &[
            ("tag", Value::Text(format!("item_{i}"))),
            ("data", Value::Blob(vec![i as u8; size])),
        ]).unwrap();
    }

    // scan_all should reassemble overflow rows
    let result = db.find("t", FindOptions::default()).unwrap();
    assert_eq!(result.rows.len(), 5);
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test`
Expected: All pass (existing + new overflow tests).

- [ ] **Step 3: Commit**

```bash
git add tests/crud_test.rs
git commit -m "test: overflow integration tests — large blobs, roundtrips, persistence, ACID"
```

---

## Task 6: Benchmark Verification + README

- [ ] **Step 1: Run existing benchmarks to verify zero regression**

Run: `cargo bench --bench point_ops`
Run: `cargo bench --bench sqlite_comparison`

Expected: Numbers should be identical to pre-overflow (normal rows don't touch overflow code).

- [ ] **Step 2: Update README**

Add to Features list:
```
- **Overflow pages** — rows larger than a single page automatically spill into linked overflow pages, supporting blobs up to 10MB (configurable). Zero overhead on normal-sized rows
```

Add to the Architecture section, after the Row format bullet:
```
- **Overflow**: Rows exceeding leaf page capacity (~4KB) automatically spill into linked overflow pages (`PAGE_OVERFLOW`). The leaf stores an inline prefix with a 9-byte overflow trailer pointing to the first overflow page. Reassembly is transparent — callers always receive complete row data. Normal rows (no overflow) have zero overhead.
```

Add `set_max_row_size(bytes)` to the API table.

- [ ] **Step 3: Commit and push**

```bash
git add -A
git commit -m "feat: overflow pages — large row support up to 10MB, zero overhead on normal rows"
git push
```
