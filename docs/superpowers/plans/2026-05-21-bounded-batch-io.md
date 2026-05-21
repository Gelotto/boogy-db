# boogy-db Bounded-Batch-I/O Primitives — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. **TDD:** every task leads with exact black-box tests (the correctness contract); make them pass by extending the engine internals per the guidance + code-region anchors. The tests are exact; the internal encoding is yours to write against them.

**Goal:** Add three boogy-db engine primitives that let downstream batch jobs run in bounded memory with correct keyed accumulation: **composite + enforced unique indexes**, an **ordered range-scan-from-a-key** (`scan_batch`), and **`upsert_increment`** (numeric int/real delta).

**Architecture:** boogy-db is a pure-Rust ACID B+ tree (per-table `RwLock`, no MVCC). Composite indexes extend the existing single-column `(value, rowid)` index key to a multi-column `(v₁, v₂, …, rowid)` sortable key; unique enforcement is a write-path existence check under the table write lock. `scan_batch` generalizes the leaf-chain walk to seek-from-a-key + bounded forward scan. `upsert_increment` is an atomic find-(via the composite index)-then-update-or-insert.

**Tech Stack:** Rust; the existing `src/index.rs` (key encoding + `IndexTreeReader`/`Writer`), `src/btree.rs` (`BTreeReader`, leaf chain), `src/db.rs` (`BoogyDb`/`Transaction`/`AcidTransaction` find/insert/create_index), `src/table.rs` (`IndexMeta`), `src/filter.rs` (`FindOptions`, `Filter`).

**Spec:** `boogy` repo `docs/superpowers/specs/2026-05-21-bounded-batch-io-design.md` (component ①). This plan is the boogy-db half; the WIT/host/SDK half is a separate plan in the boogy repo after this lands + is pushed.

**Conventions:** match the existing test style (in-module `#[cfg(test)]`, `#[test]`/`#[tokio::test]` as the surrounding module uses, one behavior per test, clear names). No `cargo fmt` reformatting of untouched code. All work on branch `feat/bounded-batch-io` (already created). Build: `cargo build`; test: `cargo test`. Do NOT push — the controller pushes after review.

---

## Spec fidelity discipline

The spec is authoritative. Composite-unique-index semantics, the `scan_batch` resume contract, and `upsert_increment`'s numeric-delta + index-keyed behavior are defined there. Flag deviations in commit messages with `DEVIATION:` + one sentence.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/index.rs` | MODIFY | Composite key encoding (`encode_composite_index_key`, composite prefix), composite range scan helpers. |
| `src/table.rs` | MODIFY | `IndexMeta.column: String` → `columns: Vec<String>` + `unique: bool`. |
| `src/db.rs` | MODIFY | `create_index_ex(name, &[col], unique)` (+ `create_index` wrapper); unique enforcement in the insert/`insert_with_id` index-maintenance path; `scan_batch`; `upsert_increment`; thread through `Transaction`/`AcidTransaction`. |
| `src/filter.rs` | MODIFY (maybe) | `ScanOrder`/`ScanKey` types if they live here (or in db.rs). |
| `src/lib.rs` | MODIFY | Re-export new public types (`ScanOrder`, `ScanKey`, `ScanBatch`). |
| README.md / CLAUDE.md / docs/design.md | MODIFY | Document composite/unique indexes, `scan_batch`, `upsert_increment`. |

---

## Task 1: `IndexMeta` carries multiple columns + a unique flag

**Files:** Modify `src/table.rs`; fix all construction/read sites flagged by the compiler (`src/db.rs` index create + system-page (de)serialization).

- [ ] **Step 1: Change the struct**

In `src/table.rs`, `IndexMeta`:
```rust
#[derive(Debug, Clone)]
pub struct IndexMeta {
    pub name: String,
    /// Columns this index covers, in key order. Length 1 = single-column (back-compat).
    pub columns: Vec<String>,
    /// When true, inserts/upserts whose key tuple already exists are rejected.
    pub unique: bool,
    pub root_page: u32,
}
```

