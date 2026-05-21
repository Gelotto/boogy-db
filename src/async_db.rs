//! Async wrapper for BoogyDb. Enabled with the `tokio` feature.
//!
//! Methods call the synchronous core directly — no spawn_blocking,
//! no thread dispatch, zero overhead. The `async` keyword lets callers
//! `.await` in async contexts.

use std::path::Path;
use std::sync::Arc;

use crate::db::{BoogyDb, Durability, Row, Transaction, TransactionCtx};
use crate::error::Result;
use crate::filter::{Filter, FindOptions, FindResult};
use crate::value::{ColumnDef, Value};

/// Async wrapper around [`BoogyDb`]. All methods delegate directly
/// to the synchronous implementation with zero overhead.
///
/// ```ignore
/// let db = AsyncBoogyDb::open("my.boogy").await?;
/// let id = db.insert("users", &[("name", Value::Text("Alice".into()))]).await?;
/// ```
#[derive(Clone)]
pub struct AsyncBoogyDb {
    inner: Arc<BoogyDb>,
}

impl AsyncBoogyDb {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = BoogyDb::open(path)?;
        Ok(Self { inner: Arc::new(db) })
    }

    pub fn set_durability(&self, d: Durability) {
        self.inner.set_durability(d);
    }

    pub fn durability(&self) -> Durability {
        self.inner.durability()
    }

    pub async fn create_table(&self, name: &str, columns: &[ColumnDef]) -> Result<()> {
        self.inner.create_table(name, columns)
    }

    pub async fn create_table_encrypted(
        &self,
        name: &str,
        columns: &[ColumnDef],
        key: &[u8; 32],
    ) -> Result<()> {
        self.inner.create_table_encrypted(name, columns, key)
    }

    pub async fn unlock_table(&self, name: &str, key: &[u8; 32]) -> Result<()> {
        self.inner.unlock_table(name, key)
    }

    pub async fn drop_table(&self, name: &str) -> Result<()> {
        self.inner.drop_table(name)
    }

    pub async fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        self.inner.insert(table, data)
    }

    pub async fn insert_with_id(
        &self,
        table: &str,
        rowid: u64,
        data: &[(&str, Value)],
    ) -> Result<()> {
        self.inner.insert_with_id(table, rowid, data)
    }

    pub async fn get(&self, table: &str, id: u64) -> Result<Option<Row>> {
        self.inner.get(table, id)
    }

    pub async fn update(
        &self,
        table: &str,
        id: u64,
        fields: &[(&str, Value)],
    ) -> Result<bool> {
        self.inner.update(table, id, fields)
    }

    pub async fn delete(&self, table: &str, id: u64) -> Result<bool> {
        self.inner.delete(table, id)
    }

    pub async fn find(&self, table: &str, opts: FindOptions) -> Result<FindResult> {
        self.inner.find(table, opts)
    }

    pub async fn count(&self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.inner.count(table, filters)
    }

    pub async fn count_with(
        &self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
    ) -> Result<u64> {
        self.inner.count_with(table, filters, or_groups)
    }

    pub async fn insert_many(
        &self,
        table: &str,
        rows: &[Vec<(&str, Value)>],
    ) -> Result<Vec<u64>> {
        self.inner.insert_many(table, rows)
    }

    pub async fn update_where(
        &self,
        table: &str,
        filters: &[Filter],
        fields: &[(&str, Value)],
    ) -> Result<u64> {
        self.inner.update_where(table, filters, fields)
    }

    pub async fn delete_where(&self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.inner.delete_where(table, filters)
    }

    pub async fn create_index(
        &self,
        table: &str,
        index_name: &str,
        column: &str,
    ) -> Result<()> {
        self.inner.create_index(table, index_name, column)
    }

    pub async fn drop_index(&self, table: &str, index_name: &str) -> Result<()> {
        self.inner.drop_index(table, index_name)
    }

    pub async fn transaction<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&TransactionCtx) -> Result<R>,
    {
        self.inner.transaction(f)
    }

    /// Begin a guard-based transaction. Returns an `AsyncTransaction` that
    /// commits on explicit `.commit()`. Operations within the transaction lock
    /// tables lazily, same as the callback-based `transaction()`.
    pub async fn begin(&self) -> Result<AsyncTransaction<'_>> {
        let inner = self.inner.begin()?;
        Ok(AsyncTransaction { inner })
    }

    pub fn set_acid(&self, enabled: bool) {
        self.inner.set_acid(enabled);
    }

    pub fn is_acid(&self) -> bool {
        self.inner.is_acid()
    }

    /// Access the underlying synchronous `BoogyDb`.
    pub fn inner(&self) -> &BoogyDb {
        &self.inner
    }
}

