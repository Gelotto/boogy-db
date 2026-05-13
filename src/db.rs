use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::btree::BTree;
use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::filter::{FindOptions, SortDir};
use crate::page::{Page, PAGE_SYSTEM};
use crate::row;
use crate::table::TableMeta;
use crate::value::{ColumnDef, Type, Value};
use crate::wal::Wal;

/// A row returned from queries.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub columns: Vec<(String, Value)>,
}

/// Durability level for write operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Fsync WAL on every commit. Survives power loss.
    Immediate,
    /// No fsync. Survives process crash (OS cache), not power loss.
    Normal,
    /// No WAL writes at all. Fastest. Data may be lost on any crash.
    None,
}

/// Per-table state protected by its own RwLock.
struct TableState {
    meta: TableMeta,
}

/// The main database handle.
///
/// Uses per-table `RwLock`s so operations on different tables never block each
/// other, and reads on the same table can proceed concurrently.
pub struct BoogyDb {
    file: Mutex<PageFile>,
    wal: Mutex<Wal>,
    tables: RwLock<HashMap<String, Arc<RwLock<TableState>>>>,
    next_table_id: Mutex<u32>,
    durability: Mutex<Durability>,
    #[allow(dead_code)]
    path: PathBuf,
}

// System page (page 0) format:
// [magic: 4 bytes = 0xB00D_5150]
// [num_tables: u16]
// for each table:
//   [table_id: u32][root_page: u32][row_count: u64]
//   [name_len: u16][name_bytes]
//   [num_columns: u16]
//   for each column:
//     [col_name_len: u16][col_name_bytes][type_tag: u8][nullable: u8][unique: u8]

const SYSTEM_PAGE_MAGIC: u32 = 0xB00D_5150;

fn type_to_tag(t: Type) -> u8 {
    match t {
        Type::Text => 1,
        Type::Integer => 2,
        Type::Real => 3,
        Type::Blob => 4,
        Type::Boolean => 5,
    }
}

fn tag_to_type(tag: u8) -> Result<Type> {
    match tag {
        1 => Ok(Type::Text),
        2 => Ok(Type::Integer),
        3 => Ok(Type::Real),
        4 => Ok(Type::Blob),
        5 => Ok(Type::Boolean),
        _ => Err(BoogyError::Corruption(format!("unknown type tag: {tag}"))),
    }
}

/// Serialize the table registry into a system page.
/// Takes pre-collected metadata to avoid needing per-table locks.
fn serialize_system_page(
    metas: &[TableMeta],
    next_table_id: u32,
) -> Page {
    let mut page = Page::new_system();
    let data = &mut page.data;

    let mut offset = 16; // after page header

    // System page magic
    data[offset..offset + 4].copy_from_slice(&SYSTEM_PAGE_MAGIC.to_le_bytes());
    offset += 4;

    // next_table_id
    data[offset..offset + 4].copy_from_slice(&next_table_id.to_le_bytes());
    offset += 4;

    // num_tables
    let num_tables = metas.len() as u16;
    data[offset..offset + 2].copy_from_slice(&num_tables.to_le_bytes());
    offset += 2;

    for meta in metas {
        // table_id
        data[offset..offset + 4].copy_from_slice(&meta.table_id.to_le_bytes());
        offset += 4;

        // root_page
        data[offset..offset + 4].copy_from_slice(&meta.root_page.to_le_bytes());
        offset += 4;

        // row_count
        data[offset..offset + 8].copy_from_slice(&meta.row_count.to_le_bytes());
        offset += 8;

        // name
        let name_bytes = meta.name.as_bytes();
        data[offset..offset + 2].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        offset += 2;
        data[offset..offset + name_bytes.len()].copy_from_slice(name_bytes);
        offset += name_bytes.len();

        // columns
        data[offset..offset + 2].copy_from_slice(&(meta.columns.len() as u16).to_le_bytes());
        offset += 2;

        for col in &meta.columns {
            let col_name = col.name.as_bytes();
            data[offset..offset + 2].copy_from_slice(&(col_name.len() as u16).to_le_bytes());
            offset += 2;
            data[offset..offset + col_name.len()].copy_from_slice(col_name);
            offset += col_name.len();
            data[offset] = type_to_tag(col.col_type);
            offset += 1;
            data[offset] = if col.nullable { 1 } else { 0 };
            offset += 1;
            data[offset] = if col.unique { 1 } else { 0 };
            offset += 1;
        }
    }

    page.update_checksum();
    page
}

