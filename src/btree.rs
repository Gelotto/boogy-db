use crate::error::{BoogyError, Result};
use crate::file::{PageFile, WriteGuard};
use crate::page::{Page, PAGE_BRANCH, PAGE_HEADER_SIZE, PAGE_LEAF, PAGE_SIZE};
use crate::row;

/// Checksum occupies the last 4 bytes of each page.
const CHECKSUM_SIZE: usize = 4;

// ===========================================================================
// BTreeReader — read-only access via &PageFile
// ===========================================================================

pub struct BTreeReader<'a> {
    file: &'a PageFile,
    root: u32,
}

impl<'a> BTreeReader<'a> {
    pub fn new(file: &'a PageFile, root: u32) -> Self {
        Self { file, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    /// Search for a row by rowid. Returns the raw row bytes if found.
    pub fn search(&self, rowid: u64) -> Result<Option<Vec<u8>>> {
        self.search_recursive(self.root, rowid)
    }

    /// Iterate all rows in key order. Returns (rowid, row_bytes) pairs.
    pub fn scan_all(&self) -> Result<Vec<(u64, Vec<u8>)>> {
        let first_leaf = self.find_leftmost_leaf(self.root)?;
        let mut results = Vec::new();
        let mut current = first_leaf;
        loop {
            let page = self.file.read_page(current)?;
            let num_rows = page.num_rows() as usize;
            for i in 0..num_rows {
                let (start, end) = row_bounds(&page, i, num_rows);
                if start < end && end <= PAGE_SIZE {
                    let data = &page.data[start..end];
                    if let Ok(id) = row::extract_id(data) {
                        results.push((id, data.to_vec()));
                    }
                }
            }
            let next = page.next_leaf();
            if next == 0 {
                break;
            }
            current = next;
        }
        Ok(results)
    }

    /// Batch-fetch rows by sorted rowids. Finds the first rowid's leaf via
    /// tree traversal, then walks the leaf chain collecting matches.
    /// Much faster than N individual searches for clustered rowids.
    /// `rowids` MUST be sorted ascending.
    pub fn multi_get_sorted(&self, rowids: &[u64]) -> Result<Vec<Vec<u8>>> {
        if rowids.is_empty() {
            return Ok(Vec::new());
        }
        // Find the leaf containing the smallest rowid
        let leaf = self.find_leaf_for_rowid(self.root, rowids[0])?;
        let mut results = Vec::with_capacity(rowids.len());
        let mut rid_idx = 0;
        let mut current = leaf;

        while rid_idx < rowids.len() {
            let page = self.file.read_page(current)?;
            let num_rows = page.num_rows() as usize;
            for i in 0..num_rows {
                let (start, end) = row_bounds(&page, i, num_rows);
                if start >= end || end > PAGE_SIZE {
                    continue;
                }
                let data = &page.data[start..end];
                if let Ok(row_id) = row::extract_id(data) {
                    // Skip past rowids smaller than current target
                    while rid_idx < rowids.len() && rowids[rid_idx] < row_id {
                        rid_idx += 1;
                    }
                    if rid_idx >= rowids.len() {
                        return Ok(results);
                    }
                    if rowids[rid_idx] == row_id {
                        results.push(data.to_vec());
                        rid_idx += 1;
                    }
                }
            }
            if rid_idx >= rowids.len() {
                break;
            }
            let next = page.next_leaf();
            if next == 0 {
                break;
            }
            current = next;
        }
        Ok(results)
    }

    /// Scan rows, evaluating a filter on raw page bytes using extract_column.
    /// Only decodes and collects rows that pass the filter.
    /// Returns (matching rows as raw bytes, total matching count).
    pub fn scan_filtered(
        &self,
        filter_col_id: u16,
        filter_op: crate::filter::FilterOp,
        filter_val: &crate::value::Value,
        limit: Option<u32>,
        offset: Option<u32>,
        stop_after: Option<u64>,
    ) -> Result<(Vec<(u64, Vec<u8>)>, u64)> {
        let first_leaf = self.find_leftmost_leaf(self.root)?;
        let mut total: u64 = 0;
        let mut results = Vec::new();
        let skip = offset.unwrap_or(0) as u64;
        let take = limit.unwrap_or(u32::MAX) as u64;
        let mut current = first_leaf;

        loop {
            let arc = self.file.read_page(current)?;
            let page_data = &arc.data;
            let num_rows = arc.num_rows() as usize;
            let next = arc.next_leaf();

            for i in 0..num_rows {
                let (start, end) = row_bounds_raw(page_data, i, num_rows);
                if start >= end || end > PAGE_SIZE {
                    continue;
                }
                let data = &page_data[start..end];

                // Try zero-alloc raw comparison first, fall back to decode
                let matches = if let Ok(Some(raw)) = row::extract_column_raw(data, filter_col_id) {
                    if let Some(result) = crate::filter::eval_filter_raw(raw, &filter_op, filter_val) {
                        result
                    } else {
                        let col_val = row::extract_column(data, filter_col_id)?;
                        let actual = col_val.as_ref().unwrap_or(&crate::value::Value::Null);
                        crate::filter::eval_filter_op(actual, &filter_op, filter_val)
                    }
                } else {
                    let col_val = row::extract_column(data, filter_col_id)?;
                    let actual = col_val.as_ref().unwrap_or(&crate::value::Value::Null);
                    crate::filter::eval_filter_op(actual, &filter_op, filter_val)
                };

                if matches {
                    total += 1;
                    if total > skip && (total - skip) <= take {
                        if let Ok(id) = row::extract_id(data) {
                            results.push((id, data.to_vec()));
                        }
                    }
                    if let Some(max) = stop_after {
                        if total >= max {
                            return Ok((results, total));
                        }
                    }
                }
            }
            if next == 0 {
                break;
            }
            current = next;
        }
        Ok((results, total))
    }

    /// Count rows matching a filter using extract_column on raw bytes.
    pub fn count_filtered(
        &self,
        filter_col_id: u16,
        filter_op: crate::filter::FilterOp,
        filter_val: &crate::value::Value,
    ) -> Result<u64> {
        let first_leaf = self.find_leftmost_leaf(self.root)?;
        let mut count: u64 = 0;
        let mut current = first_leaf;

        loop {
            let arc = self.file.read_page(current)?;
            let page_data = &arc.data;
            let num_rows = arc.num_rows() as usize;
            let next = arc.next_leaf();

            for i in 0..num_rows {
                let (start, end) = row_bounds_raw(page_data, i, num_rows);
                if start >= end || end > PAGE_SIZE {
                    continue;
                }
                let data = &page_data[start..end];
                let matches = if let Ok(Some(raw)) = row::extract_column_raw(data, filter_col_id) {
                    if let Some(result) = crate::filter::eval_filter_raw(raw, &filter_op, filter_val) {
                        result
                    } else {
                        let col_val = row::extract_column(data, filter_col_id)?;
                        let actual = col_val.as_ref().unwrap_or(&crate::value::Value::Null);
                        crate::filter::eval_filter_op(actual, &filter_op, filter_val)
                    }
                } else {
                    let col_val = row::extract_column(data, filter_col_id)?;
                    let actual = col_val.as_ref().unwrap_or(&crate::value::Value::Null);
                    crate::filter::eval_filter_op(actual, &filter_op, filter_val)
                };

                if matches {
                    count += 1;
                }
            }
            if next == 0 {
                break;
            }
            current = next;
        }
        Ok(count)
    }

    // --- Internal methods ---

    fn find_leftmost_leaf(&self, page_no: u32) -> Result<u32> {
        let page = self.file.read_page(page_no)?;
        if page.is_leaf() {
            Ok(page_no)
        } else {
            let child = get_branch_child(&page, 0);
            self.find_leftmost_leaf(child)
        }
    }

    fn search_recursive(&self, page_no: u32, rowid: u64) -> Result<Option<Vec<u8>>> {
        let page = self.file.read_page(page_no)?;

        if page.is_leaf() {
            let num_rows = page.num_rows() as usize;
            if num_rows == 0 {
                return Ok(None);
            }

            // Binary search for the target rowid.
            let (pos, found) = find_insertion_point(&page, rowid)?;
            if found {
                let (start, end) = row_bounds(&page, pos, num_rows);
                if start < end && end <= PAGE_SIZE {
                    return Ok(Some(page.data[start..end].to_vec()));
                }
            }
            Ok(None)
        } else {
            let (_, child_page_no) = find_child(&page, rowid);
            self.search_recursive(child_page_no, rowid)
        }
    }

    /// Find the leaf page containing (or that would contain) the given rowid.
    fn find_leaf_for_rowid(&self, page_no: u32, rowid: u64) -> Result<u32> {
        let page = self.file.read_page(page_no)?;
        if page.is_leaf() {
            Ok(page_no)
        } else {
            let (_, child) = find_child(&page, rowid);
            self.find_leaf_for_rowid(child, rowid)
        }
    }
}

// ===========================================================================
// BTreeWriter — write access via &mut WriteGuard
// ===========================================================================

pub struct BTreeWriter<'a, 'b> {
    guard: &'a mut WriteGuard<'b>,
    root: u32,
}

impl<'a, 'b> BTreeWriter<'a, 'b> {
    pub fn new(guard: &'a mut WriteGuard<'b>, root: u32) -> Self {
        Self { guard, root }
    }

    pub fn root_page(&self) -> u32 {
        self.root
    }

    /// Create a new empty B+ tree (single empty leaf page).
    pub fn create(guard: &mut WriteGuard) -> Result<u32> {
        let page_no = guard.allocate_page()?;
        let page = Page::new_leaf();
        guard.put_page(page_no, page);
        Ok(page_no)
    }

    /// Insert a row. Returns the (possibly new) root page number.
    pub fn insert(&mut self, rowid: u64, row_data: &[u8]) -> Result<u32> {
        let result = self.insert_recursive(self.root, rowid, row_data)?;
        match result {
            InsertResult::Fit => Ok(self.root),
            InsertResult::Split {
                new_page,
                separator,
            } => {
                let new_root = self.guard.allocate_page()?;
                let mut root_page = Page::new_branch();
                write_branch_entry(&mut root_page, 0, self.root, separator);
                set_branch_child(&mut root_page, 1, new_page);
                root_page.set_num_rows(1);
                root_page.update_checksum();
                self.guard.put_page(new_root, root_page);
                self.root = new_root;
                Ok(self.root)
            }
        }
    }

    /// Delete a row by rowid. Returns true if the row existed.
    pub fn delete(&mut self, rowid: u64) -> Result<bool> {
        self.delete_recursive(self.root, rowid)
    }

    /// Search for a row by rowid through the WriteGuard (sees dirty overlay).
    pub fn search(&self, rowid: u64) -> Result<Option<Vec<u8>>> {
        self.search_recursive_w(self.root, rowid)
    }

    /// Scan all rows through the WriteGuard (sees dirty overlay).
    pub fn scan_all_w(&self) -> Result<Vec<(u64, Vec<u8>)>> {
        let first_leaf = self.find_leftmost_leaf_w(self.root)?;
        let mut results = Vec::new();
        let mut current = first_leaf;
        loop {
            let page = self.guard.read_page_cloned(current)?;
            let num_rows = page.num_rows() as usize;
            for i in 0..num_rows {
                let (start, end) = row_bounds(&page, i, num_rows);
                if start < end && end <= PAGE_SIZE {
                    let data = &page.data[start..end];
                    if let Ok(id) = row::extract_id(data) {
                        results.push((id, data.to_vec()));
                    }
                }
            }
            let next = page.next_leaf();
            if next == 0 {
                break;
            }
            current = next;
        }
        Ok(results)
    }

    /// Delete all rows whose raw bytes satisfy `pred`. Walks the leaf chain once
    /// and rebuilds each modified page in a single pass (no per-row delete+reinsert).
    /// Returns the deleted rows as `(rowid, old_row_bytes)` pairs.
    pub fn delete_matching<F>(&mut self, pred: F) -> Result<Vec<(u64, Vec<u8>)>>
    where
        F: Fn(&[u8]) -> bool,
    {
        let first_leaf = self.find_leftmost_leaf_w(self.root)?;
        let mut deleted = Vec::new();
        let mut current = first_leaf;

        loop {
            let page = self.guard.read_page_cloned(current)?;
            let num_rows = page.num_rows() as usize;
            let next = page.next_leaf();
            let prev = page.prev_leaf();

            // Find matching row indices in this page.
            let mut match_indices = Vec::new();
            for i in 0..num_rows {
                let (start, end) = row_bounds_raw(&page.data, i, num_rows);
                if start >= end || end > PAGE_SIZE {
                    continue;
                }
                let data = &page.data[start..end];
                if pred(data) {
                    if let Ok(id) = row::extract_id(data) {
                        deleted.push((id, data.to_vec()));
                        match_indices.push(i);
                    }
                }
            }

            // Rebuild the page if any rows were deleted.
            if !match_indices.is_empty() {
                let snapshot = page.data;
                let wp = self.guard.write_page(current)?;
                write_leaf_without_multiple(wp, &snapshot, num_rows, &match_indices);
                wp.set_next_leaf(next);
                wp.set_prev_leaf(prev);
                wp.update_checksum();
            }

            if next == 0 {
                break;
            }
            current = next;
        }

        Ok(deleted)
    }

    /// Update all rows whose raw bytes satisfy `pred`. Walks the leaf chain once.
    /// For each matching row, calls `updater(old_bytes)` to get new bytes, then
    /// tries to replace in-place. If the page would overflow, the row is added to
    /// the overflow list instead.
    ///
    /// Returns `(updated_in_place, overflow)`.
    /// Each entry is `(rowid, old_bytes, new_bytes)`.
    pub fn update_matching<F, U>(
        &mut self,
        pred: F,
        updater: U,
    ) -> Result<(Vec<(u64, Vec<u8>, Vec<u8>)>, Vec<(u64, Vec<u8>, Vec<u8>)>)>
    where
        F: Fn(&[u8]) -> bool,
        U: Fn(&[u8]) -> Vec<u8>,
    {
        let first_leaf = self.find_leftmost_leaf_w(self.root)?;
        let mut updated = Vec::new();
        let mut overflow = Vec::new();
        let mut current = first_leaf;

        loop {
            let page = self.guard.read_page_cloned(current)?;
            let num_rows = page.num_rows() as usize;
            let next = page.next_leaf();

            // Collect matches and their replacements.
            let mut replacements: Vec<(usize, Vec<u8>, u64, Vec<u8>)> = Vec::new();
            for i in 0..num_rows {
                let (start, end) = row_bounds_raw(&page.data, i, num_rows);
                if start >= end || end > PAGE_SIZE {
                    continue;
                }
                let data = &page.data[start..end];
                if pred(data) {
                    let new_bytes = updater(data);
                    if let Ok(id) = row::extract_id(data) {
                        replacements.push((i, new_bytes, id, data.to_vec()));
                    }
                }
            }

            if !replacements.is_empty() {
                // Build the replacement slice for write_leaf_with_replacements.
                let repl_refs: Vec<(usize, &[u8])> = replacements
                    .iter()
                    .map(|(idx, new_bytes, _, _)| (*idx, new_bytes.as_slice()))
                    .collect();

                let snapshot = page.data;
                let wp = self.guard.write_page(current)?;
                let fits = write_leaf_with_replacements(wp, &snapshot, num_rows, &repl_refs);

                if fits {
                    // All replacements fit in-place.
                    for (_, new_bytes, id, old_bytes) in replacements {
                        updated.push((id, old_bytes, new_bytes));
                    }
                } else {
                    // Overflow: restore the original page data.
                    wp.data = snapshot;
                    for (_, new_bytes, id, old_bytes) in replacements {
                        overflow.push((id, old_bytes, new_bytes));
                    }
                }
            }

            if next == 0 {
                break;
            }
            current = next;
        }

        Ok((updated, overflow))
    }

    // --- Internal methods ---

    fn search_recursive_w(&self, page_no: u32, rowid: u64) -> Result<Option<Vec<u8>>> {
        // Zero-copy branch navigation (same pattern as delete_recursive)
        let (is_leaf, child) = if let Some(p) = self.guard.peek_dirty(page_no) {
            if p.is_leaf() {
                (true, 0)
            } else {
                let (_, c) = find_child(p, rowid);
                (false, c)
            }
        } else {
            let arc = self.guard.page_file().read_page(page_no)?;
            if arc.is_leaf() {
                (true, 0)
            } else {
                let (_, c) = find_child(&arc, rowid);
                (false, c)
            }
        };

        if !is_leaf {
            return self.search_recursive_w(child, rowid);
        }

        let page = self.guard.read_page_cloned(page_no)?;
        let num_rows = page.num_rows() as usize;
        if num_rows == 0 {
            return Ok(None);
        }
        let (pos, found) = find_insertion_point(&page, rowid)?;
        if !found {
            return Ok(None);
        }
        let (start, end) = row_bounds(&page, pos, num_rows);
        if start < end && end <= PAGE_SIZE {
            Ok(Some(page.data[start..end].to_vec()))
        } else {
            Ok(None)
        }
    }

    /// Navigate branch pages to find the leftmost leaf page.
    fn find_leftmost_leaf_w(&self, page_no: u32) -> Result<u32> {
        if let Some(p) = self.guard.peek_dirty(page_no) {
            if p.is_leaf() {
                return Ok(page_no);
            }
            let child = get_branch_child(p, 0);
            return self.find_leftmost_leaf_w(child);
        }
        let arc = self.guard.page_file().read_page(page_no)?;
        if arc.is_leaf() {
            return Ok(page_no);
        }
        let child = get_branch_child(&arc, 0);
        self.find_leftmost_leaf_w(child)
    }

    fn insert_recursive(
        &mut self,
        page_no: u32,
        rowid: u64,
        row_data: &[u8],
    ) -> Result<InsertResult> {
        // Check dirty overlay first (zero-copy), then cache (Arc deref without clone).
        // Only clone at the leaf where we need page data for rebuild.
        let (is_leaf, child_idx, child_page_no) = if let Some(p) = self.guard.peek_dirty(page_no) {
            if p.is_leaf() {
                (true, 0, 0)
            } else {
                let (ci, cp) = find_child(p, rowid);
                (false, ci, cp)
            }
        } else {
            let arc = self.guard.page_file().read_page(page_no)?;
            if arc.is_leaf() {
                (true, 0, 0)
            } else {
                let (ci, cp) = find_child(&arc, rowid);
                (false, ci, cp)
            }
        };

        if is_leaf {
            let page = self.guard.read_page_cloned(page_no)?;
            self.insert_into_leaf(page_no, &page, rowid, row_data)
        } else {
            let result = self.insert_recursive(child_page_no, rowid, row_data)?;
            match result {
                InsertResult::Fit => Ok(InsertResult::Fit),
                InsertResult::Split {
                    new_page,
                    separator,
                } => self.insert_into_branch(page_no, child_idx, separator, new_page),
            }
        }
    }

    fn insert_into_leaf(
        &mut self,
        page_no: u32,
        page: &Page,
        rowid: u64,
        row_data: &[u8],
    ) -> Result<InsertResult> {
        let num_rows = page.num_rows() as usize;

        // Binary search for insertion point and duplicate check.
        let (pos, found) = find_insertion_point(page, rowid)?;
        if found {
            return Err(BoogyError::DuplicateKey(rowid));
        }

        // Check if the new row fits in the current page.
        let offset_array_start = PAGE_HEADER_SIZE + num_rows * 2;
        let current_free = page.free_space_offset() as usize;
        let existing_data_size = if current_free > offset_array_start {
            current_free - offset_array_start
        } else {
            0
        };
        let needed = PAGE_HEADER_SIZE
            + (num_rows + 1) * 2
            + existing_data_size
            + row_data.len()
            + CHECKSUM_SIZE;

        if needed <= PAGE_SIZE {
            // --- Fits: in-place insert ---
            let snapshot = page.data;

            let page = self.guard.write_page(page_no)?;
            write_leaf_with_insert(page, &snapshot, num_rows, pos, row_data);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            // --- Split ---
            let snapshot = page.data;
            let next_leaf = page.next_leaf();
            let prev_leaf = page.prev_leaf();
            let total = num_rows + 1;
            let mid = total / 2;

            // Allocate right page first so we have its page_no for linking.
            let new_page_no = self.guard.allocate_page()?;

            // Extract separator: the _id of the first row in the right half.
            let separator = extract_id_at_virtual_pos(&snapshot, num_rows, pos, row_data, mid)?;

            // Write left half (indices 0..mid).
            let left_page = self.guard.write_page(page_no)?;
            write_leaf_range(left_page, &snapshot, num_rows, pos, row_data, 0, mid);
            left_page.set_next_leaf(new_page_no);
            left_page.set_prev_leaf(prev_leaf);
            left_page.update_checksum();

            // Write right half (indices mid..total).
            let mut right_page = Page::new_leaf();
            write_leaf_range(
                &mut right_page,
                &snapshot,
                num_rows,
                pos,
                row_data,
                mid,
                total,
            );
            right_page.set_prev_leaf(page_no);
            right_page.set_next_leaf(next_leaf);
            right_page.update_checksum();
            self.guard.put_page(new_page_no, right_page);

            // Fix up the old next page's prev pointer.
            if next_leaf != 0 {
                let np = self.guard.write_page(next_leaf)?;
                np.set_prev_leaf(new_page_no);
                np.update_checksum();
            }

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator,
            })
        }
    }