- [ ] **Step 2: Build to find every break, fix construction + persistence sites**

`cargo build 2>&1 | tail -40`. Fix each error:
- `create_index` (`src/db.rs:1423`) constructs `IndexMeta { name, column, root_page }` → `IndexMeta { name, columns: vec![column.to_string()], unique: false, root_page }` (Task 3 adds the multi-column/unique entry points; this keeps the existing single-column path compiling).
- The system-page (de)serializer for table metadata (search `src/db.rs` for where `IndexMeta`/indexes are written to / read from page 0 — grep `idx`/`index` near `serialize_system_page` / the registry persistence around `persist_registry`). Persist `columns` (count + each name) + the `unique` byte. **Bump the on-disk registry format** if it has a version tag; if not, note in the commit that this is a format change (the dev DBs are disposable). Read it back symmetrically.
- `find_index_for_column(col)` and any `meta.column` reads → use `columns` (e.g. "index covers col" = `idx.columns.first() == Some(col)` for the existing single-column lookups, or `idx.columns.contains(col)`; preserve current behavior for single-column indexes).

- [ ] **Step 3: Test — existing single-column indexes still work**

The existing index tests in `src/db.rs` must still pass (regression guard). Run `cargo test 2>&1 | tail -20`. Expected: all pre-existing tests pass.

- [ ] **Step 4: Commit**
```bash
git add src/table.rs src/db.rs
git commit -m "refactor(index): IndexMeta carries columns: Vec<String> + unique flag"
```

---

## Task 2: Composite index key encoding

**Files:** Modify `src/index.rs` (+ tests there).

Context: `encode_index_key(col_type, val, rowid)` (`src/index.rs:11`) builds a single `(value, rowid)` sortable key per type (`encode_index_key_integer/real/text`). `encode_value_prefix` builds the value-only prefix for scans. Composite keys concatenate the per-column sortable encodings in column order, then the rowid suffix.

- [ ] **Step 1: Write failing tests for composite ordering**

Add to `src/index.rs`'s `#[cfg(test)]` module:
```rust
#[test]
fn test_composite_key_orders_lexicographically_by_column() {
    use crate::value::{Type, Value};
    // (Integer, Integer) composite. Keys must sort by col1 then col2 then rowid.
    let types = [Type::Integer, Type::Integer];
    let k = |a: i64, b: i64, rid: u64|
        encode_composite_index_key(&types, &[Value::Integer(a), Value::Integer(b)], rid).unwrap();
    let mut keys = vec![k(5, 9, 1), k(5, 2, 2), k(4, 100, 3), k(5, 9, 0)];
    keys.sort();
    // Expected order: (4,100,r3) < (5,2,r2) < (5,9,r0) < (5,9,r1)
    assert_eq!(keys, vec![k(4,100,3), k(5,2,2), k(5,9,0), k(5,9,1)]);
}

#[test]
fn test_composite_key_text_then_integer() {
    use crate::value::{Type, Value};
    let types = [Type::Text, Type::Integer];
    let k = |s: &str, n: i64, rid: u64|
        encode_composite_index_key(&types, &[Value::Text(s.into()), Value::Integer(n)], rid).unwrap();
    let mut keys = vec![k("bob", 1, 1), k("alice", 9, 2), k("alice", 1, 3)];
    keys.sort();
    assert_eq!(keys, vec![k("alice",1,3), k("alice",9,2), k("bob",1,1)]);
}

#[test]
fn test_composite_key_null_component_not_indexed() {
    use crate::value::{Type, Value};
    let types = [Type::Integer, Type::Integer];
    // A null in any key component → None (consistent with single-column null handling).
    assert!(encode_composite_index_key(&types, &[Value::Integer(1), Value::Null], 1).is_none());
}

#[test]
fn test_composite_prefix_is_key_without_rowid() {
    use crate::value::{Type, Value};
    let types = [Type::Integer, Type::Integer];
    let full = encode_composite_index_key(&types, &[Value::Integer(5), Value::Integer(9)], 7).unwrap();
    let prefix = encode_composite_value_prefix(&types, &[Value::Integer(5), Value::Integer(9)]).unwrap();
    assert!(full.starts_with(&prefix));
    assert_eq!(full.len(), prefix.len() + 8); // rowid is 8 bytes appended
}
```

