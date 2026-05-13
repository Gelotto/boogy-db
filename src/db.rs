use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::btree::BTree;
use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::filter::{FindOptions, SortDir};
use crate::row;
use crate::table::TableMeta;
use crate::value::{ColumnDef, Value};

/// A row returned from queries.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub columns: Vec<(String, Value)>,
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
    tables: RwLock<HashMap<String, Arc<RwLock<TableState>>>>,
    next_table_id: Mutex<u32>,
    #[allow(dead_code)]
    path: PathBuf,
}

impl BoogyDb {
    /// Open or create a database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = PageFile::open(&path)?;

        Ok(Self {
            file: Mutex::new(file),
            tables: RwLock::new(HashMap::new()),
            next_table_id: Mutex::new(1),
            path,
        })
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

        // Allocate the root page (needs file lock).
        let root = {
            let mut file = self.file.lock().unwrap();
            let root = BTree::create(&mut file)?;
            file.flush()?;
            root
        };

        // Assign a table id.
        let table_id = {
            let mut next = self.next_table_id.lock().unwrap();
            let id = *next;
            *next += 1;
            id
        };

        let meta = TableMeta::new(name.to_string(), table_id, columns.to_vec(), root);
        let state = Arc::new(RwLock::new(TableState { meta }));

        // Write-lock the table map to insert.
        let mut tables = self.tables.write().unwrap();
        if tables.contains_key(name) {
            // Another thread raced us.
            return Err(BoogyError::TableExists(name.to_string()));
        }
        tables.insert(name.to_string(), state);
        Ok(())
    }

    /// Drop a table.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut tables = self.tables.write().unwrap();
        if tables.remove(name).is_none() {
            return Err(BoogyError::TableNotFound(name.to_string()));
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

        // 4. Brief file lock for B-tree insert.
        let new_root = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            let new_root = tree.insert(&id, &row_bytes)?;
            file.flush()?;
            new_root
        };

        // 5. Update table state.
        if new_root != state.meta.root_page {
            state.meta.root_page = new_root;
        }
        state.meta.row_count += 1;

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

        // 3. Read existing row, merge, write back (file lock).
        let new_root = {
            let mut file = self.file.lock().unwrap();
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
            file.flush()?;
            new_root
        };

        // 4. Update table state.
        if new_root != state.meta.root_page {
            state.meta.root_page = new_root;
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

        // 3. Brief file lock for B-tree delete.
        let deleted = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, state.meta.root_page);
            let deleted = tree.delete(id)?;
            file.flush()?;
            deleted
        };

        // 4. Update row count.
        if deleted {
            state.meta.row_count -= 1;
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
        // Flush all changes.
        let mut file = self.file.lock().unwrap();
        file.flush()?;
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