    fn insert_into_branch(
        &mut self,
        page_no: u32,
        child_idx: usize,
        separator: u64,
        new_child: u32,
    ) -> Result<InsertResult> {
        let page = self.guard.read_page_cloned(page_no)?;
        let num_keys = page.num_rows() as usize;

        let max_keys = (PAGE_SIZE - PAGE_HEADER_SIZE - 4 - CHECKSUM_SIZE) / BRANCH_ENTRY_SIZE;

        if num_keys < max_keys {
            let page = self.guard.write_page(page_no)?;
            insert_branch_entry(page, child_idx, separator, new_child);
            page.update_checksum();
            Ok(InsertResult::Fit)
        } else {
            let (children, keys) = collect_branch_flat(&page);
            let mut new_children = children;
            let mut new_keys = keys;
            new_keys.insert(child_idx, separator);
            new_children.insert(child_idx + 1, new_child);

            let total_keys = new_keys.len();
            let mid = total_keys / 2;
            let split_key = new_keys[mid];

            let left_keys = &new_keys[..mid];
            let left_children = &new_children[..mid + 1];
            let right_keys = &new_keys[mid + 1..];
            let right_children = &new_children[mid + 1..];

            let left_page = self.guard.write_page(page_no)?;
            rebuild_branch_flat(left_page, left_children, left_keys);
            left_page.update_checksum();

            let new_page_no = self.guard.allocate_page()?;
            let mut new_page = Page::new_branch();
            rebuild_branch_flat(&mut new_page, right_children, right_keys);
            new_page.update_checksum();
            self.guard.put_page(new_page_no, new_page);

            Ok(InsertResult::Split {
                new_page: new_page_no,
                separator: split_key,
            })
        }
    }

