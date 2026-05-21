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
    assert_eq!(row.get("name").unwrap(), Value::Text("alice".into()));
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
    assert_eq!(row.get("age").unwrap(), Value::Integer(31));
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
                or_groups: vec![],
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
                or_groups: vec![],
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
            r.get("value").map(|v| if let Value::Integer(i) = v { i } else { -1 })
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
        assert_eq!(row.get("value").unwrap(), Value::Integer(i as i64));
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

// --- Security: null byte in path ---

#[test]
fn test_null_byte_in_path_rejected() {
    // std::path::Path handles this at the OS level -- paths with null bytes
    // will either be rejected by the OS or by our validate_path function.
    let result = BoogyDb::open("/tmp/test\0evil.boogy");
    assert!(result.is_err(), "null byte in path should be rejected");
}

// --- Security: empty table name ---

#[test]
fn test_empty_table_name() {
    let (db, _dir) = create_db();
    // Empty table name -- should work at the API level but is weird
    let result = db.create_table("", &[ColumnDef::new("v", Type::Integer)]);
    // The API doesn't explicitly reject empty names but it should work without panicking
    if result.is_ok() {
        db.insert("", &[("v", Value::Integer(1))]).unwrap();
        assert_eq!(db.count("", &[]).unwrap(), 1);
    }
}

// --- Security: insert into nonexistent table ---

#[test]
fn test_insert_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    let result = db.insert("does_not_exist", &[("v", Value::Integer(1))]);
    assert!(result.is_err());
}

#[test]
fn test_get_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    assert!(db.get("does_not_exist", 1).is_err());
}

#[test]
fn test_delete_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    assert!(db.delete("does_not_exist", 1).is_err());
}

#[test]
fn test_update_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    assert!(db.update("does_not_exist", 1, &[]).is_err());
}

#[test]
fn test_count_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    assert!(db.count("does_not_exist", &[]).is_err());
}

#[test]
fn test_find_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    assert!(db.find("does_not_exist", FindOptions::default()).is_err());
}

// --- Security: very large column values ---

#[test]
fn test_large_text_value_no_crash() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Text)])
        .unwrap();

    // Text value near page capacity (~3500 bytes fits in a page with header overhead).
    // Row layout: rowid(8) + num_cols(2) + offset_dir(4) + tag(1) + len(4) + text_bytes
    // Max usable per row in a page ≈ 4096 - 16(header) - 2(offset_array) - 4(checksum) = 4074
    let big_text = "x".repeat(3500);
    let id = db
        .insert("t", &[("v", Value::Text(big_text.clone()))])
        .unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    let retrieved = row.get("v").unwrap();
    if let Value::Text(s) = retrieved {
        assert_eq!(s.len(), 3500);
    } else {
        panic!("expected Text value");
    }
}

#[test]
fn test_large_blob_value_no_crash() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Blob)])
        .unwrap();

    let big_blob = vec![0xABu8; 3500];
    let id = db
        .insert("t", &[("v", Value::Blob(big_blob.clone()))])
        .unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    let retrieved = row.get("v").unwrap();
    if let Value::Blob(b) = retrieved {
        assert_eq!(b.len(), 3500);
    } else {
        panic!("expected Blob value");
    }
}

// --- Security: many columns ---

#[test]
fn test_many_columns_no_crash() {
    let (db, _dir) = create_db();
    let cols: Vec<ColumnDef> = (0..100)
        .map(|i| ColumnDef::new(format!("col_{i}"), Type::Integer))
        .collect();
    db.create_table("t", &cols).unwrap();

    // Insert a row with all columns set
    let data: Vec<(&str, Value)> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), Value::Integer(i as i64)))
        .collect();
    let id = db.insert("t", &data).unwrap();

    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("col_0").unwrap(), Value::Integer(0));
    assert_eq!(row.get("col_99").unwrap(), Value::Integer(99));
}

// --- Security: integer overflow / boundary rowid ---

#[test]
fn test_insert_with_max_rowid() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();

    // Insert with a very large rowid
    db.insert_with_id("t", u64::MAX - 1, &[("v", Value::Integer(42))])
        .unwrap();

    let row = db.get("t", u64::MAX - 1).unwrap().unwrap();
    assert_eq!(row.id, u64::MAX - 1);
    assert_eq!(row.get("v").unwrap(), Value::Integer(42));
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

// ===========================================================================
// Durability / Recovery tests
// ===========================================================================

#[test]
fn test_durability_normal_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::Normal);
        db.create_table(
            "t",
            &[
                ColumnDef::new("name", Type::Text),
                ColumnDef::new("age", Type::Integer),
            ],
        )
        .unwrap();
        db.insert(
            "t",
            &[
                ("name", Value::Text("alice".into())),
                ("age", Value::Integer(30)),
            ],
        )
        .unwrap();
        db.insert(
            "t",
            &[
                ("name", Value::Text("bob".into())),
                ("age", Value::Integer(25)),
            ],
        )
        .unwrap();
        // Drop triggers clean shutdown with flush
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 2);
        let row = db.get("t", 1).unwrap().unwrap();
        assert_eq!(row.get("name").unwrap(), Value::Text("alice".into()));
        assert_eq!(row.get("age").unwrap(), Value::Integer(30));
    }
}

#[test]
fn test_durability_immediate_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::Immediate);
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
            .unwrap();
        for i in 0..10 {
            db.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 10);
        for i in 1..=10u64 {
            let row = db.get("t", i).unwrap().unwrap();
            assert_eq!(row.get("v").unwrap(), Value::Integer(i as i64 - 1));
        }
    }
}

