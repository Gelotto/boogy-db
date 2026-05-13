# Modifying the Page Format

All pages are `PAGE_SIZE` (4096) bytes, defined in `src/page.rs`.

### Layout Summary

**Header** (16 bytes): `[magic:2][flags:2][num_rows:2][free_space_offset:2][next_leaf:4][prev_leaf:4]`
Flags: `PAGE_LEAF=0x01`, `PAGE_BRANCH=0x02`, `PAGE_SYSTEM=0x04`, `PAGE_FREE=0x08`

**Leaf data**: `[row offset array: N*2 bytes][row data packed sequentially]`
**Branch data**: `[entries: N*12 bytes, each [child:4][key:8]][rightmost_child:4]`
**System page 0**: `[magic:4 (0xB00D_5150)][next_table_id:4][num_tables:2][table entries...]`
**Checksum**: Last 4 bytes, CRC32 of `[0..4092]`

## Adding a New Page Type

1. Add flag constant in `page.rs`: `pub const PAGE_MY_TYPE: u16 = 0x10;`
2. Add helper: `pub fn is_my_type(&self) -> bool { self.flags() & PAGE_MY_TYPE != 0 }`
3. Add constructor (`new_my_type`) setting magic, flags, and calling `update_checksum()`.
4. Always call `page.update_checksum()` after any data modification.

## Changing the Leaf Page Layout

1. Update `row_bounds` and `row_bounds_raw` in `btree.rs` -- they compute `(start, end)` byte ranges for each row from the offset array at `PAGE_HEADER_SIZE`.
2. Update all leaf page writer functions in `btree.rs`: `write_leaf_with_insert`, `write_leaf_without`, `write_leaf_without_multiple`, `write_leaf_with_replacements`, `write_leaf_range`.
3. Update index leaf writers in `index.rs`: `write_idx_leaf_with_insert`, `write_idx_leaf_without`, `write_idx_leaf_range`.
4. Update `Page::free_space()` if data packing changed.

## Changing the Branch Page Layout

Branch entries: `BRANCH_ENTRY_SIZE = 12` in `btree.rs` (`[child:4][key:8]`).
Index branch entries: `IDX_BRANCH_ENTRY_SIZE = 42` in `index.rs` (`[child:4][key_len:2][key_data:36]`).

Update: `get_branch_child`, `get_branch_key`, `write_branch_entry`, `find_child`, `collect_branch_flat`, `rebuild_branch_flat` (and the `idx_` variants in `index.rs`).

## Compatibility

There is no on-disk version number yet. If you change the page format:

1. Existing database files will not be readable.
2. Consider adding a version field to the system page header.
3. The WAL entry format (`wal.rs`) stores raw page bytes -- it will also be incompatible.

## Checklist

- [ ] New flag constant added to `page.rs` if adding a page type
- [ ] All leaf page writer functions updated in `btree.rs`
- [ ] All index leaf page writer functions updated in `index.rs`
- [ ] `row_bounds` / `row_bounds_raw` updated if header changed
- [ ] `Page::free_space()` updated if data packing changed
- [ ] Checksum called after every page modification
- [ ] `cargo test` passes (especially `btree::tests` and `db::tests`)
