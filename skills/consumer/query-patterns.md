# Query Patterns for boogy-db

Common data access patterns and how to implement them efficiently.

## Point Lookup

Fetch a single row by its auto-generated rowid (O(log n) B+ tree search):

```rust
if let Some(row) = db.get("users", user_id)? {
    let name = row.get("name");  // decodes only this column
    let all = row.columns();     // decodes every column
    let id: u64 = row.id;        // always available without decoding
}
```

Use `insert_with_id` when you need to control the rowid (e.g., importing data). Returns `BoogyError::DuplicateKey` if the id already exists.

## Filtered Queries

Use `find()` with filters. When an index exists on the filtered column, boogy-db uses it automatically.

```rust
use boogy_db::{Filter, FindOptions, Sort};

let result = db.find("posts", FindOptions {
    filters: vec![
        Filter::eq("user_id", 42i64),
        Filter::eq("published", true),
    ],
    sort: vec![Sort::desc("created_at"), Sort::asc("title")],
    limit: Some(20),
    offset: Some(0),          // for pagination
    include_total: true,       // populates result.total for "page X of Y"
})?;

for row in &result.rows {
    println!("{}: {}", row.id, row.get("title").unwrap());
}
let total_pages = (result.total.unwrap() + 19) / 20;
```

**Filter operators**: `Filter::eq`, `ne`, `lt`, `le`, `gt`, `ge`.

**Sorting**: `Sort::asc("col")`, `Sort::desc("col")`. Multiple sort fields apply in order.

## Counting

Count rows matching filters. Uses the index when one is available on the filtered column:

```rust
let active = db.count("users", &[Filter::eq("is_active", true)])?;
let total = db.count("users", &[])?;
```

## JOIN Pattern

boogy-db has no built-in joins. Use two calls:

```rust
let user = db.get("users", user_id)?.unwrap();
let posts = db.find("posts", FindOptions {
    filters: vec![Filter::eq("user_id", user_id as i64)],
    sort: vec![Sort::desc("created_at")],
    ..Default::default()
})?;
```

For fetching related data in bulk, query once and group in application code.

## Bulk Operations

Insert, update, or delete many rows in a single call:

```rust
// Bulk insert -- returns all generated rowids
let ids = db.insert_many("events", &[
    vec![("type", Value::Text("click".into())), ("ts", Value::Integer(100))],
    vec![("type", Value::Text("view".into())),  ("ts", Value::Integer(101))],
])?;

// Update all matching rows -- returns count updated
let updated = db.update_where(
    "events",
    &[Filter::eq("type", "click")],
    &[("type", Value::Text("tap".into()))],
)?;

// Delete all matching rows -- returns count deleted
let deleted = db.delete_where("events", &[Filter::lt("ts", 101i64)])?;
```

## Transactions

### Callback style
```rust
db.transaction(|tx| {
    let id = tx.insert("accounts", &[("balance", Value::Integer(1000))])?;
    tx.update("accounts", id, &[("balance", Value::Integer(900))])?;
    Ok(id)
})?;
```

### Guard style
```rust
let mut tx = db.begin()?;
tx.insert("orders", &[("item", Value::Text("widget".into()))])?;
tx.insert("ledger", &[("amount", Value::Integer(-50))])?;
tx.commit()?;  // atomic commit; drop without commit = rollback
```

Enable `db.set_acid(true)` for true all-or-nothing semantics across tables. See `skills/configure-database.md` for details.
