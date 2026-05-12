use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::btree::BTree;
use crate::error::{BoogyError, Result};
use crate::file::PageFile;
use crate::filter::{FindOptions, SortDir};
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
    #[allow(dead_code)]
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
        let meta = {
            let registry = self.registry.lock().unwrap();
            registry
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        let id = uuid::Uuid::new_v4().to_string();

        // Convert column names to IDs
        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| meta.col_id(name).map(|cid| (cid, val)))
            .collect();

        let row_bytes = row::encode_row(&id, &col_values);

        let new_root = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, meta.root_page);
            let new_root = tree.insert(&id, &row_bytes)?;
            file.flush()?;
            new_root
        };

        // Update registry
        {
            let mut registry = self.registry.lock().unwrap();
            let meta = registry.get_mut(table).unwrap();
            if new_root != meta.root_page {
                meta.root_page = new_root;
            }
            meta.row_count += 1;
        }

        Ok(id)
    }

    /// Get a row by _id.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Row>> {
        let meta = {
            let registry = self.registry.lock().unwrap();
            registry
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

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
        let meta = {
            let registry = self.registry.lock().unwrap();
            registry
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        let new_root = {
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
            new_root
        };

        // Update root if changed
        {
            let mut registry = self.registry.lock().unwrap();
            let meta = registry.get_mut(table).unwrap();
            if new_root != meta.root_page {
                meta.root_page = new_root;
            }
        }

        Ok(true)
    }

    /// Delete a row by _id.
    pub fn delete(&self, table: &str, id: &str) -> Result<bool> {
        let meta = {
            let registry = self.registry.lock().unwrap();
            registry
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        let deleted = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, meta.root_page);
            let deleted = tree.delete(id)?;
            file.flush()?;
            deleted
        };

        if deleted {
            let mut registry = self.registry.lock().unwrap();
            let meta = registry.get_mut(table).unwrap();
            meta.row_count -= 1;
        }
        Ok(deleted)
    }

    /// Find rows matching filters, with sort and pagination.
    /// Returns (matching_rows, total_count).
    pub fn find(&self, table: &str, opts: FindOptions) -> Result<(Vec<Row>, u64)> {
        let meta = {
            let registry = self.registry.lock().unwrap();
            registry
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        let all = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, meta.root_page);
            tree.scan_all()?
        };

        // Decode and filter
        let mut matching: Vec<Row> = Vec::new();
        for (_, bytes) in &all {
            let decoded = row::decode_row(bytes)?;
            let row = decoded_to_row(&decoded, &meta);

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

        // Sort
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

        // Pagination
        let skip = opts.offset.unwrap_or(0) as usize;
        let take = opts.limit.unwrap_or(u32::MAX) as usize;
        let page: Vec<Row> = matching.into_iter().skip(skip).take(take).collect();

        Ok((page, total))
    }

    /// Count rows matching filters.
    pub fn count(&self, table: &str, filters: &[crate::filter::Filter]) -> Result<u64> {
        let meta = {
            let registry = self.registry.lock().unwrap();
            registry
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        if filters.is_empty() {
            return Ok(meta.row_count);
        }

        let all = {
            let mut file = self.file.lock().unwrap();
            let mut tree = BTree::new(&mut file, meta.root_page);
            tree.scan_all()?
        };

        let mut count = 0u64;
        for (_, bytes) in &all {
            let decoded = row::decode_row(bytes)?;
            let row = decoded_to_row(&decoded, &meta);

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
        // Flush all changes
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
