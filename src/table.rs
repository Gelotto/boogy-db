use std::collections::HashMap;

use crate::value::ColumnDef;

/// Metadata for a registered table.
#[derive(Debug, Clone)]
pub struct TableMeta {
    pub name: String,
    pub table_id: u32,
    pub columns: Vec<ColumnDef>,
    /// Column name -> column ID mapping.
    pub col_name_to_id: HashMap<String, u16>,
    /// B+ tree root page number.
    pub root_page: u32,
    /// Number of rows (maintained by insert/delete).
    pub row_count: u64,
}

impl TableMeta {
    pub fn new(name: String, table_id: u32, columns: Vec<ColumnDef>, root_page: u32) -> Self {
        let col_name_to_id: HashMap<String, u16> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i as u16))
            .collect();
        Self {
            name,
            table_id,
            columns,
            col_name_to_id,
            root_page,
            row_count: 0,
        }
    }

    pub fn col_id(&self, name: &str) -> Option<u16> {
        self.col_name_to_id.get(name).copied()
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