#[test]
fn test_durability_none_no_crash_on_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
            .unwrap();
        db.insert("t", &[("v", Value::Integer(1))]).unwrap();
        // Data may or may not persist with Durability::None, but reopen must not crash
    }
    {
        // This must not crash regardless of what was flushed
        let _db = BoogyDb::open(&path).unwrap();
        // We don't assert the count because Durability::None makes no guarantees
    }
}

#[test]
fn test_index_data_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("category", Type::Text)])
            .unwrap();
        db.create_index("t", "idx_category", "category").unwrap();
        for i in 0..50 {
            db.insert(
                "t",
                &[("category", Value::Text(format!("cat_{}", i % 5)))],
            )
            .unwrap();
        }
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        // Index should still be usable for queries
        let count = db
            .count("t", &[Filter::eq("category", "cat_3")])
            .unwrap();
        assert_eq!(count, 10);

        // Find via index path
        let result = db
            .find(
                "t",
                FindOptions {
                    filters: vec![Filter::eq("category", "cat_0")],
                    include_total: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.total, Some(10));
        assert_eq!(result.rows.len(), 10);
    }
}

#[test]
fn test_multiple_reopen_cycles_with_writes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");

    // Cycle 1: create table and insert
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
            .unwrap();
        for i in 0..10 {
            db.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
    }

    // Cycle 2: reopen, verify, insert more
    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 10);
        for i in 10..20 {
            db.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
    }

    // Cycle 3: reopen, verify, delete some
    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 20);
        db.delete("t", 1).unwrap();
        db.delete("t", 2).unwrap();
    }

    // Cycle 4: reopen, verify, update
    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 18);
        db.update("t", 3, &[("v", Value::Integer(999))]).unwrap();
    }

    // Cycle 5: final verification
    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 18);
        let row = db.get("t", 3).unwrap().unwrap();
        assert_eq!(row.get("v").unwrap(), Value::Integer(999));
        // Deleted rows should be gone
        assert!(db.get("t", 1).unwrap().is_none());
        assert!(db.get("t", 2).unwrap().is_none());
    }
}

#[test]
fn test_reopen_preserves_multiple_tables_and_indexes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table(
            "users",
            &[
                ColumnDef::new("name", Type::Text),
                ColumnDef::new("email", Type::Text),
            ],
        )
        .unwrap();
        db.create_table(
            "posts",
            &[
                ColumnDef::new("title", Type::Text),
                ColumnDef::new("author_id", Type::Integer),
            ],
        )
        .unwrap();
        db.create_index("users", "idx_name", "name").unwrap();
        db.create_index("posts", "idx_author", "author_id").unwrap();

        db.insert(
            "users",
            &[
                ("name", Value::Text("alice".into())),
                ("email", Value::Text("alice@example.com".into())),
            ],
        )
        .unwrap();
        db.insert(
            "posts",
            &[
                ("title", Value::Text("Hello World".into())),
                ("author_id", Value::Integer(1)),
            ],
        )
        .unwrap();
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        // Tables should be restored
        assert_eq!(db.count("users", &[]).unwrap(), 1);
        assert_eq!(db.count("posts", &[]).unwrap(), 1);

        // Indexes should work
        let result = db
            .find(
                "users",
                FindOptions {
                    filters: vec![Filter::eq("name", "alice")],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("email").unwrap(),
            Value::Text("alice@example.com".into())
        );

        let result = db
            .find(
                "posts",
                FindOptions {
                    filters: vec![Filter::eq("author_id", 1i64)],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
    }
}

#[test]
fn test_update_where_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table(
            "t",
            &[
                ColumnDef::new("status", Type::Text),
                ColumnDef::new("v", Type::Integer),
            ],
        )
        .unwrap();
        for i in 0..20 {
            db.insert(
                "t",
                &[
                    ("status", Value::Text("active".into())),
                    ("v", Value::Integer(i)),
                ],
            )
            .unwrap();
        }
        db.update_where(
            "t",
            &[Filter::ge("v", 10i64)],
            &[("status", Value::Text("archived".into()))],
        )
        .unwrap();
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        let active = db
            .count("t", &[Filter::eq("status", "active")])
            .unwrap();
        let archived = db
            .count("t", &[Filter::eq("status", "archived")])
            .unwrap();
        assert_eq!(active, 10);
        assert_eq!(archived, 10);
    }
}

// ===========================================================================
// Additional security / edge case tests
// ===========================================================================

#[test]
fn test_drop_nonexistent_table_returns_error() {
    let (db, _dir) = create_db();
    assert!(db.drop_table("does_not_exist").is_err());
}

#[test]
fn test_create_index_on_nonexistent_column() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    let result = db.create_index("t", "idx_bad", "nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_create_index_on_nonexistent_table() {
    let (db, _dir) = create_db();
    let result = db.create_index("does_not_exist", "idx", "col");
    assert!(result.is_err());
}

#[test]
fn test_drop_index_on_nonexistent_table() {
    let (db, _dir) = create_db();
    let result = db.drop_index("does_not_exist", "idx");
    assert!(result.is_err());
}

#[test]
fn test_update_nonexistent_row_returns_false() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    let result = db.update("t", 999, &[("v", Value::Integer(1))]).unwrap();
    assert!(!result);
}

#[test]
fn test_delete_nonexistent_row_returns_false() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    let result = db.delete("t", 999).unwrap();
    assert!(!result);
}