- [ ] **Step 2: Run, verify they fail to compile** (functions undefined)

`cargo test --lib index:: 2>&1 | tail -10`. Expected: compile error, `encode_composite_index_key` not found.

- [ ] **Step 3: Implement composite encoding**

In `src/index.rs`, add (reusing the existing per-type encoders so byte-level ordering matches single-column behavior):
```rust
/// Encode a composite index key: each column's sortable value-encoding in
/// order, then the 8-byte big-endian rowid suffix. Returns None if ANY
/// component is Null (nulls are not indexed — matches single-column).
pub fn encode_composite_index_key(col_types: &[Type], vals: &[Value], rowid: u64) -> Option<Vec<u8>> {
    let mut out = encode_composite_value_prefix(col_types, vals)?;
    out.extend_from_slice(&rowid.to_be_bytes());
    Some(out)
}

/// Composite value prefix (no rowid) for range/equality scans.
pub fn encode_composite_value_prefix(col_types: &[Type], vals: &[Value]) -> Option<Vec<u8>> {
    debug_assert_eq!(col_types.len(), vals.len());
    let mut out = Vec::new();
    for (t, v) in col_types.iter().zip(vals.iter()) {
        // Reuse the existing per-type value encoding (the bytes encode_value_prefix
        // would produce for a single column), so multi-col ordering == nesting of
        // the per-col orderings. For Text, the existing encoding must be
        // length-or-terminator-delimited so concatenation stays prefix-unambiguous;
        // if encode_value_prefix for Text is not self-delimiting, add a length
        // prefix here. Verify against the single-column text ordering tests.
        let part = encode_value_prefix(*t, v)?;
        out.extend_from_slice(&part);
    }
    Some(out)
}
```
**Implementation note:** confirm the existing `encode_value_prefix` for `Text` is self-delimiting (length-prefixed) so `("a","bc")` and `("ab","c")` don't collide when concatenated. If it isn't, prepend a `u32` length to each text component inside the composite encoder (and keep single-column encoding unchanged). The tests in Step 1 (text-then-integer ordering) will catch a non-self-delimiting bug.

- [ ] **Step 4: Run tests, verify pass**

`cargo test --lib index:: 2>&1 | tail -10`. Expected: the 4 new tests pass + existing index tests pass.

- [ ] **Step 5: Commit**
```bash
git add src/index.rs
git commit -m "feat(index): composite multi-column index key encoding"
```

---

## Task 3: `create_index_ex` (multi-column + unique) + populate

**Files:** Modify `src/db.rs` (+ tests).

- [ ] **Step 1: Write failing tests**

