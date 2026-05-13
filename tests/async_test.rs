#![cfg(feature = "tokio")]

use boogy_db::*;
use tempfile::TempDir;

#[tokio::test]
async fn test_async_insert_and_get() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)])
        .await
        .unwrap();
    let id = db
        .insert("users", &[("name", Value::Text("Alice".into()))])
        .await
        .unwrap();
    let row = db.get("users", id).await.unwrap().unwrap();
    assert_eq!(row.get("name").unwrap(), Value::Text("Alice".into()));
}

#[tokio::test]
async fn test_async_find_and_count() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .await
        .unwrap();
    for i in 0..10 {
        db.insert("t", &[("v", Value::Integer(i))]).await.unwrap();
    }
    let count = db.count("t", &[]).await.unwrap();
    assert_eq!(count, 10);

    let result = db
        .find(
            "t",
            FindOptions {
                filters: vec![Filter::gt("v", 5i64)],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 4);
}

#[tokio::test]
async fn test_async_update_and_delete() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .await
        .unwrap();
    let id = db.insert("t", &[("v", Value::Integer(1))]).await.unwrap();
    db.update("t", id, &[("v", Value::Integer(2))])
        .await
        .unwrap();
    let row = db.get("t", id).await.unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(2));
    db.delete("t", id).await.unwrap();
    assert!(db.get("t", id).await.unwrap().is_none());
}

#[tokio::test]
async fn test_async_clone_shared() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .await
        .unwrap();

    let db2 = db.clone();
    db.insert("t", &[("v", Value::Integer(1))]).await.unwrap();
    let count = db2.count("t", &[]).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_async_encrypted_table() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    let key = [0x42u8; 32];
    db.create_table_encrypted("secrets", &[ColumnDef::new("v", Type::Text)], &key)
        .await
        .unwrap();
    let id = db
        .insert("secrets", &[("v", Value::Text("secret".into()))])
        .await
        .unwrap();
    let row = db.get("secrets", id).await.unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Text("secret".into()));
}

#[tokio::test]
async fn test_async_bulk_ops() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .await
        .unwrap();

    let rows: Vec<Vec<(&str, Value)>> = (0..100).map(|i| vec![("v", Value::Integer(i))]).collect();
    let ids = db.insert_many("t", &rows).await.unwrap();
    assert_eq!(ids.len(), 100);

    let deleted = db
        .delete_where("t", &[Filter::lt("v", 50i64)])
        .await
        .unwrap();
    assert_eq!(deleted, 50);
    assert_eq!(db.count("t", &[]).await.unwrap(), 50);
}

#[tokio::test]
async fn test_async_begin_commit() {
    let dir = TempDir::new().unwrap();
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy")).await.unwrap();
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)])
        .await
        .unwrap();

    let tx = db.begin().await.unwrap();
    tx.insert("a", &[("v", Value::Integer(1))]).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(db.count("a", &[]).await.unwrap(), 1);
}
