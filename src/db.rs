use std::collections::HashMap;
use std::collections::HashMap as StdHashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::btree::{BTreeReader, BTreeWriter};
use crate::error::{BoogyError, Result};
use crate::file::{PageFile, WriteGuard};
use crate::filter::{Filter, FilterOp, FindOptions, FindResult, SortDir};
use crate::index::{self, IndexTreeReader, IndexTreeWriter};
use crate::page::{Page, PAGE_HEADER_SIZE, PAGE_SIZE, PAGE_SYSTEM};
use crate::row;
use crate::table::{IndexInfo, IndexMeta, TableInfo, TableMeta};
use crate::value::{ColumnDef, Type, Value};
use crate::wal::Wal;

#[cfg(feature = "vector")]
use crate::vector::VectorCollection;
#[cfg(feature = "vector")]
use crate::vector::{VectorCollectionOptions, VectorResult, VectorSearchOptions};

/// Evaluate a single filter against one row's raw bytes.
///
/// Resolves the filter's column via `col_name_to_id`, then takes the raw-byte
/// fast path (`eval_filter_raw_full`) when possible, falling back to decoding
/// the column to a `Value` and `Filter::matches`. A filter on an unknown column
/// matches against `Value::Null` (mirroring the historical inline logic).
///
/// When a column is physically absent from the row (e.g. added via `add_column`
/// after this row was written) AND the column has a stored `default`, the
/// predicate matches against the default — exactly mirroring `Row::get()`'s
/// "absent → `default` else Null" logic.  A column absent with no default is
/// treated as `Null` (current behaviour; `IS NULL` matches).
///
/// This is the single home for the per-row predicate that used to be copy-pasted
/// across the find/count scan branches.
fn filter_matches_row(
    col_name_to_id: &HashMap<String, u16>,
    col_defs: &[ColumnDef],
    bytes: &[u8],
    f: &Filter,
) -> bool {
    if let Some(col_id) = col_name_to_id.get(&f.column).copied() {
        if let Ok(Some(raw)) = row::extract_column_raw(bytes, col_id) {
            if let Some(result) = crate::filter::eval_filter_raw_full(raw, f) {
                return result;
            }
        }
        let col_val = row::extract_column(bytes, col_id).ok().flatten();
        match col_val {
            Some(ref v) => f.matches(v),
            None => {
                // Column absent from this row — apply the stored default (if any),
                // mirroring Row::get()'s semantics exactly.
                let default_val = col_defs
                    .get(col_id as usize)
                    .and_then(|d| d.default.clone())
                    .unwrap_or(Value::Null);
                f.matches(&default_val)
            }
        }
    } else {
        f.matches(&Value::Null)
    }
}

/// Evaluate the full find predicate against one row's raw bytes:
/// `ALL(filters) AND (or_groups.is_empty() OR ANY(group: ALL(group)))`.
///
/// `filters` is the mandatory AND-prefix; `or_groups` adds an OR-of-AND clause
/// when non-empty. With an empty `or_groups` this is exactly the historical
/// `filters.iter().all(...)` behavior.
///
/// `col_defs` is the positional column-definition slice (from `TableMeta::col_defs`)
/// used to resolve stored defaults for columns absent from older row bytes.
fn row_passes(
    col_name_to_id: &HashMap<String, u16>,
    col_defs: &[ColumnDef],
    bytes: &[u8],
    filters: &[Filter],
    or_groups: &[Vec<Filter>],
) -> bool {
    filters.iter().all(|f| filter_matches_row(col_name_to_id, col_defs, bytes, f))
        && (or_groups.is_empty()
            || or_groups
                .iter()
                .any(|g| g.iter().all(|f| filter_matches_row(col_name_to_id, col_defs, bytes, f))))
}

/// Add a numeric `delta` to a `current` counter value, preserving type:
/// Integer+Integer→Integer, Real+Real→Real, mixed→Real. A Null/absent
/// `current` is treated as zero of `delta`'s type. `delta` is assumed numeric
/// (callers validate before this point). Integer addition wraps (matching the
/// platform's i64 semantics) rather than panicking on overflow.
fn add_counter(current: &Value, delta: &Value) -> Result<Value> {
    match (current, delta) {
        // Integer accumulation.
        (Value::Null, Value::Integer(d)) => Ok(Value::Integer(*d)),
        (Value::Integer(c), Value::Integer(d)) => Ok(Value::Integer(c.wrapping_add(*d))),
        // Real accumulation.
        (Value::Null, Value::Real(d)) => Ok(Value::Real(*d)),
        (Value::Real(c), Value::Real(d)) => Ok(Value::Real(c + d)),
        // Mixed numeric → Real.
        (Value::Integer(c), Value::Real(d)) => Ok(Value::Real(*c as f64 + d)),
        (Value::Real(c), Value::Integer(d)) => Ok(Value::Real(c + *d as f64)),
        // Current is present but non-numeric → type error.
        (other, _) => Err(BoogyError::SchemaMismatch(format!(
            "upsert_increment counter is not numeric: current value {:?}",
            other.value_type()
        ))),
    }
}

/// Extract the rowid suffix from an index leaf key. Every index key —
/// single-column or composite — appends the 8-byte big-endian rowid as its
/// final bytes, so the rowid is always the last 8 bytes regardless of the
/// column type(s) the key encodes.
fn rowid_from_index_key(key: &[u8]) -> u64 {
    debug_assert!(key.len() >= 8, "index key shorter than rowid suffix");
    let n = key.len();
    u64::from_be_bytes(key[n - 8..n].try_into().unwrap())
}

/// A row returned from queries. Wraps raw bytes; decodes columns on demand.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    data: Vec<u8>,
    col_names: Arc<Vec<String>>,
    /// Column definitions (parallel to col_names) for default-at-read and
    /// dropped-column skipping. Shared Arc from the table meta snapshot.
    col_defs: Arc<Vec<crate::value::ColumnDef>>,
}

impl Row {
    fn from_raw(
        bytes: &[u8],
        col_names: Arc<Vec<String>>,
        col_defs: Arc<Vec<crate::value::ColumnDef>>,
    ) -> Result<Self> {
        let id = row::extract_id(bytes)?;
        Ok(Self { id, data: bytes.to_vec(), col_names, col_defs })
    }

    /// Get a single column value by name.
    ///
    /// - Returns `None` when the column name is unknown, or when the column is
    ///   dropped.
    /// - For columns that are physically absent from the row bytes AND that have
    ///   a stored `default` (e.g. they were added via `add_column` after this row
    ///   was written), returns `Some(default)`.
    /// - For columns that are physically absent AND have no stored default (e.g.
    ///   the column was part of the original schema but simply not set during the
    ///   insert), returns `None` — preserving the historical contract.
    pub fn get(&self, column: &str) -> Option<Value> {
        // Scan col_names for all positions matching `column`, picking the first
        // one that is NOT dropped.  This handles the case where a column was
        // dropped and re-added under the same name: the old slot is marked
        // dropped and sits at an earlier position, while the new live slot sits
        // at a later position.
        let col_id = self
            .col_names
            .iter()
            .enumerate()
            .find(|(idx, n)| {
                *n == column
                    && self
                        .col_defs
                        .get(*idx)
                        .map(|d| !d.dropped)
                        .unwrap_or(false)
            })
            .map(|(idx, _)| idx as u16)?;

        let stored = row::extract_column(&self.data, col_id).ok().flatten();
        match stored {
            Some(v) => Some(v),
            None => {
                // Apply the stored default only when one is explicitly set;
                // otherwise propagate None (historically: column was omitted).
                self.col_defs
                    .get(col_id as usize)
                    .and_then(|d| d.default.clone())
                    .map(Some)
                    .unwrap_or(None)
            }
        }
    }

    /// Decode all live (non-dropped) columns.
    ///
    /// For columns that are physically absent from the row bytes AND have a
    /// stored `default`, returns the default value. Columns absent with no stored
    /// default are omitted (matching `Row::get` semantics).
    pub fn columns(&self) -> Vec<(String, Value)> {
        let mut result = Vec::with_capacity(self.col_names.len());
        for (idx, def) in self.col_defs.iter().enumerate() {
            if def.dropped {
                continue;
            }
            let name = match self.col_names.get(idx) {
                Some(n) => n.clone(),
                None => continue,
            };
            let col_id = idx as u16;
            let stored = row::extract_column(&self.data, col_id).ok().flatten();
            let val = match stored {
                Some(v) => v,
                None => match def.default.clone() {
                    Some(d) => d,
                    None => continue, // absent + no default → omit from result
                },
            };
            result.push((name, val));
        }
        result
    }
}

/// Which ordering a [`BoogyDb::scan_batch`] cursor walks.
#[derive(Debug, Clone)]
pub enum ScanOrderKind {
    /// Primary-key (rowid / insertion) order. Walks the base table B+ tree.
    PrimaryKey,
    /// A named secondary index's order. Walks that index tree.
    Index(String),
}

/// Ordered cursor specification for [`BoogyDb::scan_batch`].
#[derive(Debug, Clone)]
pub struct ScanOrder {
    pub kind: ScanOrderKind,
    pub dir: SortDir,
}

impl ScanOrder {
    /// Walk the table in primary-key (rowid) order.
    pub fn primary_key(dir: SortDir) -> Self {
        Self { kind: ScanOrderKind::PrimaryKey, dir }
    }
    /// Walk the table in the order of the named secondary index.
    pub fn index(name: &str, dir: SortDir) -> Self {
        Self { kind: ScanOrderKind::Index(name.into()), dir }
    }
}

/// Opaque resume token for [`BoogyDb::scan_batch`]: identifies the last row a
/// batch returned so the next batch resumes strictly after it.
///
/// - PrimaryKey order: `bytes` is empty, `rowid` is the last rowid returned.
/// - Index order: `bytes` is the last index key (value-tuple + rowid suffix),
///   `rowid` is that key's rowid. Both fields together form the exclusive bound
///   — the index key alone is exclusive (composite keys embed the rowid, so the
///   key uniquely identifies the row).
///
/// The token is **exclusive**: the row it identifies was already returned and
/// will not reappear in the next batch.
#[derive(Debug, Clone)]
pub struct ScanKey {
    pub(crate) bytes: Vec<u8>,
    pub(crate) rowid: u64,
}

/// One page of a [`BoogyDb::scan_batch`] cursor walk.
pub struct ScanBatch {
    /// The rows in this batch, in scan order.
    pub rows: Vec<Row>,
    /// The resume token to pass as `after` for the next batch, or `None` when
    /// the scan is exhausted (this batch returned fewer than `limit` rows / no
    /// more rows remain).
    pub last_key: Option<ScanKey>,
}

/// Durability level for write operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Durability {
    /// Fsync WAL on every commit. Survives power loss.
    Immediate = 0,
    /// No fsync. Survives process crash (OS cache), not power loss.
    Normal = 1,
    /// No WAL writes at all. Fastest. Data may be lost on any crash.
    None = 2,
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
    acid: std::sync::atomic::AtomicBool,
    #[allow(dead_code)]
    path: PathBuf,
    table_ciphers: RwLock<HashMap<u32, Arc<crate::crypto::Cipher>>>,
    max_row_size: std::sync::atomic::AtomicU32,
    #[cfg(feature = "vector")]
    vector_collections: RwLock<HashMap<(String, String), Arc<RwLock<VectorCollection>>>>,
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

/// System page magic. Written by all serialization; corruption if absent.
/// Layout: magic(4) + next_table_id(4) + num_tables(2) + per-table records.
/// Per-column records carry: [name_len:u16][name][type_tag:1][nullable:1][unique:1]
///   [dropped:1][has_default:1][ if has_default: [value_len:u16][encoded Value] ]
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

/// Inline budget for the system page: payload (4076) minus the 10-byte
/// overflow trailer slot. Unconditionally reserved even for inline-only
/// writes so the read path is single-case.
///
/// = PAGE_SIZE - PAGE_HEADER_SIZE - 4 (checksum) - OVERFLOW_TRAILER_SIZE
/// = 4096 - 16 - 4 - 10 = 4066
///
/// **Invariant:** must accommodate the largest single TableMeta record's
/// trailing `encrypted` flag byte. Shrinking this value below what the
/// last table's encrypted flag would need would silently default that
/// flag to `false` on inline-only reads. Currently safe because a single
/// TableMeta never exceeds a few hundred bytes; flagged so a future
/// trailer-extension doesn't accidentally cut into the last-flag slot.
const SYSTEM_INLINE_BUDGET: usize = PAGE_SIZE
    - PAGE_HEADER_SIZE
    - 4 /* checksum */
    - crate::overflow::OVERFLOW_TRAILER_SIZE;

/// Serialize the table registry into a contiguous byte stream. Pure data —
/// no page in mind, no bounds check, no failure mode. The page-layer
/// `serialize_system_page` decides inline-or-chain based on the result's
/// length.
///
/// Format (stable across the inline/overflow boundary — the chain just
/// continues the byte stream from where the inline payload ended):
///   [magic:4][next_table_id:4][num_tables:2][per-table records...]
fn serialize_system_metas(metas: &[TableMeta], next_table_id: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(SYSTEM_INLINE_BUDGET);

    buf.extend_from_slice(&SYSTEM_PAGE_MAGIC.to_le_bytes());
    buf.extend_from_slice(&next_table_id.to_le_bytes());
    buf.extend_from_slice(&(metas.len() as u16).to_le_bytes());

    for meta in metas {
        buf.extend_from_slice(&meta.table_id.to_le_bytes());
        buf.extend_from_slice(&meta.root_page.to_le_bytes());
        buf.extend_from_slice(&meta.row_count.to_le_bytes());
        buf.extend_from_slice(&meta.next_rowid.to_le_bytes());

        let name_bytes = meta.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);

        buf.extend_from_slice(&(meta.columns.len() as u16).to_le_bytes());
        for col in &meta.columns {
            let col_name = col.name.as_bytes();
            buf.extend_from_slice(&(col_name.len() as u16).to_le_bytes());
            buf.extend_from_slice(col_name);
            buf.push(type_to_tag(col.col_type));
            buf.push(if col.nullable { 1 } else { 0 });
            buf.push(if col.unique { 1 } else { 0 });
            buf.push(if col.dropped { 1 } else { 0 });
            match &col.default {
                None => buf.push(0), // has_default = false
                Some(v) => {
                    buf.push(1); // has_default = true
                    let vb = crate::row::encode_value_to_vec(v);
                    buf.extend_from_slice(&(vb.len() as u16).to_le_bytes());
                    buf.extend_from_slice(&vb);
                }
            }
        }

        buf.extend_from_slice(&(meta.indexes.len() as u16).to_le_bytes());
        for idx in &meta.indexes {
            let idx_name = idx.name.as_bytes();
            buf.extend_from_slice(&(idx_name.len() as u16).to_le_bytes());
            buf.extend_from_slice(idx_name);

            buf.extend_from_slice(&(idx.columns.len() as u16).to_le_bytes());
            for col in &idx.columns {
                let col_bytes = col.as_bytes();
                buf.extend_from_slice(&(col_bytes.len() as u16).to_le_bytes());
                buf.extend_from_slice(col_bytes);
            }

            buf.push(if idx.unique { 1 } else { 0 });
            buf.extend_from_slice(&idx.root_page.to_le_bytes());
        }

        buf.push(if meta.encrypted { 1 } else { 0 });
    }

    buf
}

/// Serialize metas into page 0 + (if needed) a chain of overflow pages.
/// Takes an allocator callback for assigning page numbers to overflow pages.
/// Returns (page0, vec of (page_no, overflow_page) in chain order).
///
/// Caller (in `persist_registry`) writes page0 first via put_page(0, _),
/// then each (page_no, overflow_page) via put_page(page_no, _). Old
/// overflow pages from a prior larger schema become orphaned — the
/// engine has no free-list, consistent with how drop_table orphans
/// data pages today.
fn serialize_system_page(
    metas: &[TableMeta],
    next_table_id: u32,
    allocator: &mut impl FnMut() -> Result<u32>,
) -> Result<(Page, Vec<(u32, Page)>)> {
    let bytes = serialize_system_metas(metas, next_table_id);

    let mut page0 = Page::new_system();

    if bytes.len() <= SYSTEM_INLINE_BUDGET {
        // Inline-only path: copy bytes into page 0 after the header. No trailer.
        let start = PAGE_HEADER_SIZE;
        page0.data[start..start + bytes.len()].copy_from_slice(&bytes);
        page0.update_checksum();
        return Ok((page0, vec![]));
    }

    // Chained path: copy the first SYSTEM_INLINE_BUDGET bytes inline + write
    // the trailer at offset PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET. Then
    // build overflow pages for the remainder.
    let inline_bytes = &bytes[..SYSTEM_INLINE_BUDGET];
    let overflow_bytes = &bytes[SYSTEM_INLINE_BUDGET..];

    // Allocate overflow pages first so we know first_page for the trailer.
    let mut overflow_pages: Vec<(u32, Page)> = Vec::new();
    let chunks: Vec<&[u8]> = overflow_bytes
        .chunks(crate::overflow::OVERFLOW_PAYLOAD_MAX)
        .collect();
    let mut page_numbers: Vec<u32> = Vec::with_capacity(chunks.len());
    for _ in 0..chunks.len() {
        page_numbers.push(allocator()?);
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let next_page = if i + 1 < page_numbers.len() {
            page_numbers[i + 1]
        } else {
            0 // end-of-chain sentinel
        };
        overflow_pages.push((
            page_numbers[i],
            crate::overflow::build_overflow_page(chunk, next_page),
        ));
    }

    // Page 0: inline bytes + trailer at offset PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET.
    let inline_start = PAGE_HEADER_SIZE;
    page0.data[inline_start..inline_start + SYSTEM_INLINE_BUDGET]
        .copy_from_slice(inline_bytes);

    let trailer_start = inline_start + SYSTEM_INLINE_BUDGET;
    page0.data[trailer_start..trailer_start + 2]
        .copy_from_slice(&crate::overflow::OVERFLOW_MAGIC);
    page0.data[trailer_start + 2..trailer_start + 6]
        .copy_from_slice(&page_numbers[0].to_le_bytes());
    // Guard against silent u32 truncation if a schema were absurdly large.
    // Practically impossible (no schema is 4GB) but a load-bearing serializer
    // shouldn't have a silent lossy cast.
    debug_assert!(
        overflow_bytes.len() <= u32::MAX as usize,
        "system page overflow exceeds u32 ({})",
        overflow_bytes.len(),
    );
    page0.data[trailer_start + 6..trailer_start + 10]
        .copy_from_slice(&(overflow_bytes.len() as u32).to_le_bytes());

    page0.update_checksum();
    Ok((page0, overflow_pages))
}