#[test]
fn test_all_value_types_roundtrip() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[
            ColumnDef::new("text_col", Type::Text),
            ColumnDef::new("int_col", Type::Integer),
            ColumnDef::new("real_col", Type::Real),
            ColumnDef::new("blob_col", Type::Blob),
            ColumnDef::new("bool_col", Type::Boolean),
        ],
    )
    .unwrap();

    let id = db
        .insert(
            "t",
            &[
                ("text_col", Value::Text("hello".into())),
                ("int_col", Value::Integer(i64::MIN)),
                ("real_col", Value::Real(std::f64::consts::PI)),
                ("blob_col", Value::Blob(vec![0, 1, 2, 255])),
                ("bool_col", Value::Boolean(true)),
            ],
        )
        .unwrap();

    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("text_col").unwrap(), Value::Text("hello".into()));
    assert_eq!(row.get("int_col").unwrap(), Value::Integer(i64::MIN));
    assert_eq!(row.get("real_col").unwrap(), Value::Real(std::f64::consts::PI));
    assert_eq!(row.get("blob_col").unwrap(), Value::Blob(vec![0, 1, 2, 255]));
    assert_eq!(row.get("bool_col").unwrap(), Value::Boolean(true));
}

#[test]
fn test_null_values_roundtrip() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[
            ColumnDef::new("a", Type::Text),
            ColumnDef::new("b", Type::Integer),
        ],
    )
    .unwrap();

    // Insert with only one column -- the other should be absent/null
    let id = db
        .insert("t", &[("a", Value::Text("hello".into()))])
        .unwrap();

    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("a").unwrap(), Value::Text("hello".into()));
    assert!(row.get("b").is_none());
}

#[test]
fn test_find_with_multiple_filters() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[
            ColumnDef::new("category", Type::Text),
            ColumnDef::new("value", Type::Integer),
        ],
    )
    .unwrap();

    for i in 0..100 {
        db.insert(
            "t",
            &[
                ("category", Value::Text(format!("cat_{}", i % 5))),
                ("value", Value::Integer(i)),
            ],
        )
        .unwrap();
    }

    // Multi-filter: category = "cat_0" AND value >= 50
    let result = db
        .find(
            "t",
            FindOptions {
                filters: vec![
                    Filter::eq("category", "cat_0"),
                    Filter::ge("value", 50i64),
                ],
                include_total: true,
                ..Default::default()
            },
        )
        .unwrap();
    // cat_0 values: 0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90, 95
    // >= 50: 50, 55, 60, 65, 70, 75, 80, 85, 90, 95 = 10
    assert_eq!(result.total, Some(10));
    assert_eq!(result.rows.len(), 10);
}

#[test]
fn test_row_columns_method() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("age", Type::Integer),
        ],
    )
    .unwrap();

    let id = db
        .insert(
            "t",
            &[
                ("name", Value::Text("alice".into())),
                ("age", Value::Integer(30)),
            ],
        )
        .unwrap();

    let row = db.get("t", id).unwrap().unwrap();
    let cols = row.columns();
    assert_eq!(cols.len(), 2);
    // columns() returns all decoded columns
    assert!(cols.iter().any(|(name, val)| name == "name" && *val == Value::Text("alice".into())));
    assert!(cols.iter().any(|(name, val)| name == "age" && *val == Value::Integer(30)));
}

#[test]
fn test_delete_where_empty_result() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    for i in 0..10 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }
    // Filter matches nothing
    let deleted = db.delete_where("t", &[Filter::eq("v", 999i64)]).unwrap();
    assert_eq!(deleted, 0);
    assert_eq!(db.count("t", &[]).unwrap(), 10);
}

#[test]
fn test_update_where_empty_result() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    for i in 0..10 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }
    let updated = db
        .update_where("t", &[Filter::eq("v", 999i64)], &[("v", Value::Integer(0))])
        .unwrap();
    assert_eq!(updated, 0);
}

#[test]
fn test_insert_many_empty_batch() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    let ids = db.insert_many("t", &[]).unwrap();
    assert!(ids.is_empty());
    assert_eq!(db.count("t", &[]).unwrap(), 0);
}

// ===========================================================================
// Encrypted table tests
// ===========================================================================

#[test]
fn test_encrypted_table_roundtrip() {
    let (db, _dir) = create_db();
    let key: [u8; 32] = [0xAB; 32];
    db.create_table_encrypted(
        "secrets",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("value", Type::Integer),
        ],
        &key,
    )
    .unwrap();

    let id1 = db
        .insert("secrets", &[("name", Value::Text("api_key".into())), ("value", Value::Integer(42))])
        .unwrap();
    let id2 = db
        .insert("secrets", &[("name", Value::Text("token".into())), ("value", Value::Integer(99))])
        .unwrap();

    let row1 = db.get("secrets", id1).unwrap().unwrap();
    assert_eq!(row1.get("name").unwrap(), Value::Text("api_key".into()));
    assert_eq!(row1.get("value").unwrap(), Value::Integer(42));

    let row2 = db.get("secrets", id2).unwrap().unwrap();
    assert_eq!(row2.get("name").unwrap(), Value::Text("token".into()));
    assert_eq!(row2.get("value").unwrap(), Value::Integer(99));

    assert_eq!(db.count("secrets", &[]).unwrap(), 2);
}