/// Deserialize the table registry from a system page.
/// Returns (tables, next_table_id).
fn deserialize_system_page(
    page: &Page,
) -> Result<(Vec<TableMeta>, u32)> {
    let data = &page.data;
    let mut offset = 16; // skip page header

    // System page magic
    let magic = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    if magic != SYSTEM_PAGE_MAGIC {
        return Err(BoogyError::Corruption(format!(
            "bad system page magic: {magic:#010x}"
        )));
    }
    offset += 4;

    // next_table_id
    let next_table_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;

    // num_tables
    let num_tables = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    let mut tables = Vec::with_capacity(num_tables);

    for _ in 0..num_tables {
        let table_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        offset += 4;

        let root_page = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        offset += 4;

        let row_count = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        offset += 8;

        let name_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        let name = String::from_utf8(data[offset..offset + name_len].to_vec())
            .map_err(|_| BoogyError::Corruption("invalid utf8 in table name".into()))?;
        offset += name_len;

        let num_columns =
            u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;

        let mut columns = Vec::with_capacity(num_columns);
        for _ in 0..num_columns {
            let col_name_len =
                u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            let col_name = String::from_utf8(data[offset..offset + col_name_len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8 in column name".into()))?;
            offset += col_name_len;
            let type_tag = data[offset];
            offset += 1;
            let nullable = data[offset] != 0;
            offset += 1;
            let unique = data[offset] != 0;
            offset += 1;

            let mut col_def = ColumnDef::new(col_name, tag_to_type(type_tag)?);
            if !nullable {
                col_def = col_def.not_null();
            }
            if unique {
                col_def = col_def.unique();
            }
            columns.push(col_def);
        }

        let mut meta = TableMeta::new(name, table_id, columns, root_page);
        meta.row_count = row_count;
        tables.push(meta);
    }

    Ok((tables, next_table_id))
}

impl BoogyDb {
    /// Open or create a database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let wal_path = path.with_extension("wal");

        // Step 1: Crash recovery -- replay WAL if it has entries.
        {
            let mut wal = Wal::open(&wal_path)?;
            if wal.entry_count() > 0 {
                let mut file = PageFile::open(&path)?;
                let entries = wal.read_entries()?;
                // Undo: restore original pages (reverse order for correctness).
                for entry in entries.iter().rev() {
                    let page = Page::from_bytes_unchecked(entry.page_data);
                    file.put_page(entry.page_no, page);
                }
                file.sync()?;
                wal.truncate()?;
            }
        }

        // Step 2: Normal open.
        let mut file = PageFile::open(&path)?;
        let wal = Wal::open(&wal_path)?;

        // Step 3: Load table registry from system page if it exists.
        let mut tables = HashMap::new();
        let mut next_table_id = 1u32;

        if file.page_count() > 0 {
            let sys_page = file.read_page(0)?.clone();
            if sys_page.flags() & PAGE_SYSTEM != 0 {
                let (metas, next_id) = deserialize_system_page(&sys_page)?;
                next_table_id = next_id;
                for meta in metas {
                    let name = meta.name.clone();
                    let state = Arc::new(RwLock::new(TableState { meta }));
                    tables.insert(name, state);
                }
            }
        }

        Ok(Self {
            file: Mutex::new(file),
            wal: Mutex::new(wal),
            tables: RwLock::new(tables),
            next_table_id: Mutex::new(next_table_id),
            durability: Mutex::new(Durability::Normal),
            path,
        })
    }

    /// Set the durability level for writes.
    pub fn set_durability(&self, d: Durability) {
        *self.durability.lock().unwrap() = d;
    }

    /// Get current durability level.
    pub fn durability(&self) -> Durability {
        *self.durability.lock().unwrap()
    }

    /// Write before-images from the PageFile to the WAL, then flush.
    /// Called after every B+ tree mutation while holding both locks.
    fn commit_with_wal(
        file: &mut PageFile,
        wal: &mut Wal,
        durability: Durability,
        table_id: u32,
    ) -> Result<()> {
        match durability {
            Durability::Immediate => {
                // Write before-images to WAL
                let before_images = file.take_before_images();
                for (page_no, data) in &before_images {
                    wal.append_before_image(table_id, *page_no, data)?;
                }
                // Fsync WAL first (durability guarantee)
                wal.sync()?;
                // Then flush data pages + fsync
                file.sync()?;
                // WAL entries are now obsolete -- truncate
                wal.truncate()?;
            }
            Durability::Normal => {
                // Write before-images to WAL (no fsync)
                let before_images = file.take_before_images();
                for (page_no, data) in &before_images {
                    wal.append_before_image(table_id, *page_no, data)?;
                }
                // Flush data pages (no fsync)
                file.flush()?;
                // Truncate WAL after successful flush
                wal.truncate()?;
            }
            Durability::None => {
                // No WAL writes. Just flush pages.
                file.take_before_images(); // discard
                file.flush()?;
            }
        }
        Ok(())
    }

    /// Collect metadata snapshots from all tables.
    /// This briefly read-locks each per-table RwLock but does NOT hold the
    /// file or WAL mutex, avoiding lock-ordering deadlocks.
    fn snapshot_table_metas(&self) -> (Vec<TableMeta>, u32) {
        let tables = self.tables.read().unwrap();
        let next_id = *self.next_table_id.lock().unwrap();
        let mut metas = Vec::with_capacity(tables.len());
        for (_, state_arc) in tables.iter() {
            let state = state_arc.read().unwrap();
            metas.push(state.meta.clone());
        }
        (metas, next_id)
    }

    /// Persist the table registry to the system page (page 0).
    /// Caller must hold file and wal locks.
    fn persist_registry_with(
        file: &mut PageFile,
        wal: &mut Wal,
        metas: &[TableMeta],
        next_table_id: u32,
        durability: Durability,
    ) -> Result<()> {
        // Ensure page 0 exists.
        if file.page_count() == 0 {
            file.allocate_page()?;
        }

        let page = serialize_system_page(metas, next_table_id);
        file.put_page(0, page);

        // Commit the system page with WAL protection.
        Self::commit_with_wal(file, wal, durability, 0)?;
        Ok(())
    }

    /// Create a new table.
    pub fn create_table(&self, name: &str, columns: &[ColumnDef]) -> Result<()> {
        // Check existence under a read lock first for the common non-conflicting case.
        {
            let tables = self.tables.read().unwrap();
            if tables.contains_key(name) {
                return Err(BoogyError::TableExists(name.to_string()));
            }
        }

        // Allocate the root page (needs file lock + wal lock).
        let (root, table_id) = {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            let durability = *self.durability.lock().unwrap();

            // Ensure system page exists before any table pages.
            if file.page_count() == 0 {
                file.allocate_page()?; // page 0 = system page
            }

            let root = BTree::create(&mut file)?;
            let table_id = {
                let mut next = self.next_table_id.lock().unwrap();
                let id = *next;
                *next += 1;
                id
            };

            Self::commit_with_wal(&mut file, &mut wal, durability, table_id)?;
            (root, table_id)
        };

        let meta = TableMeta::new(name.to_string(), table_id, columns.to_vec(), root);
        let state = Arc::new(RwLock::new(TableState { meta }));

        // Write-lock the table map to insert.
        {
            let mut tables = self.tables.write().unwrap();
            if tables.contains_key(name) {
                // Another thread raced us.
                return Err(BoogyError::TableExists(name.to_string()));
            }
            tables.insert(name.to_string(), state);
        }

        // Persist registry to system page.
        // Snapshot metadata first (no file lock held), then write.
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = *self.durability.lock().unwrap();
        {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            Self::persist_registry_with(&mut file, &mut wal, &metas, next_id, durability)?;
        }

        Ok(())
    }

    /// Drop a table.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        {
            let mut tables = self.tables.write().unwrap();
            if tables.remove(name).is_none() {
                return Err(BoogyError::TableNotFound(name.to_string()));
            }
        }

        // Persist updated registry.
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = *self.durability.lock().unwrap();
        {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            Self::persist_registry_with(&mut file, &mut wal, &metas, next_id, durability)?;
        }

        Ok(())
    }

    /// Insert a row. Returns the auto-generated _id.
    pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<String> {
        // 1. Read-lock registry, clone Arc.
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Write-lock the specific table.
        let mut state = table_state.write().unwrap();

        // 3. Encode row (no file lock needed).
        let id = uuid::Uuid::new_v4().to_string();
        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(&id, &col_values);

        // 4. Brief file lock + WAL lock for B-tree insert.
        let new_root = {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            let durability = *self.durability.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            let new_root = tree.insert(&id, &row_bytes)?;
            Self::commit_with_wal(&mut file, &mut wal, durability, state.meta.table_id)?;
            new_root
        };

        // 5. Update table state.
        if new_root != state.meta.root_page {
            state.meta.root_page = new_root;
        }
        state.meta.row_count += 1;

        // 6. Persist registry (root_page / row_count may have changed).
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = *self.durability.lock().unwrap();
        {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            Self::persist_registry_with(&mut file, &mut wal, &metas, next_id, durability)?;
        }

        Ok(id)
    }

    /// Get a row by _id.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Row>> {
        // 1. Read-lock registry, clone Arc.
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Read-lock the specific table.
        let state = table_state.read().unwrap();

        // 3. Brief file lock for B-tree search.
        let result = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            tree.search(id)?
        };

        // 4. Decode outside any file lock.
        match result {
            Some(bytes) => {
                let decoded = row::decode_row(&bytes)?;
                Ok(Some(decoded_to_row(&decoded, &state.meta)))
            }
            None => Ok(None),
        }
    }

    /// Update a row by _id. Replaces specified columns.
    pub fn update(&self, table: &str, id: &str, fields: &[(&str, Value)]) -> Result<bool> {
        // 1. Read-lock registry, clone Arc.
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Write-lock the specific table.
        let mut state = table_state.write().unwrap();

        // 3. Read existing row, merge, write back (file lock + WAL lock).
        let new_root = {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            let durability = *self.durability.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);

            let existing = match tree.search(id)? {
                Some(bytes) => row::decode_row(&bytes)?,
                None => return Ok(false),
            };

            // Merge updates.
            let mut col_map: HashMap<u16, Value> = existing.columns.into_iter().collect();
            for (name, val) in fields {
                if let Some(col_id) = state.meta.col_id(name) {
                    col_map.insert(col_id, val.clone());
                }
            }

            let col_values: Vec<(u16, &Value)> = col_map.iter().map(|(k, v)| (*k, v)).collect();
            let new_row = row::encode_row(id, &col_values);

            // Delete + re-insert.
            tree.delete(id)?;
            let new_root = tree.insert(id, &new_row)?;
            Self::commit_with_wal(&mut file, &mut wal, durability, state.meta.table_id)?;
            new_root
        };

        // 4. Update table state.
        if new_root != state.meta.root_page {
            state.meta.root_page = new_root;
        }

        // 5. Persist registry if root changed.
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = *self.durability.lock().unwrap();
        {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            Self::persist_registry_with(&mut file, &mut wal, &metas, next_id, durability)?;
        }

        Ok(true)
    }

    /// Delete a row by _id.
    pub fn delete(&self, table: &str, id: &str) -> Result<bool> {
        // 1. Read-lock registry, clone Arc.
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Write-lock the specific table.
        let mut state = table_state.write().unwrap();

        // 3. Brief file lock + WAL lock for B-tree delete.
        let deleted = {
            let mut file = self.file.lock().unwrap();
            let mut wal = self.wal.lock().unwrap();
            let durability = *self.durability.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            let deleted = tree.delete(id)?;
            Self::commit_with_wal(&mut file, &mut wal, durability, state.meta.table_id)?;
            deleted
        };

        // 4. Update row count + persist.
        if deleted {
            state.meta.row_count -= 1;
            drop(state);
            let (metas, next_id) = self.snapshot_table_metas();
            let durability = *self.durability.lock().unwrap();
            {
                let mut file = self.file.lock().unwrap();
                let mut wal = self.wal.lock().unwrap();
                Self::persist_registry_with(&mut file, &mut wal, &metas, next_id, durability)?;
            }
        }
        Ok(deleted)
    }

    /// Find rows matching filters, with sort and pagination.
    /// Returns (matching_rows, total_count).
    pub fn find(&self, table: &str, opts: FindOptions) -> Result<(Vec<Row>, u64)> {
        // 1. Read-lock registry, clone Arc.
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Read-lock the specific table.
        let state = table_state.read().unwrap();

        // 3. Brief file lock for scan.
        let all = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            tree.scan_all()?
        };

        // 4. Decode and filter outside any file lock.
        let mut matching: Vec<Row> = Vec::new();
        for (_, bytes) in &all {
            let decoded = row::decode_row(bytes)?;
            let row = decoded_to_row(&decoded, &state.meta);

            let passes = opts.filters.iter().all(|f| {
                let col_val = row
                    .columns
                    .iter()
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

        // Sort.
        for sort in opts.sort.iter().rev() {
            matching.sort_by(|a, b| {
                let va = a
                    .columns
                    .iter()
                    .find(|(n, _)| n == &sort.column)
                    .map(|(_, v)| v);
                let vb = b
                    .columns
                    .iter()
                    .find(|(n, _)| n == &sort.column)
                    .map(|(_, v)| v);
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

        // Pagination.
        let skip = opts.offset.unwrap_or(0) as usize;
        let take = opts.limit.unwrap_or(u32::MAX) as usize;
        let page: Vec<Row> = matching.into_iter().skip(skip).take(take).collect();

        Ok((page, total))
    }

    /// Count rows matching filters.
    pub fn count(&self, table: &str, filters: &[crate::filter::Filter]) -> Result<u64> {
        // 1. Read-lock registry, clone Arc.
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Read-lock the specific table.
        let state = table_state.read().unwrap();

        // Fast path: no filters, just return the cached count.
        if filters.is_empty() {
            return Ok(state.meta.row_count);
        }

        // 3. Brief file lock for scan.
        let all = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            tree.scan_all()?
        };

        // 4. Decode and count outside any file lock.
        let mut count = 0u64;
        for (_, bytes) in &all {
            let decoded = row::decode_row(bytes)?;
            let row = decoded_to_row(&decoded, &state.meta);

            let passes = filters.iter().all(|f| {
                let col_val = row
                    .columns
                    .iter()
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
        // Flush all changes (individual operations already committed via WAL).
        let mut file = self.file.lock().unwrap();
        let mut wal = self.wal.lock().unwrap();
        let durability = *self.durability.lock().unwrap();
        Self::commit_with_wal(&mut file, &mut wal, durability, 0)?;
        Ok(result)
    }
}

/// Transaction context -- provides the same API as BoogyDb but within a transaction.
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
    let columns: Vec<(String, Value)> = decoded
        .columns
        .iter()
        .filter_map(|(col_id, val)| {
            meta.columns
                .get(*col_id as usize)
                .map(|def| (def.name.clone(), val.clone()))
        })
        .collect();
    Row {
        id: decoded.id.clone(),
        columns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_insert_and_get() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
        let id = db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
        let row = db.get("users", &id).unwrap().unwrap();
        assert_eq!(row.columns[0].1, Value::Text("alice".into()));
    }

    #[test]
    fn test_update() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
        let id = db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
        db.update("users", &id, &[("name", Value::Text("bob".into()))]).unwrap();
        let row = db.get("users", &id).unwrap().unwrap();
        assert_eq!(row.columns[0].1, Value::Text("bob".into()));
    }

    #[test]
    fn test_delete() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
        let id = db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
        assert!(db.delete("users", &id).unwrap());
        assert!(db.get("users", &id).unwrap().is_none());
    }

    #[test]
    fn test_table_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        assert!(db.insert("nope", &[]).is_err());
    }

    #[test]
    fn test_get_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("x", Type::Integer)]).unwrap();
        assert!(db.get("t", "nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_count() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.insert("t", &[("v", Value::Integer(1))]).unwrap();
        db.insert("t", &[("v", Value::Integer(2))]).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 2);
    }

    #[test]
    fn test_find_with_filter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.insert("t", &[("v", Value::Integer(1))]).unwrap();
        db.insert("t", &[("v", Value::Integer(2))]).unwrap();
        db.insert("t", &[("v", Value::Integer(3))]).unwrap();
        let opts = FindOptions {
            filters: vec![crate::filter::Filter::gt("v", 1i64)],
            ..Default::default()
        };
        let (rows, total) = db.find("t", opts).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_find_with_sort_and_pagination() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..10 {
            db.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
        let opts = FindOptions {
            sort: vec![crate::filter::Sort::desc("v")],
            limit: Some(3),
            offset: Some(0),
            ..Default::default()
        };
        let (rows, total) = db.find("t", opts).unwrap();
        assert_eq!(total, 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].columns[0].1, Value::Integer(9));
    }

    #[test]
    fn test_transaction() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.transaction(|tx| {
            tx.insert("a", &[("v", Value::Integer(1))])?;
            tx.insert("b", &[("v", Value::Integer(2))])?;
            Ok(())
        }).unwrap();
        assert_eq!(db.count("a", &[]).unwrap(), 1);
        assert_eq!(db.count("b", &[]).unwrap(), 1);
    }

    #[test]
    fn test_many_inserts() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..100 {
            db.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
        assert_eq!(db.count("t", &[]).unwrap(), 100);
    }

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = Arc::new(BoogyDb::open(&path).unwrap());
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        let id = db.insert("t", &[("v", Value::Integer(42))]).unwrap();

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let db = Arc::clone(&db);
                let id = id.clone();
                thread::spawn(move || {
                    let row = db.get("t", &id).unwrap().unwrap();
                    assert_eq!(row.columns[0].1, Value::Integer(42));
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_different_tables() {
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = Arc::new(BoogyDb::open(&path).unwrap());
        db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let db = Arc::clone(&db);
                let table = if i % 2 == 0 { "a" } else { "b" };
                thread::spawn(move || {
                    for j in 0..10 {
                        db.insert(table, &[("v", Value::Integer(j))]).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(db.count("a", &[]).unwrap(), 20);
        assert_eq!(db.count("b", &[]).unwrap(), 20);
    }

    // --- New tests for WAL, persistence, and durability ---

    #[test]
    fn test_data_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
            db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
            db.insert("users", &[("name", Value::Text("bob".into()))]).unwrap();
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            let count = db.count("users", &[]).unwrap();
            assert_eq!(count, 2);
        }
    }

    #[test]
    fn test_multiple_tables_persist() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
            db.create_table("posts", &[
                ColumnDef::new("title", Type::Text),
                ColumnDef::new("likes", Type::Integer),
            ]).unwrap();
            db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
            db.insert("posts", &[
                ("title", Value::Text("hello".into())),
                ("likes", Value::Integer(5)),
            ]).unwrap();
            db.insert("posts", &[
                ("title", Value::Text("world".into())),
                ("likes", Value::Integer(10)),
            ]).unwrap();
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            assert_eq!(db.count("users", &[]).unwrap(), 1);
            assert_eq!(db.count("posts", &[]).unwrap(), 2);

            // Verify data is correct
            let opts = FindOptions {
                filters: vec![crate::filter::Filter::eq("title", "world")],
                ..Default::default()
            };
            let (rows, _) = db.find("posts", opts).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].columns.iter().find(|(n, _)| n == "likes").unwrap().1,
                Value::Integer(10)
            );
        }
    }

    #[test]
    fn test_durability_immediate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.set_durability(Durability::Immediate);
            db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
            db.insert("t", &[("v", Value::Integer(1))]).unwrap();
            db.insert("t", &[("v", Value::Integer(2))]).unwrap();
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            assert_eq!(db.count("t", &[]).unwrap(), 2);
        }
    }

    #[test]
    fn test_durability_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.insert("t", &[("v", Value::Integer(1))]).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 1);
    }

    #[test]
    fn test_drop_table_persists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
            db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();
            db.insert("a", &[("v", Value::Integer(1))]).unwrap();
            db.insert("b", &[("v", Value::Integer(2))]).unwrap();
            db.drop_table("a").unwrap();
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            assert!(db.get("a", "x").is_err()); // table "a" should not exist
            assert_eq!(db.count("b", &[]).unwrap(), 1);
        }
    }

    #[test]
    fn test_reopen_and_continue_inserting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
            for i in 0..5 {
                db.insert("t", &[("v", Value::Integer(i))]).unwrap();
            }
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            assert_eq!(db.count("t", &[]).unwrap(), 5);
            for i in 5..10 {
                db.insert("t", &[("v", Value::Integer(i))]).unwrap();
            }
            assert_eq!(db.count("t", &[]).unwrap(), 10);
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            assert_eq!(db.count("t", &[]).unwrap(), 10);
        }
    }
}
