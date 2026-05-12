use boogy_db::*;
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    (db, dir)
}

#[test]
fn test_create_table_and_insert() {
    let (db, _dir) = create_db();
    db.create_table(
        "users",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("age", Type::Integer),
        ],
    )
    .unwrap();

    let id = db
        .insert(
            "users",
            &[
                ("name", Value::Text("alice".into())),
                ("age", Value::Integer(30)),
            ],
        )
        .unwrap();

    let row = db.get("users", &id).unwrap().unwrap();
    assert_eq!(row.id, id);
    let name = row.columns.iter().find(|(n, _)| n == "name").unwrap();
    assert_eq!(name.1, Value::Text("alice".into()));
}

#[test]
fn test_get_not_found() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)])
        .unwrap();
    assert!(db.get("users", "nonexistent").unwrap().is_none());
}

#[test]
fn test_update() {
    let (db, _dir) = create_db();
    db.create_table(
        "users",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("age", Type::Integer),
        ],
    )
    .unwrap();

    let id = db
        .insert(
            "users",
            &[
                ("name", Value::Text("alice".into())),
                ("age", Value::Integer(30)),
            ],
        )
        .unwrap();

    db.update("users", &id, &[("age", Value::Integer(31))])
        .unwrap();

    let row = db.get("users", &id).unwrap().unwrap();
    let age = row.columns.iter().find(|(n, _)| n == "age").unwrap();
    assert_eq!(age.1, Value::Integer(31));
}

#[test]
fn test_delete() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)])
        .unwrap();

    let id = db
        .insert("users", &[("name", Value::Text("alice".into()))])
        .unwrap();
    assert!(db.delete("users", &id).unwrap());
    assert!(db.get("users", &id).unwrap().is_none());
    assert!(!db.delete("users", &id).unwrap());
}

#[test]
fn test_find_with_filter() {
    let (db, _dir) = create_db();
    db.create_table(
        "posts",
        &[
            ColumnDef::new("author", Type::Text),
            ColumnDef::new("title", Type::Text),
        ],
    )
    .unwrap();

    for i in 0..20 {
        let author = if i % 2 == 0 { "alice" } else { "bob" };
        db.insert(
            "posts",
            &[
                ("author", Value::Text(author.into())),
                ("title", Value::Text(format!("Post {i}"))),
            ],
        )
        .unwrap();
    }

    let (rows, total) = db
        .find(
            "posts",
            FindOptions {
                filters: vec![Filter::eq("author", "alice")],
                sort: vec![],
                limit: Some(5),
                offset: None,
            },
        )
        .unwrap();

    assert_eq!(total, 10); // 10 by alice
    assert_eq!(rows.len(), 5); // limited to 5
}

#[test]
fn test_find_with_sort_and_pagination() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("value", Type::Integer)])
        .unwrap();

    for i in 0..10 {
        db.insert("items", &[("value", Value::Integer(i))]).unwrap();
    }

    let (rows, total) = db
        .find(
            "items",
            FindOptions {
                filters: vec![],
                sort: vec![Sort::desc("value")],
                limit: Some(3),
                offset: Some(2),
            },
        )
        .unwrap();

    assert_eq!(total, 10);
    assert_eq!(rows.len(), 3);
    // Descending: 9,8,7,6,5,4,3,2,1,0 -> skip 2 -> 7,6,5
    let values: Vec<i64> = rows
        .iter()
        .filter_map(|r| {
            r.columns
                .iter()
                .find(|(n, _)| n == "value")
                .map(|(_, v)| if let Value::Integer(i) = v { *i } else { -1 })
        })
        .collect();
    assert_eq!(values, vec![7, 6, 5]);
}

#[test]
fn test_count() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("category", Type::Text)])
        .unwrap();

    for i in 0..30 {
        let cat = format!("cat_{}", i % 3);
        db.insert("items", &[("category", Value::Text(cat))])
            .unwrap();
    }

    assert_eq!(db.count("items", &[]).unwrap(), 30);
    assert_eq!(
        db.count("items", &[Filter::eq("category", "cat_0")])
            .unwrap(),
        10
    );
}

#[test]
fn test_transaction() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)])
        .unwrap();
    db.create_table("posts", &[ColumnDef::new("title", Type::Text)])
        .unwrap();

    db.transaction(|tx| {
        tx.insert("users", &[("name", Value::Text("alice".into()))])?;
        tx.insert("posts", &[("title", Value::Text("hello".into()))])?;
        Ok(())
    })
    .unwrap();

    assert_eq!(db.count("users", &[]).unwrap(), 1);
    assert_eq!(db.count("posts", &[]).unwrap(), 1);
}

#[test]
fn test_many_inserts() {
    let (db, _dir) = create_db();
    db.create_table("data", &[ColumnDef::new("value", Type::Integer)])
        .unwrap();

    let mut ids = Vec::new();
    for i in 0..500 {
        let id = db
            .insert("data", &[("value", Value::Integer(i))])
            .unwrap();
        ids.push(id);
    }

    assert_eq!(db.count("data", &[]).unwrap(), 500);

    // Spot check
    for (i, id) in ids.iter().enumerate().step_by(50) {
        let row = db.get("data", id).unwrap().unwrap();
        let val = row.columns.iter().find(|(n, _)| n == "value").unwrap();
        assert_eq!(val.1, Value::Integer(i as i64));
    }
}

#[test]
fn test_duplicate_table_rejected() {
    let (db, _dir) = create_db();
    db.create_table("t", &[]).unwrap();
    assert!(db.create_table("t", &[]).is_err());
}

#[test]
fn test_table_not_found() {
    let (db, _dir) = create_db();
    assert!(db.insert("nonexistent", &[]).is_err());
    assert!(db.get("nonexistent", "id").is_err());
}