#[test]
fn test_encrypted_table_locked_without_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let key: [u8; 32] = [0xCD; 32];

    // Create encrypted table and insert data.
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table_encrypted(
            "secrets",
            &[ColumnDef::new("v", Type::Integer)],
            &key,
        )
        .unwrap();
        db.insert("secrets", &[("v", Value::Integer(1))]).unwrap();
        db.insert("secrets", &[("v", Value::Integer(2))]).unwrap();
    }

    // Reopen without unlocking: all operations should fail with TableLocked.
    {
        let db = BoogyDb::open(&path).unwrap();

        // insert should fail
        let err = db.insert("secrets", &[("v", Value::Integer(3))]);
        assert!(err.is_err());

        // get should fail
        let err = db.get("secrets", 1);
        assert!(err.is_err());

        // count should fail
        let err = db.count("secrets", &[]);
        assert!(err.is_err());

        // find should fail
        let err = db.find("secrets", FindOptions::default());
        assert!(err.is_err());

        // Now unlock and verify everything works.
        db.unlock_table("secrets", &key).unwrap();

        assert_eq!(db.count("secrets", &[]).unwrap(), 2);
        let row = db.get("secrets", 1).unwrap().unwrap();
        assert_eq!(row.get("v").unwrap(), Value::Integer(1));
    }
}

#[test]
fn test_encrypted_table_wrong_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let key: [u8; 32] = [0xEF; 32];
    let wrong_key: [u8; 32] = [0x11; 32];

    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table_encrypted(
            "secrets",
            &[ColumnDef::new("v", Type::Integer)],
            &key,
        )
        .unwrap();
        db.insert("secrets", &[("v", Value::Integer(42))]).unwrap();
    }

    {
        let db = BoogyDb::open(&path).unwrap();
        let result = db.unlock_table("secrets", &wrong_key);
        assert!(result.is_err(), "wrong key should be rejected");

        // Correct key should work.
        db.unlock_table("secrets", &key).unwrap();
        let row = db.get("secrets", 1).unwrap().unwrap();
        assert_eq!(row.get("v").unwrap(), Value::Integer(42));
    }
}

#[test]
fn test_mixed_encrypted_unencrypted() {
    let (db, _dir) = create_db();
    let key: [u8; 32] = [0x99; 32];

    db.create_table("public", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();
    db.create_table_encrypted(
        "private",
        &[ColumnDef::new("v", Type::Integer)],
        &key,
    )
    .unwrap();

    // Both tables work independently.
    let pub_id = db.insert("public", &[("v", Value::Integer(1))]).unwrap();
    let priv_id = db.insert("private", &[("v", Value::Integer(2))]).unwrap();

    let pub_row = db.get("public", pub_id).unwrap().unwrap();
    assert_eq!(pub_row.get("v").unwrap(), Value::Integer(1));

    let priv_row = db.get("private", priv_id).unwrap().unwrap();
    assert_eq!(priv_row.get("v").unwrap(), Value::Integer(2));

    assert_eq!(db.count("public", &[]).unwrap(), 1);
    assert_eq!(db.count("private", &[]).unwrap(), 1);
}

#[test]
fn test_encrypted_table_with_index() {
    let (db, _dir) = create_db();
    let key: [u8; 32] = [0x77; 32];

    db.create_table_encrypted(
        "items",
        &[
            ColumnDef::new("category", Type::Text),
            ColumnDef::new("value", Type::Integer),
        ],
        &key,
    )
    .unwrap();
    db.create_index("items", "idx_category", "category").unwrap();

    for i in 0..50 {
        let cat = format!("cat_{}", i % 5);
        db.insert(
            "items",
            &[
                ("category", Value::Text(cat)),
                ("value", Value::Integer(i)),
            ],
        )
        .unwrap();
    }

    // Query via index.
    let result = db
        .find(
            "items",
            FindOptions {
                filters: vec![Filter::eq("category", "cat_2")],
                include_total: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(result.total.unwrap(), 10);
    assert_eq!(result.rows.len(), 10);

    // Count via index.
    let count = db
        .count("items", &[Filter::eq("category", "cat_0")])
        .unwrap();
    assert_eq!(count, 10);
}

// ===========================================================================
// Guard-based transaction tests
// ===========================================================================

#[test]
fn test_begin_commit_transaction() {
    let (db, _dir) = create_db();
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    tx.insert("a", &[("v", Value::Integer(1))]).unwrap();
    tx.insert("b", &[("v", Value::Integer(2))]).unwrap();
    tx.commit().unwrap();

    assert_eq!(db.count("a", &[]).unwrap(), 1);
    assert_eq!(db.count("b", &[]).unwrap(), 1);
}

#[test]
fn test_begin_drop_without_commit() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("v", Value::Integer(1))]).unwrap();
        // Drop without commit — operations already applied (lazy locking)
    }

    // Data persists because individual ops committed their own writes
    assert_eq!(db.count("t", &[]).unwrap(), 1);
}

#[test]
fn test_begin_read_within_transaction() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    let id = tx.insert("t", &[("v", Value::Integer(42))]).unwrap();
    let row = tx.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(42));
    tx.commit().unwrap();
}

// ===========================================================================
// ACID transaction tests
// ===========================================================================

#[test]
fn test_acid_commit() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    tx.insert("a", &[("v", Value::Integer(1))]).unwrap();
    tx.insert("b", &[("v", Value::Integer(2))]).unwrap();
    tx.commit().unwrap();

    assert_eq!(db.count("a", &[]).unwrap(), 1);
    assert_eq!(db.count("b", &[]).unwrap(), 1);
}

#[test]
fn test_acid_rollback() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    db.insert("t", &[("v", Value::Integer(1))]).unwrap();
    assert_eq!(db.count("t", &[]).unwrap(), 1);

    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("v", Value::Integer(2))]).unwrap();
        tx.insert("t", &[("v", Value::Integer(3))]).unwrap();
        // Drop without commit
    }

    assert_eq!(db.count("t", &[]).unwrap(), 1); // only the first insert survived
}

