# Using boogy-db in Async Applications

How to use boogy-db with tokio-based async runtimes.

## Enable the Feature

```toml
# Cargo.toml
[dependencies]
boogy-db = { version = "0.1", features = ["tokio"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## Basic Usage

`AsyncBoogyDb` is a zero-cost async wrapper. Methods are `async fn` but delegate directly to the synchronous core -- no `spawn_blocking`, no thread dispatch.

```rust
use boogy_db::{AsyncBoogyDb, ColumnDef, Type, Value, Filter, FindOptions, Durability};

#[tokio::main]
async fn main() -> boogy_db::Result<()> {
    let db = AsyncBoogyDb::open("app.boogy").await?;
    db.set_durability(Durability::Immediate);

    db.create_table("users", &[
        ColumnDef::new("name", Type::Text).not_null(),
        ColumnDef::new("email", Type::Text).not_null(),
    ]).await?;

    let id = db.insert("users", &[
        ("name", Value::Text("Alice".into())),
        ("email", Value::Text("alice@example.com".into())),
    ]).await?;

    let user = db.get("users", id).await?;
    Ok(())
}
```

## Sharing Across Tasks

`AsyncBoogyDb` implements `Clone` (wraps an `Arc<BoogyDb>` internally). Clone it freely and move into tasks:

```rust
let db = AsyncBoogyDb::open("app.boogy").await?;

let db2 = db.clone();
let handle = tokio::spawn(async move {
    db2.insert("events", &[("type", Value::Text("bg_task".into()))]).await
});

// Main task continues using db
let count = db.count("events", &[]).await?;
handle.await??;
```

No `Arc<AsyncBoogyDb>` needed -- the Arc is already inside.

## Integration with Axum

Store the database handle in application state:

```rust
use axum::{Router, extract::State, routing::get, Json};
use boogy_db::{AsyncBoogyDb, Value, FindOptions};

#[derive(Clone)]
struct AppState {
    db: AsyncBoogyDb,
}

async fn list_users(State(state): State<AppState>) -> Json<Vec<String>> {
    let result = state.db.find("users", FindOptions::default()).await.unwrap();
    let names: Vec<String> = result.rows.iter().filter_map(|r| {
        match r.get("name") {
            Some(Value::Text(s)) => Some(s),
            _ => None,
        }
    }).collect();
    Json(names)
}

#[tokio::main]
async fn main() {
    let db = AsyncBoogyDb::open("app.boogy").await.unwrap();
    let state = AppState { db };
    let app = Router::new()
        .route("/users", get(list_users))
        .with_state(state);
    // axum::serve(...)
}
```

For actix-web, wrap in `web::Data<AsyncBoogyDb>` the same way.

## Async Transactions

```rust
let db = AsyncBoogyDb::open("app.boogy").await?;
db.set_acid(true);

// Guard-based
let mut tx = db.begin().await?;
tx.insert("orders", &[("total", Value::Integer(100))]).await?;
tx.insert("ledger", &[("amount", Value::Integer(-100))]).await?;
tx.commit().await?;

// Callback-based (note: callback body is synchronous)
db.transaction(|tx| {
    tx.insert("log", &[("msg", Value::Text("done".into()))])?;
    Ok(())
}).await?;
```

## spawn_blocking and Sync Access

Almost never needed. Consider `spawn_blocking` only for opening very large databases (WAL replay does disk I/O). Access the sync handle with `async_db.inner()`.
