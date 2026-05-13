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

    let row = db.get("users", id).unwrap().unwrap();
    assert_eq!(row.id, id);
    let name = row.columns.iter().find(|(n, _)| n == "name").unwrap();
    assert_eq!(name.1, Value::Text("alice".into()));
}

#[test]
fn test_get_not_found() {
    let (db, _dir) = create_db();
    db.create_table("users", &[ColumnDef::new("name", Type::Text)])
        .unwrap();
    assert!(db.get("users", 999999).unwrap().is_none());
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

    db.update("users", id, &[("age", Value::Integer(31))])
        .unwrap();

    let row = db.get("users", id).unwrap().unwrap();
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
    assert!(db.delete("users", id).unwrap());
    assert!(db.get("users", id).unwrap().is_none());
    assert!(!db.delete("users", id).unwrap());
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

    let result = db
        .find(
            "posts",
            FindOptions {
                filters: vec![Filter::eq("author", "alice")],
                sort: vec![],
                limit: Some(5),
                offset: None,
                include_total: true,
            },
        )
        .unwrap();

    assert_eq!(result.total.unwrap(), 10); // 10 by alice
    assert_eq!(result.rows.len(), 5); // limited to 5
}

#[test]
fn test_find_with_sort_and_pagination() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("value", Type::Integer)])
        .unwrap();

    for i in 0..10 {
        db.insert("items", &[("value", Value::Integer(i))]).unwrap();
    }

    let result = db
        .find(
            "items",
            FindOptions {
                filters: vec![],
                sort: vec![Sort::desc("value")],
                limit: Some(3),
                offset: Some(2),
                include_total: true,
            },
        )
        .unwrap();

    assert_eq!(result.total.unwrap(), 10);
    assert_eq!(result.rows.len(), 3);
    // Descending: 9,8,7,6,5,4,3,2,1,0 -> skip 2 -> 7,6,5
    let values: Vec<i64> = result.rows
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
    for (i, &id) in ids.iter().enumerate().step_by(50) {
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
    assert!(db.get("nonexistent", 0).is_err());
}

#[test]
fn test_concurrent_reads() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();

    let db = std::sync::Arc::new(db);
    for i in 0..100 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    let mut handles = Vec::new();
    for _ in 0..4 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                let count = db.count("t", &[]).unwrap();
                assert_eq!(count, 100);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn test_concurrent_different_tables() {
    let (db, _dir) = create_db();
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    db.create_table("b", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();

    let db = std::sync::Arc::new(db);
    let mut handles = Vec::new();

    // Thread 1: writes to table "a"
    let db1 = db.clone();
    handles.push(std::thread::spawn(move || {
        for i in 0..200 {
            db1.insert("a", &[("v", Value::Integer(i))]).unwrap();
        }
    }));

    // Thread 2: writes to table "b" concurrently
    let db2 = db.clone();
    handles.push(std::thread::spawn(move || {
        for i in 0..200 {
            db2.insert("b", &[("v", Value::Integer(i))]).unwrap();
        }
    }));

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(db.count("a", &[]).unwrap(), 200);
    assert_eq!(db.count("b", &[]).unwrap(), 200);
}

// --- Secondary index tests ---

#[test]
fn test_index_speeds_up_find() {
    let (db, _dir) = create_db();
    db.create_table(
        "posts",
        &[
            ColumnDef::new("author", Type::Text),
            ColumnDef::new("title", Type::Text),
        ],
    )
    .unwrap();

    for i in 0..1000 {
        db.insert(
            "posts",
            &[
                ("author", Value::Text(format!("user_{}", i % 10))),
                ("title", Value::Text(format!("post_{i}"))),
            ],
        )
        .unwrap();
    }

    db.create_index("posts", "idx_author", "author").unwrap();

    let result = db
        .find(
            "posts",
            FindOptions {
                filters: vec![Filter::eq("author", "user_5")],
                include_total: true,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(result.total.unwrap(), 100);
    assert_eq!(result.rows.len(), 100);
}

#[test]
fn test_index_maintained_on_insert() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[ColumnDef::new("v", Type::Text)],
    )
    .unwrap();

    db.create_index("t", "idx_v", "v").unwrap();

    for i in 0..50 {
        db.insert("t", &[("v", Value::Text(format!("val_{}", i % 5)))]).unwrap();
    }

    let result = db
        .find(
            "t",
            FindOptions {
                filters: vec![Filter::eq("v", "val_0")],
                include_total: true,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(result.total.unwrap(), 10);
    assert_eq!(result.rows.len(), 10);
}

#[test]
fn test_index_maintained_on_update() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[ColumnDef::new("v", Type::Text)],
    )
    .unwrap();

    let id = db.insert("t", &[("v", Value::Text("old".into()))]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();

    db.update("t", id, &[("v", Value::Text("new".into()))]).unwrap();

    let count_old = db.count("t", &[Filter::eq("v", "old")]).unwrap();
    let count_new = db.count("t", &[Filter::eq("v", "new")]).unwrap();
    assert_eq!(count_old, 0);
    assert_eq!(count_new, 1);
}

#[test]
fn test_index_maintained_on_delete() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[ColumnDef::new("v", Type::Text)],
    )
    .unwrap();

    let id = db.insert("t", &[("v", Value::Text("hello".into()))]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();

    let count = db.count("t", &[Filter::eq("v", "hello")]).unwrap();
    assert_eq!(count, 1);

    db.delete("t", id).unwrap();

    let count = db.count("t", &[Filter::eq("v", "hello")]).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_drop_index() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[ColumnDef::new("v", Type::Text)],
    )
    .unwrap();

    db.create_index("t", "idx_v", "v").unwrap();
    db.drop_index("t", "idx_v").unwrap();
    assert!(db.drop_index("t", "idx_v").is_err());
}

