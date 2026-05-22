use std::collections::HashMap;
use std::sync::Arc;

use crate::crypto::Cipher;
use crate::value::ColumnDef;

/// Metadata for a secondary index.
#[derive(Debug, Clone)]
pub struct IndexMeta {
    /// Name of this index (e.g. "idx_author").
    pub name: String,
    /// Columns this index covers, in key order. Length 1 = single-column (back-compat).
    pub columns: Vec<String>,
    /// When true, inserts/upserts whose key tuple already exists are rejected.
    pub unique: bool,
    /// B+ tree root page number for the index tree.
    pub root_page: u32,
}

/// Metadata for a registered table.
#[derive(Debug, Clone)]
pub struct TableMeta {
    pub name: String,
    pub table_id: u32,
    pub columns: Vec<ColumnDef>,
    /// Column name -> column ID mapping.
    pub col_name_to_id: HashMap<String, u16>,
    /// Shared column names for lazy Row decoding (positional, indexed by col_id).
    pub col_names: Arc<Vec<String>>,
    /// Shared column definitions for default-at-read (positional, in lockstep with col_names).
    pub col_defs: Arc<Vec<ColumnDef>>,
    /// B+ tree root page number.
    pub root_page: u32,
    /// Number of rows (maintained by insert/delete).
    pub row_count: u64,
    /// Next rowid to assign on insert.
    pub next_rowid: u64,
    /// Secondary indexes on this table.
    pub indexes: Vec<IndexMeta>,
    /// Whether this table's pages are encrypted at rest.
    pub encrypted: bool,
    /// In-memory cipher for encrypt/decrypt (not persisted).
    pub cipher: Option<Cipher>,
}

impl TableMeta {
    pub fn new(name: String, table_id: u32, columns: Vec<ColumnDef>, root_page: u32) -> Self {
        let col_name_to_id: HashMap<String, u16> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i as u16))
            .collect();
        let col_names = Arc::new(columns.iter().map(|c| c.name.clone()).collect());
        let col_defs = Arc::new(columns.clone());
        Self {
            name,
            table_id,
            columns,
            col_name_to_id,
            col_names,
            col_defs,
            root_page,
            row_count: 0,
            next_rowid: 1,
            indexes: Vec::new(),
            encrypted: false,
            cipher: None,
        }
    }

    pub fn col_id(&self, name: &str) -> Option<u16> {
        self.col_name_to_id.get(name).copied()
    }

    /// Find index metadata by index name.
    pub fn find_index(&self, name: &str) -> Option<&IndexMeta> {
        self.indexes.iter().find(|idx| idx.name == name)
    }

    /// Find an index whose leading column matches `column`.
    pub fn find_index_for_column(&self, column: &str) -> Option<&IndexMeta> {
        self.indexes
            .iter()
            .find(|idx| idx.columns.first().map(|c| c == column).unwrap_or(false))
    }

    /// Append a new column. Its col_id is the new last index (vec position).
    /// Caller has already validated name-uniqueness and NOT-NULL-needs-default.
    pub fn add_column(&mut self, col: ColumnDef) {
        let col_id = self.columns.len() as u16;
        self.col_name_to_id.insert(col.name.clone(), col_id);
        // col_names is positional (indexed by col_id) — push in lockstep.
        let mut names = (*self.col_names).clone();
        names.push(col.name.clone());
        self.col_names = std::sync::Arc::new(names);
        self.columns.push(col);
        // col_defs is also positional — rebuild from columns to stay in sync.
        self.col_defs = std::sync::Arc::new(self.columns.clone());
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

    pub fn register(
        &mut self,
        name: String,
        columns: Vec<ColumnDef>,
        root_page: u32,
    ) -> &TableMeta {
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
