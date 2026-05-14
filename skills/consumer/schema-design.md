# Designing a Schema for boogy-db

Guide for application developers designing tables, columns, and indexes.

## Column Types

| Type | Rust | Use for |
|------|------|---------|
| `Type::Text` | `String` | Names, emails, URLs, JSON blobs |
| `Type::Integer` | `i64` | IDs, counters, timestamps (Unix epoch) |
| `Type::Real` | `f64` | Prices, coordinates, measurements |
| `Type::Blob` | `Vec<u8>` | Binary data, hashes, serialized structs |
| `Type::Boolean` | `bool` | Flags, soft deletes, feature toggles |

Use `ColumnDef::new("name", Type::Text)` with optional `.not_null()` and `.unique()` modifiers.

## Indexes

Create an index on a column when you frequently filter by equality on it:

```rust
db.create_index("users", "idx_users_email", "email")?;
```

**When to index**: Columns used in `Filter::eq()` queries that run often (e.g., lookup by email, filter by status).

**When NOT to index**: Columns rarely filtered on, tables with very few rows, or write-heavy tables where insert throughput matters more than read speed.

**Trade-off**: Indexes speed up `find()` and `count()` but slow down `insert`, `update`, and `delete` because each write must maintain the index B+ tree.

## Naming Conventions

- **Tables**: lowercase snake_case, plural nouns -- `users`, `blog_posts`, `order_items`
- **Columns**: lowercase snake_case -- `created_at`, `user_id`, `is_active`
- **Indexes**: `idx_{table}_{column}` -- `idx_users_email`, `idx_posts_author_id`

## Schema Migrations

boogy-db has no ALTER TABLE. To change a schema:

```rust
// 1. Create the new table
db.create_table("users_v2", &new_columns)?;

// 2. Migrate data
let result = db.find("users", FindOptions::default())?;
for row in &result.rows {
    let name = row.get("name").unwrap_or(Value::Null);
    // Transform/add columns as needed
    db.insert("users_v2", &[("name", name), ("role", Value::Text("member".into()))])?;
}

// 3. Drop the old table
db.drop_table("users")?;
```

For zero-downtime migrations, build the migration into your app startup before serving requests.

## System Page Limit

The system page (table registry) is limited to a single 4KB page. This constrains the total metadata size -- table names, column definitions, and index names all share this space. Expect approximately 20-30 tables with typical schemas (3-5 columns, 1-2 indexes per table). If you approach this limit, use shorter table/column/index names.

## Row Size Limit

Each row must fit within a single 4096-byte page minus overhead (~4068 bytes usable). Keep rows compact:

- Avoid storing large blobs inline -- store a file path or external key instead
- Long text fields (articles, descriptions) may need to be truncated or stored externally
- A row with 10 short text columns (~50 bytes each) + a few integers is well within limits

## Example: Social App Schema

```rust
use boogy_db::{BoogyDb, ColumnDef, Type, Value, Filter, FindOptions, Sort};

let db = BoogyDb::open("social.boogy")?;

// Users table
db.create_table("users", &[
    ColumnDef::new("username", Type::Text).not_null().unique(),
    ColumnDef::new("email", Type::Text).not_null(),
    ColumnDef::new("bio", Type::Text),
    ColumnDef::new("created_at", Type::Integer).not_null(),
])?;
db.create_index("users", "idx_users_email", "email")?;

// Posts table -- user_id is the rowid of the author
db.create_table("posts", &[
    ColumnDef::new("user_id", Type::Integer).not_null(),
    ColumnDef::new("title", Type::Text).not_null(),
    ColumnDef::new("body", Type::Text),
    ColumnDef::new("published", Type::Boolean).not_null(),
    ColumnDef::new("created_at", Type::Integer).not_null(),
])?;
db.create_index("posts", "idx_posts_user_id", "user_id")?;

// Insert a user
let user_id = db.insert("users", &[
    ("username", Value::Text("alice".into())),
    ("email", Value::Text("alice@example.com".into())),
    ("bio", Value::Text("Rust enthusiast".into())),
    ("created_at", Value::Integer(1715500000)),
])?;

// Insert a post by that user
let post_id = db.insert("posts", &[
    ("user_id", Value::Integer(user_id as i64)),
    ("title", Value::Text("Hello World".into())),
    ("body", Value::Text("My first post!".into())),
    ("published", Value::Boolean(true)),
    ("created_at", Value::Integer(1715500100)),
])?;

// Find all posts by a user (uses idx_posts_user_id)
let posts = db.find("posts", FindOptions {
    filters: vec![Filter::eq("user_id", user_id as i64)],
    sort: vec![Sort::desc("created_at")],
    ..Default::default()
})?;
```
