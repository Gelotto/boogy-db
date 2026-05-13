use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::btree::{BTreeReader, BTreeWriter};
use crate::error::{BoogyError, Result};
use crate::file::{PageFile, WriteGuard};
use crate::filter::{Filter, FilterOp, FindOptions, FindResult, SortDir};
use crate::index::{self, IndexTreeReader, IndexTreeWriter};
use crate::page::{Page, PAGE_SYSTEM};
use crate::row;
use crate::table::{IndexMeta, TableMeta};
use crate::value::{ColumnDef, Type, Value};
use crate::wal::Wal;

/// A row returned from queries. Wraps raw bytes; decodes columns on demand.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    data: Vec<u8>,
    col_names: Arc<Vec<String>>,
}

impl Row {
    fn from_raw(bytes: &[u8], col_names: Arc<Vec<String>>) -> Result<Self> {
        let id = row::extract_id(bytes)?;
        Ok(Self { id, data: bytes.to_vec(), col_names })
    }

    /// Get a single column value by name. Decodes only that column.
    pub fn get(&self, column: &str) -> Option<Value> {
        let col_id = self.col_names.iter().position(|n| n == column)? as u16;
        row::extract_column(&self.data, col_id).ok().flatten()
    }

    /// Decode all columns.
    pub fn columns(&self) -> Vec<(String, Value)> {
        match row::decode_row(&self.data) {
            Ok(decoded) => decoded.columns.into_iter().filter_map(|(col_id, val)| {
                self.col_names.get(col_id as usize).map(|name| (name.clone(), val))
            }).collect(),
            Err(_) => Vec::new(),
        }
    }
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
    file: PageFile,
    wal: Mutex<Wal>,
    tables: RwLock<HashMap<String, Arc<RwLock<TableState>>>>,
    next_table_id: Mutex<u32>,
    durability: std::sync::atomic::AtomicU8,
    #[allow(dead_code)]
    path: PathBuf,
}

// System page (page 0) format:
// [magic: 4 bytes = 0xB00D_5150]
// [next_table_id: u32]
// [num_tables: u16]
// for each table:
//   [table_id: u32][root_page: u32][row_count: u64][next_rowid: u64]
//   [name_len: u16][name_bytes]
//   [num_columns: u16]
//   for each column:
//     [col_name_len: u16][col_name_bytes][type_tag: u8][nullable: u8][unique: u8]
//   [num_indexes: u16]
//   for each index:
//     [idx_name_len: u16][idx_name_bytes]
//     [idx_col_len: u16][idx_col_bytes]
//     [idx_root_page: u32]

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

        // next_rowid
        data[offset..offset + 8].copy_from_slice(&meta.next_rowid.to_le_bytes());
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

        // Indexes
        let num_indexes = meta.indexes.len() as u16;
        data[offset..offset + 2].copy_from_slice(&num_indexes.to_le_bytes());
        offset += 2;

        for idx in &meta.indexes {
            let idx_name = idx.name.as_bytes();
            data[offset..offset + 2].copy_from_slice(&(idx_name.len() as u16).to_le_bytes());
            offset += 2;
            data[offset..offset + idx_name.len()].copy_from_slice(idx_name);
            offset += idx_name.len();

            let idx_col = idx.column.as_bytes();
            data[offset..offset + 2].copy_from_slice(&(idx_col.len() as u16).to_le_bytes());
            offset += 2;
            data[offset..offset + idx_col.len()].copy_from_slice(idx_col);
            offset += idx_col.len();

            data[offset..offset + 4].copy_from_slice(&idx.root_page.to_le_bytes());
            offset += 4;
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

        let next_rowid = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
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
        meta.next_rowid = next_rowid;

        // Indexes
        let num_indexes = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;

        for _ in 0..num_indexes {
            let idx_name_len =
                u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            let idx_name = String::from_utf8(data[offset..offset + idx_name_len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8 in index name".into()))?;
            offset += idx_name_len;

            let idx_col_len =
                u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            let idx_col = String::from_utf8(data[offset..offset + idx_col_len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8 in index column".into()))?;
            offset += idx_col_len;

            let idx_root_page = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;

            meta.indexes.push(IndexMeta {
                name: idx_name,
                column: idx_col,
                root_page: idx_root_page,
            });
        }

        tables.push(meta);
    }

    Ok((tables, next_table_id))
}

/// Validate that a path does not contain traversal attacks or null bytes.
fn validate_path(path: &Path) -> Result<()> {
    let path_str = path.to_str().ok_or_else(|| {
        BoogyError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains invalid UTF-8",
        ))
    })?;

    if path_str.contains('\0') {
        return Err(BoogyError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains null byte",
        )));
    }

    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(BoogyError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains '..' component",
            )));
        }
    }

    Ok(())
}

impl BoogyDb {
    /// Open or create a database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        validate_path(&path)?;
        let wal_path = path.with_extension("wal");

        // Step 1: Crash recovery -- replay WAL if it has entries.
        {
            let mut wal = Wal::open(&wal_path)?;
            if wal.entry_count() > 0 {
                let file = PageFile::open(&path)?;
                let entries = wal.read_entries()?;
                // Undo: restore original pages (reverse order for correctness).
                for entry in entries.iter().rev() {
                    let page = Page::from_bytes_unchecked(entry.page_data);
                    file.put_page_direct(entry.page_no, page);
                }
                file.sync_all()?;
                wal.truncate()?;
            }
        }