#[test]
fn test_acid_read_own_writes() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    let id = tx.insert("t", &[("v", Value::Integer(42))]).unwrap();
    let row = tx.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(42));
    tx.commit().unwrap();
}

#[test]
fn test_acid_auto_wrap_standalone() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    db.insert("t", &[("v", Value::Integer(1))]).unwrap();
    db.insert("t", &[("v", Value::Integer(2))]).unwrap();
    assert_eq!(db.count("t", &[]).unwrap(), 2);

    let row = db.get("t", 1).unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(1));
}

#[test]
fn test_acid_multi_table_rollback() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("users", &[ColumnDef::new("name", Type::Text)]).unwrap();
    db.create_table("posts", &[ColumnDef::new("title", Type::Text)]).unwrap();

    db.insert("users", &[("name", Value::Text("Alice".into()))]).unwrap();

    {
        let mut tx = db.begin().unwrap();
        tx.insert("users", &[("name", Value::Text("Bob".into()))]).unwrap();
        tx.insert("posts", &[("title", Value::Text("Hello".into()))]).unwrap();
        // Drop — neither Bob nor the post should exist
    }

    assert_eq!(db.count("users", &[]).unwrap(), 1); // only Alice
    assert_eq!(db.count("posts", &[]).unwrap(), 0);
}

#[test]
fn test_acid_update_and_delete() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let id1 = db.insert("t", &[("v", Value::Integer(1))]).unwrap();
    let id2 = db.insert("t", &[("v", Value::Integer(2))]).unwrap();

    let mut tx = db.begin().unwrap();
    tx.update("t", id1, &[("v", Value::Integer(10))]).unwrap();
    tx.delete("t", id2).unwrap();
    tx.commit().unwrap();

    let row = db.get("t", id1).unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(10));
    assert!(db.get("t", id2).unwrap().is_none());
    assert_eq!(db.count("t", &[]).unwrap(), 1);
}

#[test]
fn test_acid_rollback_update() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let id = db.insert("t", &[("v", Value::Integer(1))]).unwrap();

    {
        let mut tx = db.begin().unwrap();
        tx.update("t", id, &[("v", Value::Integer(999))]).unwrap();
        // Drop — update should be rolled back
    }

    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("v").unwrap(), Value::Integer(1)); // unchanged
}

#[test]
fn test_acid_find_within_transaction() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    for i in 0..10 {
        tx.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    let result = tx.find("t", FindOptions {
        filters: vec![Filter::gt("v", 5i64)],
        ..Default::default()
    }).unwrap();
    assert_eq!(result.rows.len(), 4); // 6, 7, 8, 9

    let count = tx.count("t", &[]).unwrap();
    assert_eq!(count, 10);

    tx.commit().unwrap();
}

#[test]
fn test_acid_doesnt_block_other_tables() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(BoogyDb::open(dir.path().join("test.boogy")).unwrap());
    db.set_acid(true);
    db.create_table("a", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_table("b", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let db1 = Arc::clone(&db);
    let h = thread::spawn(move || {
        let mut tx = db1.begin().unwrap();
        for i in 0..100 {
            tx.insert("a", &[("v", Value::Integer(i))]).unwrap();
        }
        tx.commit().unwrap();
    });

    for i in 0..100 {
        db.insert("b", &[("v", Value::Integer(i))]).unwrap();
    }

    h.join().unwrap();
    assert_eq!(db.count("a", &[]).unwrap(), 100);
    assert_eq!(db.count("b", &[]).unwrap(), 100);
}

#[test]
fn test_acid_insert_many() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut tx = db.begin().unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..50).map(|i| vec![("v", Value::Integer(i))]).collect();
    let ids = tx.insert_many("t", &rows).unwrap();
    assert_eq!(ids.len(), 50);
    tx.commit().unwrap();

    assert_eq!(db.count("t", &[]).unwrap(), 50);
}

#[test]
fn test_acid_persists_across_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");

    {
        let db = BoogyDb::open(&path).unwrap();
        db.set_acid(true);
        db.set_durability(Durability::Normal);
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("v", Value::Integer(1))]).unwrap();
        tx.insert("t", &[("v", Value::Integer(2))]).unwrap();
        tx.commit().unwrap();
    }

    {
        let db = BoogyDb::open(&path).unwrap();
        assert_eq!(db.count("t", &[]).unwrap(), 2);
    }
}

// ===========================================================================
// Overflow page tests
// ===========================================================================

#[test]
fn test_overflow_insert_get_10kb() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xABu8; 10_000];
    let id = db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
}

#[test]
fn test_overflow_insert_get_100kb() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xCDu8; 100_000];
    let id = db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
}

#[test]
fn test_overflow_insert_get_1mb() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xEFu8; 1_000_000];
    let id = db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
}

#[test]
fn test_overflow_long_text() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("content", Type::Text)]).unwrap();
    let text = "x".repeat(100_000);
    let id = db.insert("t", &[("content", Value::Text(text.clone()))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("content").unwrap(), Value::Text(text));
}

#[test]
fn test_overflow_delete() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let id = db.insert("t", &[("data", Value::Blob(vec![0xAB; 50_000]))]).unwrap();
    assert!(db.delete("t", id).unwrap());
    assert!(db.get("t", id).unwrap().is_none());
}