/// Deserialize the table registry from a contiguous byte stream. Pure data —
/// no page in mind. The page-layer `deserialize_system_page` reassembles
/// the byte stream from page 0 + optional overflow chain before calling here.
fn deserialize_system_metas(bytes: &[u8]) -> Result<(Vec<TableMeta>, u32)> {
    let mut offset = 0usize;

    // Magic
    if bytes.len() < 4 {
        return Err(BoogyError::Corruption("system page: truncated (no magic)".into()));
    }
    let magic = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    offset += 4;
    if magic != SYSTEM_PAGE_MAGIC {
        return Err(BoogyError::Corruption(format!(
            "system page magic mismatch: 0x{magic:08X} != 0x{:08X}",
            SYSTEM_PAGE_MAGIC
        )));
    }

    // next_table_id
    if bytes.len() < offset + 4 {
        return Err(BoogyError::Corruption("system page: truncated at next_table_id".into()));
    }
    let next_table_id = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    offset += 4;

    // num_tables
    if bytes.len() < offset + 2 {
        return Err(BoogyError::Corruption("system page: truncated at num_tables".into()));
    }
    let num_tables = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    let mut tables = Vec::with_capacity(num_tables);

    for _ in 0..num_tables {
        if bytes.len() < offset + 24 {
            return Err(BoogyError::Corruption("system page truncated in table header".into()));
        }
        let table_id = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        offset += 4;

        let root_page = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        offset += 4;

        let row_count = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        offset += 8;

        let next_rowid = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        offset += 8;

        if bytes.len() < offset + 2 {
            return Err(BoogyError::Corruption("system page truncated at table name length".into()));
        }
        let name_len = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        if bytes.len() < offset + name_len {
            return Err(BoogyError::Corruption("system page truncated at table name".into()));
        }
        let name = String::from_utf8(bytes[offset..offset + name_len].to_vec())
            .map_err(|_| BoogyError::Corruption("invalid utf8 in table name".into()))?;
        offset += name_len;

        if bytes.len() < offset + 2 {
            return Err(BoogyError::Corruption("system page truncated at column count".into()));
        }
        let num_columns =
            u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;

        let mut columns = Vec::with_capacity(num_columns);
        for _ in 0..num_columns {
            if bytes.len() < offset + 2 {
                return Err(BoogyError::Corruption("system page truncated at column name length".into()));
            }
            let col_name_len =
                u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            if bytes.len() < offset + col_name_len + 3 {
                return Err(BoogyError::Corruption("system page truncated at column data".into()));
            }
            let col_name = String::from_utf8(bytes[offset..offset + col_name_len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8 in column name".into()))?;
            offset += col_name_len;
            let type_tag = bytes[offset];
            offset += 1;
            let nullable = bytes[offset] != 0;
            offset += 1;
            let unique = bytes[offset] != 0;
            offset += 1;

            // dropped flag + optional default value
            if bytes.len() < offset + 1 {
                return Err(BoogyError::Corruption("system page truncated at column dropped flag".into()));
            }
            let dropped = bytes[offset] != 0;
            offset += 1;
            if bytes.len() < offset + 1 {
                return Err(BoogyError::Corruption("system page truncated at column has_default flag".into()));
            }
            let has_default = bytes[offset] != 0;
            offset += 1;
            let col_default = if has_default {
                if bytes.len() < offset + 2 {
                    return Err(BoogyError::Corruption("system page truncated at column default length".into()));
                }
                let vlen = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
                offset += 2;
                if bytes.len() < offset + vlen {
                    return Err(BoogyError::Corruption("system page truncated at column default value".into()));
                }
                let (v, _) = crate::row::decode_value(&bytes[offset..offset + vlen])?;
                offset += vlen;
                Some(v)
            } else {
                None
            };

            let mut col_def = ColumnDef::new(col_name, tag_to_type(type_tag)?);
            if !nullable {
                col_def = col_def.not_null();
            }
            if unique {
                col_def = col_def.unique();
            }
            col_def.dropped = dropped;
            col_def.default = col_default;
            columns.push(col_def);
        }

        let mut meta = TableMeta::new(name, table_id, columns, root_page);
        meta.row_count = row_count;
        meta.next_rowid = next_rowid;

        // Indexes
        if bytes.len() < offset + 2 {
            return Err(BoogyError::Corruption("system page truncated at index count".into()));
        }
        let num_indexes = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;

        for _ in 0..num_indexes {
            if bytes.len() < offset + 2 {
                return Err(BoogyError::Corruption("system page truncated at index name length".into()));
            }
            let idx_name_len =
                u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            if bytes.len() < offset + idx_name_len {
                return Err(BoogyError::Corruption("system page truncated at index name".into()));
            }
            let idx_name = String::from_utf8(bytes[offset..offset + idx_name_len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8 in index name".into()))?;
            offset += idx_name_len;

            // columns: count (u16) + each (len-prefixed name)
            if bytes.len() < offset + 2 {
                return Err(BoogyError::Corruption("system page truncated at index column count".into()));
            }
            let num_idx_cols =
                u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            let mut idx_columns = Vec::with_capacity(num_idx_cols);
            for _ in 0..num_idx_cols {
                if bytes.len() < offset + 2 {
                    return Err(BoogyError::Corruption("system page truncated at index column length".into()));
                }
                let idx_col_len =
                    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
                offset += 2;
                if bytes.len() < offset + idx_col_len {
                    return Err(BoogyError::Corruption("system page truncated at index column".into()));
                }
                let idx_col = String::from_utf8(bytes[offset..offset + idx_col_len].to_vec())
                    .map_err(|_| BoogyError::Corruption("invalid utf8 in index column".into()))?;
                offset += idx_col_len;
                idx_columns.push(idx_col);
            }

            // unique byte
            if bytes.len() < offset + 1 {
                return Err(BoogyError::Corruption("system page truncated at index unique flag".into()));
            }
            let idx_unique = bytes[offset] != 0;
            offset += 1;

            if bytes.len() < offset + 4 {
                return Err(BoogyError::Corruption("system page truncated at index root page".into()));
            }
            let idx_root_page = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            offset += 4;

            meta.indexes.push(IndexMeta {
                name: idx_name,
                columns: idx_columns,
                unique: idx_unique,
                root_page: idx_root_page,
            });
        }

        // encrypted flag
        let encrypted = if offset < bytes.len() {
            let e = bytes[offset] != 0;
            offset += 1;
            e
        } else {
            false
        };
        meta.encrypted = encrypted;

        tables.push(meta);
    }

    Ok((tables, next_table_id))
}

/// Deserialize page 0 (+ any overflow chain) into the table registry.
fn deserialize_system_page(
    page0: &Page,
    file: &PageFile,
) -> Result<(Vec<TableMeta>, u32)> {
    // Inline bytes start right after the page header.
    let inline_start = PAGE_HEADER_SIZE;
    let trailer_start = inline_start + SYSTEM_INLINE_BUDGET;
    let trailer_end = trailer_start + crate::overflow::OVERFLOW_TRAILER_SIZE;

    // Check for overflow trailer at the fixed offset.
    let has_trailer = page0.data[trailer_start..trailer_start + 2]
        == crate::overflow::OVERFLOW_MAGIC;

    if !has_trailer {
        // Inline-only: the bytes the metas were serialized into start at
        // PAGE_HEADER_SIZE and end at PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET.
        // The pure-data deserializer reads until num_tables tells it to stop;
        // any trailing zero bytes are ignored by that bounds-driven parse.
        return deserialize_system_metas(&page0.data[inline_start..trailer_start]);
    }

    // Chained: decode the trailer + walk the chain.
    let first_page = u32::from_le_bytes(
        page0.data[trailer_start + 2..trailer_start + 6].try_into().unwrap(),
    );
    let remaining_len = u32::from_le_bytes(
        page0.data[trailer_start + 6..trailer_end].try_into().unwrap(),
    ) as usize;

    // Defensive: a present trailer with first_page == 0 and remaining_len > 0
    // is a corrupt trailer (clearer diagnostic than "chain truncated: 0 of N").
    if first_page == 0 && remaining_len > 0 {
        return Err(BoogyError::Corruption(format!(
            "system page trailer present but first_page is 0 (remaining_len={remaining_len})"
        )));
    }

    let mut bytes = Vec::with_capacity(SYSTEM_INLINE_BUDGET + remaining_len);
    bytes.extend_from_slice(&page0.data[inline_start..trailer_start]);

    let mut current = first_page;
    let mut accumulated = 0usize;
    while current != 0 && accumulated < remaining_len {
        let page = file.read_page(current)?;
        // Defensive: in a corrupt or future-free-list scenario, a chain link
        // could point at a non-overflow page (e.g. a btree node). Validate the
        // page type before trusting its overflow-payload-len byte.
        if !page.is_overflow() {
            return Err(BoogyError::Corruption(format!(
                "system page chain hit non-overflow page {current} (flags=0x{:04X})",
                page.flags()
            )));
        }
        let payload = crate::overflow::read_overflow_payload(&page);
        let take = (remaining_len - accumulated).min(payload.len());
        bytes.extend_from_slice(&payload[..take]);
        accumulated += take;
        current = page.overflow_next();
    }

    if accumulated < remaining_len {
        return Err(BoogyError::Corruption(format!(
            "system page overflow chain truncated: accumulated {} of {} bytes",
            accumulated, remaining_len
        )));
    }

    deserialize_system_metas(&bytes)
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

        // Step 1: Crash recovery -- replay WAL forward (redo).
        {
            let mut wal = Wal::open(&wal_path)?;
            if wal.entry_count() > 0 {
                let file = PageFile::open(&path)?;
                let entries = wal.read_entries()?;
                // Redo: apply after-images in forward order.
                for entry in &entries {
                    let page = Page::from_bytes_unchecked(entry.page_data);
                    file.put_page_direct(entry.page_no, page)?;
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
                let (metas, next_id) = deserialize_system_page(&sys_page, &file)?;
                next_table_id = next_id;
                for meta in metas {
                    let name = meta.name.clone();
                    let state = Arc::new(RwLock::new(TableState { meta }));
                    tables.insert(name, state);
                }
            }
        }

        let db = Self {
            file,
            wal: Mutex::new(wal),
            tables: RwLock::new(tables),
            next_table_id: Mutex::new(next_table_id),
            durability: std::sync::atomic::AtomicU8::new(Durability::Normal as u8),
            acid: std::sync::atomic::AtomicBool::new(false),
            path,
            table_ciphers: RwLock::new(HashMap::new()),
            max_row_size: std::sync::atomic::AtomicU32::new(10 * 1024 * 1024),
            #[cfg(feature = "vector")]
            vector_collections: RwLock::new(HashMap::new()),
        };

        // Reopen any persisted vector collections.
        #[cfg(feature = "vector")]
        db.reopen_vector_collections()?;

        Ok(db)
    }

    /// Set the durability level for writes.
    pub fn set_durability(&self, d: Durability) {
        self.durability.store(d as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get current durability level.
    pub fn durability(&self) -> Durability {
        match self.durability.load(std::sync::atomic::Ordering::Relaxed) {
            0 => Durability::Immediate,
            1 => Durability::Normal,
            _ => Durability::None,
        }
    }

    /// Enable or disable ACID transaction mode. When enabled, standalone
    /// write operations and `begin()` use the AcidTransaction path.
    pub fn set_acid(&self, enabled: bool) {
        self.acid.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Check whether ACID transaction mode is enabled.
    pub fn is_acid(&self) -> bool {
        self.acid.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the maximum encoded row size in bytes. Rows larger than this will
    /// be rejected with `BoogyError::RowTooLarge`. Default is 10 MiB.
    pub fn set_max_row_size(&self, bytes: u32) {
        self.max_row_size.store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get the current maximum row size in bytes.
    pub fn max_row_size(&self) -> u32 {
        self.max_row_size.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check that an encrypted table has been unlocked before any operation.
    fn check_table_accessible(meta: &TableMeta, table: &str) -> Result<()> {
        if meta.encrypted && meta.cipher.is_none() {
            return Err(BoogyError::TableLocked(table.to_string()));
        }
        Ok(())
    }

    /// Commit a WriteGuard, writing after-images to the WAL as appropriate
    /// for the durability level. This is the single commit path used by all
    /// write operations.
    fn commit_write(
        guard: WriteGuard,
        file: &PageFile,
        wal: &Mutex<Wal>,
        durability: Durability,
        table_id: u32,
        cipher: Option<&crate::crypto::Cipher>,
    ) -> Result<()> {
        let after_images = guard.commit()?;

        // Register page ciphers for encrypted tables so sync_all encrypts them.
        if let Some(c) = cipher {
            let arc = Arc::new(c.clone());
            for (page_no, _) in &after_images {
                file.register_page_cipher(*page_no, Arc::clone(&arc));
            }
        }

        match durability {
            Durability::Immediate => {
                let mut wal = wal.lock().unwrap();
                for (page_no, data) in &after_images {
                    let write_data = if let Some(c) = cipher {
                        c.encrypt_page(&data[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE])?
                    } else {
                        *data
                    };
                    wal.append_page_image(table_id, *page_no, &write_data)?;
                }
                wal.sync()?;
            }
            Durability::Normal => {
                let mut wal = wal.lock().unwrap();
                for (page_no, data) in &after_images {
                    let write_data = if let Some(c) = cipher {
                        c.encrypt_page(&data[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE])?
                    } else {
                        *data
                    };
                    wal.append_page_image(table_id, *page_no, &write_data)?;
                }
            }
            Durability::None => {
                // No WAL write
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

        let (page0, overflow_pages) =
            serialize_system_page(metas, next_table_id, &mut || guard.allocate_page())?;
        guard.put_page(0, page0);
        for (page_no, overflow_page) in overflow_pages {
            guard.put_page(page_no, overflow_page);
        }

        Self::commit_write(guard, file, wal, durability, 0, None)?;
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
            // Resolve every index column to (col_id, col_type); a composite
            // index maintains the full tuple key. Single-column indexes are the
            // degenerate one-element case (byte-identical to the old behavior).
            let mut col_ids: Vec<u16> = Vec::with_capacity(idx.columns.len());
            let mut col_types: Vec<crate::value::Type> = Vec::with_capacity(idx.columns.len());
            let mut resolved = true;
            for idx_col in &idx.columns {
                match (
                    meta.col_name_to_id.get(idx_col).copied(),
                    meta.columns.iter().find(|c| &c.name == idx_col).map(|c| c.col_type),
                ) {
                    (Some(cid), Some(ct)) => {
                        col_ids.push(cid);
                        col_types.push(ct);
                    }
                    _ => {
                        resolved = false;
                        break;
                    }
                }
            }
            if !resolved {
                continue;
            }
            let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
            for cid in &col_ids {
                col_vals.push(crate::row::extract_column(row_bytes, *cid)?.unwrap_or(Value::Null));
            }
            if let Some(key) = index::encode_composite_index_key(&col_types, &col_vals, rowid) {
                let mut tree = IndexTreeWriter::new(guard, idx.root_page);
                if remove {
                    tree.delete(&key)?;
                } else {
                    tree.insert(&key)?;
                }
                idx.root_page = tree.root_page();
            }
        }
        Ok(())
    }

    /// Enforce uniqueness for every `unique` index on `meta` against the
    /// encoded `row_bytes` of a row about to be inserted.
    ///
    /// Returns `Err(UniqueViolation)` if any unique index already holds an
    /// entry whose value tuple (all index columns, no rowid) matches this row.
    /// A Null in any index column means the tuple is not indexed (matching the
    /// single-column convention) and is therefore never a uniqueness conflict.
    ///
    /// IMPORTANT — atomicity: this reads the committed index trees via a
    /// read-only `IndexTreeReader` and performs NO mutation. Callers MUST run
    /// it under the table write lock and BEFORE opening the write guard (i.e.
    /// before any main-tree or index-tree write), so a rejected duplicate
    /// leaves the table byte-for-byte unchanged.
    fn enforce_unique_indexes(
        file: &PageFile,
        meta: &TableMeta,
        row_bytes: &[u8],
    ) -> Result<()> {
        for idx in &meta.indexes {
            if !idx.unique {
                continue;
            }
            // Resolve all index columns to (col_id, col_type).
            let mut col_ids: Vec<u16> = Vec::with_capacity(idx.columns.len());
            let mut col_types: Vec<crate::value::Type> = Vec::with_capacity(idx.columns.len());
            let mut resolved = true;
            for idx_col in &idx.columns {
                match (
                    meta.col_name_to_id.get(idx_col).copied(),
                    meta.columns.iter().find(|c| &c.name == idx_col).map(|c| c.col_type),
                ) {
                    (Some(cid), Some(ct)) => {
                        col_ids.push(cid);
                        col_types.push(ct);
                    }
                    _ => {
                        resolved = false;
                        break;
                    }
                }
            }
            if !resolved {
                continue;
            }
            let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
            for cid in &col_ids {
                col_vals.push(crate::row::extract_column(row_bytes, *cid)?.unwrap_or(Value::Null));
            }
            // A Null component is not indexed, so it cannot collide.
            if let Some(prefix) = index::encode_composite_value_prefix(&col_types, &col_vals) {
                let reader = IndexTreeReader::new(file, idx.root_page);
                if !reader.scan_prefix(&prefix)?.is_empty() {
                    return Err(BoogyError::UniqueViolation {
                        index: idx.name.clone(),
                    });
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
            // Single-column index type enforcement keys on the leading column.
            let idx_col = &idx.columns[0];
            if let Some((_, val)) = data.iter().find(|(name, _)| name == idx_col) {
                if val.is_null() {
                    continue;
                }
                let col_type = meta
                    .columns
                    .iter()
                    .find(|c| &c.name == idx_col)
                    .map(|c| c.col_type);
                if let Some(ct) = col_type {
                    if val.value_type() != Some(ct) {
                        return Err(BoogyError::TypeMismatch(format!(
                            "column '{}' expects {:?}, got {:?}",
                            idx_col,
                            ct,
                            val.value_type()
                        )));
                    }
                    if let Value::Real(f) = val {
                        if f.is_nan() {
                            return Err(BoogyError::TypeMismatch(format!(
                                "column '{}': NaN not allowed in indexed columns",
                                idx_col
                            )));
                        }
                    }
                    if let Value::Text(s) = val {
                        if s.as_bytes().contains(&0x00) {
                            return Err(BoogyError::TypeMismatch(format!(
                                "column '{}': null bytes not allowed in indexed text columns",
                                idx_col
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

            Self::commit_write(guard, &self.file, &self.wal, durability, table_id, None)?;
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
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            let result = tx.insert(table, data)?;
            tx.commit()?;
            return Ok(result);
        }
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
        Self::check_table_accessible(&state.meta, table)?;

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

        if row_bytes.len() > self.max_row_size() as usize {
            return Err(BoogyError::RowTooLarge(row_bytes.len()));
        }

        // 5b. Unique-index enforcement. Read-only check against committed index
        //     trees, under the table write lock and BEFORE any write guard is
        //     opened — a rejection here mutates nothing (no rowid is consumed
        //     either, since next_rowid is only advanced inside the guard below
        //     in insert_with_id; here we already bumped it, so undo on reject).
        if !state.meta.indexes.is_empty() {
            if let Err(e) = Self::enforce_unique_indexes(&self.file, &state.meta, &row_bytes) {
                // Roll back the rowid we tentatively consumed in step 4 so a
                // rejected duplicate leaves table state byte-for-byte unchanged.
                state.meta.next_rowid -= 1;
                return Err(e);
            }
        }

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

            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
        }

        // 7. Update table state.
        state.meta.row_count += 1;

        Ok(rowid)
    }

    /// Insert a row with a caller-supplied rowid.
    pub fn insert_with_id(&self, table: &str, rowid: u64, data: &[(&str, Value)]) -> Result<()> {
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            tx.insert_with_id(table, rowid, data)?;
            tx.commit()?;
            return Ok(());
        }
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
        Self::check_table_accessible(&state.meta, table)?;

        // 3. Type enforcement for indexed columns.
        Self::enforce_index_types(&state.meta, data)?;

        // 5. Encode row. (next_rowid is advanced only after a successful
        //    uniqueness check, so a rejected duplicate leaves it unchanged.)
        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(rowid, &col_values);

        if row_bytes.len() > self.max_row_size() as usize {
            return Err(BoogyError::RowTooLarge(row_bytes.len()));
        }

        // 5b. Unique-index enforcement: read-only check against committed index
        //     trees, under the table write lock and BEFORE any write guard or
        //     metadata mutation. A rejection here leaves table state unchanged.
        if !state.meta.indexes.is_empty() {
            Self::enforce_unique_indexes(&self.file, &state.meta, &row_bytes)?;
        }

        // 4. Advance next_rowid if necessary (only after uniqueness passes).
        if rowid >= state.meta.next_rowid {
            state.meta.next_rowid = rowid + 1;
        }

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

            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
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
        Self::check_table_accessible(&state.meta, table)?;

        // 3. Concurrent read via BTreeReader (no file mutex).
        let reader = BTreeReader::new(&self.file, state.meta.root_page);
        let result = reader.search(id)?;

        // 4. Decode.
        match result {
            Some(bytes) => {
                Ok(Some(Row::from_raw(&bytes, state.meta.col_names.clone(), state.meta.col_defs.clone())?))
            }
            None => Ok(None),
        }
    }

    /// Update a row by rowid. Replaces specified columns.
    pub fn update(&self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            let result = tx.update(table, id, fields)?;
            tx.commit()?;
            return Ok(result);
        }
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
        Self::check_table_accessible(&state.meta, table)?;

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

            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
        }

        Ok(true)
    }

    /// Delete a row by rowid.
    pub fn delete(&self, table: &str, id: u64) -> Result<bool> {
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            let result = tx.delete(table, id)?;
            tx.commit()?;
            return Ok(result);
        }
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
        Self::check_table_accessible(&state.meta, table)?;

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

            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
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
        Self::check_table_accessible(&state.meta, table)?;

        // Can we short-circuit (stop early without scanning everything)?
        // Only when: no sort (ordering requires full collection) and not requesting total.
        let can_short_circuit = opts.sort.is_empty() && !opts.include_total;

        // 3. Check for index-accelerated path (Eq or In filter on an indexed column).
        // The index/single-filter/empty-filter fast paths only know about `filters`;
        // when `or_groups` is non-empty we must fall through to the scan-all branch
        // (which applies the full predicate via `row_passes`) or those paths would
        // silently ignore the OR groups and return wrong rows.
        let index_candidate = if opts.or_groups.is_empty() {
            opts.filters.iter().find(|f| {
                (f.op == FilterOp::Eq || f.op == FilterOp::In)
                    && state.meta.find_index_for_column(&f.column).is_some()
            })
        } else {
            None
        };

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

            let idx_reader = IndexTreeReader::new(&self.file, idx_meta.root_page);

            // Collect matching rowids from index — Eq uses single prefix, In scans each value.
            let matching_rowids = if idx_filter.op == FilterOp::In {
                let values = match idx_filter.in_values {
                    Some(ref v) => v,
                    None => return Ok(FindResult { rows: Vec::new(), total: if opts.include_total { Some(0) } else { None } }),
                };
                if values.is_empty() {
                    return Ok(FindResult { rows: Vec::new(), total: if opts.include_total { Some(0) } else { None } });
                }
                let mut rowids = Vec::new();
                for val in values {
                    if let Some(prefix) = index::encode_value_prefix(col_type, val) {
                        let keys = idx_reader.scan_prefix(&prefix)?;
                        for k in &keys {
                            rowids.push(index::extract_rowid(col_type, k));
                        }
                    }
                }
                rowids.sort_unstable();
                rowids.dedup();
                rowids
            } else {
                // Eq path (original logic)
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

                let keys = if let Some(n) = need {
                    idx_reader.scan_prefix_limit(&prefix, n)?
                } else {
                    idx_reader.scan_prefix(&prefix)?
                };

                let mut rowids: Vec<u64> = keys
                    .iter()
                    .map(|k| index::extract_rowid(col_type, k))
                    .collect();
                rowids.sort_unstable();
                rowids
            };

            // Batch-fetch rows via leaf-chain walk (much faster than N individual searches)
            let btree_reader = BTreeReader::new(&self.file, state.meta.root_page);
            let raw_rows = btree_reader.multi_get_sorted(&matching_rowids)?;

            // Check if we need to apply additional filters beyond the indexed one
            let has_extra_filters = opts.filters.len() > 1;

            let col_names = state.meta.col_names.clone();
            let col_defs = state.meta.col_defs.clone();
            let mut rows = Vec::with_capacity(raw_rows.len());
            for bytes in &raw_rows {
                if has_extra_filters {
                    // or_groups is guaranteed empty here (index path is gated on it),
                    // so the index filter + extra `filters` are the entire predicate.
                    let passes = opts
                        .filters
                        .iter()
                        .all(|f| filter_matches_row(&state.meta.col_name_to_id, &col_defs, bytes, f));
                    if passes {
                        rows.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
                    }
                } else {
                    rows.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
                }
            }

            // Determine total if requested.
            let total: Option<u64> = if opts.include_total {
                Some(rows.len() as u64)
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
        } else if opts.or_groups.is_empty()
            && opts.filters.len() == 1
            && opts.filters[0].op != FilterOp::In
        {
            // Single filter (non-IN): use scan_filtered (extract_column on raw bytes, no full decode)
            // Gated on empty or_groups: scan_filtered only applies the one filter.
            //
            // IMPORTANT: scan_filtered operates on raw bytes and treats absent columns as
            // Null. If the column has a stored default (added via add_column), absent rows
            // should match against the default, not Null. In that case we must bypass
            // scan_filtered and fall through to the row_passes scan-all path below.
            let f = &opts.filters[0];
            let col_has_default = state.meta.col_id(&f.column)
                .and_then(|id| state.meta.col_defs.get(id as usize))
                .map(|d| d.default.is_some())
                .unwrap_or(false);
            if let Some(col_id) = state.meta.col_id(&f.column) {
                if col_has_default {
                    // Column has a stored default.  Original scan_filtered treated absent
                    // columns as Null, which misses old rows that should match the default.
                    // Fix: pass the stored default to scan_filtered so absent-column rows
                    // are evaluated against the correct value — keeping the fast raw-byte
                    // comparison path and its existing limit/offset short-circuit.
                    let reader = BTreeReader::new(&self.file, state.meta.root_page);
                    let stored_default = state.meta.col_defs
                        .get(col_id as usize)
                        .and_then(|d| d.default.clone())
                        .unwrap_or(Value::Null);
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
                    let (raw_rows, count) = reader.scan_filtered(col_id, f.op, &f.value, lim, off, stop, Some(&stored_default))?;
                    let col_names = state.meta.col_names.clone();
                    let col_defs = state.meta.col_defs.clone();
                    let matching: Vec<Row> = raw_rows.iter()
                        .map(|(_, bytes)| Row::from_raw(bytes, col_names.clone(), col_defs.clone()).unwrap())
                        .collect();
                    let total = if opts.include_total { Some(count) } else { None };
                    // scan_filtered already handled limit/offset when sort is empty.
                    if opts.sort.is_empty() {
                        pagination_applied = true;
                    }
                    (matching, total)
                } else {
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
                    let (raw_rows, count) = reader.scan_filtered(col_id, f.op, &f.value, lim, off, stop, None)?;
                    let col_names = state.meta.col_names.clone();
                    let col_defs = state.meta.col_defs.clone();
                    let matching: Vec<Row> = raw_rows.iter()
                        .map(|(_, bytes)| Row::from_raw(bytes, col_names.clone(), col_defs.clone()).unwrap())
                        .collect();
                    let total = if opts.include_total { Some(count) } else { None };
                    // scan_filtered already handled limit/offset when sort is empty.
                    if opts.sort.is_empty() {
                        pagination_applied = true;
                    }
                    (matching, total)
                }
            } else {
                // Column not found -- no matches
                let total = if opts.include_total { Some(0) } else { None };
                (Vec::new(), total)
            }
        } else if opts.filters.is_empty() && opts.or_groups.is_empty() {
            // No predicate at all: full scan but skip decode, just collect raw.
            // Gated on empty or_groups: an OR-only query (empty filters, non-empty
            // or_groups) must fall through to the scan + row_passes branch.
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            let all = reader.scan_all()?;
            let total = if opts.include_total { Some(all.len() as u64) } else { None };
            let col_names = state.meta.col_names.clone();
            let col_defs = state.meta.col_defs.clone();
            let matching: Vec<Row> = all.iter()
                .map(|(_, bytes)| Row::from_raw(bytes, col_names.clone(), col_defs.clone()).unwrap())
                .collect();
            (matching, total)
        } else {
            // Multi-filter, single-IN filter, or any query with or_groups present:
            // scan all, apply the full predicate (filters AND or-of-groups), lazy Row.
            // Uses scan_all_matching so the predicate + limit short-circuit is fused into
            // the page-walk, avoiding loading the entire table when a limit is given.
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            let col_name_to_id = state.meta.col_name_to_id.clone();
            let col_names = state.meta.col_names.clone();
            let col_defs = state.meta.col_defs.clone();
            let stop_after: Option<usize> = if can_short_circuit {
                opts.limit.map(|l| opts.offset.unwrap_or(0) as usize + l as usize)
            } else {
                None
            };
            let filters = opts.filters.clone();
            let or_groups = opts.or_groups.clone();
            let col_defs2 = col_defs.clone();
            let raw_rows = reader.scan_all_matching(
                move |bytes| row_passes(&col_name_to_id, &col_defs2, bytes, &filters, &or_groups),
                stop_after,
            )?;
            let mut matching = Vec::with_capacity(raw_rows.len());
            for (_, bytes) in &raw_rows {
                matching.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
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

    /// Ordered range-scan-from-a-key: the cursor primitive for paging through a
    /// table in bounded memory.
    ///
    /// Returns up to `limit` rows in `order`, starting strictly after the row
    /// identified by `after` (the resume token is **exclusive** — the row it
    /// names was already returned and does not reappear). `after = None` starts
    /// from the beginning of the ordering. The full predicate
    /// `ALL(filters) AND (or_groups.is_empty() OR ANY(group: ALL(group)))` is
    /// applied per row (same semantics as [`find`]).
    ///
    /// The returned [`ScanBatch::last_key`] is the exclusive resume token for the
    /// next call, or `None` when the scan is exhausted. Threading
    /// `after = previous.last_key` visits every matching row exactly once, in
    /// order, with no gaps or duplicates, and terminates.
    ///
    /// `order`:
    /// - [`ScanOrderKind::PrimaryKey`] walks the base table B+ tree by rowid
    ///   (both directions; leaves are doubly-linked). `last_key.bytes` is empty;
    ///   `last_key.rowid` is the bound.
    /// - [`ScanOrderKind::Index`] walks the named index tree by key order (both
    ///   directions). `last_key.bytes` is the last index key; an unknown index
    ///   name is an error.
    pub fn scan_batch(
        &self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
        order: ScanOrder,
        after: Option<ScanKey>,
        limit: u32,
    ) -> Result<ScanBatch> {
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
        Self::check_table_accessible(&state.meta, table)?;

        let col_names = state.meta.col_names.clone();
        let col_defs = state.meta.col_defs.clone();
        let col_name_to_id = &state.meta.col_name_to_id;

        match order.kind {
            ScanOrderKind::PrimaryKey => {
                let after_rowid = after.as_ref().map(|k| k.rowid);
                let reader = BTreeReader::new(&self.file, state.meta.root_page);
                let (raw, last_rowid) = reader.scan_from(after_rowid, order.dir, limit, |bytes| {
                    row_passes(col_name_to_id, &col_defs, bytes, filters, or_groups)
                })?;
                let mut rows = Vec::with_capacity(raw.len());
                for (_, bytes) in &raw {
                    rows.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
                }
                let last_key = last_rowid.map(|rowid| ScanKey { bytes: Vec::new(), rowid });
                Ok(ScanBatch { rows, last_key })
            }
            ScanOrderKind::Index(ref idx_name) => {
                let idx_meta = state
                    .meta
                    .find_index(idx_name)
                    .ok_or_else(|| {
                        BoogyError::SchemaMismatch(format!(
                            "scan_batch: no index named '{idx_name}' on table '{table}'"
                        ))
                    })?
                    .clone();

                let after_bytes = after.as_ref().map(|k| k.bytes.as_slice());
                let idx_reader = IndexTreeReader::new(&self.file, idx_meta.root_page);
                let (keys, _more) = idx_reader.scan_from_key(after_bytes, order.dir, limit as usize)?;

                // Fetch each row by rowid, apply the predicate, preserve index order.
                let btree_reader = BTreeReader::new(&self.file, state.meta.root_page);
                let mut rows = Vec::with_capacity(keys.len());
                let mut last_key: Option<ScanKey> = None;
                for key in &keys {
                    let rowid = rowid_from_index_key(key);
                    // Advance the cursor for EVERY key the index returned, even
                    // when the row fails the predicate — the bound must stay
                    // exclusive regardless of filter outcome.
                    last_key = Some(ScanKey { bytes: key.clone(), rowid });
                    if let Some(bytes) = btree_reader.search(rowid)? {
                        if row_passes(col_name_to_id, &col_defs, &bytes, filters, or_groups) {
                            rows.push(Row::from_raw(&bytes, col_names.clone(), col_defs.clone())?);
                        }
                    }
                }
                // If the index returned fewer than `limit` keys, the scan is
                // exhausted: no more pages to walk, so drop the resume token.
                if keys.len() < limit as usize {
                    last_key = None;
                }
                Ok(ScanBatch { rows, last_key })
            }
        }
    }

    /// Count rows matching filters (implicit AND).
    ///
    /// Back-compatible signature. For OR-of-AND counting use [`count_with`].
    pub fn count(&self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.count_with(table, filters, &[])
    }

    /// Count rows matching the full predicate
    /// `ALL(filters) AND (or_groups.is_empty() OR ANY(group: ALL(group)))`.
    ///
    /// With empty `or_groups` this is identical to [`count`] and keeps all the
    /// cached-count / index / single-filter fast paths. When `or_groups` is
    /// non-empty those fast paths only know about `filters`, so we fall through
    /// to a full scan that applies the complete predicate via `row_passes`.
    pub fn count_with(
        &self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
    ) -> Result<u64> {
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
        Self::check_table_accessible(&state.meta, table)?;

        if or_groups.is_empty() {
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

            // Single filter (non-IN): use count_filtered (extract_column on raw bytes)
            // IMPORTANT: count_filtered treats absent columns as Null; bypass it when
            // the column has a stored default so absent rows are matched correctly.
            if filters.len() == 1 && filters[0].op != FilterOp::In {
                let f = &filters[0];
                let col_has_default = state.meta.col_id(&f.column)
                    .and_then(|id| state.meta.col_defs.get(id as usize))
                    .map(|d| d.default.is_some())
                    .unwrap_or(false);
                if !col_has_default {
                    if let Some(col_id) = state.meta.col_id(&f.column) {
                        let reader = BTreeReader::new(&self.file, state.meta.root_page);
                        return reader.count_filtered(col_id, f.op, &f.value);
                    }
                    return Ok(0);
                }
                // col_has_default: fall through to row_passes scan-all below.
            }
        }

        // Multi-filter, IN filter, or any query with or_groups present:
        // scan all and apply the full predicate.
        let reader = BTreeReader::new(&self.file, state.meta.root_page);
        let all = reader.scan_all()?;
        let col_defs = state.meta.col_defs.clone();

        let mut count = 0u64;
        for (_, bytes) in &all {
            if row_passes(&state.meta.col_name_to_id, &col_defs, bytes, filters, or_groups) {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Create a secondary index on a table column.
    pub fn create_index(&self, table: &str, index_name: &str, column: &str) -> Result<()> {
        self.create_index_ex(table, index_name, &[column], false)
    }

    /// Create a (possibly multi-column, possibly unique) secondary index and
    /// populate it from existing rows.
    ///
    /// The index tree is keyed on `encode_composite_index_key` over `columns`
    /// (in the order given) plus the rowid suffix. When `unique`, the build
    /// fails with [`BoogyError::UniqueViolation`] if two existing rows already
    /// share the same value tuple — a unique index cannot be built over data
    /// that already contains duplicates.
    pub fn create_index_ex(
        &self,
        table: &str,
        index_name: &str,
        columns: &[&str],
        unique: bool,
    ) -> Result<()> {
        if columns.is_empty() {
            return Err(BoogyError::SchemaMismatch(
                "create_index_ex requires at least one column".to_string(),
            ));
        }

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
        Self::check_table_accessible(&state.meta, table)?;

        // Check index doesn't already exist.
        if state.meta.find_index(index_name).is_some() {
            return Err(BoogyError::IndexExists(index_name.to_string()));
        }

        // Resolve ALL columns to (col_id, col_type), validating existence.
        let mut col_ids: Vec<u16> = Vec::with_capacity(columns.len());
        let mut col_types: Vec<crate::value::Type> = Vec::with_capacity(columns.len());
        for column in columns {
            let col_id = state.meta.col_id(column).ok_or_else(|| {
                BoogyError::SchemaMismatch(format!(
                    "column '{column}' not found in table '{table}'"
                ))
            })?;
            let col_type = state
                .meta
                .columns
                .iter()
                .find(|c| &c.name == column)
                .map(|c| c.col_type)
                .unwrap();
            col_ids.push(col_id);
            col_types.push(col_type);
        }

        // 3. Read all existing rows first (concurrent read, no write guard).
        let all = {
            let reader = BTreeReader::new(&self.file, state.meta.root_page);
            reader.scan_all()?
        };

        // 4. Create the index B+ tree and populate via WriteGuard.
        //    For a unique index, detect existing-data duplicates via the set of
        //    value-prefixes seen so far (a unique index cannot be built over
        //    data that already contains a duplicate tuple).
        let mut seen_prefixes: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let idx_root = {
            let durability = self.durability();
            let mut guard = self.file.begin_write();
            let idx_root = IndexTreeWriter::create(&mut guard)?;

            let mut current_root = idx_root;
            for (rowid, bytes) in &all {
                let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
                for cid in &col_ids {
                    col_vals.push(row::extract_column(bytes, *cid)?.unwrap_or(Value::Null));
                }
                if unique {
                    if let Some(prefix) =
                        index::encode_composite_value_prefix(&col_types, &col_vals)
                    {
                        if !seen_prefixes.insert(prefix) {
                            return Err(BoogyError::UniqueViolation {
                                index: index_name.to_string(),
                            });
                        }
                    }
                }
                if let Some(key) =
                    index::encode_composite_index_key(&col_types, &col_vals, *rowid)
                {
                    let mut tree = IndexTreeWriter::new(&mut guard, current_root);
                    current_root = tree.insert(&key)?;
                }
            }

            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
            current_root
        };

        // 5. Register the index in table metadata.
        state.meta.indexes.push(IndexMeta {
            name: index_name.to_string(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            unique,
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
        Self::check_table_accessible(&state.meta, table)?;

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

    /// Add a new column to an existing table (metadata-only, no row rewrite).
    ///
    /// The column is appended with the next available `col_id`. Rows that predate
    /// the column (or new inserts that omit it) will read the column's stored
    /// `default` (or `Null` if none was given) via the default-at-read path.
    ///
    /// Validation:
    /// - The column name must not collide with any live column.
    /// - A NOT-NULL column without a default cannot be added to a non-empty table
    ///   (there is no way to backfill existing rows).
    pub fn add_column(&self, table: &str, column: ColumnDef) -> Result<()> {
        // Validate the column name as an identifier (no NUL bytes, non-empty).
        if column.name.is_empty() {
            return Err(BoogyError::SchemaMismatch(
                "column name must not be empty".to_string(),
            ));
        }
        if column.name.contains('\0') {
            return Err(BoogyError::SchemaMismatch(
                "column name must not contain null bytes".to_string(),
            ));
        }

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
        Self::check_table_accessible(&state.meta, table)?;

        // 3. Reject if a LIVE column already has this name.
        if state.meta.col_name_to_id.contains_key(&column.name) {
            return Err(BoogyError::SchemaMismatch(format!(
                "column '{}' already exists on table '{}'",
                column.name, table
            )));
        }

        // 4. Reject NOT-NULL column without a default on a non-empty table.
        if !column.nullable && column.default.is_none() && state.meta.row_count > 0 {
            return Err(BoogyError::SchemaMismatch(format!(
                "cannot add NOT-NULL column '{}' without a default to non-empty table '{}' ({} rows)",
                column.name, table, state.meta.row_count
            )));
        }

        // 5. Append the column to the table metadata.
        state.meta.add_column(column);

        // 6. Persist registry atomically (mirrors create_index / drop_index).
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

        Ok(())
    }

    /// Rename a column in `table` from `old` to `new`, keeping the column's
    /// `col_id` intact so all existing row data is preserved under the new name.
    ///
    /// Any secondary indexes that recorded `old` as a column name are updated
    /// in-place to record `new` instead (index keys are stored by `col_id` so
    /// the B+-tree data is unaffected — only the metadata name needs fixing).
    pub fn rename_column(&self, table: &str, old: &str, new: &str) -> Result<()> {
        // Validate the new name as an identifier (non-empty, no NUL bytes).
        if new.is_empty() {
            return Err(BoogyError::SchemaMismatch(
                "column name must not be empty".to_string(),
            ));
        }
        if new.contains('\0') {
            return Err(BoogyError::SchemaMismatch(
                "column name must not contain null bytes".to_string(),
            ));
        }

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
        Self::check_table_accessible(&state.meta, table)?;

        // 3. Reject if the old column doesn't exist as a live column.
        if !state.meta.col_name_to_id.contains_key(old) {
            return Err(BoogyError::SchemaMismatch(format!(
                "column '{}' does not exist on table '{}'",
                old, table
            )));
        }

        // 4. Reject if the new name already belongs to a live column.
        if state.meta.col_name_to_id.contains_key(new) {
            return Err(BoogyError::SchemaMismatch(format!(
                "column '{}' already exists on table '{}'",
                new, table
            )));
        }

        // 5. Rename the column in the table metadata (col_id preserved).
        state.meta.rename_column(old, new);

        // 6. Fix up any index that records `old` as a column name.
        //    Index keys are keyed by col_id in the B+-tree, so the tree data is
        //    unaffected — only the recorded column name in IndexMeta needs updating.
        for idx in state.meta.indexes.iter_mut() {
            for col_name in idx.columns.iter_mut() {
                if col_name == old {
                    *col_name = new.to_string();
                }
            }
        }

        // 7. Persist registry atomically (mirrors add_column / create_index).
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

        Ok(())
    }

    /// Drop (tombstone) a column from an existing table (metadata-only, no row rewrite).
    ///
    /// The column's slot (`col_id`) is kept with `dropped = true` so that later
    /// columns' `col_id`s are not shifted and existing row data is not corrupted.
    /// The `col_id` is never reused; subsequent `add_column` calls append a new slot.
    ///
    /// Validation:
    /// - `name` must be a live column (present in `col_name_to_id`).
    /// - The column must not be referenced by any secondary index.
    pub fn drop_column(&self, table: &str, name: &str) -> Result<()> {
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
        Self::check_table_accessible(&state.meta, table)?;

        // 3. Reject if the column is not a live column.
        if !state.meta.col_name_to_id.contains_key(name) {
            return Err(BoogyError::SchemaMismatch(format!(
                "column '{}' does not exist on table '{}' (already dropped or unknown)",
                name, table
            )));
        }

        // 4. Reject if any index references this column.
        for idx in &state.meta.indexes {
            if idx.columns.iter().any(|c| c == name) {
                return Err(BoogyError::SchemaMismatch(format!(
                    "column `{}` is indexed by `{}`; drop the index first",
                    name, idx.name
                )));
            }
        }

        // 5. Tombstone the column in the table metadata.
        state.meta.drop_column(name);

        // 6. Persist registry atomically (mirrors add_column / rename_column).
        drop(state);
        let (metas, next_id) = self.snapshot_table_metas();
        let durability = self.durability();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;

        Ok(())
    }

    /// Return the live (non-dropped) columns for `table`, in schema order.
    ///
    /// This is a read — it takes no write-gate and only holds a shared lock on
    /// the per-table `RwLock` for the duration of the clone.  Returns
    /// `TableNotFound` when the table does not exist.
    pub fn list_columns(&self, table: &str) -> Result<Vec<ColumnDef>> {
        // 1. Read-lock the registry, clone the Arc (same pattern as `get`).
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };

        // 2. Read-lock the specific table and filter to non-dropped columns.
        let state = table_state.read().unwrap();
        let cols = state
            .meta
            .columns
            .iter()
            .filter(|c| !c.dropped)
            .cloned()
            .collect();
        Ok(cols)
    }

    /// List the indexes on `table`. Reads from the live `TableMeta.indexes`
    /// snapshot via the same read-lock pattern as `list_columns`: takes no
    /// write-gate and only holds a shared lock on the per-table `RwLock`
    /// for the duration of the `IndexInfo`-Vec clone. Returns
    /// `TableNotFound` when the table does not exist.
    pub fn list_indexes(&self, table: &str) -> Result<Vec<IndexInfo>> {
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables
                .get(table)
                .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))?
                .clone()
        };
        let state = table_state.read().unwrap();
        Ok(state
            .meta
            .indexes
            .iter()
            .map(|m| IndexInfo {
                name: m.name.clone(),
                columns: m.columns.clone(),
                unique: m.unique,
            })
            .collect())
    }

    /// Lightweight per-table introspection — names + live-column +
    /// index counts. Reads `self.tables` under a shared read-lock
    /// (no write-gate), then takes a per-table shared lock just long
    /// enough to count live columns + indexes from each `TableMeta`.
    ///
    /// Returned order is sorted ascending by name — the underlying
    /// HashMap iteration is non-deterministic, and a stable order
    /// makes the contract easy to test against.
    pub fn list_tables(&self) -> Result<Vec<TableInfo>> {
        // 1. Read-lock the registry; clone the Arc list so we drop
        //    the registry lock before taking any per-table locks.
        let table_states: Vec<(String, Arc<RwLock<TableState>>)> = {
            let tables = self.tables.read().unwrap();
            tables.iter().map(|(n, ts)| (n.clone(), ts.clone())).collect()
        };

        // 2. Per-table: shared lock + count + drop.
        let mut out: Vec<TableInfo> = table_states
            .into_iter()
            .map(|(name, ts)| {
                let state = ts.read().unwrap();
                let column_count = state
                    .meta
                    .columns
                    .iter()
                    .filter(|c| !c.dropped)
                    .count() as u32;
                let index_count = state.meta.indexes.len() as u32;
                TableInfo { name, column_count, index_count }
            })
            .collect();

        // 3. Deterministic order for the contract.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Update all rows matching filters. Returns number of rows updated.
    pub fn update_where(
        &self,
        table: &str,
        filters: &[Filter],
        fields: &[(&str, Value)],
    ) -> Result<u64> {
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            let result = tx.update_where(table, filters, fields)?;
            tx.commit()?;
            return Ok(result);
        }
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
        Self::check_table_accessible(&state.meta, table)?;

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
                                crate::filter::eval_filter_raw_full(raw, f)
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
            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
        }

        Ok(count)
    }

    /// Delete all rows matching filters. Returns number of rows deleted.
    pub fn delete_where(&self, table: &str, filters: &[Filter]) -> Result<u64> {
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            let result = tx.delete_where(table, filters)?;
            tx.commit()?;
            return Ok(result);
        }
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
        Self::check_table_accessible(&state.meta, table)?;

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
                                crate::filter::eval_filter_raw_full(raw, f)
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
            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;
        }

        state.meta.row_count -= count;
        Ok(count)
    }

    /// Insert multiple rows in a single transaction. Returns list of assigned rowids.
    pub fn insert_many(&self, table: &str, rows: &[Vec<(&str, Value)>]) -> Result<Vec<u64>> {
        if self.is_acid() {
            let mut tx = AcidTransaction::new(self);
            let result = tx.insert_many(table, rows)?;
            tx.commit()?;
            return Ok(result);
        }
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
        Self::check_table_accessible(&state.meta, table)?;

        // 3. Type enforcement for all rows.
        for row_data in rows {
            Self::enforce_index_types(&state.meta, row_data)?;
        }

        // 4. Insert all rows under a single WriteGuard.
        // Track metadata changes separately so they are only applied after
        // commit_write succeeds. If commit fails, metadata is unchanged.
        let durability = self.durability();
        let starting_rowid = state.meta.next_rowid;
        let starting_root = state.meta.root_page;
        let ids = {
            let mut guard = self.file.begin_write();
            let mut ids = Vec::with_capacity(rows.len());
            let mut current_root = starting_root;
            let mut next_rowid = starting_rowid;

            for row_data in rows {
                let rowid = next_rowid;
                next_rowid += 1;

                let col_values: Vec<(u16, &Value)> = row_data
                    .iter()
                    .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
                    .collect();
                let row_bytes = row::encode_row(rowid, &col_values);

                if row_bytes.len() > self.max_row_size() as usize {
                    return Err(BoogyError::RowTooLarge(row_bytes.len()));
                }

                let mut tree = BTreeWriter::new(&mut guard, current_root);
                let new_root = tree.insert(rowid, &row_bytes)?;
                current_root = new_root;

                if !state.meta.indexes.is_empty() {
                    Self::index_update_row(&mut guard, &mut state.meta, rowid, &row_bytes, false)?;
                }

                ids.push(rowid);
            }

            Self::commit_write(guard, &self.file, &self.wal, durability, state.meta.table_id, state.meta.cipher.as_ref())?;

            // Commit succeeded -- now update metadata.
            let count = ids.len() as u64;
            state.meta.root_page = current_root;
            state.meta.next_rowid = next_rowid;
            state.meta.row_count += count;
            ids
        };

        Ok(ids)
    }

    /// Atomic keyed counter: find-or-insert-and-add. Finds the single row
    /// matching the `key` tuple; if found, adds the numeric `delta` to its
    /// `counter` column (Integer+Integer→Integer, Real+Real→Real, mixed→Real;
    /// a null/absent counter is treated as zero of `delta`'s type) and applies
    /// the `set` columns. If absent, inserts a row with the key columns,
    /// `counter = delta`, and `set`. Returns the affected rowid.
    ///
    /// Atomicity: the find + the write run inside a SINGLE `AcidTransaction`
    /// (one private dirty overlay, one commit), so there is no find→write
    /// window in which another writer could insert the same key. A composite
    /// `unique` index on the key columns (when present) is the durable backstop
    /// that turns any racing double-insert into a `UniqueViolation` rather than
    /// a duplicate row. `delta` must be `Integer` or `Real`.
    pub fn upsert_increment(
        &self,
        table: &str,
        key: &[(&str, Value)],
        counter: &str,
        delta: Value,
        set: &[(&str, Value)],
    ) -> Result<u64> {
        let mut tx = AcidTransaction::new(self);
        let rowid = tx.upsert_increment(table, key, counter, delta, set)?;
        tx.commit()?;
        Ok(rowid)
    }

    /// Atomic insert-or-update by unique-index key. Wraps the
    /// transaction-scoped `AcidTransaction::upsert` in a one-shot
    /// transaction for non-tx callers. See
    /// `AcidTransaction::upsert` for the contract.
    pub fn upsert(
        &self,
        table: &str,
        key: &[(&str, Value)],
        set: &[(&str, Value)],
    ) -> Result<u64> {
        let mut tx = AcidTransaction::new(self);
        let rowid = tx.upsert(table, key, set)?;
        tx.commit()?;
        Ok(rowid)
    }

    /// Run a multi-table transaction.
    pub fn transaction<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&TransactionCtx) -> Result<R>,
    {
        let ctx = TransactionCtx { db: self };
        let result = f(&ctx)?;
        // Individual operations already committed. Do a final flush for consistency.
        self.flush_transaction()?;
        Ok(result)
    }

    /// Begin a guard-based transaction. Returns a `Transaction` that commits
    /// on explicit `.commit()`. Operations within the transaction lock tables
    /// lazily, same as the callback-based `transaction()`.
    ///
    /// ```ignore
    /// let mut tx = db.begin()?;
    /// tx.insert("users", &[("name", Value::Text("Alice".into()))])?;
    /// tx.insert("posts", &[("title", Value::Text("Hello".into()))])?;
    /// tx.commit()?;
    /// ```
    pub fn begin(&self) -> Result<Transaction<'_>> {
        if self.is_acid() {
            Ok(Transaction::Acid(AcidTransaction::new(self)))
        } else {
            Ok(Transaction::Light(LightTransaction { db: self, committed: false }))
        }
    }

    /// Final flush for guard-based transactions.
    pub(crate) fn flush_transaction(&self) -> Result<()> {
        let durability = self.durability();
        let guard = self.file.begin_write();
        Self::commit_write(guard, &self.file, &self.wal, durability, 0, None)?;
        Ok(())
    }

    /// Create a new encrypted table. Data pages are encrypted at rest with
    /// AES-256-GCM using the provided 32-byte key.
    pub fn create_table_encrypted(&self, name: &str, columns: &[ColumnDef], key: &[u8; 32]) -> Result<()> {
        {
            let tables = self.tables.read().unwrap();
            if tables.contains_key(name) {
                return Err(BoogyError::TableExists(name.to_string()));
            }
        }

        let durability = self.durability();
        let cipher = crate::crypto::Cipher::new(key);
        let (root, table_id) = {
            let mut guard = self.file.begin_write();
            if self.file.page_count() == 0 {
                guard.allocate_page()?;
            }
            let root = BTreeWriter::create(&mut guard)?;
            let table_id = {
                let mut next = self.next_table_id.lock().unwrap();
                let id = *next;
                *next += 1;
                id
            };
            // The initial root page is an empty B+ tree leaf containing no user data.
            // We commit with cipher: None because this batch also includes the system
            // page (page 0), which must always be plaintext. Subsequent data writes
            // use the cipher for encryption.
            Self::commit_write(guard, &self.file, &self.wal, durability, table_id, None)?;
            (root, table_id)
        };
        let mut meta = TableMeta::new(name.to_string(), table_id, columns.to_vec(), root);
        meta.encrypted = true;
        meta.cipher = Some(cipher.clone());
        let state = Arc::new(RwLock::new(TableState { meta }));

        {
            let mut tables = self.tables.write().unwrap();
            if tables.contains_key(name) {
                return Err(BoogyError::TableExists(name.to_string()));
            }
            tables.insert(name.to_string(), state);
        }

        // Register cipher for page-level encryption on sync.
        {
            let mut ciphers = self.table_ciphers.write().unwrap();
            ciphers.insert(table_id, Arc::new(cipher));
        }

        let (metas, next_id) = self.snapshot_table_metas();
        Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;
        Ok(())
    }

    /// Unlock an encrypted table by providing its key. Operations on the
    /// table will fail with `TableLocked` until this is called after open.
    pub fn unlock_table(&self, name: &str, key: &[u8; 32]) -> Result<()> {
        let table_state = {
            let tables = self.tables.read().unwrap();
            tables.get(name)
                .ok_or_else(|| BoogyError::TableNotFound(name.to_string()))?
                .clone()
        };

        let mut state = table_state.write().unwrap();
        if !state.meta.encrypted {
            return Err(BoogyError::SchemaMismatch(format!(
                "table '{name}' is not encrypted"
            )));
        }
        if state.meta.cipher.is_some() {
            // Already unlocked
            return Ok(());
        }

        let cipher = crate::crypto::Cipher::new(key);

        // Verify key by attempting to decrypt the root page from disk.
        let root_page_no = state.meta.root_page;
        if root_page_no < self.file.page_count() {
            match self.file.read_page_raw(root_page_no) {
                Ok(raw) => {
                    cipher.decrypt_page(&raw)
                        .map_err(|_| BoogyError::InvalidKey(name.to_string()))?;
                }
                Err(_) => {
                    // Page not on disk -- could be only in WAL. Accept the key.
                }
            }
        }

        // Key accepted. Store cipher.
        let table_id = state.meta.table_id;
        state.meta.cipher = Some(cipher.clone());

        {
            let mut ciphers = self.table_ciphers.write().unwrap();
            ciphers.insert(table_id, Arc::new(cipher));
        }

        // Eagerly decrypt and cache all pages in the table's B+ tree.
        self.preload_encrypted_table(&state)?;

        Ok(())
    }

    /// Walk the B+ tree of an encrypted table, decrypt each page from disk,
    /// and insert the plaintext into the page cache. This avoids any changes
    /// to the normal read path (which validates CRC on cache miss).
    fn preload_encrypted_table(&self, state: &TableState) -> Result<()> {
        let cipher = state.meta.cipher.as_ref().unwrap();
        let mut to_visit = vec![state.meta.root_page];

        while let Some(page_no) = to_visit.pop() {
            if self.file.is_cached(page_no) {
                continue;
            }

            if page_no < self.file.page_count() {
                match self.file.read_page_raw(page_no) {
                    Ok(raw) => {
                        let decrypted = cipher.decrypt_page(&raw)?;
                        let mut padded = [0u8; PAGE_SIZE];
                        padded[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE].copy_from_slice(&decrypted);
                        let mut page = Page::from_bytes_unchecked(padded);
                        page.update_checksum();
                        self.file.put_cached_page(page_no, page.clone());

                        // Register this page's cipher for future sync_all
                        self.file.register_page_cipher(page_no, Arc::new(cipher.clone()));

                        if page.is_branch() {
                            let num_keys = page.num_rows() as usize;
                            for i in 0..=num_keys {
                                let child = Self::get_branch_child_from_page(&page, i);
                                to_visit.push(child);
                            }
                        }
                        if page.is_leaf() {
                            let next = page.next_leaf();
                            if next != 0 {
                                to_visit.push(next);
                            }
                        }
                    }
                    Err(_) => {} // page only in cache/WAL, skip
                }
            }
        }
        Ok(())
    }

    /// Read a branch page child pointer at the given index.
    fn get_branch_child_from_page(page: &Page, idx: usize) -> u32 {
        let offset = 16 + idx * 12; // PAGE_HEADER_SIZE + idx * BRANCH_ENTRY_SIZE
        u32::from_le_bytes(page.data[offset..offset + 4].try_into().unwrap())
    }
}

// ---------------------------------------------------------------------------
// Vector search API
// ---------------------------------------------------------------------------

#[cfg(feature = "vector")]
impl BoogyDb {
    /// Path for a vector collection's data file.
    fn vector_file_path(&self, table: &str, collection: &str) -> PathBuf {
        self.path.with_extension(format!("{table}.{collection}.vec"))
    }

    /// Path for a vector collection's WAL file.
    fn vector_wal_path(&self, table: &str, collection: &str) -> PathBuf {
        self.path.with_extension(format!("{table}.{collection}.vec.wal"))
    }

    /// Build the HashMap key for a vector collection.
    fn collection_key(table: &str, collection: &str) -> (String, String) {
        (table.to_string(), collection.to_string())
    }

    /// Internal table name for storing rowid<->node_id mappings.
    fn vector_mapping_table(table: &str, collection: &str) -> String {
        format!("__vec_{table}_{collection}")
    }

    /// Parse an internal mapping table name back into (table, collection).
    fn parse_vector_mapping_table(name: &str) -> Option<(String, String)> {
        let rest = name.strip_prefix("__vec_")?;
        let underscore = rest.find('_')?;
        let table = &rest[..underscore];
        let collection = &rest[underscore + 1..];
        if table.is_empty() || collection.is_empty() {
            return None;
        }
        Some((table.to_string(), collection.to_string()))
    }

    /// Discover and reopen all vector collections that were persisted.
    /// Called from BoogyDb::open() after tables are loaded.
    fn reopen_vector_collections(&self) -> Result<()> {
        // Collect mapping table names.
        let mapping_tables: Vec<(String, String, String)> = {
            let tables = self.tables.read().unwrap();
            tables.keys()
                .filter_map(|name| {
                    Self::parse_vector_mapping_table(name)
                        .map(|(t, c)| (name.clone(), t, c))
                })
                .collect()
        };

        for (mapping_name, table, collection) in mapping_tables {
            let vec_path = self.vector_file_path(&table, &collection);
            let wal_path = self.vector_wal_path(&table, &collection);

            if !vec_path.exists() {
                continue; // orphaned mapping table — skip
            }

            // Try to open without a key. If the file is encrypted, skip it —
            // the user must call unlock_vector_collection() to provide the key.
            let mut coll = match VectorCollection::open(&vec_path, &wal_path, None) {
                Ok(c) => c,
                Err(BoogyError::DecryptionFailed(_)) => continue,
                Err(e) => return Err(e),
            };

            // Rebuild mappings from the internal table.
            let rows = self.find(&mapping_name, crate::filter::FindOptions::default())?;
            let mappings: Vec<(u64, u32)> = rows.rows.iter()
                .filter_map(|row| {
                    let node_id = match row.get("node_id") {
                        Some(Value::Integer(n)) => n as u32,
                        _ => return None,
                    };
                    Some((row.id, node_id))
                })
                .collect();
            coll.rebuild_mappings(mappings);

            let key = Self::collection_key(&table, &collection);
            self.vector_collections.write().unwrap().insert(key, Arc::new(RwLock::new(coll)));
        }

        Ok(())
    }

    /// Create a new vector collection attached to an existing table.
    pub fn create_vector_collection(
        &self,
        table: &str,
        name: &str,
        options: &VectorCollectionOptions,
    ) -> Result<()> {
        // Verify table exists.
        {
            let tables = self.tables.read().unwrap();
            if !tables.contains_key(table) {
                return Err(BoogyError::TableNotFound(table.to_string()));
            }
        }

        let key = Self::collection_key(table, name);
        let mut collections = self.vector_collections.write().unwrap();

        if collections.contains_key(&key) {
            return Err(BoogyError::VectorCollectionExists(format!(
                "{table}.{name}"
            )));
        }

        let vec_path = self.vector_file_path(table, name);
        let wal_path = self.vector_wal_path(table, name);
        let collection = VectorCollection::create(vec_path, wal_path, options)?;
        collections.insert(key, Arc::new(RwLock::new(collection)));

        // Drop the write lock before creating the mapping table (it takes its own locks).
        drop(collections);

        // Create internal mapping table for persistence.
        let mapping_table = Self::vector_mapping_table(table, name);
        self.create_table(&mapping_table, &[
            ColumnDef::new("node_id", Type::Integer),
        ])?;

        Ok(())
    }

    /// Open an existing vector collection from disk.
    ///
    /// Use this after re-opening a `BoogyDb` to re-attach a vector collection
    /// that was created in a previous session. After opening, call
    /// `vector_rebuild_mappings` to restore rowid<->node_id linkage.
    pub fn open_vector_collection(&self, table: &str, name: &str) -> Result<()> {
        // Verify table exists.
        {
            let tables = self.tables.read().unwrap();
            if !tables.contains_key(table) {
                return Err(BoogyError::TableNotFound(table.to_string()));
            }
        }

        let key = Self::collection_key(table, name);
        let mut collections = self.vector_collections.write().unwrap();

        if collections.contains_key(&key) {
            return Err(BoogyError::VectorCollectionExists(format!(
                "{table}.{name}"
            )));
        }

        let vec_path = self.vector_file_path(table, name);
        let wal_path = self.vector_wal_path(table, name);
        let collection = VectorCollection::open(vec_path, wal_path, None)?;
        collections.insert(key, Arc::new(RwLock::new(collection)));

        Ok(())
    }

    /// Rebuild rowid<->node_id mappings for an opened vector collection.
    pub fn vector_rebuild_mappings(
        &self,
        table: &str,
        collection: &str,
        mappings: Vec<(u64, u32)>,
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let coll_arc = {
            let collections = self.vector_collections.read().unwrap();
            collections.get(&key).ok_or_else(|| {
                BoogyError::VectorCollectionNotFound(format!("{table}.{collection}"))
            })?.clone()
        };
        let mut coll = coll_arc.write().unwrap();
        coll.rebuild_mappings(mappings);
        Ok(())
    }

    /// Unlock an encrypted vector collection by providing its key.
    ///
    /// Encrypted collections are skipped during `reopen_vector_collections`.
    /// Call this method to provide the key and open them.
    pub fn unlock_vector_collection(&self, table: &str, name: &str, key: &[u8; 32]) -> Result<()> {
        // Verify table exists.
        {
            let tables = self.tables.read().unwrap();
            if !tables.contains_key(table) {
                return Err(BoogyError::TableNotFound(table.to_string()));
            }
        }

        let coll_key = Self::collection_key(table, name);
        {
            let collections = self.vector_collections.read().unwrap();
            if collections.contains_key(&coll_key) {
                return Err(BoogyError::VectorCollectionExists(format!(
                    "{table}.{name} is already open"
                )));
            }
        }

        let vec_path = self.vector_file_path(table, name);
        let wal_path = self.vector_wal_path(table, name);

        if !vec_path.exists() {
            return Err(BoogyError::VectorCollectionNotFound(format!("{table}.{name}")));
        }

        let mut coll = VectorCollection::open(&vec_path, &wal_path, Some(key))?;

        // Rebuild mappings from the internal mapping table.
        let mapping_table = Self::vector_mapping_table(table, name);
        let rows = self.find(&mapping_table, crate::filter::FindOptions::default())?;
        let mappings: Vec<(u64, u32)> = rows.rows.iter()
            .filter_map(|row| {
                let node_id = match row.get("node_id") {
                    Some(Value::Integer(n)) => n as u32,
                    _ => return None,
                };
                Some((row.id, node_id))
            })
            .collect();
        coll.rebuild_mappings(mappings);

        self.vector_collections.write().unwrap().insert(coll_key, Arc::new(RwLock::new(coll)));
        Ok(())
    }

    /// Drop a vector collection, removing its files from disk.
    pub fn drop_vector_collection(&self, table: &str, name: &str) -> Result<()> {
        let key = Self::collection_key(table, name);
        let mut collections = self.vector_collections.write().unwrap();

        if collections.remove(&key).is_none() {
            return Err(BoogyError::VectorCollectionNotFound(format!(
                "{table}.{name}"
            )));
        }

        // Drop the write lock before modifying tables.
        drop(collections);

        // Remove files from disk.
        let vec_path = self.vector_file_path(table, name);
        let wal_path = self.vector_wal_path(table, name);
        let _ = std::fs::remove_file(vec_path);
        let _ = std::fs::remove_file(wal_path);

        // Drop the internal mapping table.
        let mapping_table = Self::vector_mapping_table(table, name);
        let _ = self.drop_table(&mapping_table);

        Ok(())
    }

    /// Insert a vector for a row in the given collection.
    pub fn vector_insert(
        &self,
        table: &str,
        collection: &str,
        rowid: u64,
        vector: &[f32],
    ) -> Result<()> {
        // Verify rowid exists in the table.
        if self.get(table, rowid)?.is_none() {
            return Err(BoogyError::RowNotFound(rowid.to_string()));
        }

        let key = Self::collection_key(table, collection);
        let coll_arc = {
            let collections = self.vector_collections.read().unwrap();
            collections.get(&key).ok_or_else(|| {
                BoogyError::VectorCollectionNotFound(format!("{table}.{collection}"))
            })?.clone()
        };

        let fsync = self.durability() == Durability::Immediate;
        let node_id = {
            let mut coll = coll_arc.write().unwrap();
            coll.insert(rowid, vector, fsync)?
        };

        // Persist the rowid<->node_id mapping.
        let mapping_table = Self::vector_mapping_table(table, collection);
        self.insert_with_id(&mapping_table, rowid, &[
            ("node_id", Value::Integer(node_id as i64)),
        ])?;

        Ok(())
    }

    /// Batch-insert vectors for multiple rows.
    pub fn vector_insert_batch(
        &self,
        table: &str,
        collection: &str,
        entries: &[(u64, Vec<f32>)],
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let coll_arc = {
            let collections = self.vector_collections.read().unwrap();
            collections.get(&key).ok_or_else(|| {
                BoogyError::VectorCollectionNotFound(format!("{table}.{collection}"))
            })?.clone()
        };

        let fsync = self.durability() == Durability::Immediate;
        let last_idx = entries.len().saturating_sub(1);
        let mut node_ids = Vec::with_capacity(entries.len());
        {
            let mut coll = coll_arc.write().unwrap();
            for (i, (rowid, vector)) in entries.iter().enumerate() {
                let do_fsync = fsync && i == last_idx;
                let node_id = coll.insert(*rowid, vector, do_fsync)?;
                node_ids.push((*rowid, node_id));
            }
        }

        // Persist mappings.
        let mapping_table = Self::vector_mapping_table(table, collection);
        for (rowid, node_id) in node_ids {
            self.insert_with_id(&mapping_table, rowid, &[
                ("node_id", Value::Integer(node_id as i64)),
            ])?;
        }

        Ok(())
    }

    /// Update the vector for a row in the given collection.
    pub fn vector_update(
        &self,
        table: &str,
        collection: &str,
        rowid: u64,
        vector: &[f32],
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let coll_arc = {
            let collections = self.vector_collections.read().unwrap();
            collections.get(&key).ok_or_else(|| {
                BoogyError::VectorCollectionNotFound(format!("{table}.{collection}"))
            })?.clone()
        };

        let fsync = self.durability() == Durability::Immediate;
        let new_node_id = {
            let mut coll = coll_arc.write().unwrap();
            coll.update(rowid, vector, fsync)?
        };

        // Update the mapping (delete old, insert new node_id).
        let mapping_table = Self::vector_mapping_table(table, collection);
        let _ = self.delete(&mapping_table, rowid);
        self.insert_with_id(&mapping_table, rowid, &[
            ("node_id", Value::Integer(new_node_id as i64)),
        ])?;

        Ok(())
    }

    /// Delete a vector from the given collection.
    pub fn vector_delete(
        &self,
        table: &str,
        collection: &str,
        rowid: u64,
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let coll_arc = {
            let collections = self.vector_collections.read().unwrap();
            collections.get(&key).ok_or_else(|| {
                BoogyError::VectorCollectionNotFound(format!("{table}.{collection}"))
            })?.clone()
        };

        let fsync = self.durability() == Durability::Immediate;
        {
            let mut coll = coll_arc.write().unwrap();
            coll.delete(rowid, fsync)?;
        }

        // Remove the mapping.
        let mapping_table = Self::vector_mapping_table(table, collection);
        let _ = self.delete(&mapping_table, rowid); // ignore if already gone

        Ok(())
    }

    /// Search for the k nearest vectors, optionally filtering by row metadata.
    pub fn vector_search(
        &self,
        table: &str,
        collection: &str,
        query: &[f32],
        options: &VectorSearchOptions,
    ) -> Result<Vec<VectorResult>> {
        let key = Self::collection_key(table, collection);
        let coll_arc = {
            let collections = self.vector_collections.read().unwrap();
            collections.get(&key).ok_or_else(|| {
                BoogyError::VectorCollectionNotFound(format!("{table}.{collection}"))
            })?.clone()
        };
        let coll = coll_arc.read().unwrap();

        let row_loader: Option<Box<dyn Fn(u64) -> Option<Vec<(String, Value)>> + '_>> =
            options.filter.as_ref().map(|_| {
                Box::new(|rowid: u64| -> Option<Vec<(String, Value)>> {
                    self.get(table, rowid).ok().flatten().map(|row| row.columns())
                }) as Box<dyn Fn(u64) -> Option<Vec<(String, Value)>>>
            });

        coll.search(
            query,
            options.k,
            options.ef_search,
            row_loader.as_ref().map(|b| b.as_ref()),
            options.filter.as_ref(),
        )
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
            if let Ok((page0, overflow_pages)) =
                serialize_system_page(&metas, next_id, &mut || guard.allocate_page())
            {
                guard.put_page(0, page0);
                for (page_no, overflow_page) in overflow_pages {
                    guard.put_page(page_no, overflow_page);
                }
            }
            let _ = guard.commit();
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

/// Light transaction (non-ACID). Each operation commits independently as it
/// executes. `commit()` performs a final metadata flush. Drop is a no-op
/// because individual operations were already committed. For true all-or-nothing
/// rollback semantics, use ACID mode (`db.set_acid(true)`) instead.
pub(crate) struct LightTransaction<'a> {
    db: &'a BoogyDb,
    committed: bool,
}

impl Drop for LightTransaction<'_> {
    fn drop(&mut self) {
        // No-op: individual operations were already committed. There is nothing
        // to roll back. ACID mode (`AcidTransaction`) must be used for true
        // rollback semantics.
    }
}

// ---------------------------------------------------------------------------
// MetaDelta — per-table metadata tracked during an AcidTransaction
// ---------------------------------------------------------------------------

struct MetaDelta {
    root_page: u32,
    row_count_delta: i64,
    next_rowid: u64,
    table_id: u32,
    cipher: Option<crate::crypto::Cipher>,
    /// Tracked index root pages: (index_name, root_page).
    /// Updated during operations; applied on commit. Keyed by index NAME (not
    /// leading column) so composite indexes — and two indexes sharing a leading
    /// column — track independently.
    index_roots: Vec<(String, u32)>,
}

// ---------------------------------------------------------------------------
// AcidTransaction — true ACID: all-or-nothing commit via inject/drain
// ---------------------------------------------------------------------------

/// A reference to the database that is either borrowed (the classic
/// scoped `begin()` transaction) or owned via `Arc` (an interactive
/// transaction holdable across async boundaries). Derefs to `BoogyDb`
/// so transaction method bodies are identical for both.
pub(crate) enum DbRef<'a> {
    Borrowed(&'a BoogyDb),
    Owned(std::sync::Arc<BoogyDb>),
}

impl<'a> std::ops::Deref for DbRef<'a> {
    type Target = BoogyDb;
    fn deref(&self) -> &BoogyDb {
        match self {
            DbRef::Borrowed(db) => db,
            DbRef::Owned(db) => db,
        }
    }
}

pub struct AcidTransaction<'a> {
    db: DbRef<'a>,
    private_dirty: StdHashMap<u32, Box<Page>>,
    new_page_count: u32,
    meta_deltas: StdHashMap<String, MetaDelta>,
    /// Within-transaction unique-index witness set, keyed by
    /// `(table, index_name)` → set of composite value-prefixes inserted so far
    /// in THIS uncommitted transaction. The committed-tree scan in
    /// [`Self::enforce_unique_indexes_acid`] cannot see writes made earlier in
    /// the same tx (they live in `private_dirty`, not the committed index
    /// tree), so multi-insert duplicates (`insert_many`, explicit `begin()`
    /// batches) are caught here instead. Empty/unused on the host's dominant
    /// single-insert-per-tx path.
    unique_seen: StdHashMap<(String, String), std::collections::HashSet<Vec<u8>>>,
    committed: bool,
}

impl<'a> AcidTransaction<'a> {
    fn new(db: &'a BoogyDb) -> Self {
        Self {
            db: DbRef::Borrowed(db),
            private_dirty: StdHashMap::new(),
            new_page_count: 0,
            meta_deltas: StdHashMap::new(),
            unique_seen: StdHashMap::new(),
            committed: false,
        }
    }

    /// Owned variant — holds an `Arc<BoogyDb>`, so the transaction is
    /// `'static` and can be held across async calls.
    pub(crate) fn new_owned(db: std::sync::Arc<BoogyDb>) -> AcidTransaction<'static> {
        AcidTransaction {
            db: DbRef::Owned(db),
            private_dirty: StdHashMap::new(),
            new_page_count: 0,
            meta_deltas: StdHashMap::new(),
            unique_seen: StdHashMap::new(),
            committed: false,
        }
    }

    /// Borrow the WriteGuard with our private dirty pages injected,
    /// run `f`, then drain dirty pages back into our private buffer.
    fn with_guard<F, R>(&mut self, f: F) -> Result<R>
    where
        F: FnOnce(&mut WriteGuard) -> Result<R>,
    {
        let mut guard = self.db.file.begin_write();
        let pages = std::mem::take(&mut self.private_dirty);
        guard.inject_dirty(pages);
        guard.set_new_page_count(self.new_page_count);

        let result = f(&mut guard);

        self.private_dirty = guard.drain_dirty();
        self.new_page_count = guard.new_page_count();
        guard.set_new_page_count(0);
        guard.discard();

        result
    }

    /// Look up a table, returning the Arc<RwLock<TableState>>.
    fn table_state(&self, table: &str) -> Result<Arc<RwLock<TableState>>> {
        let tables = self.db.tables.read().unwrap();
        tables
            .get(table)
            .cloned()
            .ok_or_else(|| BoogyError::TableNotFound(table.to_string()))
    }

    /// Get the current root_page for a table, preferring meta_deltas.
    fn current_root(&self, table: &str, meta: &TableMeta) -> u32 {
        self.meta_deltas
            .get(table)
            .map(|d| d.root_page)
            .unwrap_or(meta.root_page)
    }

    /// Get the current next_rowid for a table, preferring meta_deltas.
    fn current_next_rowid(&self, table: &str, meta: &TableMeta) -> u64 {
        self.meta_deltas
            .get(table)
            .map(|d| d.next_rowid)
            .unwrap_or(meta.next_rowid)
    }

    /// Get the current root page for the named index, preferring meta_deltas.
    fn current_index_root(&self, table: &str, index_name: &str, original_root: u32) -> u32 {
        if let Some(delta) = self.meta_deltas.get(table) {
            for (name, root) in &delta.index_roots {
                if name == index_name {
                    return *root;
                }
            }
        }
        original_root
    }

    /// Enforce uniqueness for every `unique` index on `meta` against the
    /// encoded `row_bytes` of a row about to be inserted in THIS transaction.
    ///
    /// Returns `Err(UniqueViolation)` if the row's composite value-tuple (all
    /// index columns, no rowid) collides with either:
    ///   1. a row already committed (read-only `IndexTreeReader::scan_prefix`
    ///      over the committed index tree — covers the host's dominant
    ///      single-insert-per-tx case), or
    ///   2. a row inserted earlier in this same uncommitted tx (the committed
    ///      tree can't see it; checked against `self.unique_seen`).
    ///
    /// A Null in any index column means the tuple is not indexed (matching the
    /// single-column convention) and is therefore never a uniqueness conflict.
    ///
    /// This performs NO mutation of database state. On the Ok path it records
    /// each unique index's value-prefix into `self.unique_seen` so a later
    /// insert in the same tx sees it. The caller MUST run this BEFORE any write
    /// guard / meta_delta mutation, so a rejection leaves the tx unchanged.
    fn enforce_unique_indexes_acid(
        &mut self,
        table: &str,
        meta: &TableMeta,
        row_bytes: &[u8],
    ) -> Result<()> {
        for idx in &meta.indexes {
            if !idx.unique {
                continue;
            }
            // Resolve all index columns to (col_id, col_type).
            let mut col_ids: Vec<u16> = Vec::with_capacity(idx.columns.len());
            let mut col_types: Vec<crate::value::Type> = Vec::with_capacity(idx.columns.len());
            let mut resolved = true;
            for idx_col in &idx.columns {
                match (
                    meta.col_name_to_id.get(idx_col).copied(),
                    meta.columns.iter().find(|c| &c.name == idx_col).map(|c| c.col_type),
                ) {
                    (Some(cid), Some(ct)) => {
                        col_ids.push(cid);
                        col_types.push(ct);
                    }
                    _ => {
                        resolved = false;
                        break;
                    }
                }
            }
            if !resolved {
                continue;
            }
            let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
            for cid in &col_ids {
                col_vals.push(crate::row::extract_column(row_bytes, *cid)?.unwrap_or(Value::Null));
            }
            // A Null component is not indexed, so it cannot collide.
            let Some(prefix) = index::encode_composite_value_prefix(&col_types, &col_vals) else {
                continue;
            };

            // (1) vs committed data. Read the CURRENT (delta-aware) root for
            // this index, but scan the COMMITTED tree image via the PageFile:
            // uncommitted writes live in private_dirty, not the file, so this
            // sees only durably-committed entries — exactly the rows from prior
            // transactions. Within-tx dups are handled by (2).
            let committed_root = idx.root_page;
            let reader = IndexTreeReader::new(&self.db.file, committed_root);
            if !reader.scan_prefix(&prefix)?.is_empty() {
                return Err(BoogyError::UniqueViolation {
                    index: idx.name.clone(),
                });
            }

            // (2) vs earlier inserts in THIS tx.
            if let Some(seen) = self.unique_seen.get(&(table.to_string(), idx.name.clone())) {
                if seen.contains(&prefix) {
                    return Err(BoogyError::UniqueViolation {
                        index: idx.name.clone(),
                    });
                }
            }
        }

        // All unique indexes passed — now record this row's prefixes so a later
        // insert in the same tx collides. Done in a second pass so a rejection
        // above records nothing.
        for idx in &meta.indexes {
            if !idx.unique {
                continue;
            }
            let mut col_ids: Vec<u16> = Vec::with_capacity(idx.columns.len());
            let mut col_types: Vec<crate::value::Type> = Vec::with_capacity(idx.columns.len());
            let mut resolved = true;
            for idx_col in &idx.columns {
                match (
                    meta.col_name_to_id.get(idx_col).copied(),
                    meta.columns.iter().find(|c| &c.name == idx_col).map(|c| c.col_type),
                ) {
                    (Some(cid), Some(ct)) => {
                        col_ids.push(cid);
                        col_types.push(ct);
                    }
                    _ => {
                        resolved = false;
                        break;
                    }
                }
            }
            if !resolved {
                continue;
            }
            let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
            for cid in &col_ids {
                col_vals.push(crate::row::extract_column(row_bytes, *cid)?.unwrap_or(Value::Null));
            }
            if let Some(prefix) = index::encode_composite_value_prefix(&col_types, &col_vals) {
                self.unique_seen
                    .entry((table.to_string(), idx.name.clone()))
                    .or_default()
                    .insert(prefix);
            }
        }

        Ok(())
    }

    /// Resolve every index on `meta` to the composite descriptor the ACID
    /// write loop needs: `(index_name, [col_id], [col_type], current_root)`.
    /// Keying on the index NAME (not leading column) lets composite indexes and
    /// indexes sharing a leading column track their root pages independently.
    fn composite_index_info(
        &self,
        table: &str,
        meta: &TableMeta,
    ) -> Vec<(String, Vec<u16>, Vec<crate::value::Type>, u32)> {
        meta.indexes
            .iter()
            .filter_map(|idx| {
                let mut col_ids: Vec<u16> = Vec::with_capacity(idx.columns.len());
                let mut col_types: Vec<crate::value::Type> = Vec::with_capacity(idx.columns.len());
                for idx_col in &idx.columns {
                    let cid = meta.col_name_to_id.get(idx_col).copied()?;
                    let ct = meta.columns.iter().find(|c| &c.name == idx_col)?.col_type;
                    col_ids.push(cid);
                    col_types.push(ct);
                }
                let root = self.current_index_root(table, &idx.name, idx.root_page);
                Some((idx.name.clone(), col_ids, col_types, root))
            })
            .collect()
    }

    /// Insert a row with auto-increment rowid.
    pub fn insert(&mut self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;
        BoogyDb::enforce_index_types(&state.meta, data)?;

        let root_page = self.current_root(table, &state.meta);
        let rowid = self.current_next_rowid(table, &state.meta);

        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(rowid, &col_values);

        if row_bytes.len() > self.db.max_row_size() as usize {
            return Err(BoogyError::RowTooLarge(row_bytes.len()));
        }

        // Unique-index enforcement. Read-only (committed-tree scan + within-tx
        // witness set) and runs BEFORE any write guard or meta_delta mutation,
        // so a rejection leaves the transaction byte-for-byte unchanged: no
        // dirty page, no rowid consumed (the rowid lives in the delta we never
        // write below), nothing to roll back beyond dropping the tx.
        if !state.meta.indexes.is_empty() {
            self.enforce_unique_indexes_acid(table, &state.meta, &row_bytes)?;
        }

        // Extract composite index info before entering with_guard. Keyed by
        // index name; delta-aware roots so subsequent ops in this tx see the
        // correct (uncommitted) index tree.
        let index_info = self.composite_index_info(table, &state.meta);
        let table_id = state.meta.table_id;
        let cipher = state.meta.cipher.clone();
        drop(state);

        let (new_root, idx_roots) = self.with_guard(|guard| {
            let mut tree = BTreeWriter::new(guard, root_page);
            let new_root = tree.insert(rowid, &row_bytes)?;

            let mut idx_roots = Vec::new();
            for (name, col_ids, col_types, idx_root) in &index_info {
                let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
                for cid in col_ids {
                    col_vals.push(
                        crate::row::extract_column(&row_bytes, *cid)?.unwrap_or(Value::Null),
                    );
                }
                if let Some(key) = index::encode_composite_index_key(col_types, &col_vals, rowid) {
                    let mut itree = IndexTreeWriter::new(guard, *idx_root);
                    let r = itree.insert(&key)?;
                    idx_roots.push((name.clone(), r));
                } else {
                    idx_roots.push((name.clone(), *idx_root));
                }
            }

            Ok((new_root, idx_roots))
        })?;

        // Update meta delta -- store index roots in the delta, NOT in st.meta.
        let delta = self.meta_deltas.entry(table.to_string()).or_insert(MetaDelta {
            root_page,
            row_count_delta: 0,
            next_rowid: rowid,
            table_id,
            cipher,
            index_roots: Vec::new(),
        });
        delta.root_page = new_root;
        delta.row_count_delta += 1;
        delta.next_rowid = rowid + 1;
        delta.index_roots = idx_roots;

        Ok(rowid)
    }

    /// Insert a row with a caller-supplied rowid.
    pub fn insert_with_id(&mut self, table: &str, rowid: u64, data: &[(&str, Value)]) -> Result<()> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;
        BoogyDb::enforce_index_types(&state.meta, data)?;

        let root_page = self.current_root(table, &state.meta);

        let col_values: Vec<(u16, &Value)> = data
            .iter()
            .filter_map(|(name, val)| state.meta.col_id(name).map(|cid| (cid, val)))
            .collect();
        let row_bytes = row::encode_row(rowid, &col_values);

        if row_bytes.len() > self.db.max_row_size() as usize {
            return Err(BoogyError::RowTooLarge(row_bytes.len()));
        }

        // Unique-index enforcement BEFORE any write guard / meta_delta change
        // (see insert() for the atomicity argument — identical here).
        if !state.meta.indexes.is_empty() {
            self.enforce_unique_indexes_acid(table, &state.meta, &row_bytes)?;
        }

        let index_info = self.composite_index_info(table, &state.meta);
        let table_id = state.meta.table_id;
        let cipher = state.meta.cipher.clone();
        let current_next = self.current_next_rowid(table, &state.meta);
        drop(state);

        let (new_root, idx_roots) = self.with_guard(|guard| {
            let mut tree = BTreeWriter::new(guard, root_page);
            let new_root = tree.insert(rowid, &row_bytes)?;

            let mut idx_roots = Vec::new();
            for (name, col_ids, col_types, idx_root) in &index_info {
                let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
                for cid in col_ids {
                    col_vals.push(
                        crate::row::extract_column(&row_bytes, *cid)?.unwrap_or(Value::Null),
                    );
                }
                if let Some(key) = index::encode_composite_index_key(col_types, &col_vals, rowid) {
                    let mut itree = IndexTreeWriter::new(guard, *idx_root);
                    let r = itree.insert(&key)?;
                    idx_roots.push((name.clone(), r));
                } else {
                    idx_roots.push((name.clone(), *idx_root));
                }
            }

            Ok((new_root, idx_roots))
        })?;

        let new_next = if rowid >= current_next { rowid + 1 } else { current_next };

        let delta = self.meta_deltas.entry(table.to_string()).or_insert(MetaDelta {
            root_page,
            row_count_delta: 0,
            next_rowid: current_next,
            table_id,
            cipher,
            index_roots: Vec::new(),
        });
        delta.root_page = new_root;
        delta.row_count_delta += 1;
        delta.next_rowid = new_next;
        delta.index_roots = idx_roots;

        Ok(())
    }

    /// Atomic keyed find-or-insert-and-add. Finds the single row matching the
    /// `key` tuple; if found, adds `delta` to its `counter` column (preserving
    /// type) and applies `set`; if absent, inserts a row carrying the key
    /// columns, `counter = delta`, and `set`. Returns the affected rowid.
    ///
    /// The find and the subsequent insert/update both run inside THIS
    /// transaction (against the same private dirty overlay) and commit as one
    /// unit, so there is no find→write gap visible to another writer.
    pub fn upsert_increment(
        &mut self,
        table: &str,
        key: &[(&str, Value)],
        counter: &str,
        delta: Value,
        set: &[(&str, Value)],
    ) -> Result<u64> {
        // Delta must be numeric (Integer or Real).
        if !matches!(delta, Value::Integer(_) | Value::Real(_)) {
            return Err(BoogyError::SchemaMismatch(format!(
                "upsert_increment delta for '{counter}' must be Integer or Real, got {:?}",
                delta.value_type()
            )));
        }

        // Locate the existing row by the full key tuple. This find runs through
        // the same uncommitted overlay as the write below.
        let filters: Vec<Filter> = key.iter().map(|(c, v)| Filter::eq(*c, v.clone())).collect();
        let existing = self.find(
            table,
            FindOptions { filters, limit: Some(1), ..Default::default() },
        )?;

        if let Some(row) = existing.rows.into_iter().next() {
            // Found: read current counter, add delta preserving type.
            let current = row.get(counter).unwrap_or(Value::Null);
            let new_value = add_counter(&current, &delta)?;
            let rowid = row.id;

            let mut fields: Vec<(&str, Value)> = Vec::with_capacity(1 + set.len());
            fields.push((counter, new_value));
            for (c, v) in set {
                fields.push((*c, v.clone()));
            }
            self.update(table, rowid, &fields)?;
            Ok(rowid)
        } else {
            // Absent: insert key cols + (counter = delta) + set cols.
            let mut data: Vec<(&str, Value)> = Vec::with_capacity(key.len() + 1 + set.len());
            for (c, v) in key {
                data.push((*c, v.clone()));
            }
            data.push((counter, delta));
            for (c, v) in set {
                data.push((*c, v.clone()));
            }
            self.insert(table, &data)
        }
    }

    /// Atomic insert-or-update by unique-index key (general set-semantics).
    ///
    /// If a row exists whose `key` column values match (looked up via a
    /// table-scan with equality filters on the key columns), update
    /// `set` columns on it (key columns untouched). Otherwise insert a
    /// new row with `key + set`. Returns the row id (existing or
    /// newly-inserted).
    ///
    /// Requires a unique index whose columns are exactly the `key`
    /// columns (in any order) — the index is validated to exist (so
    /// concurrent racing inserts on the same key fail at commit time
    /// rather than producing duplicates), but the index is NOT used to
    /// accelerate the lookup. Errors with `SchemaMismatch` if no such
    /// unique index exists.
    ///
    /// If `set` overlaps `key` (i.e. `set` contains a column also in
    /// `key`), the `set` value wins on the update path and the caller's
    /// supplied key value is overwritten — same field-merge semantics
    /// as `update`. Callers who need the key columns preserved should
    /// pass them only in `key`, not in both.
    pub fn upsert(
        &mut self,
        table: &str,
        key: &[(&str, Value)],
        set: &[(&str, Value)],
    ) -> Result<u64> {
        // Validate that a unique index exists with exactly the key columns
        // (in any order — set-equality). Without this guard, an ambiguous
        // key would silently use whatever filter find chooses — surface the
        // contract violation as a loud SchemaMismatch instead.
        let key_col_names: Vec<&str> = key.iter().map(|(c, _)| *c).collect();
        let has_unique_idx = {
            let table_state = self.table_state(table)?;
            let state = table_state.read().unwrap();
            let mut want: Vec<&str> = key_col_names.iter().copied().collect();
            want.sort_unstable();
            state.meta.indexes.iter().any(|m| {
                if !m.unique || m.columns.len() != key_col_names.len() {
                    return false;
                }
                let mut have: Vec<&str> = m.columns.iter().map(|s| s.as_str()).collect();
                have.sort_unstable();
                have == want
            })
        };
        if !has_unique_idx {
            let cols_disp = key_col_names.join(", ");
            return Err(BoogyError::SchemaMismatch(format!(
                "upsert requires a unique index on columns [{cols_disp}]; none found on table '{table}'"
            )));
        }

        // Lookup by the full key tuple. Same uncommitted overlay as the write below.
        let filters: Vec<Filter> = key.iter().map(|(c, v)| Filter::eq(*c, v.clone())).collect();
        let existing = self.find(
            table,
            FindOptions { filters, limit: Some(1), ..Default::default() },
        )?;

        if let Some(row) = existing.rows.into_iter().next() {
            // Update path: write set columns; key columns untouched.
            let fields: Vec<(&str, Value)> = set.iter().map(|(c, v)| (*c, v.clone())).collect();
            self.update(table, row.id, &fields)?;
            Ok(row.id)
        } else {
            // Insert path: combine key + set columns into one insert.
            let mut data: Vec<(&str, Value)> = Vec::with_capacity(key.len() + set.len());
            for (c, v) in key {
                data.push((*c, v.clone()));
            }
            for (c, v) in set {
                data.push((*c, v.clone()));
            }
            self.insert(table, &data)
        }
    }

    /// Get a row by rowid (sees dirty overlay).
    pub fn get(&mut self, table: &str, id: u64) -> Result<Option<Row>> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root_page = self.current_root(table, &state.meta);
        let col_names = state.meta.col_names.clone();
        let col_defs = state.meta.col_defs.clone();
        drop(state);

        let result = self.with_guard(|guard| {
            let tree = BTreeWriter::new(guard, root_page);
            tree.search(id)
        })?;

        match result {
            Some(bytes) => Ok(Some(Row::from_raw(&bytes, col_names, col_defs)?)),
            None => Ok(None),
        }
    }

    /// Update a row by rowid.
    pub fn update(&mut self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;
        BoogyDb::enforce_index_types(&state.meta, fields)?;

        let root_page = self.current_root(table, &state.meta);
        let table_id = state.meta.table_id;
        let cipher = state.meta.cipher.clone();
        let current_next = self.current_next_rowid(table, &state.meta);

        // Extract column info for merge
        let col_name_to_id = state.meta.col_name_to_id.clone();

        let index_info = self.composite_index_info(table, &state.meta);
        drop(state);

        // Prepare field mappings
        let field_updates: Vec<(u16, Value)> = fields
            .iter()
            .filter_map(|(name, val)| col_name_to_id.get(*name).map(|cid| (*cid, val.clone())))
            .collect();

        let result = self.with_guard(|guard| {
            // Read existing row
            let existing_bytes = {
                let tree = BTreeWriter::new(guard, root_page);
                match tree.search(id)? {
                    Some(bytes) => bytes,
                    None => return Ok((false, root_page, Vec::new())),
                }
            };
            let existing = row::decode_row(&existing_bytes)?;

            // Merge updates
            let mut col_map: HashMap<u16, Value> = existing.columns.into_iter().collect();
            for (col_id, val) in &field_updates {
                col_map.insert(*col_id, val.clone());
            }
            let col_values: Vec<(u16, &Value)> = col_map.iter().map(|(k, v)| (*k, v)).collect();
            let new_row = row::encode_row(id, &col_values);

            // Remove old composite index entries, then insert the new tuple.
            let mut idx_roots = Vec::new();
            for (name, col_ids, col_types, idx_root) in &index_info {
                let mut current_root = *idx_root;
                let mut old_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
                for cid in col_ids {
                    old_vals.push(
                        crate::row::extract_column(&existing_bytes, *cid)?.unwrap_or(Value::Null),
                    );
                }
                if let Some(key) = index::encode_composite_index_key(col_types, &old_vals, id) {
                    let mut itree = IndexTreeWriter::new(guard, current_root);
                    itree.delete(&key)?;
                    current_root = itree.root_page();
                }
                let mut new_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
                for cid in col_ids {
                    new_vals.push(
                        crate::row::extract_column(&new_row, *cid)?.unwrap_or(Value::Null),
                    );
                }
                if let Some(key) = index::encode_composite_index_key(col_types, &new_vals, id) {
                    let mut itree = IndexTreeWriter::new(guard, current_root);
                    let r = itree.insert(&key)?;
                    idx_roots.push((name.clone(), r));
                } else {
                    idx_roots.push((name.clone(), current_root));
                }
            }

            // Delete + re-insert
            {
                let mut tree = BTreeWriter::new(guard, root_page);
                tree.delete(id)?;
                let new_root = tree.insert(id, &new_row)?;
                Ok((true, new_root, idx_roots))
            }
        })?;

        let (updated, new_root, idx_roots) = result;
        if !updated {
            return Ok(false);
        }

        let delta = self.meta_deltas.entry(table.to_string()).or_insert(MetaDelta {
            root_page,
            row_count_delta: 0,
            next_rowid: current_next,
            table_id,
            cipher,
            index_roots: Vec::new(),
        });
        delta.root_page = new_root;
        delta.index_roots = idx_roots;

        Ok(true)
    }

    /// Delete a row by rowid.
    pub fn delete(&mut self, table: &str, id: u64) -> Result<bool> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root_page = self.current_root(table, &state.meta);
        let table_id = state.meta.table_id;
        let cipher = state.meta.cipher.clone();
        let current_next = self.current_next_rowid(table, &state.meta);

        let has_indexes = !state.meta.indexes.is_empty();
        let index_info = self.composite_index_info(table, &state.meta);
        drop(state);

        let result = self.with_guard(|guard| {
            // Read the row before deletion for index maintenance
            let row_bytes_for_index = if has_indexes {
                let tree = BTreeWriter::new(guard, root_page);
                tree.search(id)?
            } else {
                None
            };

            let mut tree = BTreeWriter::new(guard, root_page);
            let deleted = tree.delete(id)?;
            let new_root = tree.root_page();

            let mut idx_roots = Vec::new();
            if deleted {
                if let Some(ref bytes) = row_bytes_for_index {
                    for (name, col_ids, col_types, idx_root) in &index_info {
                        let mut col_vals: Vec<Value> = Vec::with_capacity(col_ids.len());
                        for cid in col_ids {
                            col_vals.push(
                                crate::row::extract_column(bytes, *cid)?.unwrap_or(Value::Null),
                            );
                        }
                        if let Some(key) =
                            index::encode_composite_index_key(col_types, &col_vals, id)
                        {
                            let mut itree = IndexTreeWriter::new(guard, *idx_root);
                            itree.delete(&key)?;
                            idx_roots.push((name.clone(), itree.root_page()));
                        } else {
                            idx_roots.push((name.clone(), *idx_root));
                        }
                    }
                }
            }

            Ok((deleted, new_root, idx_roots))
        })?;

        let (deleted, new_root, idx_roots) = result;
        if !deleted {
            return Ok(false);
        }

        let delta = self.meta_deltas.entry(table.to_string()).or_insert(MetaDelta {
            root_page,
            row_count_delta: 0,
            next_rowid: current_next,
            table_id,
            cipher,
            index_roots: Vec::new(),
        });
        delta.root_page = new_root;
        delta.row_count_delta -= 1;
        delta.index_roots = idx_roots;

        Ok(true)
    }

    /// Find rows matching filters (scans all rows through dirty overlay, filters in memory).
    pub fn find(&mut self, table: &str, opts: FindOptions) -> Result<FindResult> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root_page = self.current_root(table, &state.meta);
        let col_names = state.meta.col_names.clone();
        let col_defs = state.meta.col_defs.clone();
        let col_name_to_id = state.meta.col_name_to_id.clone();
        drop(state);

        let all = self.with_guard(|guard| {
            let tree = BTreeWriter::new(guard, root_page);
            tree.scan_all_w()
        })?;

        // Filter in memory (full predicate: filters AND or-of-groups).
        // This path always scans every row, so there are no index/single-filter
        // fast paths to gate — `row_passes` simply incorporates or_groups.
        //
        // Short-circuit: when no sort is requested and we're not computing a total,
        // stop collecting once we have offset+limit matches.
        let can_short_circuit = opts.sort.is_empty() && !opts.include_total;
        let stop_after: Option<usize> = if can_short_circuit {
            opts.limit.map(|l| opts.offset.unwrap_or(0) as usize + l as usize)
        } else {
            None
        };
        let mut matching = Vec::new();
        for (_, bytes) in &all {
            if let Some(cap) = stop_after {
                if matching.len() >= cap {
                    break;
                }
            }
            if row_passes(&col_name_to_id, &col_defs, bytes, &opts.filters, &opts.or_groups) {
                matching.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
            }
        }

        let total = if opts.include_total { Some(matching.len() as u64) } else { None };

        // Sort
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

        // Pagination
        let skip = opts.offset.unwrap_or(0) as usize;
        let take = opts.limit.unwrap_or(u32::MAX) as usize;
        let rows: Vec<Row> = matching.into_iter().skip(skip).take(take).collect();

        Ok(FindResult { rows, total })
    }

    /// Resumable ordered cursor over the transaction overlay (read-your-writes).
    ///
    /// Produces the same result shape as [`BoogyDb::scan_batch`] but reads the
    /// **tx-current roots** (including uncommitted writes staged in the
    /// `private_dirty` overlay), so a cursor inside a transaction sees rows it
    /// has inserted or mutated but not yet committed.
    ///
    /// Because `BTreeWriter::scan_all_w` (the only overlay-aware scan) does a
    /// full leaf-chain walk without a seek, the after/limit windowing is applied
    /// in memory — the same tradeoff `find` makes vs. the committed read path.
    ///
    /// `order`:
    /// - [`ScanOrderKind::PrimaryKey`] — walks by rowid; `after.rowid` is the
    ///   exclusive resume bound; `after.bytes` is ignored.
    /// - [`ScanOrderKind::Index`] — walks in encoded-key order for the named
    ///   index; `after.bytes` is the exclusive resume key (same encoding as
    ///   [`BoogyDb::scan_batch`] produces).
    pub fn scan_batch(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
        order: ScanOrder,
        after: Option<ScanKey>,
        limit: u32,
    ) -> Result<ScanBatch> {
        if limit == 0 {
            return Ok(ScanBatch { rows: Vec::new(), last_key: None });
        }

        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root_page = self.current_root(table, &state.meta);
        let col_names = state.meta.col_names.clone();
        let col_defs = state.meta.col_defs.clone();
        let col_name_to_id = state.meta.col_name_to_id.clone();

        match order.kind {
            ScanOrderKind::PrimaryKey => {
                let after_rowid = after.as_ref().map(|k| k.rowid);
                let limit_usize = limit as usize;

                // Read all rows through the overlay (read-your-writes).
                let all = self.with_guard(|guard| {
                    let tree = BTreeWriter::new(guard, root_page);
                    tree.scan_all_w()
                })?;
                // scan_all_w yields rows in Asc rowid order (B+ tree leaf walk
                // is always left-to-right). Apply direction, after-bound, filter,
                // and limit in memory.
                use crate::filter::SortDir;
                let mut results: Vec<(u64, Vec<u8>)> = Vec::new();
                let mut last_rowid: Option<u64> = None;

                match order.dir {
                    SortDir::Asc => {
                        for (id, bytes) in &all {
                            // Exclusive lower bound.
                            if let Some(rid) = after_rowid {
                                if *id <= rid {
                                    continue;
                                }
                            }
                            if results.len() >= limit_usize {
                                // Batch full and another candidate exists — only
                                // emit a resume token when we know more rows remain.
                                last_rowid = results.last().map(|(rid, _)| *rid);
                                break;
                            }
                            if row_passes(&col_name_to_id, &col_defs, bytes, filters, or_groups) {
                                results.push((*id, bytes.clone()));
                            }
                        }
                        // If we exited the loop without breaking (scan exhausted),
                        // last_rowid stays None — no false resume token.
                    }
                    SortDir::Desc => {
                        for (id, bytes) in all.iter().rev() {
                            // Exclusive upper bound.
                            if let Some(rid) = after_rowid {
                                if *id >= rid {
                                    continue;
                                }
                            }
                            if results.len() >= limit_usize {
                                last_rowid = results.last().map(|(rid, _)| *rid);
                                break;
                            }
                            if row_passes(&col_name_to_id, &col_defs, bytes, filters, or_groups) {
                                results.push((*id, bytes.clone()));
                            }
                        }
                        // If we exited the loop without breaking (scan exhausted),
                        // last_rowid stays None — no false resume token.
                    }
                }

                let mut rows = Vec::with_capacity(results.len());
                for (_, bytes) in &results {
                    rows.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
                }
                let last_key = last_rowid.map(|rowid| ScanKey { bytes: Vec::new(), rowid });
                Ok(ScanBatch { rows, last_key })
            }

            ScanOrderKind::Index(ref idx_name) => {
                // Look up the index metadata.
                let idx_meta = state
                    .meta
                    .find_index(idx_name)
                    .ok_or_else(|| {
                        BoogyError::SchemaMismatch(format!(
                            "scan_batch: no index named '{idx_name}' on table '{table}'"
                        ))
                    })?
                    .clone();

                // Resolve column ids + types for the index key encoder.
                let mut col_ids: Vec<u16> = Vec::with_capacity(idx_meta.columns.len());
                let mut col_types: Vec<crate::value::Type> =
                    Vec::with_capacity(idx_meta.columns.len());
                for idx_col in &idx_meta.columns {
                    match (
                        state.meta.col_name_to_id.get(idx_col).copied(),
                        state
                            .meta
                            .columns
                            .iter()
                            .find(|c| &c.name == idx_col)
                            .map(|c| c.col_type),
                    ) {
                        (Some(cid), Some(ct)) => {
                            col_ids.push(cid);
                            col_types.push(ct);
                        }
                        _ => {
                            return Err(BoogyError::SchemaMismatch(format!(
                                "scan_batch(tx): index '{idx_name}' column '{idx_col}' not found on table '{table}'"
                            )));
                        }
                    }
                }
                drop(state);

                // Read all rows through the overlay.
                let all = self.with_guard(|guard| {
                    let tree = BTreeWriter::new(guard, root_page);
                    tree.scan_all_w()
                })?;

                // For each row: apply predicate, encode the index key.
                let mut keyed: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (encoded_key, row_bytes)
                for (_, bytes) in &all {
                    if !row_passes(&col_name_to_id, &col_defs, &bytes, filters, or_groups) {
                        continue;
                    }
                    let rowid = crate::row::extract_id(bytes)?;
                    // Encode index key (returns None if any indexed column is Null).
                    let mut col_vals: Vec<crate::value::Value> =
                        Vec::with_capacity(col_ids.len());
                    for cid in &col_ids {
                        col_vals.push(
                            crate::row::extract_column(bytes, *cid)?.unwrap_or(Value::Null),
                        );
                    }
                    if let Some(key) =
                        index::encode_composite_index_key(&col_types, &col_vals, rowid)
                    {
                        keyed.push((key, bytes.clone()));
                    }
                    // Rows with Null in any index column are skipped (not indexed).
                }

                // Sort by encoded key (index order — same ordering as the index tree).
                use crate::filter::SortDir;
                keyed.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

                let after_bytes: Option<&[u8]> = after.as_ref().map(|k| k.bytes.as_slice());
                let limit_usize = limit as usize;
                let mut rows = Vec::new();
                // last_key tracks the last *collected* key (the limit-th row).
                // It is set to None when the scan is exhausted (no overflow key
                // was seen), so the caller receives no false resume token.
                let mut last_key: Option<ScanKey> = None;
                let mut has_more = false;

                let iter: Box<dyn Iterator<Item = &(Vec<u8>, Vec<u8>)>> = match order.dir {
                    SortDir::Asc => Box::new(keyed.iter()),
                    SortDir::Desc => Box::new(keyed.iter().rev()),
                };

                for (key, bytes) in iter {
                    // Exclusive bound.
                    if let Some(ab) = after_bytes {
                        match order.dir {
                            SortDir::Asc => {
                                if key.as_slice() <= ab {
                                    continue;
                                }
                            }
                            SortDir::Desc => {
                                if key.as_slice() >= ab {
                                    continue;
                                }
                            }
                        }
                    }
                    if rows.len() < limit_usize {
                        // Collect this row and record it as the current last key.
                        rows.push(Row::from_raw(bytes, col_names.clone(), col_defs.clone())?);
                        last_key = Some(ScanKey {
                            bytes: key.clone(),
                            rowid: rowid_from_index_key(key),
                        });
                    } else {
                        // Batch is full and an overflow key exists — more pages remain.
                        has_more = true;
                        break;
                    }
                }

                // If no overflow key was seen the scan is exhausted: clear the
                // resume token so the caller gets None and stops paging.
                if !has_more {
                    last_key = None;
                }
                Ok(ScanBatch { rows, last_key })
            }
        }
    }

    /// Count rows matching filters (scans all rows through dirty overlay).
    ///
    /// Back-compatible signature. For OR-of-AND counting use [`count_with`].
    pub fn count(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        self.count_with(table, filters, &[])
    }

    /// Count rows matching the full predicate
    /// `ALL(filters) AND (or_groups.is_empty() OR ANY(group: ALL(group)))`,
    /// through the dirty overlay. Mirrors [`BoogyDb::count_with`].
    pub fn count_with(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
    ) -> Result<u64> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root_page = self.current_root(table, &state.meta);

        // Cached-count fast path only valid for a truly unfiltered count.
        if filters.is_empty() && or_groups.is_empty() {
            // Use cached count + delta
            let base_count = state.meta.row_count;
            let delta_adj = self.meta_deltas
                .get(table)
                .map(|d| d.row_count_delta)
                .unwrap_or(0);
            return Ok((base_count as i64 + delta_adj) as u64);
        }

        let col_name_to_id = state.meta.col_name_to_id.clone();
        let col_defs = state.meta.col_defs.clone();
        drop(state);

        let all = self.with_guard(|guard| {
            let tree = BTreeWriter::new(guard, root_page);
            tree.scan_all_w()
        })?;

        let mut count = 0u64;
        for (_, bytes) in &all {
            if row_passes(&col_name_to_id, &col_defs, bytes, filters, or_groups) {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Insert multiple rows in a single transaction.
    pub fn insert_many(&mut self, table: &str, rows: &[Vec<(&str, Value)>]) -> Result<Vec<u64>> {
        let mut ids = Vec::with_capacity(rows.len());
        for row_data in rows {
            let id = self.insert(table, row_data)?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Update all rows matching filters.
    pub fn update_where(
        &mut self,
        table: &str,
        filters: &[Filter],
        fields: &[(&str, Value)],
    ) -> Result<u64> {
        // Find matching row IDs first
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;
        BoogyDb::enforce_index_types(&state.meta, fields)?;

        let root_page = self.current_root(table, &state.meta);
        let col_name_to_id = state.meta.col_name_to_id.clone();
        let col_defs = state.meta.col_defs.clone();
        drop(state);

        let all = self.with_guard(|guard| {
            let tree = BTreeWriter::new(guard, root_page);
            tree.scan_all_w()
        })?;

        let matching_ids: Vec<u64> = all
            .iter()
            .filter(|(_, bytes)| {
                filters.iter().all(|f| filter_matches_row(&col_name_to_id, &col_defs, bytes, f))
            })
            .map(|(id, _)| *id)
            .collect();

        let mut count = 0u64;
        for id in matching_ids {
            if self.update(table, id, fields)? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Delete all rows matching filters.
    pub fn delete_where(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        let table_state = self.table_state(table)?;
        let state = table_state.read().unwrap();
        BoogyDb::check_table_accessible(&state.meta, table)?;

        let root_page = self.current_root(table, &state.meta);
        let col_name_to_id = state.meta.col_name_to_id.clone();
        let col_defs = state.meta.col_defs.clone();
        drop(state);

        let all = self.with_guard(|guard| {
            let tree = BTreeWriter::new(guard, root_page);
            tree.scan_all_w()
        })?;

        let matching_ids: Vec<u64> = all
            .iter()
            .filter(|(_, bytes)| {
                filters.iter().all(|f| filter_matches_row(&col_name_to_id, &col_defs, bytes, f))
            })
            .map(|(id, _)| *id)
            .collect();

        let mut count = 0u64;
        for id in matching_ids {
            if self.delete(table, id)? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Commit the transaction: publish all dirty pages atomically and apply metadata.
    pub fn commit(mut self) -> Result<()> {
        self.committed = true;

        if self.private_dirty.is_empty() && self.meta_deltas.is_empty() {
            return Ok(()); // nothing to commit
        }

        let durability = self.db.durability();

        // Publish all dirty pages atomically
        let mut guard = self.db.file.begin_write();
        let pages = std::mem::take(&mut self.private_dirty);
        guard.inject_dirty(pages);
        guard.set_new_page_count(self.new_page_count);
        let after_images = guard.commit()?;

        // Build a page -> (table_id, cipher) mapping so WAL entries are
        // written with the correct table_id and encrypted when needed.
        // Also register page ciphers for sync_all.
        let mut page_table_map: StdHashMap<u32, (u32, Option<&crate::crypto::Cipher>)> = StdHashMap::new();
        for delta in self.meta_deltas.values() {
            let cipher_ref = delta.cipher.as_ref();
            for (page_no, _) in &after_images {
                // Associate each dirty page with this table's id and cipher.
                // In a multi-table transaction, pages may belong to different
                // tables; we conservatively apply the delta whose pages were
                // created during its operations. Since we cannot cheaply track
                // page ownership, we map every page to the first delta that
                // claims it. Pages from different tables are disjoint in
                // practice because each table has its own B+ tree.
                page_table_map.entry(*page_no)
                    .or_insert((delta.table_id, cipher_ref));
            }
            // Register page ciphers for encrypted tables so sync_all encrypts them.
            if let Some(c) = cipher_ref {
                let arc = Arc::new(c.clone());
                for (page_no, _) in &after_images {
                    self.db.file.register_page_cipher(*page_no, Arc::clone(&arc));
                }
            }
        }

        // WAL
        match durability {
            Durability::Immediate | Durability::Normal => {
                let mut wal = self.db.wal.lock().unwrap();
                for (page_no, data) in &after_images {
                    let (table_id, cipher) = page_table_map.get(page_no)
                        .copied()
                        .unwrap_or((0, None));
                    let write_data = if let Some(c) = cipher {
                        c.encrypt_page(&data[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE])?
                    } else {
                        *data
                    };
                    wal.append_page_image(table_id, *page_no, &write_data)?;
                }
                if matches!(durability, Durability::Immediate) {
                    wal.sync()?;
                }
            }
            Durability::None => {}
        }

        // Apply metadata deltas to actual TableMeta
        for (table_name, delta) in &self.meta_deltas {
            if let Some(table_state) = {
                let tables = self.db.tables.read().unwrap();
                tables.get(table_name).cloned()
            } {
                let mut state = table_state.write().unwrap();
                state.meta.root_page = delta.root_page;
                state.meta.row_count = (state.meta.row_count as i64 + delta.row_count_delta) as u64;
                if delta.next_rowid > state.meta.next_rowid {
                    state.meta.next_rowid = delta.next_rowid;
                }
                // Apply index root pages from delta (keyed by index name).
                for (name, root) in &delta.index_roots {
                    if let Some(idx) = state
                        .meta
                        .indexes
                        .iter_mut()
                        .find(|i| &i.name == name)
                    {
                        idx.root_page = *root;
                    }
                }
            }
        }

        Ok(())
    }
}

impl Drop for AcidTransaction<'_> {
    fn drop(&mut self) {
        // If not committed, everything is simply dropped.
        // private_dirty pages discarded, meta_deltas discarded.
        // Database state is unchanged — clean rollback.
    }
}

// ---------------------------------------------------------------------------
// Transaction — enum wrapping Light and Acid transactions
// ---------------------------------------------------------------------------

#[allow(private_interfaces)]
pub enum Transaction<'a> {
    Light(LightTransaction<'a>),
    Acid(AcidTransaction<'a>),
}

impl<'a> Transaction<'a> {
    /// Commit the transaction.
    pub fn commit(self) -> Result<()> {
        match self {
            Transaction::Light(mut t) => {
                t.committed = true;
                t.db.flush_transaction()
            }
            Transaction::Acid(t) => t.commit(),
        }
    }

    pub fn insert(&mut self, table: &str, data: &[(&str, Value)]) -> Result<u64> {
        match self {
            Transaction::Light(t) => t.db.insert(table, data),
            Transaction::Acid(t) => t.insert(table, data),
        }
    }

    pub fn insert_with_id(&mut self, table: &str, rowid: u64, data: &[(&str, Value)]) -> Result<()> {
        match self {
            Transaction::Light(t) => t.db.insert_with_id(table, rowid, data),
            Transaction::Acid(t) => t.insert_with_id(table, rowid, data),
        }
    }

    pub fn get(&mut self, table: &str, id: u64) -> Result<Option<Row>> {
        match self {
            Transaction::Light(t) => t.db.get(table, id),
            Transaction::Acid(t) => t.get(table, id),
        }
    }

    pub fn update(&mut self, table: &str, id: u64, fields: &[(&str, Value)]) -> Result<bool> {
        match self {
            Transaction::Light(t) => t.db.update(table, id, fields),
            Transaction::Acid(t) => t.update(table, id, fields),
        }
    }

    pub fn delete(&mut self, table: &str, id: u64) -> Result<bool> {
        match self {
            Transaction::Light(t) => t.db.delete(table, id),
            Transaction::Acid(t) => t.delete(table, id),
        }
    }

    pub fn find(&mut self, table: &str, opts: FindOptions) -> Result<FindResult> {
        match self {
            Transaction::Light(t) => t.db.find(table, opts),
            Transaction::Acid(t) => t.find(table, opts),
        }
    }

    pub fn count(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        match self {
            Transaction::Light(t) => t.db.count(table, filters),
            Transaction::Acid(t) => t.count(table, filters),
        }
    }

    pub fn count_with(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
    ) -> Result<u64> {
        match self {
            Transaction::Light(t) => t.db.count_with(table, filters, or_groups),
            Transaction::Acid(t) => t.count_with(table, filters, or_groups),
        }
    }

    pub fn insert_many(&mut self, table: &str, rows: &[Vec<(&str, Value)>]) -> Result<Vec<u64>> {
        match self {
            Transaction::Light(t) => t.db.insert_many(table, rows),
            Transaction::Acid(t) => t.insert_many(table, rows),
        }
    }

    pub fn update_where(&mut self, table: &str, filters: &[Filter], fields: &[(&str, Value)]) -> Result<u64> {
        match self {
            Transaction::Light(t) => t.db.update_where(table, filters, fields),
            Transaction::Acid(t) => t.update_where(table, filters, fields),
        }
    }

    pub fn delete_where(&mut self, table: &str, filters: &[Filter]) -> Result<u64> {
        match self {
            Transaction::Light(t) => t.db.delete_where(table, filters),
            Transaction::Acid(t) => t.delete_where(table, filters),
        }
    }

    /// Resumable ordered cursor. For an ACID transaction this reads through
    /// the tx overlay (read-your-writes). For a light transaction it reads
    /// committed state (no overlay).
    pub fn scan_batch(
        &mut self,
        table: &str,
        filters: &[Filter],
        or_groups: &[Vec<Filter>],
        order: ScanOrder,
        after: Option<ScanKey>,
        limit: u32,
    ) -> Result<ScanBatch> {
        match self {
            Transaction::Light(t) => t.db.scan_batch(table, filters, or_groups, order, after, limit),
            Transaction::Acid(t) => t.scan_batch(table, filters, or_groups, order, after, limit),
        }
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
    fn test_index_meta_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
            db.create_index("t", "idx_v", "v").unwrap();
            db.insert("t", &[("v", Value::Text("hello".into()))]).unwrap();
        }
        {
            let db = BoogyDb::open(&path).unwrap();
            // IndexMeta round-trips through the system page: name, columns, unique.
            {
                let tables = db.tables.read().unwrap();
                let state = tables.get("t").unwrap().read().unwrap();
                assert_eq!(state.meta.indexes.len(), 1);
                let idx = &state.meta.indexes[0];
                assert_eq!(idx.name, "idx_v");
                assert_eq!(idx.columns, vec!["v".to_string()]);
                assert!(!idx.unique);
            }
            // And the index is still functional after reopen.
            let opts = crate::filter::FindOptions {
                filters: vec![crate::filter::Filter::eq("v", "hello")],
                include_total: true,
                ..Default::default()
            };
            let result = db.find("t", opts).unwrap();
            assert_eq!(result.total, Some(1));
            assert_eq!(result.rows.len(), 1);
        }
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

    // --- or_groups (OR-of-AND filter groups) ---

    /// Helper: fetch the sorted list of `id` integer-column values from a result.
    fn ids(result: &FindResult) -> Vec<i64> {
        let mut v: Vec<i64> = result
            .rows
            .iter()
            .map(|r| match r.get("id").unwrap() {
                Value::Integer(i) => i,
                other => panic!("expected integer id, got {:?}", other),
            })
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn test_or_groups_empty_is_back_compat() {
        // Empty or_groups must behave exactly like a plain filters-only query.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("id", Type::Integer), ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..5 {
            db.insert("t", &[("id", Value::Integer(i)), ("v", Value::Integer(i))]).unwrap();
        }
        let with_empty = db.find("t", FindOptions {
            filters: vec![Filter::gt("v", 1i64)],
            or_groups: vec![],
            include_total: true,
            ..Default::default()
        }).unwrap();
        let baseline = db.find("t", FindOptions {
            filters: vec![Filter::gt("v", 1i64)],
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&with_empty), ids(&baseline));
        assert_eq!(with_empty.total, baseline.total);
        assert_eq!(ids(&with_empty), vec![2, 3, 4]);
    }

    #[test]
    fn test_or_groups_pure_or() {
        // filters=[], or_groups=[[a],[b]] returns rows matching a OR b only.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("id", Type::Integer), ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..6 {
            db.insert("t", &[("id", Value::Integer(i)), ("v", Value::Integer(i))]).unwrap();
        }
        // v == 1 OR v == 4
        let result = db.find("t", FindOptions {
            filters: vec![],
            or_groups: vec![
                vec![Filter::eq("v", 1i64)],
                vec![Filter::eq("v", 4i64)],
            ],
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&result), vec![1, 4]);
        assert_eq!(result.total, Some(2));
    }

    #[test]
    fn test_or_groups_and_prefix_with_or_keyset_shape() {
        // The composite keyset shape:
        //   filters=[k eq "x"]
        //   or_groups=[[score lt 5], [score eq 5, id lt 10]]
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[
            ColumnDef::new("id", Type::Integer),
            ColumnDef::new("k", Type::Text),
            ColumnDef::new("score", Type::Integer),
        ]).unwrap();
        // Rows we want to assert precisely:
        let insert = |db: &BoogyDb, id: i64, k: &str, score: i64| {
            db.insert("t", &[
                ("id", Value::Integer(id)),
                ("k", Value::Text(k.into())),
                ("score", Value::Integer(score)),
            ]).unwrap();
        };
        insert(&db, 1, "x", 3);   // score < 5 -> in
        insert(&db, 2, "x", 4);   // score < 5 -> in
        insert(&db, 9, "x", 5);   // score == 5, id < 10 -> in (boundary included)
        insert(&db, 10, "x", 5);  // score == 5, id == 10 -> excluded (id not < 10)
        insert(&db, 11, "x", 6);  // score == 6 -> excluded
        insert(&db, 3, "y", 3);   // wrong k -> excluded by AND-prefix even though score<5
        insert(&db, 4, "x", 5);   // score == 5, id < 10 -> in

        let result = db.find("t", FindOptions {
            filters: vec![Filter::eq("k", "x")],
            or_groups: vec![
                vec![Filter::lt("score", 5i64)],
                vec![Filter::eq("score", 5i64), Filter::lt("id", 10i64)],
            ],
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&result), vec![1, 2, 4, 9]);
        assert_eq!(result.total, Some(4));
    }

    #[test]
    fn test_or_groups_with_sort_and_limit() {
        // Sort desc + limit returns the top N of the OR-matched set.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("id", Type::Integer), ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..10 {
            db.insert("t", &[("id", Value::Integer(i)), ("v", Value::Integer(i))]).unwrap();
        }
        // OR-match v < 2 (ids 0,1) OR v > 6 (ids 7,8,9) -> matched set {0,1,7,8,9}
        let result = db.find("t", FindOptions {
            filters: vec![],
            or_groups: vec![
                vec![Filter::lt("v", 2i64)],
                vec![Filter::gt("v", 6i64)],
            ],
            sort: vec![crate::filter::Sort::desc("v")],
            limit: Some(3),
            ..Default::default()
        }).unwrap();
        // Top 3 by v desc of {0,1,7,8,9} = 9,8,7
        let got: Vec<i64> = result.rows.iter().map(|r| match r.get("v").unwrap() {
            Value::Integer(i) => i,
            o => panic!("{:?}", o),
        }).collect();
        assert_eq!(got, vec![9, 8, 7]);
    }

    #[test]
    fn test_or_groups_include_total_counts_full_matched_set() {
        // include_total reflects the whole OR-matched set, not just the page.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("id", Type::Integer), ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..10 {
            db.insert("t", &[("id", Value::Integer(i)), ("v", Value::Integer(i))]).unwrap();
        }
        // matched set {0,1,7,8,9} = 5 rows, page limited to 2
        let result = db.find("t", FindOptions {
            filters: vec![],
            or_groups: vec![
                vec![Filter::lt("v", 2i64)],
                vec![Filter::gt("v", 6i64)],
            ],
            sort: vec![crate::filter::Sort::asc("v")],
            limit: Some(2),
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.total, Some(5));
    }

    #[test]
    fn test_or_groups_multi_filter_group_partial_vs_full_match() {
        // A group of multiple filters passes only when ALL its filters match.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[
            ColumnDef::new("id", Type::Integer),
            ColumnDef::new("a", Type::Integer),
            ColumnDef::new("b", Type::Integer),
        ]).unwrap();
        let ins = |id: i64, a: i64, b: i64| {
            db.insert("t", &[
                ("id", Value::Integer(id)),
                ("a", Value::Integer(a)),
                ("b", Value::Integer(b)),
            ]).unwrap();
        };
        ins(1, 1, 1); // a matches, b doesn't -> group fails
        ins(2, 1, 2); // a matches AND b matches -> group passes
        ins(3, 9, 2); // a doesn't match -> group fails
        // single group [a == 1, b == 2]
        let result = db.find("t", FindOptions {
            filters: vec![],
            or_groups: vec![vec![Filter::eq("a", 1i64), Filter::eq("b", 2i64)]],
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&result), vec![2]);
        assert_eq!(result.total, Some(1));
    }

    #[test]
    fn test_or_groups_bypasses_index_fast_path() {
        // With an index present on the AND-prefix column, the index fast path
        // must be bypassed when or_groups is non-empty, and the OR clause still
        // applied correctly (regression guard against the silent-wrong-rows bug).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[
            ColumnDef::new("id", Type::Integer),
            ColumnDef::new("k", Type::Text),
            ColumnDef::new("v", Type::Integer),
        ]).unwrap();
        db.create_index("t", "idx_k", "k").unwrap();
        let ins = |id: i64, k: &str, v: i64| {
            db.insert("t", &[
                ("id", Value::Integer(id)),
                ("k", Value::Text(k.into())),
                ("v", Value::Integer(v)),
            ]).unwrap();
        };
        ins(1, "x", 1);
        ins(2, "x", 2);
        ins(3, "x", 9);
        ins(4, "y", 1); // wrong k, excluded by AND-prefix
        // filters=[k eq "x"] (indexed Eq), or_groups=[[v eq 1],[v eq 9]]
        let result = db.find("t", FindOptions {
            filters: vec![Filter::eq("k", "x")],
            or_groups: vec![
                vec![Filter::eq("v", 1i64)],
                vec![Filter::eq("v", 9i64)],
            ],
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&result), vec![1, 3]);
        assert_eq!(result.total, Some(2));
    }

    #[test]
    fn test_or_groups_bypasses_single_filter_fast_path() {
        // A single AND-prefix filter would normally hit the scan_filtered path;
        // with or_groups present it must fall through to the full predicate.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[
            ColumnDef::new("id", Type::Integer),
            ColumnDef::new("k", Type::Integer),
            ColumnDef::new("v", Type::Integer),
        ]).unwrap();
        let ins = |id: i64, k: i64, v: i64| {
            db.insert("t", &[
                ("id", Value::Integer(id)),
                ("k", Value::Integer(k)),
                ("v", Value::Integer(v)),
            ]).unwrap();
        };
        ins(1, 1, 10);
        ins(2, 1, 20);
        ins(3, 1, 30);
        ins(4, 2, 10); // k != 1 excluded
        // filters=[k eq 1] (single, non-IN), or_groups=[[v eq 10],[v eq 30]]
        let result = db.find("t", FindOptions {
            filters: vec![Filter::eq("k", 1i64)],
            or_groups: vec![
                vec![Filter::eq("v", 10i64)],
                vec![Filter::eq("v", 30i64)],
            ],
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&result), vec![1, 3]);
    }

    #[test]
    fn test_count_with_or_groups() {
        // count_with applies the full predicate; plain count is unaffected.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("id", Type::Integer), ColumnDef::new("v", Type::Integer)]).unwrap();
        for i in 0..10 {
            db.insert("t", &[("id", Value::Integer(i)), ("v", Value::Integer(i))]).unwrap();
        }
        // v < 2 OR v > 6 -> {0,1,7,8,9} = 5
        let or_groups = vec![
            vec![Filter::lt("v", 2i64)],
            vec![Filter::gt("v", 6i64)],
        ];
        assert_eq!(db.count_with("t", &[], &or_groups).unwrap(), 5);
        // AND-prefix + OR: v even AND (v<2 OR v>6) -> {0,8} = 2
        let with_prefix = db.count_with(
            "t",
            &[Filter::eq("v", 8i64)],
            &or_groups,
        ).unwrap();
        // 8 satisfies prefix and group2 -> 1
        assert_eq!(with_prefix, 1);
        // plain count unaffected by or_groups (no groups passed)
        assert_eq!(db.count("t", &[]).unwrap(), 10);
        assert_eq!(db.count("t", &[Filter::eq("v", 5i64)]).unwrap(), 1);
    }

    #[test]
    fn test_or_groups_acid_transaction_find() {
        // The transactional (AcidTransaction) find path must apply or_groups too,
        // including over uncommitted overlay rows.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_acid(true); // ensure begin() yields an AcidTransaction (overlay path)
        db.create_table("t", &[ColumnDef::new("id", Type::Integer), ColumnDef::new("v", Type::Integer)]).unwrap();
        // committed rows
        for i in 0..4 {
            db.insert("t", &[("id", Value::Integer(i)), ("v", Value::Integer(i))]).unwrap();
        }
        let mut tx = db.begin().unwrap();
        assert!(matches!(&tx, Transaction::Acid(_)), "test must exercise AcidTransaction::find");
        // uncommitted overlay rows
        tx.insert("t", &[("id", Value::Integer(8)), ("v", Value::Integer(8))]).unwrap();
        tx.insert("t", &[("id", Value::Integer(9)), ("v", Value::Integer(9))]).unwrap();
        // v < 1 OR v > 7 -> {0, 8, 9}
        let result = tx.find("t", FindOptions {
            filters: vec![],
            or_groups: vec![
                vec![Filter::lt("v", 1i64)],
                vec![Filter::gt("v", 7i64)],
            ],
            include_total: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(ids(&result), vec![0, 8, 9]);
        assert_eq!(result.total, Some(3));
        // count_with through the tx
        assert_eq!(
            tx.count_with("t", &[], &[
                vec![Filter::lt("v", 1i64)],
                vec![Filter::gt("v", 7i64)],
            ]).unwrap(),
            3
        );
        tx.commit().unwrap();
    }

    #[test]
    fn test_create_composite_index_and_find_by_both_columns() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
        db.insert("edges", &[("user_a", "a".into()), ("user_b", "b".into()), ("n", 1i64.into())]).unwrap();
        db.insert("edges", &[("user_a", "a".into()), ("user_b", "c".into()), ("n", 2i64.into())]).unwrap();
        // find by both key columns returns exactly the one row
        let res = db.find("edges", FindOptions {
            filters: vec![Filter::eq("user_a", "a"), Filter::eq("user_b", "b")],
            ..Default::default()
        }).unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    #[test]
    fn test_unique_composite_index_rejects_duplicate_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
        db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",1i64.into())]).unwrap();
        let dup = db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",9i64.into())]);
        assert!(matches!(dup, Err(BoogyError::UniqueViolation { .. })));
        // a different pair is fine
        assert!(db.insert("edges", &[("user_a","a".into()),("user_b","c".into()),("n",1i64.into())]).is_ok());
        // a rejected duplicate must leave NO partial state: only the two
        // accepted rows exist, and re-inserting the unique pair still rejects.
        let all = db.find("edges", FindOptions::default()).unwrap();
        assert_eq!(all.rows.len(), 2);
        let dup2 = db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",7i64.into())]);
        assert!(matches!(dup2, Err(BoogyError::UniqueViolation { .. })));
        assert_eq!(db.find("edges", FindOptions::default()).unwrap().rows.len(), 2);
    }

    #[test]
    fn test_nonunique_composite_index_allows_duplicate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[
            ColumnDef::new("x", Type::Integer),
            ColumnDef::new("y", Type::Integer),
        ]).unwrap();
        db.create_index_ex("t", "by_xy", &["x", "y"], false).unwrap();
        db.insert("t", &[("x",1i64.into()),("y",2i64.into())]).unwrap();
        assert!(db.insert("t", &[("x",1i64.into()),("y",2i64.into())]).is_ok());
    }

    // ----- ACID-path index correctness (the path the host actually runs) -----
    // The host opens every per-API store with set_acid(true), so BoogyDb::insert
    // routes through AcidTransaction::insert. These mirror the non-ACID tests
    // above but assert the enforcement happens on the ACID write path.

    #[test]
    fn test_acid_unique_composite_index_rejects_duplicate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_acid(true);
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
        db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",1i64.into())]).unwrap();
        let dup = db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",9i64.into())]);
        assert!(matches!(dup, Err(BoogyError::UniqueViolation { .. })));
        // a different pair is fine
        assert!(db.insert("edges", &[("user_a","a".into()),("user_b","c".into()),("n",1i64.into())]).is_ok());
        // the rejected duplicate left NO partial state: exactly the two
        // accepted rows exist.
        let all = db.find("edges", FindOptions::default()).unwrap();
        assert_eq!(all.rows.len(), 2);
        // and re-inserting the unique pair still rejects (no rowid leak / no
        // half-applied index entry).
        let dup2 = db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",7i64.into())]);
        assert!(matches!(dup2, Err(BoogyError::UniqueViolation { .. })));
        assert_eq!(db.find("edges", FindOptions::default()).unwrap().rows.len(), 2);
    }

    #[test]
    fn test_acid_composite_index_find_by_both_columns() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_acid(true);
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
        db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",1i64.into())]).unwrap();
        db.insert("edges", &[("user_a","a".into()),("user_b","c".into()),("n",2i64.into())]).unwrap();
        // filter on BOTH index columns returns exactly the one matching row.
        let res = db.find("edges", FindOptions {
            filters: vec![Filter::eq("user_a", "a"), Filter::eq("user_b", "b")],
            ..Default::default()
        }).unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("user_b"), Some(Value::Text("b".to_string())));
    }

    #[test]
    fn test_acid_insert_many_rejects_duplicate_within_same_tx() {
        // Two inserts of the same composite key within ONE AcidTransaction:
        // the committed-tree scan can't see the first (uncommitted) insert, so
        // this exercises the within-tx duplicate set. The whole tx must reject
        // and, being dropped without commit, leave the table empty.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_acid(true);
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();

        let res = db.insert_many("edges", &[
            vec![("user_a","a".into()),("user_b","b".into()),("n",1i64.into())],
            vec![("user_a","a".into()),("user_b","b".into()),("n",2i64.into())],
        ]);
        assert!(matches!(res, Err(BoogyError::UniqueViolation { .. })));
        // The aborted multi-insert tx committed nothing.
        assert_eq!(db.find("edges", FindOptions::default()).unwrap().rows.len(), 0);

        // A non-conflicting batch then commits cleanly.
        let ok = db.insert_many("edges", &[
            vec![("user_a","a".into()),("user_b","b".into()),("n",1i64.into())],
            vec![("user_a","a".into()),("user_b","c".into()),("n",2i64.into())],
        ]).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(db.find("edges", FindOptions::default()).unwrap().rows.len(), 2);
    }

    // ----- scan_batch (cursor primitive) -----

    #[test]
    fn test_scan_batch_tiles_primary_key_order_no_gaps_or_dups() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        for v in 0..25i64 { db.insert("t", &[("v", v.into())]).unwrap(); }
        // Page through in primary-key (rowid) order in batches of 10; concatenation
        // must equal the full ordered set exactly once.
        let mut seen = Vec::new();
        let mut after: Option<ScanKey> = None;
        loop {
            let b = db.scan_batch("t", &[], &[], ScanOrder::primary_key(SortDir::Asc), after.clone(), 10).unwrap();
            if b.rows.is_empty() { break; }
            for r in &b.rows { seen.push(r.get("v").unwrap().clone()); }
            after = b.last_key;
            if after.is_none() { break; }
        }
        assert_eq!(seen.len(), 25);
        // strictly increasing rowids ⇒ values 0..25 in order
        let got: Vec<i64> = seen.iter().map(|v| if let Value::Integer(n)=v {*n} else {-1}).collect();
        assert_eq!(got, (0..25).collect::<Vec<_>>());
    }

    #[test]
    fn test_scan_batch_with_filter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        for v in 0..20i64 { db.insert("t", &[("v", v.into())]).unwrap(); }
        // Only v >= 10, batches of 3.
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let b = db.scan_batch("t", &[Filter::ge("v", 10i64)], &[], ScanOrder::primary_key(SortDir::Asc), after.clone(), 3).unwrap();
            if b.rows.is_empty() { break; }
            for r in &b.rows { if let Some(Value::Integer(n)) = r.get("v") { seen.push(n); } }
            after = b.last_key;
            if after.is_none() { break; }
        }
        assert_eq!(seen, (10..20).collect::<Vec<_>>());
    }

    #[test]
    fn test_scan_batch_index_order_desc() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("score", Type::Integer)]).unwrap();
        db.create_index("t", "by_score", "score").unwrap();
        for s in [3i64,1,4,1,5,9,2,6] { db.insert("t", &[("score", s.into())]).unwrap(); }
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let b = db.scan_batch("t", &[], &[], ScanOrder::index("by_score", SortDir::Desc), after.clone(), 3).unwrap();
            if b.rows.is_empty() { break; }
            for r in &b.rows { if let Some(Value::Integer(n)) = r.get("score") { seen.push(n); } }
            after = b.last_key;
            if after.is_none() { break; }
        }
        assert_eq!(seen, vec![9,6,5,4,3,2,1,1]); // descending, dups preserved
    }

    #[test]
    fn test_scan_batch_no_index_for_order_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
        db.insert("t", &[("v", 1i64.into())]).unwrap();
        let r = db.scan_batch("t", &[], &[], ScanOrder::index("nonexistent", SortDir::Asc), None, 10);
        assert!(r.is_err());
    }

    // ----- upsert_increment (atomic keyed counter) -----

    #[test]
    fn test_upsert_increment_inserts_then_increments_integer() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
            ColumnDef::new("updated_at", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
        // first call inserts n=1
        db.upsert_increment("edges", &[("user_a", "a".into()), ("user_b", "b".into())], "n", Value::Integer(1),
            &[("updated_at", Value::Integer(100))]).unwrap();
        // second call increments to n=3 (delta 2) and updates set col
        db.upsert_increment("edges", &[("user_a", "a".into()), ("user_b", "b".into())], "n", Value::Integer(2),
            &[("updated_at", Value::Integer(200))]).unwrap();
        let res = db.find("edges", FindOptions {
            filters: vec![Filter::eq("user_a", "a"), Filter::eq("user_b", "b")], ..Default::default() }).unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("n"), Some(Value::Integer(3)));
        assert_eq!(res.rows[0].get("updated_at"), Some(Value::Integer(200)));
    }

    #[test]
    fn test_upsert_increment_real_counter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("w", &[
            ColumnDef::new("k", Type::Text),
            ColumnDef::new("weight", Type::Real),
        ]).unwrap();
        db.create_index_ex("w", "by_k", &["k"], true).unwrap();
        db.upsert_increment("w", &[("k", "x".into())], "weight", Value::Real(0.5), &[]).unwrap();
        db.upsert_increment("w", &[("k", "x".into())], "weight", Value::Real(0.25), &[]).unwrap();
        let res = db.find("w", FindOptions { filters: vec![Filter::eq("k", "x")], ..Default::default() }).unwrap();
        if let Some(Value::Real(f)) = res.rows[0].get("weight") { assert!((f - 0.75).abs() < 1e-9); } else { panic!(); }
    }

    // The host opens every per-API store with set_acid(true); upsert_increment
    // must be atomic + type-correct on the ACID write path it actually runs on.
    #[test]
    fn test_acid_upsert_increment_inserts_then_increments_integer() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.set_acid(true);
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("n", Type::Integer),
            ColumnDef::new("updated_at", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
        db.upsert_increment("edges", &[("user_a", "a".into()), ("user_b", "b".into())], "n", Value::Integer(1),
            &[("updated_at", Value::Integer(100))]).unwrap();
        db.upsert_increment("edges", &[("user_a", "a".into()), ("user_b", "b".into())], "n", Value::Integer(2),
            &[("updated_at", Value::Integer(200))]).unwrap();
        let res = db.find("edges", FindOptions {
            filters: vec![Filter::eq("user_a", "a"), Filter::eq("user_b", "b")], ..Default::default() }).unwrap();
        // Exactly one row — the increment updated in place rather than inserting
        // a second row (which the unique index would have rejected anyway).
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("n"), Some(Value::Integer(3)));
        assert_eq!(res.rows[0].get("updated_at"), Some(Value::Integer(200)));
    }

    #[test]
    fn test_upsert_increment_rejects_non_numeric_delta() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[
            ColumnDef::new("k", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).unwrap();
        let r = db.upsert_increment("t", &[("k", "x".into())], "n", Value::Text("nope".into()), &[]);
        assert!(matches!(r, Err(BoogyError::SchemaMismatch(_))));
    }

    // ----- upsert (general set-semantics) -----

    #[test]
    fn test_upsert_insert_path_when_no_match() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("weight", Type::Real),
            ColumnDef::new("updated_at", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "uq_pair", &["user_a", "user_b"], true).unwrap();

        let rowid = db.upsert(
            "edges",
            &[("user_a", Value::Text("alice".into())), ("user_b", Value::Text("bob".into()))],
            &[("weight", Value::Real(0.85)), ("updated_at", Value::Integer(100))],
        ).unwrap();

        assert!(rowid > 0, "upsert should return a real row id, got {rowid}");

        let row = db.get("edges", rowid).unwrap().expect("row should exist");
        assert_eq!(row.get("user_a").unwrap(), Value::Text("alice".into()));
        assert_eq!(row.get("user_b").unwrap(), Value::Text("bob".into()));
        assert_eq!(row.get("weight").unwrap(), Value::Real(0.85));
        assert_eq!(row.get("updated_at").unwrap(), Value::Integer(100));
    }

    #[test]
    fn test_upsert_update_path_when_match_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("weight", Type::Real),
            ColumnDef::new("updated_at", Type::Integer),
        ]).unwrap();
        db.create_index_ex("edges", "uq_pair", &["user_a", "user_b"], true).unwrap();

        let first_id = db.insert("edges", &[
            ("user_a", Value::Text("alice".into())),
            ("user_b", Value::Text("bob".into())),
            ("weight", Value::Real(0.5)),
            ("updated_at", Value::Integer(50)),
        ]).unwrap();

        let returned_id = db.upsert(
            "edges",
            &[("user_a", Value::Text("alice".into())), ("user_b", Value::Text("bob".into()))],
            &[("weight", Value::Real(0.9)), ("updated_at", Value::Integer(200))],
        ).unwrap();

        assert_eq!(returned_id, first_id,
            "update path must return the existing row id, not allocate a new one");

        let all = db.find("edges", FindOptions::default()).unwrap();
        assert_eq!(all.rows.len(), 1, "upsert must not duplicate on key match");

        let row = db.get("edges", first_id).unwrap().expect("row should still exist");
        assert_eq!(row.get("user_a").unwrap(), Value::Text("alice".into()), "key column unchanged");
        assert_eq!(row.get("user_b").unwrap(), Value::Text("bob".into()), "key column unchanged");
        assert_eq!(row.get("weight").unwrap(), Value::Real(0.9), "set column updated");
        assert_eq!(row.get("updated_at").unwrap(), Value::Integer(200), "set column updated");
    }

    #[test]
    fn test_upsert_errors_without_matching_unique_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("weight", Type::Real),
        ]).unwrap();
        // Note: NO unique index on (user_a, user_b).

        let err = db.upsert(
            "edges",
            &[("user_a", Value::Text("alice".into())), ("user_b", Value::Text("bob".into()))],
            &[("weight", Value::Real(0.5))],
        ).unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("unique index") || msg.contains("upsert"),
            "error should clearly identify the missing-unique-index condition, got: {msg}",
        );
    }

    #[test]
    fn test_upsert_set_overlapping_key_overwrites_on_update_path() {
        // Spec: when `set` overlaps `key`, the `set` value wins on the
        // update path (same field-merge semantics as `update`).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("weight", Type::Real),
        ]).unwrap();
        db.create_index_ex("edges", "uq_pair", &["user_a", "user_b"], true).unwrap();

        // Insert.
        db.upsert(
            "edges",
            &[("user_a", Value::Text("alice".into())), ("user_b", Value::Text("bob".into()))],
            &[("weight", Value::Real(0.5))],
        ).unwrap();

        // Second upsert with same key, BUT `set` also includes user_a with
        // a different value. The find still matches the existing row
        // (key filter on the original value), then the update writes the
        // `set` columns — including user_a's new value.
        db.upsert(
            "edges",
            &[("user_a", Value::Text("alice".into())), ("user_b", Value::Text("bob".into()))],
            &[
                ("user_a", Value::Text("alice_renamed".into())),  // overlaps key; wins on update
                ("weight", Value::Real(0.9)),
            ],
        ).unwrap();

        // Verify the overlap behavior: user_a is now "alice_renamed".
        let all = db.find("edges", FindOptions::default()).unwrap();
        assert_eq!(all.rows.len(), 1, "still only one row");
        assert_eq!(all.rows[0].get("user_a").unwrap(), Value::Text("alice_renamed".into()),
            "set value for user_a overrode the key value on the update path");
        assert_eq!(all.rows[0].get("user_b").unwrap(), Value::Text("bob".into()),
            "user_b unchanged (not in set)");
        assert_eq!(all.rows[0].get("weight").unwrap(), Value::Real(0.9));
    }

    #[test]
    fn test_upsert_unique_index_match_is_order_insensitive() {
        // Spec: unique-index match is set-equality, not order-sensitive.
        // A caller can pass key columns in any order as long as a unique
        // index with the same column set exists.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("edges", &[
            ColumnDef::new("user_a", Type::Text),
            ColumnDef::new("user_b", Type::Text),
            ColumnDef::new("weight", Type::Real),
        ]).unwrap();
        // Index declared as (user_a, user_b).
        db.create_index_ex("edges", "uq_pair", &["user_a", "user_b"], true).unwrap();

        // Caller passes key in REVERSED order: (user_b, user_a). Should still match.
        let rowid = db.upsert(
            "edges",
            &[("user_b", Value::Text("bob".into())), ("user_a", Value::Text("alice".into()))],
            &[("weight", Value::Real(0.7))],
        ).unwrap();
        assert!(rowid > 0, "reversed-key upsert should succeed");

        // Verify the row.
        let row = db.get("edges", rowid).unwrap().expect("row exists");
        assert_eq!(row.get("user_a").unwrap(), Value::Text("alice".into()));
        assert_eq!(row.get("user_b").unwrap(), Value::Text("bob".into()));
        assert_eq!(row.get("weight").unwrap(), Value::Real(0.7));

        // Re-upsert with key in YET ANOTHER order — should hit update path.
        let same_id = db.upsert(
            "edges",
            &[("user_a", Value::Text("alice".into())), ("user_b", Value::Text("bob".into()))],
            &[("weight", Value::Real(0.99))],
        ).unwrap();
        assert_eq!(same_id, rowid, "different key column order still finds the same row");
    }

    // --- Task 2: versioned system-page column format tests ---

    /// Round-trip: columns with `dropped=true` and `default=Some(...)` survive
    /// serialize → write → reopen → deserialize without loss.
    #[test]
    fn test_sys_page_column_default_and_dropped_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");

        // Create table with one "normal" column, one with a default, one marked dropped.
        // We set dropped manually on the ColumnDef (Task 3+ ops will set it via API;
        // here we exercise the ser/de path directly).
        {
            let db = BoogyDb::open(&path).unwrap();
            let cols = vec![
                ColumnDef::new("id", Type::Integer),
                ColumnDef::new("note", Type::Text).default(Value::Text("hello".into())),
                {
                    let mut c = ColumnDef::new("gone", Type::Boolean);
                    c.dropped = true;
                    c
                },
                ColumnDef::new("score", Type::Real).default(Value::Real(3.14)),
            ];
            db.create_table("t", &cols).unwrap();
            // Verify the schema was stored in memory correctly before reopen.
            let tables = db.tables.read().unwrap();
            let state = tables.get("t").unwrap().read().unwrap();
            assert_eq!(state.meta.columns.len(), 4);
            assert_eq!(state.meta.columns[1].default, Some(Value::Text("hello".into())));
            assert!(!state.meta.columns[1].dropped);
            assert!(state.meta.columns[2].dropped);
            assert_eq!(state.meta.columns[3].default, Some(Value::Real(3.14)));
        }

        // Reopen and verify the fields survived the system-page round-trip.
        {
            let db = BoogyDb::open(&path).unwrap();
            let tables = db.tables.read().unwrap();
            let state = tables.get("t").unwrap().read().unwrap();
            let cols = &state.meta.columns;

            assert_eq!(cols.len(), 4);

            // col 0: plain integer, no default, not dropped
            assert_eq!(cols[0].name, "id");
            assert_eq!(cols[0].default, None);
            assert!(!cols[0].dropped);

            // col 1: text with default "hello"
            assert_eq!(cols[1].name, "note");
            assert_eq!(cols[1].default, Some(Value::Text("hello".into())));
            assert!(!cols[1].dropped);

            // col 2: dropped
            assert_eq!(cols[2].name, "gone");
            assert!(cols[2].dropped);
            assert_eq!(cols[2].default, None);

            // col 3: real with default 3.14
            assert_eq!(cols[3].name, "score");
            assert_eq!(cols[3].default, Some(Value::Real(3.14)));
            assert!(!cols[3].dropped);
        }
    }

    /// All Value variants can round-trip as column defaults through the system page.
    #[test]
    fn test_sys_page_column_default_all_value_types() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");

        {
            let db = BoogyDb::open(&path).unwrap();
            let cols = vec![
                ColumnDef::new("a", Type::Integer).default(Value::Integer(-42)),
                ColumnDef::new("b", Type::Text).default(Value::Text("world".into())),
                ColumnDef::new("c", Type::Real).default(Value::Real(-1.5)),
                ColumnDef::new("d", Type::Boolean).default(Value::Boolean(true)),
                ColumnDef::new("e", Type::Blob).default(Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF])),
                ColumnDef::new("f", Type::Integer).default(Value::Null),
            ];
            db.create_table("all_defaults", &cols).unwrap();
        }

        {
            let db = BoogyDb::open(&path).unwrap();
            let tables = db.tables.read().unwrap();
            let state = tables.get("all_defaults").unwrap().read().unwrap();
            let cols = &state.meta.columns;

            assert_eq!(cols[0].default, Some(Value::Integer(-42)));
            assert_eq!(cols[1].default, Some(Value::Text("world".into())));
            assert_eq!(cols[2].default, Some(Value::Real(-1.5)));
            assert_eq!(cols[3].default, Some(Value::Boolean(true)));
            assert_eq!(cols[4].default, Some(Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF])));
            assert_eq!(cols[5].default, Some(Value::Null));
        }
    }

    /// Round-trip through reopen: create_table + insert + reopen must preserve data.
    /// Verifies the system-page ser/de path end-to-end on a normal workflow.
    #[test]
    fn test_sys_page_normal_workflow_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");

        {
            let db = BoogyDb::open(&path).unwrap();
            db.create_table("users", &[
                ColumnDef::new("name", Type::Text),
                ColumnDef::new("age", Type::Integer),
            ]).unwrap();
            db.insert("users", &[
                ("name", Value::Text("alice".into())),
                ("age", Value::Integer(30)),
            ]).unwrap();
        }

        {
            let db = BoogyDb::open(&path).unwrap();
            let row = db.find("users", crate::filter::FindOptions::default()).unwrap();
            assert_eq!(row.rows.len(), 1);
            assert_eq!(row.rows[0].get("name"), Some(Value::Text("alice".into())));
            assert_eq!(row.rows[0].get("age"), Some(Value::Integer(30)));

            // Verify that the re-opened columns have no stale dropped/default
            let tables = db.tables.read().unwrap();
            let state = tables.get("users").unwrap().read().unwrap();
            assert!(!state.meta.columns[0].dropped);
            assert_eq!(state.meta.columns[0].default, None);
            assert!(!state.meta.columns[1].dropped);
            assert_eq!(state.meta.columns[1].default, None);
        }
    }

    #[test]
    fn test_list_indexes_returns_all_created() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("posts", &[
            ColumnDef::new("title",      Type::Text),
            ColumnDef::new("author",     Type::Text),
            ColumnDef::new("created_at", Type::Integer),
        ]).unwrap();
        db.create_index("posts", "idx_author", "author").unwrap();
        db.create_index_ex("posts", "idx_composite", &["author", "created_at"], false).unwrap();
        db.create_index_ex("posts", "idx_uniq_title", &["title"], true).unwrap();

        let indexes = db.list_indexes("posts").unwrap();
        assert_eq!(indexes.len(), 3, "expected 3 indexes, got: {:?}", indexes.iter().map(|i| &i.name).collect::<Vec<_>>());

        let composite = indexes.iter().find(|i| i.name == "idx_composite").expect("idx_composite missing");
        assert_eq!(composite.columns, vec!["author".to_string(), "created_at".to_string()]);
        assert!(!composite.unique);

        let uniq = indexes.iter().find(|i| i.name == "idx_uniq_title").expect("idx_uniq_title missing");
        assert_eq!(uniq.columns, vec!["title".to_string()]);
        assert!(uniq.unique);
    }

    #[test]
    fn test_list_indexes_empty_for_table_with_no_indexes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();
        let indexes = db.list_indexes("items").unwrap();
        assert!(indexes.is_empty(), "fresh table must have zero indexes");
    }

    #[test]
    fn test_list_indexes_after_drop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("posts", &[ColumnDef::new("author", Type::Text)]).unwrap();
        db.create_index("posts", "idx_author", "author").unwrap();
        assert_eq!(db.list_indexes("posts").unwrap().len(), 1);
        db.drop_index("posts", "idx_author").unwrap();
        assert!(db.list_indexes("posts").unwrap().is_empty(), "after drop, list_indexes must be empty");
    }

    #[test]
    fn test_list_indexes_table_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        let err = db.list_indexes("no_such_table").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no_such_table"),
            "error must name the missing table; got: {msg}",
        );
    }

    #[test]
    fn test_list_tables_returns_all_table_names_sorted() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        // Create in non-alphabetical order to prove sort:
        db.create_table("z_orders",   &[ColumnDef::new("amount", Type::Integer)]).unwrap();
        db.create_table("a_users",    &[
            ColumnDef::new("name",  Type::Text),
            ColumnDef::new("email", Type::Text),
        ]).unwrap();
        db.create_table("m_products", &[ColumnDef::new("sku", Type::Text)]).unwrap();
        db.create_index("a_users", "idx_email", "email").unwrap();

        let tables = db.list_tables().unwrap();
        assert_eq!(tables.len(), 3, "expected 3 tables, got: {:?}", tables);

        // Sorted ascending by name.
        assert_eq!(tables[0].name, "a_users");
        assert_eq!(tables[1].name, "m_products");
        assert_eq!(tables[2].name, "z_orders");

        // Column counts match each create_table's columns vec length.
        assert_eq!(tables[0].column_count, 2, "a_users has 2 cols");
        assert_eq!(tables[1].column_count, 1, "m_products has 1 col");
        assert_eq!(tables[2].column_count, 1, "z_orders has 1 col");

        // Index counts: only a_users has a user index.
        assert_eq!(tables[0].index_count, 1, "a_users has 1 index");
        assert_eq!(tables[1].index_count, 0, "m_products has 0 indexes");
        assert_eq!(tables[2].index_count, 0, "z_orders has 0 indexes");
    }

    #[test]
    fn test_list_tables_returns_empty_when_no_tables() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        let tables = db.list_tables().unwrap();
        assert!(tables.is_empty(), "fresh DB must return empty Vec, got: {:?}", tables);
    }

    #[test]
    fn test_list_tables_counts_exclude_dropped_columns() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("items", &[
            ColumnDef::new("a", Type::Text),
            ColumnDef::new("b", Type::Text),
            ColumnDef::new("c", Type::Text),
            ColumnDef::new("d", Type::Text),
            ColumnDef::new("e", Type::Text),
        ]).unwrap();
        db.drop_column("items", "b").unwrap();
        db.drop_column("items", "d").unwrap();

        let tables = db.list_tables().unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "items");
        assert_eq!(tables[0].column_count, 3,
            "expected 3 live cols after dropping b and d, got {}", tables[0].column_count);
    }

    #[test]
    fn test_list_tables_after_drop_table() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("alpha", &[ColumnDef::new("name", Type::Text)]).unwrap();
        db.create_table("bravo", &[ColumnDef::new("name", Type::Text)]).unwrap();
        assert_eq!(db.list_tables().unwrap().len(), 2);

        db.drop_table("alpha").unwrap();

        let tables = db.list_tables().unwrap();
        assert_eq!(tables.len(), 1, "after drop, only one table remains");
        assert_eq!(tables[0].name, "bravo", "alpha must be gone, bravo must remain");
    }

    // ─── System-page overflow chain tests ────────────────────────────────────

    #[test]
    fn test_system_page_inline_when_schema_fits() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();

        // 3 small tables — fits comfortably inline (~150 bytes total).
        for i in 0..3 {
            db.create_table(
                &format!("t{i}"),
                &[ColumnDef::new("c", Type::Text)],
            ).unwrap();
        }

        // Reopen + verify all 3 tables present.
        drop(db);
        let db = BoogyDb::open(&path).unwrap();
        let tables = db.tables.read().unwrap();
        assert_eq!(tables.len(), 3);
        for i in 0..3 {
            assert!(tables.contains_key(&format!("t{i}")));
        }

        // Verify page 0 has NO overflow trailer.
        let page0 = db.file.read_page(0).unwrap();
        let trailer_start = PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET;
        assert_ne!(
            page0.data[trailer_start..trailer_start + 2],
            crate::overflow::OVERFLOW_MAGIC,
            "small schema must not produce an overflow trailer"
        );
    }

    #[test]
    fn test_system_page_overflows_to_chain() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();

        // 25 tables with long names + 2 indexes each → ~200-300 bytes per table,
        // ~5-7.5KB total > SYSTEM_INLINE_BUDGET.
        let long_col_a = "very_long_column_name_for_overflow_test_a";
        let long_col_b = "very_long_column_name_for_overflow_test_b";
        for i in 0..25 {
            let table = format!("table_with_long_name_{i:02}");
            db.create_table(&table, &[
                ColumnDef::new(long_col_a, Type::Text),
                ColumnDef::new(long_col_b, Type::Integer),
            ]).unwrap();
            db.create_index(&table, &format!("idx_a_{i}"), long_col_a).unwrap();
            db.create_index(&table, &format!("idx_b_{i}"), long_col_b).unwrap();
        }

        // Page 0 must have the overflow trailer.
        let page0 = db.file.read_page(0).unwrap();
        let trailer_start = PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET;
        assert_eq!(
            page0.data[trailer_start..trailer_start + 2],
            crate::overflow::OVERFLOW_MAGIC,
            "schema with 25 tables must produce an overflow trailer"
        );

        // All 25 tables present.
        let tables = db.tables.read().unwrap();
        assert_eq!(tables.len(), 25);
        for i in 0..25 {
            assert!(tables.contains_key(&format!("table_with_long_name_{i:02}")));
        }
    }

    #[test]
    fn test_system_page_overflow_chain_reopen_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");

        {
            let db = BoogyDb::open(&path).unwrap();
            for i in 0..25 {
                let table = format!("table_persist_{i:02}");
                db.create_table(&table, &[
                    ColumnDef::new("text_col", Type::Text),
                    ColumnDef::new("int_col", Type::Integer),
                    ColumnDef::new("real_col", Type::Real),
                    ColumnDef::new("bool_col", Type::Boolean),
                    ColumnDef::new("blob_col", Type::Blob),
                ]).unwrap();
                db.create_index(&table, &format!("idx_text_{i}"), "text_col").unwrap();
                db.create_index_ex(&table, &format!("idx_uniq_{i}"), &["int_col"], true).unwrap();
            }
        } // db dropped — WAL flushed

        // Reopen + verify full round-trip.
        let db = BoogyDb::open(&path).unwrap();
        let tables = db.tables.read().unwrap();
        assert_eq!(tables.len(), 25);

        for i in 0..25 {
            let name = format!("table_persist_{i:02}");
            let table_state = tables.get(&name).expect("table missing after reopen");
            let state = table_state.read().unwrap();
            let meta = &state.meta;

            assert_eq!(meta.columns.len(), 5);
            assert_eq!(meta.columns[0].name, "text_col");
            assert_eq!(meta.columns[0].col_type, Type::Text);
            assert_eq!(meta.columns[1].col_type, Type::Integer);
            assert_eq!(meta.columns[2].col_type, Type::Real);
            assert_eq!(meta.columns[3].col_type, Type::Boolean);
            assert_eq!(meta.columns[4].col_type, Type::Blob);

            assert_eq!(meta.indexes.len(), 2);
            assert!(meta.indexes.iter().any(|idx| idx.name == format!("idx_text_{i}") && !idx.unique));
            assert!(meta.indexes.iter().any(|idx| idx.name == format!("idx_uniq_{i}") && idx.unique));
        }
    }

    #[test]
    fn test_system_page_overflow_chain_shrinks_when_tables_dropped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();

        // Use same sizing as test_system_page_overflows_to_chain: 2 long columns
        // + 2 indexes per table to guarantee we exceed the inline budget.
        let long_col_a = "very_long_column_name_for_overflow_test_a";
        let long_col_b = "very_long_column_name_for_overflow_test_b";
        for i in 0..25 {
            let table = format!("shrink_table_{i:02}");
            db.create_table(&table, &[
                ColumnDef::new(long_col_a, Type::Text),
                ColumnDef::new(long_col_b, Type::Integer),
            ]).unwrap();
            db.create_index(&table, &format!("shrink_idx_a_{i}"), long_col_a).unwrap();
            db.create_index(&table, &format!("shrink_idx_b_{i}"), long_col_b).unwrap();
        }

        // Trailer present after creating 25 large tables.
        {
            let page0 = db.file.read_page(0).unwrap();
            let trailer_start = PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET;
            assert_eq!(
                page0.data[trailer_start..trailer_start + 2],
                crate::overflow::OVERFLOW_MAGIC,
                "25 large tables must produce an overflow trailer"
            );
        }

        // Drop 22 of 25.
        for i in 0..22 {
            db.drop_table(&format!("shrink_table_{i:02}")).unwrap();
        }

        // Trailer GONE — schema now fits inline.
        let page0 = db.file.read_page(0).unwrap();
        let trailer_start = PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET;
        assert_ne!(
            page0.data[trailer_start..trailer_start + 2],
            crate::overflow::OVERFLOW_MAGIC,
            "after dropping most tables, schema should fit inline (no trailer)"
        );
        // NOTE: page reuse is NOT asserted — the engine has no free-list;
        // old overflow pages are orphaned, consistent with drop_table's existing
        // behavior for data pages (see followup §5 in migration-followups).
    }

    #[test]
    fn test_system_page_overflow_survives_partial_write_via_wal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");

        // Phase 1: small schema, persisted normally.
        {
            let db = BoogyDb::open(&path).unwrap();
            for i in 0..3 {
                db.create_table(
                    &format!("phase1_{i}"),
                    &[ColumnDef::new("c", Type::Text)],
                ).unwrap();
            }
        } // db dropped — WAL checkpointed

        // Phase 2: add many tables to force overflow, then drop without explicit
        // sync (simulating a process exit before WAL checkpoint).
        {
            let db = BoogyDb::open(&path).unwrap();
            let long_col = "very_long_column_name_for_overflow_test";
            for i in 0..25 {
                db.create_table(
                    &format!("phase2_{i:02}"),
                    &[ColumnDef::new(long_col, Type::Text)],
                ).unwrap();
            }
            // Drop without explicit sync — WAL atomicity protects us.
        }

        // Phase 3: reopen. Schema must be parseable (no corruption); EITHER
        // the pre-phase-2 state (3 tables) or the post-phase-2 state (28 tables)
        // is acceptable — a torn intermediate state is NOT acceptable.
        let db = BoogyDb::open(&path).unwrap();
        let tables = db.tables.read().unwrap();
        let count = tables.len();
        assert!(
            count == 3 || count == 28,
            "schema must be either pre-crash (3) or post-crash (28), got {count}",
        );
        for (name, table_state) in tables.iter() {
            let state = table_state.read().unwrap();
            assert!(!state.meta.columns.is_empty(), "table {name} has no columns — corrupt");
        }
    }

    #[test]
    fn test_serialize_system_metas_handles_large_schemas_without_panic() {
        let metas: Vec<TableMeta> = (0..100)
            .map(|i| {
                let mut meta = TableMeta::new(
                    format!("t{i:03}"),
                    i as u32 + 1,
                    vec![
                        ColumnDef::new("c1", Type::Text),
                        ColumnDef::new("c2", Type::Integer),
                    ],
                    100 + i as u32, // root_page
                );
                meta.indexes.push(IndexMeta {
                    name: format!("idx_{i}"),
                    columns: vec!["c1".to_string()],
                    unique: false,
                    root_page: 1000 + i as u32,
                });
                meta
            })
            .collect();

        let bytes = serialize_system_metas(&metas, 101);

        // Must be large enough to need overflow.
        assert!(
            bytes.len() > SYSTEM_INLINE_BUDGET,
            "100 tables must overflow inline budget (got {} bytes)", bytes.len(),
        );

        // Must round-trip through deserialize_system_metas.
        let (recovered_metas, next_id) = deserialize_system_metas(&bytes).unwrap();
        assert_eq!(recovered_metas.len(), 100);
        assert_eq!(next_id, 101);
        for (i, meta) in recovered_metas.iter().enumerate() {
            assert_eq!(meta.name, format!("t{i:03}"));
            assert_eq!(meta.columns.len(), 2);
            assert_eq!(meta.indexes.len(), 1);
        }
    }

    // ─── Regression tests for T1's defensive paths ───────────────────────────

    #[test]
    fn test_deserialize_system_page_errors_on_corrupt_trailer_with_zero_first_page() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("c", Type::Text)]).unwrap();

        // Read page 0, craft a corrupt trailer: magic present, first_page == 0,
        // remaining_len > 0. This should trigger the defensive check:
        // "trailer present but first_page is 0".
        let mut corrupted = (*db.file.read_page(0).unwrap()).clone();
        let trailer_start = PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET;
        corrupted.data[trailer_start..trailer_start + 2]
            .copy_from_slice(&crate::overflow::OVERFLOW_MAGIC);
        corrupted.data[trailer_start + 2..trailer_start + 6]
            .copy_from_slice(&0u32.to_le_bytes()); // first_page = 0 (corrupt)
        corrupted.data[trailer_start + 6..trailer_start + 10]
            .copy_from_slice(&100u32.to_le_bytes()); // remaining_len = 100 (> 0)
        corrupted.update_checksum();

        let err = deserialize_system_page(&corrupted, &db.file).unwrap_err();
        let msg = format!("{err:?}").to_lowercase();
        assert!(
            msg.contains("first_page is 0") || msg.contains("first_page=0"),
            "expected first-page-zero corruption error, got: {err:?}",
        );
    }

    #[test]
    fn test_deserialize_system_page_errors_on_chain_pointing_at_non_overflow_page() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.boogy");
        let db = BoogyDb::open(&path).unwrap();

        // Create enough tables to force an overflow chain so we have at least
        // one real data page (page 1 = system page's first overflow page).
        // Then create one table (a btree root) to guarantee a non-overflow page
        // exists at some page number > 0.
        db.create_table("t", &[ColumnDef::new("c", Type::Text)]).unwrap();

        // Craft a corrupt trailer on page 0 that points at page 0's next data
        // page (page 1, which is a btree root — NOT an overflow page).
        // This hits the `is_overflow()` defensive check in the chain walk.
        //
        // We need a page > 0 to point at. After create_table the btree root
        // for "t" is at some page > 0. We'll point at page 1 regardless.
        // If the engine happens to write an overflow page at page 1 this test
        // might not hit the is_overflow() path, but for a single small table
        // page 1 will be a btree leaf, not an overflow page.
        let mut corrupted = (*db.file.read_page(0).unwrap()).clone();
        let trailer_start = PAGE_HEADER_SIZE + SYSTEM_INLINE_BUDGET;
        corrupted.data[trailer_start..trailer_start + 2]
            .copy_from_slice(&crate::overflow::OVERFLOW_MAGIC);
        corrupted.data[trailer_start + 2..trailer_start + 6]
            .copy_from_slice(&1u32.to_le_bytes()); // first_page = 1 (btree node, not overflow)
        corrupted.data[trailer_start + 6..trailer_start + 10]
            .copy_from_slice(&100u32.to_le_bytes()); // remaining_len = 100 (> 0)
        corrupted.update_checksum();

        let err = deserialize_system_page(&corrupted, &db.file).unwrap_err();
        // Must be a corruption error — either "non-overflow page" from the
        // is_overflow() check, or another corruption family.
        let msg = format!("{err:?}").to_lowercase();
        assert!(
            msg.contains("non-overflow") || msg.contains("corruption"),
            "expected a corruption error, got: {err:?}",
        );
    }

}