        // Step 2: Normal open.
        let file = PageFile::open(&path)?;
        let wal = Wal::open(&wal_path)?;

        // Step 3: Load table registry from system page if it exists.
        let mut tables = HashMap::new();
        let mut next_table_id = 1u32;

        if file.page_count() > 0 {
            let sys_page = file.read_page(0)?;
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
            file,
            wal: Mutex::new(wal),
            tables: RwLock::new(tables),
            next_table_id: Mutex::new(next_table_id),
            durability: std::sync::atomic::AtomicU8::new(Durability::Normal as u8),
            path,
        })
    }

    /// Set the durability level for writes.
    pub fn set_durability(&self, d: Durability) {
        self.durability.store(d as u8, std::sync::atomic::Ordering::Relaxed);
        self.file.set_capture_before_images(!matches!(d, Durability::None));
    }

    /// Get current durability level.
    pub fn durability(&self) -> Durability {
        match self.durability.load(std::sync::atomic::Ordering::Relaxed) {
            0 => Durability::Immediate,
            1 => Durability::Normal,
            _ => Durability::None,
        }
    }

    /// Commit a WriteGuard, writing before-images to the WAL as appropriate
    /// for the durability level. This is the single commit path used by all
    /// write operations.
    fn commit_write(
        guard: WriteGuard,
        wal: &Mutex<Wal>,
        durability: Durability,
        table_id: u32,
    ) -> Result<()> {
        match durability {
            Durability::Immediate => {
                let before_images = guard.commit(true)?;
                let mut wal = wal.lock().unwrap();
                for (page_no, data) in &before_images {
                    wal.append_before_image(table_id, *page_no, data)?;
                }
                wal.sync()?;
                wal.truncate()?;
            }
            Durability::Normal => {
                let before_images = guard.commit(true)?;
                let mut wal = wal.lock().unwrap();
                for (page_no, data) in &before_images {
                    wal.append_before_image(table_id, *page_no, data)?;
                }
            }
            Durability::None => {
                guard.commit(false)?; // publish to cache only, no disk flush
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
    fn persist_registry(
        file: &PageFile,
        wal: &Mutex<Wal>,
        metas: &[TableMeta],
        next_table_id: u32,
        durability: Durability,
    ) -> Result<()> {
        let mut guard = file.begin_write();
        // Ensure page 0 exists.
        if file.page_count() == 0 {
            guard.allocate_page()?;
        }

        let page = serialize_system_page(metas, next_table_id);
        guard.put_page(0, page);

        Self::commit_write(guard, wal, durability, 0)?;
        Ok(())
    }

    /// Update all indexes for a row using encoded row bytes.
    /// remove=true deletes from indexes, remove=false inserts.
    fn index_update_row(
        guard: &mut WriteGuard,
        meta: &mut TableMeta,
        rowid: u64,
        row_bytes: &[u8],
        remove: bool,
    ) -> Result<()> {
        for idx in &mut meta.indexes {
            let col_id = meta.col_name_to_id.get(&idx.column).copied();
            let col_type = meta
                .columns
                .iter()
                .find(|c| c.name == idx.column)
                .map(|c| c.col_type);
            if let (Some(cid), Some(ct)) = (col_id, col_type) {
                let val = crate::row::extract_column(row_bytes, cid)?
                    .unwrap_or(Value::Null);
                if let Some(key) = index::encode_index_key(ct, &val, rowid) {
                    let mut tree = IndexTreeWriter::new(guard, idx.root_page);
                    if remove {
                        tree.delete(&key)?;
                    } else {
                        tree.insert(&key)?;
                    }
                    idx.root_page = tree.root_page();
                }
            }
        }
        Ok(())
    }

    /// Enforce type constraints on indexed columns before insert/update.
    fn enforce_index_types(
        meta: &TableMeta,
        data: &[(&str, Value)],
    ) -> Result<()> {
        for idx in &meta.indexes {
            if let Some((_, val)) = data.iter().find(|(name, _)| *name == idx.column) {
                if val.is_null() {
                    continue;
                }
                let col_type = meta
                    .columns
                    .iter()
                    .find(|c| c.name == idx.column)
                    .map(|c| c.col_type);
                if let Some(ct) = col_type {
                    if val.value_type() != Some(ct) {
                        return Err(BoogyError::TypeMismatch(format!(
                            "column '{}' expects {:?}, got {:?}",
                            idx.column,
                            ct,
                            val.value_type()
                        )));
                    }
                    if let Value::Real(f) = val {
                        if f.is_nan() {
                            return Err(BoogyError::TypeMismatch(format!(
                                "column '{}': NaN not allowed in indexed columns",
                                idx.column
                            )));
                        }
                    }
                    if let Value::Text(s) = val {
                        if s.as_bytes().contains(&0x00) {
                            return Err(BoogyError::TypeMismatch(format!(
                                "column '{}': null bytes not allowed in indexed text columns",
                                idx.column
                            )));
                        }
                    }
                }
            }
        }
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

        // Allocate the root page via WriteGuard.
        let (root, table_id) = {
            let durability = self.durability();
            let mut guard = self.file.begin_write();

            // Ensure system page exists before any table pages.
            if self.file.page_count() == 0 {
                guard.allocate_page()?; // page 0 = system page
            }

            let root = BTreeWriter::create(&mut guard)?;
            let table_id = {
                let mut next = self.next_table_id.lock().unwrap();
                let id = *next;
                *next += 1;
                id
            };

            Self::commit_write(guard, &self.wal, durability, table_id)?;
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
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

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
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

        Ok(())
    }

    /// Insert a row with auto-increment rowid. Returns the assigned rowid.
    pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
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

        // 3. Type enforcement for indexed columns.
        Self::enforce_index_types(&state.meta, data)?;

        // 4. Auto-assign rowid.
        let rowid = state.meta.next_rowid;
        state.meta.next_rowid += 1;

        // 5. Encode row (no file lock needed).
        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(rowid, &col_values);

        // 6. WriteGuard for B-tree insert + index maintenance.
        let durability = self.durability();
        {
            let mut guard = self.file.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
            let new_root = tree.insert(rowid, &row_bytes)?;
            state.meta.root_page = new_root;

            if !state.meta.indexes.is_empty() {
                Self::index_update_row(&mut guard, &mut state.meta, rowid, &row_bytes, false)?;
            }

            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
        }

        // 7. Update table state.
        state.meta.row_count += 1;

        Ok(rowid)
    }

    /// Insert a row with a caller-supplied rowid.
    pub fn insert_with_id(&self, table: &str, rowid: u64, data: &[(&str, Value)]) -> Result<()> {
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

        // 3. Type enforcement for indexed columns.
        Self::enforce_index_types(&state.meta, data)?;

        // 4. Advance next_rowid if necessary.
        if rowid >= state.meta.next_rowid {
            state.meta.next_rowid = rowid + 1;
        }

        // 5. Encode row.
        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(rowid, &col_values);

        // 6. WriteGuard for B-tree insert + index maintenance.
        let durability = self.durability();
        {
            let mut guard = self.file.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
            let new_root = tree.insert(rowid, &row_bytes)?;
            state.meta.root_page = new_root;

            if !state.meta.indexes.is_empty() {
                Self::index_update_row(&mut guard, &mut state.meta, rowid, &row_bytes, false)?;
            }

            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
        }

        // 7. Update table state.
        state.meta.row_count += 1;

        Ok(())
    }

    /// Get a row by rowid.
    pub fn get(&self, table: &str, id: u64) -> Result<Option<Row>> {
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

        // 3. Concurrent read via BTreeReader (no file mutex).
        let reader = BTreeReader::new(&self.file, state.meta.root_page);
        let result = reader.search(id)?;

        // 4. Decode.
        match result {
            Some(bytes) => {
                Ok(Some(Row::from_raw(&bytes, state.meta.col_names.clone())?))
            }
            None => Ok(None),
        }
    }

    /// Update a row by rowid. Replaces specified columns.
    pub fn update(&self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
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

        // 3. Type enforcement for indexed columns.
        Self::enforce_index_types(&state.meta, fields)?;

        // 4. Read existing row via concurrent reader (safe: we hold table write lock).
        let existing_bytes = {
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            match reader.search(id)? {
                Some(bytes) => bytes,
                None => return Ok(false),
            }
        };
        let existing = row::decode_row(&existing_bytes)?;

        // Merge updates.
        let mut col_map: HashMap<u16, Value> = existing.columns.into_iter().collect();
        for (name, val) in fields {
            if let Some(col_id) = state.meta.col_id(name) {
                col_map.insert(col_id, val.clone());
            }
        }

        let col_values: Vec<(u16, &Value)> = col_map.iter().map(|(k, v)| (*k, v)).collect();
        let new_row = row::encode_row(id, &col_values);

        // 5. Write via WriteGuard.
        let durability = self.durability();
        {
            let mut guard = self.file.begin_write();

            // Remove old index entries.
            if !state.meta.indexes.is_empty() {
                Self::index_update_row(&mut guard, &mut state.meta, id, &existing_bytes, true)?;
            }

            // Delete + re-insert.
            {
                let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
                tree.delete(id)?;
                let new_root = tree.insert(id, &new_row)?;
                state.meta.root_page = new_root;
            }

            // Insert new index entries.
            if !state.meta.indexes.is_empty() {
                Self::index_update_row(&mut guard, &mut state.meta, id, &new_row, false)?;
            }

            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
        }

        Ok(true)
    }

    /// Delete a row by rowid.
    pub fn delete(&self, table: &str, id: u64) -> Result<bool> {
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

        // 3. Read the row before deletion for index maintenance (concurrent read, safe under table write lock).
        let row_bytes_for_index = if !state.meta.indexes.is_empty() {
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            reader.search(id)?
        } else {
            None
        };

        // 4. Write via WriteGuard.
        let durability = self.durability();
        let deleted = {
            let mut guard = self.file.begin_write();
            let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
            let deleted = tree.delete(id)?;
            state.meta.root_page = tree.root_page();

            if deleted {
                if let Some(ref bytes) = row_bytes_for_index {
                    Self::index_update_row(&mut guard, &mut state.meta, id, bytes, true)?;
                }
            }

            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
            deleted
        };

        // 5. Update row count.
        if deleted {
            state.meta.row_count -= 1;
        }
        Ok(deleted)
    }

    /// Find rows matching filters, with sort and pagination.
    pub fn find(&self, table: &str, opts: FindOptions) -> Result<FindResult> {
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

        // Can we short-circuit (stop early without scanning everything)?
        // Only when: no sort (ordering requires full collection) and not requesting total.
        let can_short_circuit = opts.sort.is_empty() && !opts.include_total;

        // 3. Check for index-accelerated path (Eq filter on an indexed column).
        let index_candidate = opts.filters.iter().find(|f| {
            f.op == FilterOp::Eq
                && state.meta.find_index_for_column(&f.column).is_some()
        });

        // Track whether the scan path already applied pagination (so we don't double-paginate).
        let mut pagination_applied = false;

        let (matching, total) = if let Some(idx_filter) = index_candidate {
            let idx_meta = state.meta.find_index_for_column(&idx_filter.column).unwrap().clone();
            let col_type = state
                .meta
                .columns
                .iter()
                .find(|c| c.name == idx_filter.column)
                .map(|c| c.col_type)
                .unwrap();

            let prefix = match index::encode_value_prefix(col_type, &idx_filter.value) {
                Some(p) => p,
                None => return Ok(FindResult { rows: Vec::new(), total: if opts.include_total { Some(0) } else { None } }),
            };

            // Compute how many index entries we need.
            let need = if can_short_circuit {
                let off = opts.offset.unwrap_or(0) as usize;
                let lim = opts.limit.unwrap_or(u32::MAX) as usize;
                Some(off.saturating_add(lim))
            } else {
                None // need all of them
            };

            let idx_reader = IndexTreeReader::new(&self.file, idx_meta.root_page);
            let keys = if let Some(n) = need {
                idx_reader.scan_prefix_limit(&prefix, n)?
            } else {
                idx_reader.scan_prefix(&prefix)?
            };

            let mut matching_rowids: Vec<u64> = keys
                .iter()
                .map(|k| index::extract_rowid(col_type, k))
                .collect();
            matching_rowids.sort_unstable();

            // Batch-fetch rows via leaf-chain walk (much faster than N individual searches)
            let btree_reader = BTreeReader::new(&self.file, state.meta.root_page);
            let raw_rows = btree_reader.multi_get_sorted(&matching_rowids)?;

            // Check if we need to apply additional filters beyond the indexed one
            let has_extra_filters = opts.filters.len() > 1;

            let col_names = state.meta.col_names.clone();
            let mut rows = Vec::with_capacity(raw_rows.len());
            for bytes in &raw_rows {
                if has_extra_filters {
                    let passes = opts.filters.iter().all(|f| {
                        if let Some(col_id) = state.meta.col_id(&f.column) {
                            if let Ok(Some(raw)) = row::extract_column_raw(bytes, col_id) {
                                if let Some(result) = crate::filter::eval_filter_raw(raw, &f.op, &f.value) {
                                    return result;
                                }
                            }
                            let col_val = row::extract_column(bytes, col_id).ok().flatten();
                            let actual = col_val.as_ref().unwrap_or(&Value::Null);
                            f.matches(actual)
                        } else {
                            f.matches(&Value::Null)
                        }
                    });
                    if passes {
                        rows.push(Row::from_raw(bytes, col_names.clone())?);
                    }
                } else {
                    rows.push(Row::from_raw(bytes, col_names.clone())?);
                }
            }

            // Determine total if requested.
            let total: Option<u64> = if opts.include_total {
                if need.is_some() {
                    // We limited the scan, so get the real count from the index.
                    Some(idx_reader.count_prefix(&prefix)?)
                } else {
                    Some(rows.len() as u64)
                }
            } else {
                None
            };

            // Index path: paginate here when sort is empty (rows are in index order).
            if opts.sort.is_empty() {
                let skip = opts.offset.unwrap_or(0) as usize;
                let take = opts.limit.unwrap_or(u32::MAX) as usize;
                rows = rows.into_iter().skip(skip).take(take).collect();
                pagination_applied = true;
            }

            (rows, total)
        } else if opts.filters.len() == 1 {
            // Single filter: use scan_filtered (extract_column on raw bytes, no full decode)
            let f = &opts.filters[0];
            if let Some(col_id) = state.meta.col_id(&f.column) {
                let reader = BTreeReader::new(&self.file, state.meta.root_page);
                // Only apply limit/offset if no sort (sorted results need full collection first)
                let (lim, off) = if opts.sort.is_empty() {
                    (opts.limit, opts.offset)
                } else {
                    (None, None)
                };
                // Compute stop_after for short-circuit
                let stop = if can_short_circuit {
                    match (opts.offset, opts.limit) {
                        (_, Some(l)) => {
                            let off = opts.offset.unwrap_or(0) as u64;
                            Some(off + l as u64)
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let (raw_rows, count) = reader.scan_filtered(col_id, f.op, &f.value, lim, off, stop)?;
                let col_names = state.meta.col_names.clone();
                let matching: Vec<Row> = raw_rows.iter()
                    .map(|(_, bytes)| Row::from_raw(bytes, col_names.clone()).unwrap())
                    .collect();
                let total = if opts.include_total { Some(count) } else { None };
                // scan_filtered already handled limit/offset when sort is empty.
                if opts.sort.is_empty() {
                    pagination_applied = true;
                }
                (matching, total)
            } else {
                // Column not found -- no matches
                let total = if opts.include_total { Some(0) } else { None };
                (Vec::new(), total)
            }
        } else if opts.filters.is_empty() {
            // No filters: full scan but skip decode, just collect raw
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            let all = reader.scan_all()?;
            let total = if opts.include_total { Some(all.len() as u64) } else { None };
            let col_names = state.meta.col_names.clone();
            let matching: Vec<Row> = all.iter()
                .map(|(_, bytes)| Row::from_raw(bytes, col_names.clone()).unwrap())
                .collect();
            (matching, total)
        } else {
            // Multi-filter: scan all, raw-byte filter, lazy Row
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            let all = reader.scan_all()?;
            let col_names = state.meta.col_names.clone();
            let mut matching = Vec::new();
            for (_, bytes) in &all {
                let passes = opts.filters.iter().all(|f| {
                    if let Some(col_id) = state.meta.col_id(&f.column) {
                        if let Ok(Some(raw)) = row::extract_column_raw(bytes, col_id) {
                            if let Some(result) = crate::filter::eval_filter_raw(raw, &f.op, &f.value) {
                                return result;
                            }
                        }
                        let col_val = row::extract_column(bytes, col_id).ok().flatten();
                        let actual = col_val.as_ref().unwrap_or(&Value::Null);
                        f.matches(actual)
                    } else {
                        f.matches(&Value::Null)
                    }
                });
                if passes {
                    matching.push(Row::from_raw(bytes, col_names.clone())?);
                }
            }
            let total = if opts.include_total { Some(matching.len() as u64) } else { None };
            (matching, total)
        };

        // Sort.
        let mut matching = matching;
        for sort in opts.sort.iter().rev() {
            matching.sort_by(|a, b| {
                let va = a.get(&sort.column);
                let vb = b.get(&sort.column);
                let ord = match (&va, &vb) {
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
        let rows = if !opts.sort.is_empty() {
            // Sort was applied -- always need post-sort pagination.
            let skip = opts.offset.unwrap_or(0) as usize;
            let take = opts.limit.unwrap_or(u32::MAX) as usize;
            matching.into_iter().skip(skip).take(take).collect()
        } else if pagination_applied {
            // Scan path already handled limit/offset.
            matching
        } else {
            // No-filter or multi-filter: apply pagination here.
            let skip = opts.offset.unwrap_or(0) as usize;
            let take = opts.limit.unwrap_or(u32::MAX) as usize;
            matching.into_iter().skip(skip).take(take).collect()
        };

        Ok(FindResult { rows, total })
    }

    /// Count rows matching filters.
    pub fn count(&self, table: &str, filters: &[Filter]) -> Result<u64> {
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

        // Index path: Eq filter on indexed column
        if filters.len() == 1 && filters[0].op == FilterOp::Eq {
            if let Some(idx_meta) = state.meta.find_index_for_column(&filters[0].column) {
                let col_type = state.meta.columns.iter()
                    .find(|c| c.name == filters[0].column)
                    .map(|c| c.col_type);
                if let Some(ct) = col_type {
                    if let Some(prefix) = index::encode_value_prefix(ct, &filters[0].value) {
                        let reader = IndexTreeReader::new(&self.file, idx_meta.root_page);
                        return reader.count_prefix(&prefix);
                    }
                }
            }
        }

        // Single filter: use count_filtered (extract_column on raw bytes)
        if filters.len() == 1 {
            let f = &filters[0];
            if let Some(col_id) = state.meta.col_id(&f.column) {
                let reader = BTreeReader::new(&self.file, state.meta.root_page);
                return reader.count_filtered(col_id, f.op, &f.value);
            }
            return Ok(0);
        }

        // Multi-filter: scan all, raw-byte filter
        let reader = BTreeReader::new(&self.file, state.meta.root_page);
        let all = reader.scan_all()?;

        let mut count = 0u64;
        for (_, bytes) in &all {
            let passes = filters.iter().all(|f| {
                if let Some(col_id) = state.meta.col_id(&f.column) {
                    if let Ok(Some(raw)) = row::extract_column_raw(bytes, col_id) {
                        if let Some(result) = crate::filter::eval_filter_raw(raw, &f.op, &f.value) {
                            return result;
                        }
                    }
                    let col_val = row::extract_column(bytes, col_id).ok().flatten();
                    let actual = col_val.as_ref().unwrap_or(&Value::Null);
                    f.matches(actual)
                } else {
                    f.matches(&Value::Null)
                }
            });
            if passes { count += 1; }
        }

        Ok(count)
    }

    /// Create a secondary index on a table column.
    pub fn create_index(&self, table: &str, index_name: &str, column: &str) -> Result<()> {
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

        // Check index doesn't already exist.
        if state.meta.find_index(index_name).is_some() {
            return Err(BoogyError::IndexExists(index_name.to_string()));
        }

        // Check column exists.
        let col_id = state.meta.col_id(column).ok_or_else(|| {
            BoogyError::SchemaMismatch(format!(
                "column '{column}' not found in table '{table}'"
            ))
        })?;

        let col_type = state
            .meta
            .columns
            .iter()
            .find(|c| c.name == column)
            .map(|c| c.col_type)
            .unwrap();

        // 3. Read all existing rows first (concurrent read, no write guard).
        let all = {
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            reader.scan_all()?
        };

        // 4. Create the index B+ tree and populate via WriteGuard.
        let idx_root = {
            let durability = self.durability();
            let mut guard = self.file.begin_write();
            let idx_root = IndexTreeWriter::create(&mut guard)?;

            let mut current_root = idx_root;
            for (rowid, bytes) in &all {
                let col_val = row::extract_column(bytes, col_id)?
                    .unwrap_or(Value::Null);
                if let Some(key) = index::encode_index_key(col_type, &col_val, *rowid) {
                    let mut tree = IndexTreeWriter::new(&mut guard, current_root);
                    current_root = tree.insert(&key)?;
                }
            }

            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
            current_root
        };

        // 5. Register the index in table metadata.
        state.meta.indexes.push(IndexMeta {
            name: index_name.to_string(),
            column: column.to_string(),
            root_page: idx_root,
        });

        // 6. Persist registry.
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

        Ok(())
    }

    /// Drop a secondary index.
    pub fn drop_index(&self, table: &str, index_name: &str) -> Result<()> {
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

        // Find and remove the index.
        let pos = state
            .meta
            .indexes
            .iter()
            .position(|idx| idx.name == index_name)
            .ok_or_else(|| BoogyError::IndexNotFound(index_name.to_string()))?;
        state.meta.indexes.remove(pos);

        // 3. Persist registry.
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

        Ok(())
    }

    /// Update all rows matching filters. Returns number of rows updated.
    pub fn update_where(
        &self,
        table: &str,
        filters: &[Filter],
        fields: &[(&str, Value)],
    ) -> Result<u64> {
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

        // 3. Type enforcement for indexed columns.
        Self::enforce_index_types(&state.meta, fields)?;

        // 4. Build col_id lookups needed by closures (avoids borrowing state.meta
        //    inside the closures which would conflict with &mut guard).
        let filter_col_ids: Vec<Option<u16>> = filters
            .iter()
            .map(|f| state.meta.col_id(&f.column))
            .collect();
        let field_col_ids: Vec<Option<u16>> = fields
            .iter()
            .map(|(name, _)| state.meta.col_id(name))
            .collect();

        // 5. Batch update via single leaf-chain walk.
        let durability = self.durability();
        let count;
        {
            let mut guard = self.file.begin_write();

            let pred = |data: &[u8]| -> bool {
                filters.iter().enumerate().all(|(i, f)| {
                    if let Some(col_id) = filter_col_ids[i] {
                        if let Ok(Some(raw)) = row::extract_column_raw(data, col_id) {
                            if let Some(result) =
                                crate::filter::eval_filter_raw(raw, &f.op, &f.value)
                            {
                                return result;
                            }
                        }
                        let col_val = row::extract_column(data, col_id).ok().flatten();
                        let actual = col_val.as_ref().unwrap_or(&Value::Null);
                        f.matches(actual)
                    } else {
                        f.matches(&Value::Null)
                    }
                })
            };

            // Build patches: (col_id, &Value) pairs for patch_row_multi
            let patches: Vec<(u16, &Value)> = fields.iter()
                .zip(field_col_ids.iter())
                .filter_map(|((_, val), col_id)| col_id.map(|cid| (cid, val)))
                .collect();

            let updater = |old_bytes: &[u8]| -> Vec<u8> {
                row::patch_row_multi(old_bytes, &patches).unwrap_or_else(|_| {
                    // Fallback: full decode/encode if patch fails
                    let decoded = row::decode_row(old_bytes).unwrap();
                    let mut col_map: HashMap<u16, Value> =
                        decoded.columns.into_iter().collect();
                    for &(col_id, val) in &patches {
                        col_map.insert(col_id, val.clone());
                    }
                    let col_values: Vec<(u16, &Value)> =
                        col_map.iter().map(|(k, v)| (*k, v)).collect();
                    row::encode_row(decoded.id, &col_values)
                })
            };

            let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
            let (in_place, overflow) = tree.update_matching(pred, updater)?;
            state.meta.root_page = tree.root_page();

            // Handle overflow rows: delete + re-insert (they didn't fit in the
            // original page after update).
            for (id, _, new_bytes) in &overflow {
                let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
                tree.delete(*id)?;
                let new_root = tree.insert(*id, new_bytes)?;
                state.meta.root_page = new_root;
            }

            // Index maintenance for all updated rows (both in-place and overflow).
            if !state.meta.indexes.is_empty() {
                for (id, old_bytes, new_bytes) in in_place.iter().chain(overflow.iter()) {
                    Self::index_update_row(
                        &mut guard,
                        &mut state.meta,
                        *id,
                        old_bytes,
                        true,
                    )?;
                    Self::index_update_row(
                        &mut guard,
                        &mut state.meta,
                        *id,
                        new_bytes,
                        false,
                    )?;
                }
            }

            count = (in_place.len() + overflow.len()) as u64;
            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
        }

        Ok(count)
    }

    /// Delete all rows matching filters. Returns number of rows deleted.
    pub fn delete_where(&self, table: &str, filters: &[Filter]) -> Result<u64> {
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

        // 3. Build col_id lookups needed by the predicate closure (avoids
        //    borrowing state.meta inside the closure).
        let filter_col_ids: Vec<Option<u16>> = filters
            .iter()
            .map(|f| state.meta.col_id(&f.column))
            .collect();

        // 4. Batch delete via single leaf-chain walk.
        let durability = self.durability();
        let count;
        {
            let mut guard = self.file.begin_write();

            let pred = |data: &[u8]| -> bool {
                filters.iter().enumerate().all(|(i, f)| {
                    if let Some(col_id) = filter_col_ids[i] {
                        if let Ok(Some(raw)) = row::extract_column_raw(data, col_id) {
                            if let Some(result) =
                                crate::filter::eval_filter_raw(raw, &f.op, &f.value)
                            {
                                return result;
                            }
                        }
                        let col_val = row::extract_column(data, col_id).ok().flatten();
                        let actual = col_val.as_ref().unwrap_or(&Value::Null);
                        f.matches(actual)
                    } else {
                        f.matches(&Value::Null)
                    }
                })
            };

            let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
            let deleted = tree.delete_matching(pred)?;
            state.meta.root_page = tree.root_page();

            // Index maintenance on deleted rows.
            if !state.meta.indexes.is_empty() {
                for (id, old_bytes) in &deleted {
                    Self::index_update_row(
                        &mut guard,
                        &mut state.meta,
                        *id,
                        old_bytes,
                        true,
                    )?;
                }
            }

            count = deleted.len() as u64;
            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
        }

        state.meta.row_count -= count;
        Ok(count)
    }

    /// Insert multiple rows in a single transaction. Returns list of assigned rowids.
    pub fn insert_many(&self, table: &str, rows: &[Vec<(&str, Value)>]) -> Result<Vec<u64>> {
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

        // 3. Type enforcement for all rows.
        for row_data in rows {
            Self::enforce_index_types(&state.meta, row_data)?;
        }

        // 4. Insert all rows under a single WriteGuard.
        let durability = self.durability();
        let ids = {
            let mut guard = self.file.begin_write();
            let mut ids = Vec::with_capacity(rows.len());

            for row_data in rows {
                let rowid = state.meta.next_rowid;
                state.meta.next_rowid += 1;

                let col_values: Vec<(u16, &Value)> = row_data
                    .iter()
                    .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
                    .collect();
                let row_bytes = row::encode_row(rowid, &col_values);

                let mut tree = BTreeWriter::new(&mut guard, state.meta.root_page);
                let new_root = tree.insert(rowid, &row_bytes)?;
                state.meta.root_page = new_root;

                if !state.meta.indexes.is_empty() {
                    Self::index_update_row(&mut guard, &mut state.meta, rowid, &row_bytes, false)?;
                }

                state.meta.row_count += 1;
                ids.push(rowid);
            }

            Self::commit_write(guard, &self.wal, durability, state.meta.table_id)?;
            ids
        };

        Ok(ids)
    }

    /// Run a multi-table transaction.
    pub fn transaction<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&TransactionCtx) -> Result<R>,
    {
        let ctx = TransactionCtx { db: self };
        let result = f(&ctx)?;
        // Individual operations already committed. Do a final flush for consistency.
        let durability = self.durability();
        let guard = self.file.begin_write();
        Self::commit_write(guard, &self.wal, durability, 0)?;
        Ok(result)
    }
}

impl Drop for BoogyDb {
    fn drop(&mut self) {
        // Flush all dirty pages + persist registry on clean shutdown
        let (metas, next_id) = self.snapshot_table_metas();
        {
            let mut guard = self.file.begin_write();
            if self.file.page_count() == 0 {
                let _ = guard.allocate_page();
            }
            let page = serialize_system_page(&metas, next_id);
            guard.put_page(0, page);
            let _ = guard.commit(true);
        }
        let _ = self.file.sync_all();
        if let Ok(mut wal) = self.wal.lock() {
            let _ = wal.truncate();
        }
    }
}

/// Transaction context -- provides the same API as BoogyDb but within a transaction.
pub struct TransactionCtx<'a> {
    db: &'a BoogyDb,
}

impl<'a> TransactionCtx<'a> {
    pub fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        self.db.insert(table, data)
    }

    pub fn get(&self, table: &str, id: u64) -> Result<Option<Row>> {
        self.db.get(table, id)
    }

    pub fn update(&self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
        self.db.update(table, id, fields)
    }

    pub fn delete(&self, table: &str, id: u64) -> Result<bool> {
        self.db.delete(table, id)
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
        let row = db.get("users", id).unwrap().unwrap();
        assert_eq!(row.get("name").unwrap(), Value::Text("alice".into()));
    }

    #[test]
    fn test_insert_returns_sequential_ids() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        let id1 = db.insert("t", &[("v", Value::Integer(1))]).unwrap();
        let id2 = db.insert("t", &[("v", Value::Integer(2))]).unwrap();
        let id3 = db.insert("t", &[("v", Value::Integer(3))]).unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_insert_with_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.insert_with_id("t", 100, &[("v", Value::Integer(42))]).unwrap();
        let row = db.get("t", 100).unwrap().unwrap();
        assert_eq!(row.id, 100);
        assert_eq!(row.get("v").unwrap(), Value::Integer(42));
        // next auto-id should be past 100
        let id = db.insert("t", &[("v", Value::Integer(99))]).unwrap();
        assert_eq!(id, 101);
    }

    #[test]
    fn test_update() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
        let id = db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
        db.update("users", id, &[("name", Value::Text("bob".into()))]).unwrap();
        let row = db.get("users", id).unwrap().unwrap();
        assert_eq!(row.get("name").unwrap(), Value::Text("bob".into()));
    }

    #[test]
    fn test_delete() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
        let id = db.insert("users", &[("name", Value::Text("alice".into()))]).unwrap();
        assert!(db.delete("users", id).unwrap());
        assert!(db.get("users", id).unwrap().is_none());
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
        assert!(db.get("t", 999).unwrap().is_none());
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
            include_total: true,
            ..Default::default()
        };
        let result = db.find("t", opts).unwrap();
        assert_eq!(result.total, Some(2));
        assert_eq!(result.rows.len(), 2);
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
            include_total: true,
            ..Default::default()
        };
        let result = db.find("t", opts).unwrap();
        assert_eq!(result.total, Some(10));
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0].get("v").unwrap(), Value::Integer(9));
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
                thread::spawn(move || {
                    let row = db.get("t", id).unwrap().unwrap();
                    assert_eq!(row.get("v").unwrap(), Value::Integer(42));
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
            let result = db.find("posts", opts).unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].get("likes").unwrap(),
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
            assert!(db.get("a", 1).is_err()); // table "a" should not exist
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

    #[test]
    fn test_next_rowid_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
            let id1 = db.insert("t", &[("v", Value::Integer(1))]).unwrap();
            let id2 = db.insert("t", &[("v", Value::Integer(2))]).unwrap();
            assert_eq!(id1, 1);
            assert_eq!(id2, 2);
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            // After reopen, next rowid should continue from where we left off
            let id3 = db.insert("t", &[("v", Value::Integer(3))]).unwrap();
            assert_eq!(id3, 3);
        }
    }

    #[test]
    fn test_scan_all_matches_count() {
        use crate::btree::BTreeReader;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();

        for i in 0..100 {
            db.insert("t", &[("v", Value::Text(format!("val_{}", i % 10)))]).unwrap();
        }

        // Verify scan_all matches cached row_count
        let tables = db.tables.read().unwrap();
        let state = tables.get("t").unwrap().read().unwrap();
        let reader = BTreeReader::new(&db.file, state.meta.root_page);
        let all = reader.scan_all().unwrap();
        assert_eq!(all.len(), 100);
    }

    #[test]
    fn test_index_basic_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
        db.create_index("t", "idx_v", "v").unwrap();
        let _id = db.insert("t", &[("v", Value::Text("hello".into()))]).unwrap();
        let opts = crate::filter::FindOptions {
            filters: vec![crate::filter::Filter::eq("v", "hello")],
            include_total: true,
            ..Default::default()
        };
        let result = db.find("t", opts).unwrap();
        assert_eq!(result.total, Some(1));
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn test_index_populated_on_create() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        // Insert rows BEFORE creating the index.
        db.insert("t", &[("v", Value::Integer(10))]).unwrap();
        db.insert("t", &[("v", Value::Integer(20))]).unwrap();
        db.insert("t", &[("v", Value::Integer(30))]).unwrap();
        // Index creation should backfill existing rows.
        db.create_index("t", "idx_v", "v").unwrap();
        let opts = crate::filter::FindOptions {
            filters: vec![crate::filter::Filter::eq("v", 20i64)],
            include_total: true,
            ..Default::default()
        };
        let result = db.find("t", opts).unwrap();
        assert_eq!(result.total, Some(1));
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("v").unwrap(), Value::Integer(20));
    }

    #[test]
    fn test_index_maintained_on_delete() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
        db.create_index("t", "idx_v", "v").unwrap();
        let id1 = db.insert("t", &[("v", Value::Text("a".into()))]).unwrap();
        let _id2 = db.insert("t", &[("v", Value::Text("b".into()))]).unwrap();
        db.delete("t", id1).unwrap();
        let opts = crate::filter::FindOptions {
            filters: vec![crate::filter::Filter::eq("v", "a")],
            ..Default::default()
        };
        let result = db.find("t", opts).unwrap();
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn test_index_maintained_on_update() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
        db.create_index("t", "idx_v", "v").unwrap();
        let id = db.insert("t", &[("v", Value::Text("old".into()))]).unwrap();
        db.update("t", id, &[("v", Value::Text("new".into()))]).unwrap();
        // Old value should not be found.
        let opts_old = crate::filter::FindOptions {
            filters: vec![crate::filter::Filter::eq("v", "old")],
            ..Default::default()
        };
        let result = db.find("t", opts_old).unwrap();
        assert_eq!(result.rows.len(), 0);
        // New value should be found.
        let opts_new = crate::filter::FindOptions {
            filters: vec![crate::filter::Filter::eq("v", "new")],
            ..Default::default()
        };
        let result = db.find("t", opts_new).unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn test_index_type_enforcement() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.create_index("t", "idx_v", "v").unwrap();
        // Inserting wrong type should fail.
        let result = db.insert("t", &[("v", Value::Text("hello".into()))]);
        assert!(result.is_err());
        // Null should be fine (not indexed).
        let result = db.insert("t", &[("v", Value::Null)]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_find_short_circuits_at_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..1000 {
            db.insert("t", &[("v", Value::Integer(i % 10))]).unwrap();
        }

        // Without include_total
        let result = db.find("t", FindOptions {
            filters: vec![crate::filter::Filter::eq("v", 5i64)],
            limit: Some(5),
            ..Default::default()
        }).unwrap();
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.total, None);

        // With include_total
        let result = db.find("t", FindOptions {
            filters: vec![crate::filter::Filter::eq("v", 5i64)],
            limit: Some(5),
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.total, Some(100));
    }

    #[test]
    fn test_count_uses_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
        db.create_index("t", "idx_v", "v").unwrap();
        for i in 0..100 {
            db.insert("t", &[("v", Value::Text(format!("val_{}", i % 5)))]).unwrap();
        }
        assert_eq!(db.count("t", &[Filter::eq("v", "val_2")]).unwrap(), 20);
    }
}