#[test]
fn test_overflow_mixed_with_normal_rows() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("data", Type::Blob),
    ]).unwrap();
    for i in 0..10 {
        db.insert("t", &[
            ("name", Value::Text(format!("small_{i}"))),
            ("data", Value::Blob(vec![i as u8; 100])),
        ]).unwrap();
    }
    for i in 0..5 {
        db.insert("t", &[
            ("name", Value::Text(format!("big_{i}"))),
            ("data", Value::Blob(vec![i as u8; 50_000])),
        ]).unwrap();
    }
    assert_eq!(db.count("t", &[]).unwrap(), 15);
    let row = db.get("t", 11).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(vec![0u8; 50_000]));
}

#[test]
fn test_overflow_max_row_size_enforced() {
    let (db, _dir) = create_db();
    db.set_max_row_size(1000);
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let result = db.insert("t", &[("data", Value::Blob(vec![0u8; 2000]))]);
    assert!(result.is_err());
}

#[test]
fn test_overflow_update_large_to_small() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let id = db.insert("t", &[("data", Value::Blob(vec![0xAB; 50_000]))]).unwrap();
    db.update("t", id, &[("data", Value::Blob(vec![0xCD; 100]))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(vec![0xCD; 100]));
}

#[test]
fn test_overflow_update_small_to_large() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let id = db.insert("t", &[("data", Value::Blob(vec![0xAB; 100]))]).unwrap();
    db.update("t", id, &[("data", Value::Blob(vec![0xCD; 50_000]))]).unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(vec![0xCD; 50_000]));
}

#[test]
fn test_overflow_persist_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let blob = vec![0xABu8; 50_000];
    {
        let db = BoogyDb::open(&path).unwrap();
        db.set_durability(Durability::Normal);
        db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
        db.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    }
    {
        let db = BoogyDb::open(&path).unwrap();
        let row = db.get("t", 1).unwrap().unwrap();
        assert_eq!(row.get("data").unwrap(), Value::Blob(blob));
    }
}

#[test]
fn test_overflow_with_acid() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("data", Type::Blob)]).unwrap();
    let blob = vec![0xABu8; 50_000];
    let mut tx = db.begin().unwrap();
    let id = tx.insert("t", &[("data", Value::Blob(blob.clone()))]).unwrap();
    tx.commit().unwrap();
    let row = db.get("t", id).unwrap().unwrap();
    assert_eq!(row.get("data").unwrap(), Value::Blob(blob));

    // Rollback
    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("data", Value::Blob(vec![0xCD; 50_000]))]).unwrap();
    }
    assert_eq!(db.count("t", &[]).unwrap(), 1);
}

#[test]
fn test_overflow_find_scan() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("tag", Type::Text),
        ColumnDef::new("data", Type::Blob),
    ]).unwrap();
    for i in 0..5 {
        let size = if i % 2 == 0 { 100 } else { 50_000 };
        db.insert("t", &[
            ("tag", Value::Text(format!("item_{i}"))),
            ("data", Value::Blob(vec![i as u8; size])),
        ]).unwrap();
    }
    let result = db.find("t", FindOptions::default()).unwrap();
    assert_eq!(result.rows.len(), 5);
}

// ---------------------------------------------------------------------------
// IN operator tests
// ---------------------------------------------------------------------------

#[test]
fn test_in_filter_without_index() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    for i in 1..=10 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![
            Value::Integer(2), Value::Integer(5), Value::Integer(8),
        ])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(3));
    assert_eq!(result.rows.len(), 3);
    let mut vals: Vec<i64> = result.rows.iter()
        .map(|r| match r.get("v").unwrap() { Value::Integer(i) => i, _ => panic!() })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![2, 5, 8]);
}

#[test]
fn test_in_filter_with_index() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();
    for i in 1..=20 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![
            Value::Integer(3), Value::Integer(7), Value::Integer(15), Value::Integer(20),
        ])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(4));
    assert_eq!(result.rows.len(), 4);
    let mut vals: Vec<i64> = result.rows.iter()
        .map(|r| match r.get("v").unwrap() { Value::Integer(i) => i, _ => panic!() })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![3, 7, 15, 20]);
}

#[test]
fn test_in_filter_with_text_index() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("age", Type::Integer),
    ]).unwrap();
    db.create_index("t", "idx_name", "name").unwrap();

    let names = ["alice", "bob", "charlie", "dave", "eve"];
    for (i, name) in names.iter().enumerate() {
        db.insert("t", &[
            ("name", Value::Text(name.to_string())),
            ("age", Value::Integer(20 + i as i64)),
        ]).unwrap();
    }

    let opts = FindOptions {
        filters: vec![Filter::in_list("name", vec![
            Value::Text("alice".into()),
            Value::Text("charlie".into()),
            Value::Text("eve".into()),
        ])],
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.rows.len(), 3);
    let mut found_names: Vec<String> = result.rows.iter()
        .map(|r| match r.get("name").unwrap() { Value::Text(s) => s, _ => panic!() })
        .collect();
    found_names.sort();
    assert_eq!(found_names, vec!["alice", "charlie", "eve"]);
}

#[test]
fn test_in_combined_with_other_filters() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("author_id", Type::Integer),
        ColumnDef::new("status", Type::Text),
    ]).unwrap();
    db.create_index("t", "idx_author", "author_id").unwrap();

    // Insert rows with author_id 1-5 and alternating status
    for i in 1..=20 {
        let status = if i % 2 == 0 { "active" } else { "inactive" };
        db.insert("t", &[
            ("author_id", Value::Integer(i % 5 + 1)),
            ("status", Value::Text(status.into())),
        ]).unwrap();
    }

    // author_id IN [1, 2, 3] AND status = 'active'
    let opts = FindOptions {
        filters: vec![
            Filter::in_list("author_id", vec![
                Value::Integer(1), Value::Integer(2), Value::Integer(3),
            ]),
            Filter::eq("status", "active"),
        ],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    // All matching rows must have author_id in {1,2,3} AND status == "active"
    for row in &result.rows {
        let aid = match row.get("author_id").unwrap() { Value::Integer(i) => i, _ => panic!() };
        assert!([1, 2, 3].contains(&aid));
        assert_eq!(row.get("status").unwrap(), Value::Text("active".into()));
    }
    assert!(result.rows.len() > 0);
}