Add to `src/db.rs` `#[cfg(test)]` (use the existing test setup helper for a `BoogyDb` — find how other index tests open a db + create a table; mirror it):
```rust
#[test]
fn test_create_composite_index_and_find_by_both_columns() {
    let db = /* open temp db per existing test helper */;
    db.create_table("edges", &[/* user_a: Text, user_b: Text, n: Integer */]).unwrap();
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
    let db = /* ... */;
    db.create_table("edges", &[/* ... */]).unwrap();
    db.create_index_ex("edges", "by_pair", &["user_a", "user_b"], true).unwrap();
    db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",1i64.into())]).unwrap();
    let dup = db.insert("edges", &[("user_a","a".into()),("user_b","b".into()),("n",9i64.into())]);
    assert!(matches!(dup, Err(BoogyError::UniqueViolation { .. })));
    // a different pair is fine
    assert!(db.insert("edges", &[("user_a","a".into()),("user_b","c".into()),("n",1i64.into())]).is_ok());
}

#[test]
fn test_nonunique_composite_index_allows_duplicate() {
    let db = /* ... */;
    db.create_table("t", &[/* x: Integer, y: Integer */]).unwrap();
    db.create_index_ex("t", "by_xy", &["x", "y"], false).unwrap();
    db.insert("t", &[("x",1i64.into()),("y",2i64.into())]).unwrap();
    assert!(db.insert("t", &[("x",1i64.into()),("y",2i64.into())]).is_ok());
}
```
Add `UniqueViolation { index: String }` to `BoogyError` (`src/error.rs`) if absent.

- [ ] **Step 2: Run, verify fail** — `cargo test --lib 2>&1 | tail`. Expected: `create_index_ex` undefined / `UniqueViolation` undefined.

- [ ] **Step 3: Implement**

