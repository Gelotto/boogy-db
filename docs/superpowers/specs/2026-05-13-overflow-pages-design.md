# Overflow Pages — Large Row Support

## Problem

Rows must currently fit within a single leaf page (~4060 usable bytes). Large blobs, long text, or many columns cause inserts to fail. SQLite handles this transparently via overflow page chains. boogy-db needs the same.

## Design

### Zero Overhead for Normal Rows

Overflow is invisible to rows that fit on a leaf page. No format changes, no extra bytes, no additional checks beyond one byte comparison on reads.

### Overflow Marker

When a row spills to overflow pages, the leaf entry stores an inline prefix followed by a 9-byte marker:

```
[inline_row_bytes...][0xFF][first_overflow_page:4 LE][remaining_len:4 LE]
```

- `0xFF` — overflow flag. Normal type tags are 0-5, so this is unambiguous.
- `first_overflow_page` — page number of the first overflow page.
- `remaining_len` — total bytes across all overflow pages.

Detection: `row.len() >= 9 && row[row.len() - 9] == 0xFF`.

### Overflow Page Layout

New page type flag `PAGE_OVERFLOW = 0x10`:

```
[page_header: 16 bytes]
  magic(2) | flags(2)=0x10 | payload_len(2) | reserved(2)
  | next_overflow(4) | reserved(4)
[payload: up to 4076 bytes]
[checksum: 4]
```

Usable payload: 4076 bytes. A 1MB row uses ~252 overflow pages linked via `next_overflow`.

### Write Path

In `BTreeWriter::insert_into_leaf`, when the row doesn't fit:

1. Calculate inline bytes: `available_space - 9` (reserve for marker).
2. If even rowid + offset directory + marker doesn't fit: split the leaf first, retry.
3. Allocate overflow pages via `guard.allocate_page()`, fill with row data chunks, link via `next_overflow`.
4. Append `[0xFF][first_page:4][remaining:4]` to the inline portion.
5. Store inline portion on the leaf page.

### Read Path

After extracting row bytes from a leaf, check for overflow:

```rust
fn has_overflow(row_bytes: &[u8]) -> bool {
    row_bytes.len() >= 9 && row_bytes[row_bytes.len() - 9] == 0xFF
}
```

If overflow: read inline prefix, walk the overflow chain concatenating payloads, return the complete row. Called by search, scan_all, scan_filtered (for matching rows), scan_all_w.

The Row type doesn't change — it receives fully reassembled bytes. Overflow is invisible above the B+ tree layer.

### Delete Path

When deleting a row with overflow: walk the chain, free each overflow page (for now, just abandon — no free list). Delete the inline entry from the leaf normally.

### Filter Evaluation

`scan_filtered` uses `extract_column_raw` on inline bytes. If the filtered column is fully inline (common — short indexed columns), it works without reassembly. If the column is in the overflow portion, `extract_column_raw` returns None and we fall back to full reassembly + decode.

### Maximum Row Size

```rust
db.set_max_row_size(10 * 1024 * 1024); // 10MB default
```

`max_row_size: AtomicU32` on BoogyDb. Checked before insert. Returns `BoogyError::RowTooLarge` if exceeded.

### Encryption

Overflow pages for encrypted tables use the same per-table cipher. Registered in `page_ciphers` during commit.

### Error Types

Add `RowTooLarge(usize)` to BoogyError.

### Performance Impact

Normal rows: one byte comparison per read (~1ns). Write path unchanged. Overflow rows: I/O proportional to size (inherent). Benchmark verification required to confirm zero regression.

## Files Changed

- `src/page.rs` — PAGE_OVERFLOW flag, overflow page accessors
- `src/row.rs` — `has_overflow`, marker encode/decode helpers
- `src/btree.rs` — Overflow write in insert, reassembly in read, cleanup in delete (both Reader and Writer)
- `src/db.rs` — max_row_size field, RowTooLarge check
- `src/error.rs` — RowTooLarge variant
- `tests/crud_test.rs` — Overflow integration tests

## Test Coverage

### Unit Tests
- has_overflow detection (positive, negative, edge cases)
- Overflow marker encode/decode roundtrip
- Overflow page create/read payload

### Integration Tests
- Insert+get roundtrip: 10KB, 100KB, 1MB rows
- Row at exact page boundary (no overflow) works
- Row 1 byte over boundary triggers overflow
- Delete overflow row
- Update overflow→normal and normal→overflow
- Scan/find with mixed overflow and normal rows
- Filter on inline column (no reassembly needed)
- Filter on overflow column (reassembly fallback)
- max_row_size enforcement (RowTooLarge error)
- Overflow + encryption
- Overflow + index
- Overflow + ACID transaction (commit + rollback)
- Overflow persistence across reopen

### Benchmark Verification
- Run all existing benchmarks, verify zero regression on normal rows