/// Async guard-based transaction. Commits on explicit `.commit()`, rolls back on drop.
pub struct AsyncTransaction<'a> {
    inner: Transaction<'a>,
}

impl<'a> AsyncTransaction<'a> {
    /// Commit the transaction. Flushes any pending WAL entries.
    pub async fn commit(self) -> Result<()> {
        self.inner.commit()
    }

    pub async fn insert(&mut self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        self.inner.insert(table, data)
    }

    pub async fn insert_with_id(&mut self, table: &str, rowid: u64, data: &[(&str, Value)]) -> Result<()> {
        self.inner.insert_with_id(table, rowid, data)
    }

    pub async fn get(&mut self, table: &str, id: u64) -> Result<Option<Row>> {
        self.inner.get(table, id)
    }

    pub async fn update(&mut self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
        self.inner.update(table, id, fields)
    }

    pub async fn delete(&mut self, table: &str, id: u64) -> Result<bool> {
        self.inner.delete(table, id)
    }

    pub async fn find(&mut self, table: &str, opts: FindOptions) -> Result<FindResult> {
        self.inner.find(table, opts)
    }

    pub async fn count(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.inner.count(table, filters)
    }

    pub async fn count_with(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
    ) -> Result<u64> {
        self.inner.count_with(table, filters, or_groups)
    }

    pub async fn insert_many(&mut self, table: &str, rows: &[Vec<(&str, Value)>]) -> Result<Vec<u64>> {
        self.inner.insert_many(table, rows)
    }

    pub async fn update_where(&mut self, table: &str, filters: &[Filter], fields: &[(&str, Value)]) -> Result<u64> {
        self.inner.update_where(table, filters, fields)
    }

    pub async fn delete_where(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.inner.delete_where(table, filters)
    }
}

// ---------------------------------------------------------------------------
// Vector search (async wrappers)
// ---------------------------------------------------------------------------

#[cfg(feature = "vector")]
impl AsyncBoogyDb {
    pub async fn create_vector_collection(
        &self,
        table: &str,
        name: &str,
        options: &crate::vector::VectorCollectionOptions,
    ) -> Result<()> {
        self.inner.create_vector_collection(table, name, options)
    }

    pub async fn drop_vector_collection(&self, table: &str, name: &str) -> Result<()> {
        self.inner.drop_vector_collection(table, name)
    }

    pub async fn vector_insert(
        &self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()> {
        self.inner.vector_insert(table, collection, rowid, vector)
    }

    pub async fn vector_insert_batch(
        &self, table: &str, collection: &str, entries: &[(u64, Vec<f32>)],
    ) -> Result<()> {
        self.inner.vector_insert_batch(table, collection, entries)
    }

    pub async fn vector_update(
        &self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()> {
        self.inner.vector_update(table, collection, rowid, vector)
    }

    pub async fn vector_delete(
        &self, table: &str, collection: &str, rowid: u64,
    ) -> Result<()> {
        self.inner.vector_delete(table, collection, rowid)
    }

    pub async fn vector_search(
        &self, table: &str, collection: &str, query: &[f32], options: &crate::vector::VectorSearchOptions,
    ) -> Result<Vec<crate::vector::VectorResult>> {
        self.inner.vector_search(table, collection, query, options)
    }

    pub async fn unlock_vector_collection(
        &self, table: &str, name: &str, key: &[u8; 32],
    ) -> Result<()> {
        self.inner.unlock_vector_collection(table, name, key)
    }
}