    fn delete_recursive(&mut self, page_no: u32, rowid: u64) -> Result<bool> {
        // Branch navigation without clone
        let (is_leaf, child_page_no) = if let Some(p) = self.guard.peek_dirty(page_no) {
            if p.is_leaf() {
                (true, 0)
            } else {
                let (_, cp) = find_child(p, rowid);
                (false, cp)
            }
        } else {
            let arc = self.guard.page_file().read_page(page_no)?;
            if arc.is_leaf() {
                (true, 0)
            } else {
                let (_, cp) = find_child(&arc, rowid);
                (false, cp)
            }
        };

        if !is_leaf {
            return self.delete_recursive(child_page_no, rowid);
        }

        // Leaf — clone for rebuild
        let page = self.guard.read_page_cloned(page_no)?;
        let num_rows = page.num_rows() as usize;
        if num_rows == 0 {
            return Ok(false);
        }

        let (pos, found) = find_insertion_point(&page, rowid)?;
        if !found {
            return Ok(false);
        }

        let snapshot = page.data;
        let next_leaf = page.next_leaf();
        let prev_leaf = page.prev_leaf();

        let page = self.guard.write_page(page_no)?;
        write_leaf_without(page, &snapshot, num_rows, pos);
        page.set_next_leaf(next_leaf);
        page.set_prev_leaf(prev_leaf);
        page.update_checksum();
        Ok(true)
    }
}

