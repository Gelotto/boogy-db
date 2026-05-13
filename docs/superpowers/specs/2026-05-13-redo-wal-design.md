# Redo-Log WAL — Halve Write I/O for Normal Durability

## Problem

With `Durability::Normal`, every commit does TWO disk writes per dirty page: one to the data file, one to the WAL (before-images). SQLite's WAL mode does ONE write per page (after-images to WAL only, data file deferred to checkpoint). This 2x I/O gap is why our Normal-mode bulk update (500K r/s) loses to SQLite (1.07M r/s) despite winning in None mode (1.3M r/s).

## Design

Switch from undo-log WAL (stores before-images) to redo-log WAL (stores after-images). Commits write only to the WAL, never to the data file. The data file is updated on checkpoint (clean shutdown or explicit call).

### Commit Flow (all durability modes)

**Durability::Immediate:**
1. Write after-images (dirty pages) to WAL
2. Fsync WAL
3. Publish dirty pages to in-memory cache
4. (Data file NOT touched — flushed on checkpoint)

**Durability::Normal:**
1. Write after-images (dirty pages) to WAL (no fsync — OS buffer survives process crash)
2. Publish dirty pages to in-memory cache
3. (Data file NOT touched)

**Durability::None:**
1. Publish dirty pages to in-memory cache
2. (No WAL, no disk writes)

One disk write per page for Immediate/Normal (was two). Zero for None (unchanged).

### Crash Recovery (on open)

Current (undo-log): replay WAL backward to restore before-images → undo uncommitted changes.

New (redo-log): replay WAL forward to apply after-images → redo committed changes.

```
1. Open WAL, read all entries
2. Open data file
3. For each WAL entry (in order): write page data to data file at page_no offset
4. Fsync data file
5. Truncate WAL
```

After recovery, the data file has all committed changes and the WAL is empty.

### Checkpoint (clean shutdown / Drop)

Same as crash recovery but without the "open WAL" step — just flush all cached pages to the data file and truncate the WAL:

```
1. Write all cached pages to data file (sync_all)
2. Truncate WAL
```

This happens in `BoogyDb::drop()`.

### WAL Entry Format

Unchanged — just the semantic meaning changes:

```
[sequence: 8][table_id: 4][page_no: 4][page_data: 4096][checksum: 4]
```

The `page_data` now contains the NEW page version (after-image) instead of the old version (before-image).

### API Changes

**wal.rs:**
- Rename `append_before_image` to `append_page_image` (or just keep the name and change the semantic — it's the same function)

**file.rs WriteGuard:**
- `commit()` returns after-images (dirty page data) instead of before-images
- Stop capturing before-images entirely — remove `before_images` field from `WriteState`
- Remove `capture_before_images` flag (no longer needed)
- `commit(flush_to_disk: bool)` → `commit()` (never flushes to disk — WAL handles persistence)

New signature:
```rust
pub fn commit(self) -> Result<Vec<(u32, [u8; PAGE_SIZE])>>
```

Returns `Vec<(page_no, new_page_data)>` — the after-images for WAL.

**db.rs commit_write:**
```rust
fn commit_write(guard: WriteGuard, wal: &Mutex<Wal>, durability: Durability, table_id: u32) -> Result<()> {
    let after_images = guard.commit()?;
    match durability {
        Durability::Immediate => {
            let mut wal = wal.lock().unwrap();
            for (page_no, data) in &after_images {
                wal.append_page_image(table_id, *page_no, data)?;
            }
            wal.sync()?;
        }
        Durability::Normal => {
            let mut wal = wal.lock().unwrap();
            for (page_no, data) in &after_images {
                wal.append_page_image(table_id, *page_no, data)?;
            }
        }
        Durability::None => {
            // after_images dropped — no WAL write
        }
    }
    Ok(())
}
```

Note: WAL is no longer truncated after each Immediate commit. It's truncated on checkpoint (shutdown). This allows multiple commits to accumulate in the WAL, which is correct for redo-log semantics.

**db.rs open() crash recovery:**
```rust
// Redo: apply after-images (forward replay)
let mut wal = Wal::open(&wal_path)?;
if wal.entry_count() > 0 {
    let file = PageFile::open(&path)?;
    let entries = wal.read_entries()?;
    for entry in &entries {  // forward order, not reversed
        let page = Page::from_bytes_unchecked(entry.page_data);
        file.put_page_direct(entry.page_no, page);
    }
    file.sync_all()?;
    wal.truncate()?;
}
```

**db.rs Drop (checkpoint):**
```rust
impl Drop for BoogyDb {
    fn drop(&mut self) {
        // Checkpoint: flush cache to data file, truncate WAL
        let (metas, next_id) = self.snapshot_table_metas();
        let mut guard = self.file.begin_write();
        // ... serialize and put system page ...
        let _ = guard.commit(); // publish to cache (no disk I/O)
        let _ = self.file.sync_all(); // flush all cached pages to data file
        if let Ok(mut wal) = self.wal.lock() {
            let _ = wal.truncate(); // WAL no longer needed
        }
    }
}
```

**db.rs set_durability:**
- Remove `set_capture_before_images` call (no before-images to capture)

## Files Changed

- `src/wal.rs` — Rename method (optional, semantic change only)
- `src/file.rs` — Remove before_images from WriteState, simplify commit()
- `src/db.rs` — Rewrite commit_write, crash recovery, Drop, remove set_capture_before_images

## Performance Target

Bulk update ~1K rows (Normal): >1M r/s (currently 500K, SQLite 1.07M)

By halving the per-commit disk I/O (one WAL write instead of WAL + data file), Normal durability should approach None-mode performance.