#[test]
fn test_index_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table(
            "t",
            &[ColumnDef::new("v", Type::Text)],
        )
        .unwrap();
        for i in 0..100 {
            db.insert("t", &[("v", Value::Text(format!("val_{}", i % 5)))]).unwrap();
        }
        db.create_index("t", "idx_v", "v").unwrap();
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        let count = db.count("t", &[Filter::eq("v", "val_2")]).unwrap();
        assert_eq!(count, 20);
    }
}

#[test]
fn test_duplicate_index_rejected() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Text)]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();
    assert!(db.create_index("t", "idx_v", "v").is_err());
}

// --- Path traversal prevention tests ---

#[test]
fn test_path_traversal_rejected() {
    assert!(BoogyDb::open("../etc/passwd.boogy").is_err());
    assert!(BoogyDb::open("/tmp/safe/../../etc/passwd").is_err());
}

#[test]
fn test_valid_path_accepted() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    assert!(BoogyDb::open(&path).is_ok());
}

// --- Bulk operation tests ---

#[test]
fn test_delete_where() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    for i in 0..20 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }
    let deleted = db
        .delete_where("t", &[Filter::lt("v", 10i64)])
        .unwrap();
    assert_eq!(deleted, 10);
    assert_eq!(db.count("t", &[]).unwrap(), 10);
}

#[test]
fn test_insert_many() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..100)
        .map(|i| vec![("v", Value::Integer(i))])
        .collect();
    let ids = db.insert_many("t", &rows).unwrap();
    assert_eq!(ids.len(), 100);
    assert_eq!(db.count("t", &[]).unwrap(), 100);
}

#[test]
fn test_update_where() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[
            ColumnDef::new("category", Type::Text),
            ColumnDef::new("status", Type::Text),
        ],
    )
    .unwrap();
    for i in 0..30 {
        let cat = if i % 3 == 0 { "a" } else { "b" };
        db.insert(
            "t",
            &[
                ("category", Value::Text(cat.into())),
                ("status", Value::Text("active".into())),
            ],
        )
        .unwrap();
    }

    let updated = db
        .update_where(
            "t",
            &[Filter::eq("category", "a")],
            &[("status", Value::Text("archived".into()))],
        )
        .unwrap();

    assert_eq!(updated, 10);
    let count = db.count("t", &[Filter::eq("status", "archived")]).unwrap();
    assert_eq!(count, 10);
}

#[test]
fn test_insert_many_with_index() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Text)])
        .unwrap();
    db.create_index("t", "idx_v", "v").unwrap();

    let rows: Vec<Vec<(&str, Value)>> = (0..100)
        .map(|i| vec![("v", Value::Text(format!("val_{}", i % 5)))])
        .collect();
    let ids = db.insert_many("t", &rows).unwrap();
    assert_eq!(ids.len(), 100);

    let count = db.count("t", &[Filter::eq("v", "val_3")]).unwrap();
    assert_eq!(count, 20);
}

#[test]
fn test_delete_where_with_index() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Text)])
        .unwrap();
    db.create_index("t", "idx_v", "v").unwrap();

    for i in 0..50 {
        db.insert("t", &[("v", Value::Text(format!("val_{}", i % 5)))]).unwrap();
    }

    let deleted = db
        .delete_where("t", &[Filter::eq("v", "val_2")])
        .unwrap();
    assert_eq!(deleted, 10);
    assert_eq!(db.count("t", &[]).unwrap(), 40);
    assert_eq!(db.count("t", &[Filter::eq("v", "val_2")]).unwrap(), 0);
}
