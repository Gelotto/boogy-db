//! Async wrapper for BoogyDb. Enabled with the `tokio` feature.
//!
//! Methods call the synchronous core directly — no spawn_blocking,
//! no thread dispatch, zero overhead. The `async` keyword lets callers
//! `.await` in async contexts.

use std::path::Path;
use std::sync::Arc;

use crate::db::{BoogyDb, Durability, Row, ScanBatch, ScanKey, ScanOrder, Transaction, TransactionCtx};
use crate::error::Result;
use crate::filter::{Filter, FindOptions, FindResult};
use crate::value::{ColumnDef, Value};

use tokio::sync::Mutex as AsyncMutex;

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
    /// Serializes all async write operations. A held interactive write-tx
    /// acquires this for its entire lifetime; non-tx writes hold it only
    /// for the duration of the call. Reads are never gated.
    write_gate: Arc<AsyncMutex<()>>,
}

impl AsyncBoogyDb {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = BoogyDb::open(path)?;
        Ok(Self {
            inner: Arc::new(db),
            write_gate: Arc::new(AsyncMutex::new(())),
        })
    }

    pub fn set_durability(&self, d: Durability) {
        self.inner.set_durability(d);
    }

    pub fn durability(&self) -> Durability {
        self.inner.durability()
    }

    pub async fn create_table(&self, name: &str, columns: &[ColumnDef]) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.create_table(name, columns)
    }

    pub async fn create_table_encrypted(
        &self,
        name: &str,
        columns: &[ColumnDef],
        key: &[u8; 32],
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.create_table_encrypted(name, columns, key)
    }

    pub async fn unlock_table(&self, name: &str, key: &[u8; 32]) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.unlock_table(name, key)
    }

    pub async fn drop_table(&self, name: &str) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.drop_table(name)
    }

    pub async fn insert(&self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        let _w = self.write_gate.lock().await;
        self.inner.insert(table, data)
    }

    pub async fn insert_with_id(
        &self,
        table: &str,
        rowid: u64,
        data: &[(&str, Value)],
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
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
        let _w = self.write_gate.lock().await;
        self.inner.update(table, id, fields)
    }

    pub async fn delete(&self, table: &str, id: u64) -> Result<bool> {
        let _w = self.write_gate.lock().await;
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
        let _w = self.write_gate.lock().await;
        self.inner.insert_many(table, rows)
    }

    pub async fn update_where(
        &self,
        table: &str,
        filters: &[Filter],
        fields: &[(&str, Value)],
    ) -> Result<u64> {
        let _w = self.write_gate.lock().await;
        self.inner.update_where(table, filters, fields)
    }

    pub async fn delete_where(&self, table: &str, filters: &[Filter]) -> Result<u64> {
        let _w = self.write_gate.lock().await;
        self.inner.delete_where(table, filters)
    }

    pub async fn upsert_increment(
        &self,
        table: &str,
        key: &[(&str, Value)],
        counter: &str,
        delta: Value,
        set: &[(&str, Value)],
    ) -> Result<u64> {
        let _w = self.write_gate.lock().await;
        self.inner.upsert_increment(table, key, counter, delta, set)
    }

    pub async fn add_column(&self, table: &str, column: ColumnDef) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.add_column(table, column)
    }

    pub async fn rename_column(&self, table: &str, old: &str, new: &str) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.rename_column(table, old, new)
    }

    pub async fn drop_column(&self, table: &str, name: &str) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.drop_column(table, name)
    }

    /// Return the live (non-dropped) columns for `table`, in schema order.
    /// This is a read — no write-gate acquired.
    pub async fn list_columns(&self, table: &str) -> Result<Vec<ColumnDef>> {
        self.inner.list_columns(table)
    }

    pub async fn create_index(
        &self,
        table: &str,
        index_name: &str,
        column: &str,
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.create_index(table, index_name, column)
    }

    pub async fn drop_index(&self, table: &str, index_name: &str) -> Result<()> {
        let _w = self.write_gate.lock().await;
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
// OwnedAsyncTransaction — owned interactive transaction holdable across .await
// ---------------------------------------------------------------------------

/// An owned interactive transaction that holds an `AcidTransaction<'static>`
/// (built from a cloned `Arc<BoogyDb>`) and lazily acquires the write-gate on
/// the first write operation. Commit applies all staged writes atomically and
/// releases the gate; drop-without-commit rolls back silently (the overlay is
/// discarded) and also releases the gate.
pub struct OwnedAsyncTransaction {
    // Use `Option` so that `commit` can move the `AcidTransaction` out of
    // `self` via `take()` while also needing to clear `guard` — avoiding
    // any borrow-checker tension from a consuming-self call on a field.
    tx: Option<crate::db::AcidTransaction<'static>>,
    gate: Arc<AsyncMutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl OwnedAsyncTransaction {
    /// Acquire the write-gate on the first write op (idempotent: a second call
    /// is a no-op because the guard is already held).
    async fn ensure_write_gate(&mut self) {
        if self.guard.is_none() {
            self.guard = Some(Arc::clone(&self.gate).lock_owned().await);
        }
    }

    // WRITE ops: acquire the gate lazily, then delegate to the inner tx.

    pub async fn insert(&mut self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().insert(table, data)
    }

    pub async fn insert_with_id(
        &mut self,
        table: &str,
        rowid: u64,
        data: &[(&str, Value)],
    ) -> Result<()> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().insert_with_id(table, rowid, data)
    }

    pub async fn update(
        &mut self,
        table: &str,
        id: u64,
        fields: &[(&str, Value)],
    ) -> Result<bool> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().update(table, id, fields)
    }

    pub async fn delete(&mut self, table: &str, id: u64) -> Result<bool> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().delete(table, id)
    }

    pub async fn insert_many(
        &mut self,
        table: &str,
        rows: &[Vec<(&str, Value)>],
    ) -> Result<Vec<u64>> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().insert_many(table, rows)
    }

    pub async fn update_where(
        &mut self,
        table: &str,
        filters: &[Filter],
        fields: &[(&str, Value)],
    ) -> Result<u64> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().update_where(table, filters, fields)
    }

    pub async fn delete_where(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().delete_where(table, filters)
    }

    pub async fn upsert_increment(
        &mut self,
        table: &str,
        key: &[(&str, Value)],
        counter: &str,
        delta: Value,
        set: &[(&str, Value)],
    ) -> Result<u64> {
        self.ensure_write_gate().await;
        self.tx.as_mut().unwrap().upsert_increment(table, key, counter, delta, set)
    }

    // READ ops: no gate; they read the overlay (read-your-writes).

    pub async fn get(&mut self, table: &str, id: u64) -> Result<Option<Row>> {
        self.tx.as_mut().unwrap().get(table, id)
    }

    pub async fn find(&mut self, table: &str, opts: FindOptions) -> Result<FindResult> {
        self.tx.as_mut().unwrap().find(table, opts)
    }

    pub async fn count(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.tx.as_mut().unwrap().count(table, filters)
    }

    pub async fn count_with(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
    ) -> Result<u64> {
        self.tx.as_mut().unwrap().count_with(table, filters, or_groups)
    }

    /// Resumable ordered cursor — reads through the tx overlay (read-your-writes).
    /// `limit` is `u32` matching `AcidTransaction::scan_batch`.
    pub async fn scan_batch(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
        order: ScanOrder,
        after: Option<ScanKey>,
        limit: u32,
    ) -> Result<ScanBatch> {
        self.tx.as_mut().unwrap().scan_batch(table, filters, or_groups, order, after, limit)
    }

    /// Commit: apply the overlay atomically, then release the write-gate.
    ///
    /// `AcidTransaction::commit` is a consuming `fn commit(mut self)`, so we
    /// take it out of the `Option`, commit it, then clear the guard.
    pub async fn commit(mut self) -> Result<()> {
        let tx = self.tx.take().expect("OwnedAsyncTransaction: tx already consumed");
        let r = tx.commit();
        // Release the gate (drop the guard). `self.guard` is dropped when
        // `self` goes out of scope at the end of this function, but make it
        // explicit for clarity.
        self.guard = None;
        r
    }

    // Drop without commit = rollback. `AcidTransaction`'s own Drop impl is a
    // no-op body — it discards `private_dirty` and `meta_deltas` without
    // touching the committed database state. The `guard` drops here too,
    // releasing the write-gate.
}