enum InsertResult {
    Fit,
    Split { new_page: u32, separator: u64 },
}

// ---------------------------------------------------------------------------
// Leaf page helpers
// ---------------------------------------------------------------------------

/// Compute the byte range [start, end) for row `i` in the page data.
fn row_bounds(page: &Page, i: usize, num_rows: usize) -> (usize, usize) {
    let start = page.row_offset(i as u16) as usize;
    let end = if i + 1 < num_rows {
        page.row_offset((i + 1) as u16) as usize
    } else {
        page.free_space_offset() as usize
    };
    (start, end)
}

/// Same as row_bounds but operates on a raw data snapshot.
fn row_bounds_raw(data: &[u8; PAGE_SIZE], i: usize, num_rows: usize) -> (usize, usize) {
    let start = raw_row_offset(data, i) as usize;
    let end = if i + 1 < num_rows {
        raw_row_offset(data, i + 1) as usize
    } else {
        raw_free_space_offset(data) as usize
    };
    (start, end)
}

fn raw_row_offset(data: &[u8; PAGE_SIZE], i: usize) -> u16 {
    let base = PAGE_HEADER_SIZE + i * 2;
    u16::from_le_bytes([data[base], data[base + 1]])
}

fn raw_free_space_offset(data: &[u8; PAGE_SIZE]) -> u16 {
    u16::from_le_bytes([data[6], data[7]])
}