In `src/db.rs`:
- Add `pub fn create_index_ex(&self, table, name, columns: &[&str], unique: bool) -> Result<()>` — generalize the existing `create_index` (`:1423`): resolve all `columns` to `(col_id, col_type)`, build the index tree populating composite keys via `encode_composite_index_key(&col_types, &col_vals, rowid)` for each existing row, store `IndexMeta { name, columns: columns.iter().map(|s| s.to_string()).collect(), unique, root_page }`.
- Make `create_index(table, name, column)` delegate: `self.create_index_ex(table, name, &[column], false)`.
- **Unique enforcement on insert:** in the insert index-maintenance path (where each index's key is computed + inserted for a new row — search `encode_index_key` call sites in the insert/`insert_with_id` flow), for a `unique` index, before inserting the key check the index tree for an existing entry with the same **value prefix** (`encode_composite_value_prefix`) via `IndexTreeReader::scan_prefix(prefix)` (non-empty ⇒ duplicate ⇒ return `Err(BoogyError::UniqueViolation { index })`). Do this under the table write lock already held by insert, before any row/page mutation, so a rejected insert leaves no partial state.
- `find` index-candidate selection (`db.rs:1070`): optionally extend to use a composite index when filters cover its leading column(s) — **not required for correctness** (the multi-filter scan path already returns correct rows); add only if trivial, else leave (the upsert path in Task 5 does its own keyed lookup). Keep this task focused on create + unique enforcement.

- [ ] **Step 4: Run tests, verify pass** — `cargo test --lib 2>&1 | tail -20`. New 3 + all existing pass.

- [ ] **Step 5: Commit**
```bash
git add src/db.rs src/error.rs
git commit -m "feat(index): create_index_ex (composite + enforced unique)"
```

---

## Task 4: `scan_batch` — ordered range scan from a key

**Files:** Modify `src/btree.rs` (seek-from-key forward walk), `src/db.rs` (`scan_batch` + `ScanOrder`/`ScanKey`/`ScanBatch` types), `src/lib.rs` (re-exports). Tests in `src/db.rs`.

- [ ] **Step 1: Write failing tests (the resume contract)**

Add to `src/db.rs` tests:
```rust
#[test]
fn test_scan_batch_tiles_primary_key_order_no_gaps_or_dups() {
    let db = /* ... */;
    db.create_table("t", &[/* v: Integer */]).unwrap();
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
    let db = /* ... */;
    db.create_table("t", &[/* v: Integer */]).unwrap();
    for v in 0..20i64 { db.insert("t", &[("v", v.into())]).unwrap(); }
    // Only even values, batches of 3.
    let filters = vec![]; // (filter for even-ness isn't expressible; use v >= 10 instead)
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let b = db.scan_batch("t", &[Filter::ge("v", 10i64)], &[], ScanOrder::primary_key(SortDir::Asc), after.clone(), 3).unwrap();
        if b.rows.is_empty() { break; }
        for r in &b.rows { if let Some(Value::Integer(n)) = r.get("v") { seen.push(*n); } }
        after = b.last_key;
        if after.is_none() { break; }
    }
    assert_eq!(seen, (10..20).collect::<Vec<_>>());
}

#[test]
fn test_scan_batch_index_order_desc() {
    let db = /* ... */;
    db.create_table("t", &[/* score: Integer */]).unwrap();
    db.create_index("t", "by_score", "score").unwrap();
    for s in [3i64,1,4,1,5,9,2,6] { db.insert("t", &[("score", s.into())]).unwrap(); }
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let b = db.scan_batch("t", &[], &[], ScanOrder::index("by_score", SortDir::Desc), after.clone(), 3).unwrap();
        if b.rows.is_empty() { break; }
        for r in &b.rows { if let Some(Value::Integer(n)) = r.get("score") { seen.push(*n); } }
        after = b.last_key;
        if after.is_none() { break; }
    }
    assert_eq!(seen, vec![9,6,5,4,3,2,1,1]); // descending, dups preserved
}

#[test]
fn test_scan_batch_no_index_for_order_errors() {
    let db = /* ... */;
    db.create_table("t", &[/* v: Integer */]).unwrap();
    db.insert("t", &[("v", 1i64.into())]).unwrap();
    let r = db.scan_batch("t", &[], &[], ScanOrder::index("nonexistent", SortDir::Asc), None, 10);
    assert!(r.is_err());
}
```

- [ ] **Step 2: Run, verify fail** — types/method undefined.

- [ ] **Step 3: Implement**

In `src/db.rs` (or `src/filter.rs` for the types, re-exported in `lib.rs`):
```rust
#[derive(Debug, Clone)]
pub enum ScanOrderKind { PrimaryKey, Index(String) }
#[derive(Debug, Clone)]
pub struct ScanOrder { pub kind: ScanOrderKind, pub dir: SortDir }
impl ScanOrder {
    pub fn primary_key(dir: SortDir) -> Self { Self { kind: ScanOrderKind::PrimaryKey, dir } }
    pub fn index(name: &str, dir: SortDir) -> Self { Self { kind: ScanOrderKind::Index(name.into()), dir } }
}
/// Opaque resume token: the ordered key bytes + rowid of the last row returned.
#[derive(Debug, Clone)]
pub struct ScanKey { pub(crate) bytes: Vec<u8>, pub(crate) rowid: u64 }
pub struct ScanBatch { pub rows: Vec<Row>, pub last_key: Option<ScanKey> }
```
`scan_batch(table, filters, or_groups, order, after, limit)`:
- **PrimaryKey order:** generalize `BTreeReader` to seek the leaf for the first rowid `> after.rowid` (Asc) / `< after.rowid` (Desc) — add a `BTreeReader::scan_from(start_rowid_exclusive, dir, limit, filter_fn)` that uses `find_leaf_for_key` (generalize `find_leftmost_leaf`) + leaf-chain walk (forward for Asc; for Desc, walk a prev-pointer or reverse — if leaves are singly-linked, Desc primary-key may require a reverse traversal helper; if that's heavy, support PrimaryKey Asc first + document Desc-PK as index-backed). Apply the `row_passes(filters, or_groups)` predicate (reuse the helper from the or_groups work) per row; collect up to `limit`. `last_key` = `{ bytes: [], rowid: last_rowid }` (rowid alone suffices for PK order).
- **Index order:** seek the index tree (`IndexTreeReader`) to the entry just after `after.bytes` in `dir`, walk the index leaf chain collecting rowids up to `limit` (applying filters after fetching each row via `multi_get`/`get`), `last_key.bytes` = the last index key. Reuse `scan_prefix`-style traversal generalized to "from key, bounded, directional."
- No usable index for `Index(name)` ⇒ `Err`.
- `last_key = None` when fewer than `limit` rows returned (exhausted).

**Note on Desc + leaf chain:** if reverse leaf traversal isn't supported, implement Desc by scanning the index in Asc and... no — that breaks batching. Instead: add a `prev_leaf` walk or, simpler, for Desc use the index tree's natural order reversed via a right-to-left seek. Pick the approach the btree structure supports; the `test_scan_batch_index_order_desc` test is the contract. If Desc proves expensive for PrimaryKey specifically, it's acceptable to require an index for Desc ordering and error on `ScanOrder::primary_key(Desc)` — but document it and adjust the test.

- [ ] **Step 4: Run tests, verify pass.**

- [ ] **Step 5: Commit**
```bash
git add src/btree.rs src/db.rs src/lib.rs
git commit -m "feat(db): scan_batch — ordered range-scan-from-a-key (cursor primitive)"
```

---

## Task 5: `upsert_increment`

**Files:** Modify `src/db.rs` (+ tests). Thread through `Transaction`/`AcidTransaction` if they need it (the host calls it on `BoogyDb`; add tx variants only if the host's tx path needs them — check the spec's host section; for now `BoogyDb::upsert_increment` suffices).

- [ ] **Step 1: Write failing tests**
```rust
#[test]
fn test_upsert_increment_inserts_then_increments_integer() {
    let db = /* ... */;
    db.create_table("edges", &[/* user_a: Text, user_b: Text, n: Integer, updated_at: Integer */]).unwrap();
    db.create_index_ex("edges", "by_pair", &["user_a","user_b"], true).unwrap();
    // first call inserts n=1
    db.upsert_increment("edges", &[("user_a","a".into()),("user_b","b".into())], "n", Value::Integer(1),
        &[("updated_at", Value::Integer(100))]).unwrap();
    // second call increments to n=3 (delta 2) and updates set col
    db.upsert_increment("edges", &[("user_a","a".into()),("user_b","b".into())], "n", Value::Integer(2),
        &[("updated_at", Value::Integer(200))]).unwrap();
    let res = db.find("edges", FindOptions {
        filters: vec![Filter::eq("user_a","a"), Filter::eq("user_b","b")], ..Default::default() }).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].get("n"), Some(&Value::Integer(3)));
    assert_eq!(res.rows[0].get("updated_at"), Some(&Value::Integer(200)));
}

#[test]
fn test_upsert_increment_real_counter() {
    let db = /* ... */;
    db.create_table("w", &[/* k: Text, weight: Real */]).unwrap();
    db.create_index_ex("w", "by_k", &["k"], true).unwrap();
    db.upsert_increment("w", &[("k","x".into())], "weight", Value::Real(0.5), &[]).unwrap();
    db.upsert_increment("w", &[("k","x".into())], "weight", Value::Real(0.25), &[]).unwrap();
    let res = db.find("w", FindOptions { filters: vec![Filter::eq("k","x")], ..Default::default() }).unwrap();
    if let Some(Value::Real(f)) = res.rows[0].get("weight") { assert!((f - 0.75).abs() < 1e-9); } else { panic!(); }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement**

`pub fn upsert_increment(&self, table, key: &[(&str, Value)], counter: &str, delta: Value, set: &[(&str, Value)]) -> Result<u64>`:
- Write-lock the table (mirror the locking in `update`/`insert`).
- Find the existing row by the `key` tuple: build `filters = key.iter().map(|(c,v)| Filter::eq(*c, v.clone()))`, run the internal find (or, if a composite index on the key columns exists, use it for an exact lookup — optional optimization; correctness via filter-find is fine).
- If found (take the first match): read current `counter` value, compute `new = current + delta` **preserving type** (Integer+Integer→Integer; Real+Real→Real; mixed→Real; null/absent current treated as 0 of delta's type), `update(rowid, [(counter, new)] + set)`.
- If not found: `insert(key cols + (counter, delta) + set)`.
- Return the rowid. Validate `delta` is `Integer`/`Real` (else `Err(SchemaMismatch)`).

- [ ] **Step 4: Run tests, verify pass.**

- [ ] **Step 5: Commit**
```bash
git add src/db.rs
git commit -m "feat(db): upsert_increment — atomic keyed counter (int/real delta)"
```

---

## Task 6: Docs + final verification

**Files:** `README.md`, `CLAUDE.md`, `docs/design.md`.

- [ ] **Step 1: README** — add an "Indexes" subsection documenting `create_index_ex(table, name, &cols, unique)` (composite + enforced unique) + an API-table row; document `scan_batch` (cursor primitive: ordered, bounded, resume token) and `upsert_increment` (atomic keyed counter, int/real). Note the unique-violation error.
- [ ] **Step 2: CLAUDE.md** — update the module map: `index.rs` (composite key encoding), `db.rs` (`scan_batch`, `upsert_increment`, `create_index_ex`).
- [ ] **Step 3: docs/design.md** — one paragraph: composite/unique indexes + the bounded-batch primitives + their intent (streaming jobs).
- [ ] **Step 4: Full verification**
```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | grep "test result:"
cargo test --features vector 2>&1 | grep "test result:"   # if vector feature exists
cargo clippy 2>&1 | tail -10   # confirm no NEW warnings vs base
```
Expected: clean build, all tests pass (existing + new), no new clippy.
- [ ] **Step 5: Commit**
```bash
git add README.md CLAUDE.md docs/design.md
git commit -m "docs: composite/unique indexes + scan_batch + upsert_increment"
```

---

## Audit log

### 2026-05-21 — boogy-db component complete (pre-push)

**Result:** PASS. All 6 plan tasks + an added T3b done via subagent-driven execution (fresh subagent per task; the controller verified each diff + re-ran tests, with extra scrutiny on the on-disk format symmetry, the ACID-path enforcement, and the `scan_batch` resume contract).

**Commits on `feat/bounded-batch-io`:**
- `4272488` T1 — `IndexMeta { columns: Vec<String>, unique }` + symmetric system-page (de)serialize + reopen test.
- `c9e961f` T2 — composite key encoding (text is self-delimiting via `0x00` terminator, so concatenation is unambiguous; single-column encoding unchanged).
- `a445be6` T3 — `create_index_ex` (composite + unique) + non-ACID write-path enforcement.
- `e4fc158` **T3b** — composite/unique enforcement in the **ACID** write path (the path the host actually runs — `set_acid(true)`); re-keyed `MetaDelta.index_roots` from leading-column to index-name across insert/update/delete/commit. (T3 alone would have been dead code for the host — caught in review.)
- `5040f7a` T4 — `scan_batch` ordered range-scan-from-a-key (PK + index order, Asc/Desc via doubly-linked leaves, filters/or_groups, exclusive-`after` resume token).
- `659fd4c` T5 — `upsert_increment` (atomic via single `AcidTransaction`; int/real/mixed/null-current type rules; unique index as race backstop).
- `c71ee0c` T6 — docs (README Indexes + Streaming/Batch I/O, CLAUDE.md, design.md).

**Verification:** `cargo test` 222 lib + 111 + 7 pass, 0 failed; `--features vector` 270 + 13 + … pass; clippy 33 warnings all pre-existing (zero from new code). Tree clean.

**Documented limitations (acceptable):** delete-then-reinsert of the same unique key *within one explicit multi-op AcidTransaction* could falsely reject (doesn't affect the host's single-insert-per-tx path); index-order `scan_batch` uses per-row point lookups (fine for bounded batches; future optimization).

**Next:** push `feat/bounded-batch-io` → boogy-db `main` (awaiting user OK), `cargo update -p boogy-db` in boogy, then the WIT/host/SDK plan, then tokenfeed slice 4 (affinity + promoted feed, MinHash/LSH).