#[test]
fn test_in_with_limit_offset_sort() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    for i in 1..=20 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // IN list = [1,5,10,15,20], sort desc, limit 3, offset 1
    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![
            Value::Integer(1), Value::Integer(5), Value::Integer(10),
            Value::Integer(15), Value::Integer(20),
        ])],
        sort: vec![Sort::desc("v")],
        limit: Some(3),
        offset: Some(1),
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    // sorted desc: [20, 15, 10, 5, 1], offset 1 -> [15, 10, 5], limit 3 -> [15, 10, 5]
    assert_eq!(result.total, Some(5));
    assert_eq!(result.rows.len(), 3);
    let vals: Vec<i64> = result.rows.iter()
        .map(|r| match r.get("v").unwrap() { Value::Integer(i) => i, _ => panic!() })
        .collect();
    assert_eq!(vals, vec![15, 10, 5]);
}

#[test]
fn test_in_empty_list() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();
    for i in 1..=5 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // Empty IN list should match nothing
    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(0));
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn test_in_empty_list_no_index() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    for i in 1..=5 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // Empty IN list without index
    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(0));
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn test_in_large_list() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();

    // Insert 500 rows
    for i in 1..=500 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // IN list with 150 values (every 3rd value from 1 to 450)
    let in_values: Vec<Value> = (1..=450).step_by(3).map(|i| Value::Integer(i)).collect();
    let expected_count = in_values.len();

    let opts = FindOptions {
        filters: vec![Filter::in_list("v", in_values)],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(expected_count as u64));
    assert_eq!(result.rows.len(), expected_count);
}

#[test]
fn test_in_with_acid_transaction() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    // Insert via ACID transaction
    {
        let mut tx = db.begin().unwrap();
        for i in 1..=10 {
            tx.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
        tx.commit().unwrap();
    }

    // Query with IN
    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![
            Value::Integer(2), Value::Integer(4), Value::Integer(6),
        ])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(3));
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn test_in_within_acid_transaction() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    for i in 1..=10 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // Query inside ACID transaction (uses AcidTransaction::find)
    {
        let mut tx = db.begin().unwrap();
        // Insert a few more rows
        tx.insert("t", &[("v", Value::Integer(11))]).unwrap();
        tx.insert("t", &[("v", Value::Integer(12))]).unwrap();

        let opts = FindOptions {
            filters: vec![Filter::in_list("v", vec![
                Value::Integer(1), Value::Integer(11), Value::Integer(12),
            ])],
            include_total: true,
            ..Default::default()
        };
        let result = tx.find("t", opts).unwrap();
        assert_eq!(result.total, Some(3));
        assert_eq!(result.rows.len(), 3);
        tx.commit().unwrap();
    }
}

#[test]
fn test_in_count() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    for i in 1..=10 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    let count = db.count("t", &[Filter::in_list("v", vec![
        Value::Integer(2), Value::Integer(5), Value::Integer(8),
    ])]).unwrap();
    assert_eq!(count, 3);
}

#[test]
fn test_in_nonexistent_values() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    for i in 1..=5 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // All values in the IN list are absent
    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![
            Value::Integer(100), Value::Integer(200),
        ])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(0));
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn test_in_duplicate_values_in_list() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_index("t", "idx_v", "v").unwrap();
    for i in 1..=5 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    // IN list has duplicates -- should still return each matching row only once
    let opts = FindOptions {
        filters: vec![Filter::in_list("v", vec![
            Value::Integer(3), Value::Integer(3), Value::Integer(3),
        ])],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(1));
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn test_in_delete_where() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    for i in 1..=10 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }

    let deleted = db.delete_where("t", &[Filter::in_list("v", vec![
        Value::Integer(2), Value::Integer(4), Value::Integer(6),
    ])]).unwrap();
    assert_eq!(deleted, 3);
    assert_eq!(db.count("t", &[]).unwrap(), 7);
}

#[test]
fn test_in_update_where() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("v", Type::Integer),
        ColumnDef::new("tag", Type::Text),
    ]).unwrap();
    for i in 1..=10 {
        db.insert("t", &[
            ("v", Value::Integer(i)),
            ("tag", Value::Text("old".into())),
        ]).unwrap();
    }

    let updated = db.update_where(
        "t",
        &[Filter::in_list("v", vec![Value::Integer(1), Value::Integer(5), Value::Integer(10)])],
        &[("tag", Value::Text("new".into()))],
    ).unwrap();
    assert_eq!(updated, 3);

    // Verify the updated rows
    let opts = FindOptions {
        filters: vec![Filter::eq("tag", "new")],
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.rows.len(), 3);
}

// ---------------------------------------------------------------------------
// IsNull / IsNotNull operator tests
// ---------------------------------------------------------------------------