/// Binary search within a leaf page for the insertion point of `rowid`.
/// Returns (index, true) if an exact match is found, or (index, false)
/// for the position where `rowid` should be inserted to maintain sort order.
fn find_insertion_point(page: &Page, rowid: u64) -> Result<(usize, bool)> {
    let num_rows = page.num_rows() as usize;
    if num_rows == 0 {
        return Ok((0, false));
    }
    let mut lo = 0usize;
    let mut hi = num_rows;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (start, end) = row_bounds(page, mid, num_rows);
        let mid_id = row::extract_id(&page.data[start..end])?;
        match mid_id.cmp(&rowid) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok((mid, true)),
        }
    }
    Ok((lo, false))
}

/// Write a leaf page that contains the rows from `snapshot` (which has
/// `old_count` rows) plus a new row inserted at position `insert_pos`.
/// No heap allocation: reads from the snapshot, writes into `page`.
fn write_leaf_with_insert(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
) {
    let total = old_count + 1;

    // Preserve leaf chain pointers from the snapshot before clearing.
    let saved_next = u32::from_le_bytes(snapshot[8..12].try_into().unwrap());
    let saved_prev = u32::from_le_bytes(snapshot[12..16].try_into().unwrap());

    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(saved_next);
    page.set_prev_leaf(saved_prev);

    // Clear data area (after header).
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + total * 2;
    let mut write_pos = data_start;

    let mut dst_idx = 0usize;
    let mut src_idx = 0usize;
    while dst_idx < total {
        if dst_idx == insert_pos {
            // Write the new row.
            page.data[write_pos..write_pos + new_row.len()].copy_from_slice(new_row);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += new_row.len();
            dst_idx += 1;
        } else {
            // Copy existing row from snapshot.
            let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += len;
            src_idx += 1;
            dst_idx += 1;
        }
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a leaf page that contains the rows from `snapshot` except the rows at
/// the given indices (must be sorted ascending). No heap allocation.
fn write_leaf_without_multiple(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    skip_indices: &[usize],
) {
    let total = old_count - skip_indices.len();
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + total * 2;
    let mut write_pos = data_start;
    let mut dst_idx = 0usize;
    let mut skip_ptr = 0usize;

    for src_idx in 0..old_count {
        if skip_ptr < skip_indices.len() && skip_indices[skip_ptr] == src_idx {
            skip_ptr += 1;
            continue;
        }
        let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
        let len = e - s;
        page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
        page.set_row_offset(dst_idx as u16, write_pos as u16);
        write_pos += len;
        dst_idx += 1;
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a leaf page that contains the rows from `snapshot` with some rows
/// replaced by new data. `replacements` is sorted by index.
/// Returns false if the rebuilt page would overflow (total size > PAGE_SIZE).
/// Preserves next_leaf/prev_leaf from the snapshot.
fn write_leaf_with_replacements(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    replacements: &[(usize, &[u8])],
) -> bool {
    // Pre-check: compute total size to see if it fits.
    let data_start = PAGE_HEADER_SIZE + old_count * 2;
    let mut total_data_size = 0usize;
    let mut repl_ptr = 0usize;
    for src_idx in 0..old_count {
        if repl_ptr < replacements.len() && replacements[repl_ptr].0 == src_idx {
            total_data_size += replacements[repl_ptr].1.len();
            repl_ptr += 1;
        } else {
            let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
            total_data_size += e - s;
        }
    }
    if data_start + total_data_size + CHECKSUM_SIZE > PAGE_SIZE {
        return false;
    }

    // Preserve leaf chain pointers from the snapshot.
    let saved_next = u32::from_le_bytes(snapshot[8..12].try_into().unwrap());
    let saved_prev = u32::from_le_bytes(snapshot[12..16].try_into().unwrap());

    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(saved_next);
    page.set_prev_leaf(saved_prev);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let mut write_pos = data_start;
    let mut repl_ptr = 0usize;

    for src_idx in 0..old_count {
        if repl_ptr < replacements.len() && replacements[repl_ptr].0 == src_idx {
            let new_data = replacements[repl_ptr].1;
            page.data[write_pos..write_pos + new_data.len()].copy_from_slice(new_data);
            page.set_row_offset(src_idx as u16, write_pos as u16);
            write_pos += new_data.len();
            repl_ptr += 1;
        } else {
            let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(src_idx as u16, write_pos as u16);
            write_pos += len;
        }
    }

    page.set_num_rows(old_count as u16);
    page.set_free_space_offset(write_pos as u16);
    page.update_checksum();
    true
}

/// Write a leaf page that contains the rows from `snapshot` except the row at
/// `skip_pos`. No heap allocation.
fn write_leaf_without(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    skip_pos: usize,
) {
    let total = old_count - 1;
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + total * 2;
    let mut write_pos = data_start;
    let mut dst_idx = 0usize;

    for src_idx in 0..old_count {
        if src_idx == skip_pos {
            continue;
        }
        let (s, e) = row_bounds_raw(snapshot, src_idx, old_count);
        let len = e - s;
        page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
        page.set_row_offset(dst_idx as u16, write_pos as u16);
        write_pos += len;
        dst_idx += 1;
    }

    page.set_num_rows(total as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Write a subset [range_start..range_end) of the "virtual" row sequence
/// (the old rows with a new row inserted at `insert_pos`) into `page`.
fn write_leaf_range(
    page: &mut Page,
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
    range_start: usize,
    range_end: usize,
) {
    let count = range_end - range_start;
    page.set_flags(PAGE_LEAF);
    page.set_num_rows(0);
    page.set_next_leaf(0);
    page.set_prev_leaf(0);

    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);

    let data_start = PAGE_HEADER_SIZE + count * 2;
    let mut write_pos = data_start;

    for dst_idx in 0..count {
        let virtual_idx = range_start + dst_idx;
        if virtual_idx == insert_pos {
            // This is the newly inserted row.
            page.data[write_pos..write_pos + new_row.len()].copy_from_slice(new_row);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += new_row.len();
        } else {
            // Map virtual index back to original index.
            let orig_idx = if virtual_idx < insert_pos {
                virtual_idx
            } else {
                virtual_idx - 1
            };
            let (s, e) = row_bounds_raw(snapshot, orig_idx, old_count);
            let len = e - s;
            page.data[write_pos..write_pos + len].copy_from_slice(&snapshot[s..e]);
            page.set_row_offset(dst_idx as u16, write_pos as u16);
            write_pos += len;
        }
    }

    page.set_num_rows(count as u16);
    page.set_free_space_offset(write_pos as u16);
}

/// Extract the rowid of the row at a given position in the virtual sequence
/// (old rows + new row inserted at `insert_pos`).
fn extract_id_at_virtual_pos(
    snapshot: &[u8; PAGE_SIZE],
    old_count: usize,
    insert_pos: usize,
    new_row: &[u8],
    virtual_pos: usize,
) -> Result<u64> {
    if virtual_pos == insert_pos {
        row::extract_id(new_row)
    } else {
        let orig_idx = if virtual_pos < insert_pos {
            virtual_pos
        } else {
            virtual_pos - 1
        };
        let (s, e) = row_bounds_raw(snapshot, orig_idx, old_count);
        row::extract_id(&snapshot[s..e])
    }
}

// ---------------------------------------------------------------------------
// Branch page helpers (fixed 12-byte entry format: [child:4][key:8])
// ---------------------------------------------------------------------------

const BRANCH_ENTRY_SIZE: usize = 12; // 4 (child) + 8 (key u64 LE)

fn get_branch_child(page: &Page, idx: usize) -> u32 {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap())
}

fn get_branch_key(page: &Page, idx: usize) -> u64 {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE + 4;
    u64::from_le_bytes(page.data[offset..offset + 8].try_into().unwrap())
}

fn find_child(page: &Page, rowid: u64) -> (usize, u32) {
    let num_keys = page.num_rows() as usize;
    for i in 0..num_keys {
        let key = get_branch_key(page, i);
        if rowid < key {
            return (i, get_branch_child(page, i));
        }
    }
    let last_child_offset = PAGE_HEADER_SIZE + num_keys * BRANCH_ENTRY_SIZE;
    let child = u32::from_le_bytes(
        page.data[last_child_offset..last_child_offset + 4]
            .try_into()
            .unwrap(),
    );
    (num_keys, child)
}

fn write_branch_entry(page: &mut Page, idx: usize, child: u32, key: u64) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
    page.data[offset + 4..offset + 12].copy_from_slice(&key.to_le_bytes());
}

fn set_branch_child(page: &mut Page, idx: usize, child: u32) {
    let offset = PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE;
    page.data[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
}

fn insert_branch_entry(page: &mut Page, child_idx: usize, key: u64, new_child: u32) {
    let (children, keys) = collect_branch_flat(page);
    let mut new_children = children;
    let mut new_keys = keys;
    new_keys.insert(child_idx, key);
    new_children.insert(child_idx + 1, new_child);
    rebuild_branch_flat(page, &new_children, &new_keys);
    page.set_num_rows(new_keys.len() as u16);
}

fn collect_branch_flat(page: &Page) -> (Vec<u32>, Vec<u64>) {
    let num_keys = page.num_rows() as usize;
    let mut children = Vec::with_capacity(num_keys + 1);
    let mut keys = Vec::with_capacity(num_keys);
    for i in 0..num_keys {
        children.push(get_branch_child(page, i));
        keys.push(get_branch_key(page, i));
    }
    children.push(get_branch_child(page, num_keys));
    (children, keys)
}

fn rebuild_branch_flat(page: &mut Page, children: &[u32], keys: &[u64]) {
    page.set_flags(PAGE_BRANCH);
    page.set_num_rows(keys.len() as u16);
    page.data[PAGE_HEADER_SIZE..PAGE_SIZE - CHECKSUM_SIZE].fill(0);
    for (i, &key) in keys.iter().enumerate() {
        write_branch_entry(page, i, children[i], key);
    }
    set_branch_child(page, keys.len(), children[keys.len()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row;
    use crate::value::Value;
    use tempfile::NamedTempFile;

    fn make_row(rowid: u64, name: &str) -> Vec<u8> {
        row::encode_row(rowid, &[(0, &Value::Text(name.into()))])
    }

    #[test]
    fn test_insert_and_search() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let row = make_row(1, "alice");
            tree.insert(1, &row).unwrap();
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        let found = reader.search(1).unwrap();
        assert!(found.is_some());
        let decoded = row::decode_row(&found.unwrap()).unwrap();
        assert_eq!(decoded.id, 1);
        assert_eq!(decoded.columns[0].1, Value::Text("alice".into()));
    }

    #[test]
    fn test_search_not_found() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        assert!(reader.search(999).unwrap().is_none());
    }

    #[test]
    fn test_duplicate_key_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut guard = pf.begin_write();
        let root = BTreeWriter::create(&mut guard).unwrap();
        let mut tree = BTreeWriter::new(&mut guard, root);

        let row = make_row(1, "alice");
        tree.insert(1, &row).unwrap();
        assert!(tree.insert(1, &row).is_err());
        guard.commit().unwrap();
    }

    #[test]
    fn test_many_inserts_trigger_split() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();

            for i in 0..100u64 {
                let row = make_row(i, &format!("name_{i}"));
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Verify all rows are findable
        let reader = BTreeReader::new(&pf, root);
        for i in 0..100u64 {
            assert!(reader.search(i).unwrap().is_some(), "missing: {i}");
        }
    }

    #[test]
    fn test_delete() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            let mut tree = BTreeWriter::new(&mut guard, root);

            let row = make_row(1, "alice");
            tree.insert(1, &row).unwrap();
            assert!(tree.delete(1).unwrap());
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        assert!(reader.search(1).unwrap().is_none());

        // Try deleting again — already deleted
        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            assert!(!tree.delete(1).unwrap());
            guard.commit().unwrap();
        }
    }

    #[test]
    fn test_scan_all() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();

            for i in 0..20u64 {
                let row = make_row(i, &format!("name_{i}"));
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        let all = reader.scan_all().unwrap();
        assert_eq!(all.len(), 20);
    }

    #[test]
    fn test_delete_matching() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();

            // Insert 100 rows with values 0-9 (cycling).
            for i in 0..100u64 {
                let val = (i % 10) as i64;
                let row = row::encode_row(i, &[(0, &Value::Integer(val))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Delete all rows with value == 5.
        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let deleted = tree.delete_matching(|data| {
                if let Ok(Some(val)) = row::extract_column(data, 0) {
                    val == Value::Integer(5)
                } else {
                    false
                }
            })
            .unwrap();
            assert_eq!(deleted.len(), 10);
            root = tree.root_page();
            guard.commit().unwrap();
        }

        // Verify 90 remain and none have value 5.
        let reader = BTreeReader::new(&pf, root);
        let all = reader.scan_all().unwrap();
        assert_eq!(all.len(), 90);
        for (_, bytes) in &all {
            let val = row::extract_column(bytes, 0).unwrap().unwrap();
            assert_ne!(val, Value::Integer(5));
        }
    }

    #[test]
    fn test_update_matching() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();

            // Insert 100 rows: even rows get "active", odd rows get "idle".
            for i in 0..100u64 {
                let status = if i % 2 == 0 { "active" } else { "idle" };
                let row = row::encode_row(i, &[(0, &Value::Text(status.into()))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Update all "active" rows to "archived".
        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let (updated, overflow) = tree
                .update_matching(
                    |data| {
                        if let Ok(Some(val)) = row::extract_column(data, 0) {
                            val == Value::Text("active".into())
                        } else {
                            false
                        }
                    },
                    |old_bytes| {
                        let decoded = row::decode_row(old_bytes).unwrap();
                        row::encode_row(decoded.id, &[(0, &Value::Text("archived".into()))])
                    },
                )
                .unwrap();
            assert_eq!(updated.len(), 50);
            assert_eq!(overflow.len(), 0);
            root = tree.root_page();
            guard.commit().unwrap();
        }

        // Verify: 50 archived, 50 idle, 0 active.
        let reader = BTreeReader::new(&pf, root);
        let all = reader.scan_all().unwrap();
        assert_eq!(all.len(), 100);
        let mut archived = 0;
        let mut idle = 0;
        let mut active = 0;
        for (_, bytes) in &all {
            if let Ok(Some(val)) = row::extract_column(bytes, 0) {
                match val {
                    Value::Text(s) if s == "archived" => archived += 1,
                    Value::Text(s) if s == "idle" => idle += 1,
                    Value::Text(s) if s == "active" => active += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(archived, 50);
        assert_eq!(idle, 50);
        assert_eq!(active, 0);
    }

    #[test]
    fn test_500_sequential_inserts_separate_tree_instances() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();

            for i in 0..500u64 {
                let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Verify all rows are findable
        let reader = BTreeReader::new(&pf, root);
        for i in 0..500u64 {
            let result = reader.search(i).unwrap();
            assert!(result.is_some(), "missing row at i={i}");
        }
    }

    // --- delete_matching edge cases ---

    #[test]
    fn test_delete_matching_empty_table() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let deleted = tree.delete_matching(|_data| true).unwrap();
            assert!(deleted.is_empty());
            guard.commit().unwrap();
        }
    }

    #[test]
    fn test_delete_matching_all_rows() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            for i in 0..20u64 {
                let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Delete ALL rows
        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let deleted = tree.delete_matching(|_data| true).unwrap();
            assert_eq!(deleted.len(), 20);
            root = tree.root_page();
            guard.commit().unwrap();
        }

        // Verify empty
        let reader = BTreeReader::new(&pf, root);
        let all = reader.scan_all().unwrap();
        assert_eq!(all.len(), 0);
    }

    #[test]
    fn test_delete_matching_no_rows_match() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            for i in 0..10u64 {
                let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Nothing matches
        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let deleted = tree.delete_matching(|_data| false).unwrap();
            assert!(deleted.is_empty());
            root = tree.root_page();
            guard.commit().unwrap();
        }

        // All rows still there
        let reader = BTreeReader::new(&pf, root);
        let all = reader.scan_all().unwrap();
        assert_eq!(all.len(), 10);
    }

    // --- update_matching edge cases ---

    #[test]
    fn test_update_matching_empty_table() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let (updated, overflow) = tree.update_matching(
                |_| true,
                |old| old.to_vec(),
            ).unwrap();
            assert!(updated.is_empty());
            assert!(overflow.is_empty());
            guard.commit().unwrap();
        }
    }

    #[test]
    fn test_update_matching_no_rows_match() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            for i in 0..10u64 {
                let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let (updated, overflow) = tree.update_matching(
                |_| false,
                |old| old.to_vec(),
            ).unwrap();
            assert!(updated.is_empty());
            assert!(overflow.is_empty());
            guard.commit().unwrap();
        }
    }

    #[test]
    fn test_update_matching_overflow() {
        // Force overflow by making rows much larger
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            // Insert rows with small values
            for i in 0..5u64 {
                let row = row::encode_row(i, &[(0, &Value::Text("x".into()))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        // Update all rows to have very large values that won't fit in the page
        {
            let mut guard = pf.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let big_text = "x".repeat(2000);
            let (updated, overflow) = tree.update_matching(
                |_| true,
                |old_bytes| {
                    let decoded = row::decode_row(old_bytes).unwrap();
                    row::encode_row(decoded.id, &[(0, &Value::Text(big_text.clone()))])
                },
            ).unwrap();
            // All should overflow since the page can't hold 5 * 2000+ bytes
            assert_eq!(updated.len() + overflow.len(), 5);
            guard.commit().unwrap();
        }
    }

    // --- multi_get_sorted tests ---

    #[test]
    fn test_multi_get_sorted_basic() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            for i in 0..50u64 {
                let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        let results = reader.multi_get_sorted(&[5, 10, 25, 49]).unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(row::extract_id(&results[0]).unwrap(), 5);
        assert_eq!(row::extract_id(&results[1]).unwrap(), 10);
        assert_eq!(row::extract_id(&results[2]).unwrap(), 25);
        assert_eq!(row::extract_id(&results[3]).unwrap(), 49);
    }

    #[test]
    fn test_multi_get_sorted_empty_input() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        let results = reader.multi_get_sorted(&[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_multi_get_sorted_missing_keys() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let mut root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            for i in (0..20u64).step_by(2) {
                let row = row::encode_row(i, &[(0, &Value::Integer(i as i64))]);
                let mut tree = BTreeWriter::new(&mut guard, root);
                root = tree.insert(i, &row).unwrap();
            }
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        // Ask for some keys that exist and some that don't
        let results = reader.multi_get_sorted(&[0, 1, 4, 5, 18, 19]).unwrap();
        // Only even numbers exist: 0, 4, 18
        assert_eq!(results.len(), 3);
    }

    // --- scan_all on empty tree ---

    #[test]
    fn test_scan_all_empty() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        let all = reader.scan_all().unwrap();
        assert!(all.is_empty());
    }

    // --- insert, delete, re-insert the same key ---

    #[test]
    fn test_delete_and_reinsert() {
        let tmp = NamedTempFile::new().unwrap();
        let pf = PageFile::open(tmp.path()).unwrap();

        let root;
        {
            let mut guard = pf.begin_write();
            root = BTreeWriter::create(&mut guard).unwrap();
            let mut tree = BTreeWriter::new(&mut guard, root);
            let row = make_row(1, "alice");
            tree.insert(1, &row).unwrap();
            assert!(tree.delete(1).unwrap());

            // Re-insert the same key with different data
            let row = make_row(1, "bob");
            tree.insert(1, &row).unwrap();
            guard.commit().unwrap();
        }

        let reader = BTreeReader::new(&pf, root);
        let found = reader.search(1).unwrap().unwrap();
        let decoded = row::decode_row(&found).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Text("bob".into()));
    }
}