impl AsyncBoogyDb {
    /// Begin an owned interactive transaction (read-your-writes + eager real
    /// ids), holdable across `.await`. Serializes with other writers via the
    /// write-gate, acquired lazily on the first write.
    pub async fn begin_interactive(&self) -> Result<OwnedAsyncTransaction> {
        let tx = crate::db::AcidTransaction::new_owned(Arc::clone(&self.inner));
        Ok(OwnedAsyncTransaction {
            tx: Some(tx),
            gate: Arc::clone(&self.write_gate),
            guard: None,
        })
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
        let _w = self.write_gate.lock().await;
        self.inner.create_vector_collection(table, name, options)
    }

    pub async fn drop_vector_collection(&self, table: &str, name: &str) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.drop_vector_collection(table, name)
    }

    pub async fn vector_insert(
        &self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.vector_insert(table, collection, rowid, vector)
    }

    pub async fn vector_insert_batch(
        &self, table: &str, collection: &str, entries: &[(u64, Vec<f32>)],
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.vector_insert_batch(table, collection, entries)
    }

    pub async fn vector_update(
        &self, table: &str, collection: &str, rowid: u64, vector: &[f32],
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
        self.inner.vector_update(table, collection, rowid, vector)
    }

    pub async fn vector_delete(
        &self, table: &str, collection: &str, rowid: u64,
    ) -> Result<()> {
        let _w = self.write_gate.lock().await;
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
        let _w = self.write_gate.lock().await;
        self.inner.unlock_vector_collection(table, name, key)
    }
}