#[test]
fn test_is_null_filter() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("email", Type::Text),
    ]).unwrap();

    // Insert rows with email set and rows without email (Null)
    db.insert("t", &[
        ("name", Value::Text("alice".into())),
        ("email", Value::Text("alice@example.com".into())),
    ]).unwrap();
    db.insert("t", &[
        ("name", Value::Text("bob".into())),
        // email omitted -> Null
    ]).unwrap();
    db.insert("t", &[
        ("name", Value::Text("charlie".into())),
        ("email", Value::Null),
    ]).unwrap();
    db.insert("t", &[
        ("name", Value::Text("dave".into())),
        ("email", Value::Text("dave@example.com".into())),
    ]).unwrap();

    // IsNull should return rows where email is absent or explicitly Null
    let opts = FindOptions {
        filters: vec![Filter::is_null("email")],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(2));
    assert_eq!(result.rows.len(), 2);
    let mut names: Vec<String> = result.rows.iter()
        .map(|r| match r.get("name").unwrap() { Value::Text(s) => s, _ => panic!() })
        .collect();
    names.sort();
    assert_eq!(names, vec!["bob", "charlie"]);
}

#[test]
fn test_is_not_null_filter() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("email", Type::Text),
    ]).unwrap();

    db.insert("t", &[
        ("name", Value::Text("alice".into())),
        ("email", Value::Text("alice@example.com".into())),
    ]).unwrap();
    db.insert("t", &[
        ("name", Value::Text("bob".into())),
    ]).unwrap();
    db.insert("t", &[
        ("name", Value::Text("charlie".into())),
        ("email", Value::Null),
    ]).unwrap();
    db.insert("t", &[
        ("name", Value::Text("dave".into())),
        ("email", Value::Text("dave@example.com".into())),
    ]).unwrap();

    // IsNotNull should return rows where email is present and not Null
    let opts = FindOptions {
        filters: vec![Filter::is_not_null("email")],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(2));
    assert_eq!(result.rows.len(), 2);
    let mut names: Vec<String> = result.rows.iter()
        .map(|r| match r.get("name").unwrap() { Value::Text(s) => s, _ => panic!() })
        .collect();
    names.sort();
    assert_eq!(names, vec!["alice", "dave"]);
}

#[test]
fn test_is_null_combined_with_other_filters() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("category", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    // Insert rows: some with value, some without
    for i in 0..10 {
        let cat = format!("cat_{}", i % 2);
        db.insert("t", &[
            ("category", Value::Text(cat)),
            ("value", Value::Integer(i)),
        ]).unwrap();
    }
    for i in 0..6 {
        let cat = format!("cat_{}", i % 2);
        db.insert("t", &[
            ("category", Value::Text(cat)),
            // value omitted -> Null
        ]).unwrap();
    }

    // category = "cat_0" AND value IS NULL
    let opts = FindOptions {
        filters: vec![
            Filter::eq("category", "cat_0"),
            Filter::is_null("value"),
        ],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(3)); // 3 rows with cat_0 and no value
    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        assert_eq!(row.get("category").unwrap(), Value::Text("cat_0".into()));
        assert!(row.get("value").is_none()); // absent column
    }
}

#[test]
fn test_is_null_with_acid_transaction() {
    let (db, _dir) = create_db();
    db.set_acid(true);
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("email", Type::Text),
    ]).unwrap();

    // Insert inside ACID transaction
    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[
            ("name", Value::Text("alice".into())),
            ("email", Value::Text("alice@example.com".into())),
        ]).unwrap();
        tx.insert("t", &[
            ("name", Value::Text("bob".into())),
            // email is Null (omitted)
        ]).unwrap();
        tx.insert("t", &[
            ("name", Value::Text("charlie".into())),
            ("email", Value::Null),
        ]).unwrap();
        tx.commit().unwrap();
    }

    // IsNull query
    let opts = FindOptions {
        filters: vec![Filter::is_null("email")],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(2));

    // IsNotNull query
    let opts = FindOptions {
        filters: vec![Filter::is_not_null("email")],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(1));

    // Also test querying within an ACID transaction
    {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[
            ("name", Value::Text("dave".into())),
            // email Null
        ]).unwrap();

        let opts = FindOptions {
            filters: vec![Filter::is_null("email")],
            include_total: true,
            ..Default::default()
        };
        let result = tx.find("t", opts).unwrap();
        assert_eq!(result.total, Some(3)); // bob, charlie, dave
        tx.commit().unwrap();
    }
}

#[test]
fn test_is_null_count() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("v", Type::Integer),
    ]).unwrap();

    for i in 0..5 {
        db.insert("t", &[("v", Value::Integer(i))]).unwrap();
    }
    for _ in 0..3 {
        db.insert("t", &[("v", Value::Null)]).unwrap();
    }

    assert_eq!(db.count("t", &[Filter::is_null("v")]).unwrap(), 3);
    assert_eq!(db.count("t", &[Filter::is_not_null("v")]).unwrap(), 5);
}

#[test]
fn test_is_not_null_combined_with_is_null() {
    let (db, _dir) = create_db();
    db.create_table("t", &[
        ColumnDef::new("a", Type::Text),
        ColumnDef::new("b", Type::Integer),
    ]).unwrap();

    db.insert("t", &[("a", Value::Text("x".into())), ("b", Value::Integer(1))]).unwrap();
    db.insert("t", &[("a", Value::Text("y".into()))]).unwrap(); // b absent
    db.insert("t", &[("b", Value::Integer(3))]).unwrap(); // a absent
    db.insert("t", &[]).unwrap(); // both absent

    // a IS NOT NULL AND b IS NULL
    let opts = FindOptions {
        filters: vec![
            Filter::is_not_null("a"),
            Filter::is_null("b"),
        ],
        include_total: true,
        ..Default::default()
    };
    let result = db.find("t", opts).unwrap();
    assert_eq!(result.total, Some(1));
    assert_eq!(result.rows[0].get("a").unwrap(), Value::Text("y".into()));
}
