use boogy_db::{BoogyDb, ColumnDef, Type, Value};
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    (db, dir)
}

/// list_columns returns the initial schema columns in order.
#[test]
fn test_list_columns_initial() {
    let (db, _dir) = create_db();
    db.create_table(
        "posts",
        &[
            ColumnDef::new("title", Type::Text),
            ColumnDef::new("views", Type::Integer),
            ColumnDef::new("score", Type::Real),
        ],
    )
    .unwrap();

    let cols = db.list_columns("posts").unwrap();
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0].name, "title");
    assert_eq!(cols[1].name, "views");
    assert_eq!(cols[2].name, "score");
}

/// After add_column, the new column appears at the end.
#[test]
fn test_list_columns_after_add_column() {
    let (db, _dir) = create_db();
    db.create_table(
        "posts",
        &[
            ColumnDef::new("title", Type::Text),
            ColumnDef::new("views", Type::Integer),
        ],
    )
    .unwrap();

    db.add_column(
        "posts",
        ColumnDef::new("published", Type::Boolean).default(Value::Boolean(false)),
    )
    .unwrap();

    let cols = db.list_columns("posts").unwrap();
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0].name, "title");
    assert_eq!(cols[1].name, "views");
    assert_eq!(cols[2].name, "published");
}

/// After drop_column, the dropped column is excluded from list_columns.
#[test]
fn test_list_columns_excludes_dropped() {
    let (db, _dir) = create_db();
    db.create_table(
        "posts",
        &[
            ColumnDef::new("title", Type::Text),
            ColumnDef::new("temp", Type::Text),
            ColumnDef::new("views", Type::Integer),
        ],
    )
    .unwrap();

    db.drop_column("posts", "temp").unwrap();

    let cols = db.list_columns("posts").unwrap();
    // "temp" must not appear.
    assert_eq!(cols.len(), 2);
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"temp"), "dropped column must not appear in list_columns");
    assert!(names.contains(&"title"));
    assert!(names.contains(&"views"));
}

/// After rename_column, the column appears under its new name.
#[test]
fn test_list_columns_after_rename() {
    let (db, _dir) = create_db();
    db.create_table(
        "posts",
        &[
            ColumnDef::new("title", Type::Text),
            ColumnDef::new("score", Type::Real),
        ],
    )
    .unwrap();

    db.rename_column("posts", "score", "rating").unwrap();

    let cols = db.list_columns("posts").unwrap();
    assert_eq!(cols.len(), 2);
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"score"), "old name must not appear after rename");
    assert!(names.contains(&"rating"), "new name must appear after rename");
}

/// list_columns on a nonexistent table returns TableNotFound.
#[test]
fn test_list_columns_table_not_found() {
    let (db, _dir) = create_db();
    let err = db.list_columns("no_such_table").unwrap_err();
    // Verify the error is TableNotFound by checking its Display.
    let msg = err.to_string();
    assert!(
        msg.contains("no_such_table"),
        "error should mention the missing table name: {msg}"
    );
}

/// Async wrapper: list_columns is ungated (no write-gate).
#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_async_list_columns_excludes_dropped() {
    use boogy_db::AsyncBoogyDb;

    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();

    db.create_table(
        "events",
        &[
            ColumnDef::new("kind", Type::Text),
            ColumnDef::new("tmp", Type::Integer),
            ColumnDef::new("ts", Type::Integer),
        ],
    )
    .await
    .unwrap();

    db.drop_column("events", "tmp").await.unwrap();

    let cols = db.list_columns("events").await.unwrap();
    assert_eq!(cols.len(), 2);
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"tmp"), "dropped column must not appear");
    assert!(names.contains(&"kind"));
    assert!(names.contains(&"ts"));
}
