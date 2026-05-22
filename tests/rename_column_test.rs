use boogy_db::{BoogyDb, ColumnDef, Filter, FindOptions, Type, Value};
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    (db, dir)
}

/// Basic rename: inserted rows are readable under the new name with unchanged values.
#[test]
fn test_rename_column_basic_data_preserved() {
    let (db, _dir) = create_db();
    db.create_table("items", &[
        ColumnDef::new("a", Type::Text),
        ColumnDef::new("b", Type::Integer),
    ])
    .unwrap();

    let id1 = db.insert("items", &[
        ("a", Value::Text("hello".into())),
        ("b", Value::Integer(42)),
    ])
    .unwrap();
    let id2 = db.insert("items", &[
        ("a", Value::Text("world".into())),
        ("b", Value::Integer(99)),
    ])
    .unwrap();

    db.rename_column("items", "a", "z").unwrap();

    // Rows are readable under the new name with the same values.
    let row1 = db.get("items", id1).unwrap().unwrap();
    assert_eq!(row1.get("z"), Some(Value::Text("hello".into())), "new name must return old value");
    assert_eq!(row1.get("b"), Some(Value::Integer(42)), "other columns must be untouched");

    let row2 = db.get("items", id2).unwrap().unwrap();
    assert_eq!(row2.get("z"), Some(Value::Text("world".into())));
    assert_eq!(row2.get("b"), Some(Value::Integer(99)));
}

/// After rename, the old column name no longer resolves (get returns None, find
/// filters on it match against Null — no row has value for the old name).
#[test]
fn test_rename_column_old_name_no_longer_resolves() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("a", Type::Integer)]).unwrap();
    let id = db.insert("items", &[("a", Value::Integer(7))]).unwrap();

    db.rename_column("items", "a", "z").unwrap();

    let row = db.get("items", id).unwrap().unwrap();
    // Old name is gone.
    assert_eq!(row.get("a"), None, "old column name must not resolve after rename");
    // New name works.
    assert_eq!(row.get("z"), Some(Value::Integer(7)));

    // A find filtering on the old name treats it as unknown (matches Null).
    // The old column is no longer in col_name_to_id, so filter_matches_row
    // calls f.matches(&Value::Null). Assert that a Null equality filter does
    // NOT match the row (7 != Null), and an IS-NULL filter does match (unknown→Null).
    let find = |filters: Vec<Filter>| {
        db.find("items", FindOptions {
            filters,
            or_groups: vec![],
            sort: vec![],
            limit: None,
            offset: None,
            include_total: false,
        })
        .unwrap()
    };

    // filter on old name "a" with a concrete value: should not match any rows.
    let by_old_val = find(vec![Filter::eq("a", Value::Integer(7))]);
    assert_eq!(by_old_val.rows.len(), 0, "filter on old name must not match rows");

    // filter on new name "z" with the correct value: must match.
    let by_new_val = find(vec![Filter::eq("z", Value::Integer(7))]);
    assert_eq!(by_new_val.rows.len(), 1, "filter on new name must match the row");
    assert_eq!(by_new_val.rows[0].id, id);
}

/// Rename to a name that already exists as a live column → Err.
#[test]
fn test_rename_column_collision_errors() {
    let (db, _dir) = create_db();
    db.create_table("items", &[
        ColumnDef::new("a", Type::Text),
        ColumnDef::new("b", Type::Integer),
    ])
    .unwrap();

    let result = db.rename_column("items", "a", "b");
    assert!(result.is_err(), "renaming to an existing live column name must fail");
}

/// Rename a column that doesn't exist → Err.
#[test]
fn test_rename_column_nonexistent_errors() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("a", Type::Text)]).unwrap();

    let result = db.rename_column("items", "missing", "z");
    assert!(result.is_err(), "renaming a non-existent column must fail");
}

/// Rename to an empty string → Err.
#[test]
fn test_rename_column_empty_new_name_errors() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("a", Type::Text)]).unwrap();

    let result = db.rename_column("items", "a", "");
    assert!(result.is_err(), "empty new column name must fail validation");
}

/// Rename to a name containing a NUL byte → Err.
#[test]
fn test_rename_column_nul_byte_in_new_name_errors() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("a", Type::Text)]).unwrap();

    let result = db.rename_column("items", "a", "bad\0name");
    assert!(result.is_err(), "NUL byte in new column name must fail validation");
}

/// Indexed rename: create_index on `a`, rename to `z`, then find(z == value)
/// via the index returns the correct rows, and a new insert + indexed find works.
#[test]
fn test_rename_column_indexed_query_works_after_rename() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("a", Type::Integer)]).unwrap();
    db.create_index("items", "idx_a", "a").unwrap();

    let id1 = db.insert("items", &[("a", Value::Integer(10))]).unwrap();
    let id2 = db.insert("items", &[("a", Value::Integer(20))]).unwrap();

    db.rename_column("items", "a", "z").unwrap();

    // find(z == 10) should return id1 via the renamed index.
    let result = db.find("items", FindOptions {
        filters: vec![Filter::eq("z", Value::Integer(10))],
        or_groups: vec![],
        sort: vec![],
        limit: None,
        offset: None,
        include_total: false,
    })
    .unwrap();
    assert_eq!(result.rows.len(), 1, "indexed find on new name must return 1 row");
    assert_eq!(result.rows[0].id, id1);

    // find(z == 20) should return id2.
    let result2 = db.find("items", FindOptions {
        filters: vec![Filter::eq("z", Value::Integer(20))],
        or_groups: vec![],
        sort: vec![],
        limit: None,
        offset: None,
        include_total: false,
    })
    .unwrap();
    assert_eq!(result2.rows.len(), 1);
    assert_eq!(result2.rows[0].id, id2);

    // New insert with the new name + indexed find still works.
    let id3 = db.insert("items", &[("z", Value::Integer(30))]).unwrap();
    let result3 = db.find("items", FindOptions {
        filters: vec![Filter::eq("z", Value::Integer(30))],
        or_groups: vec![],
        sort: vec![],
        limit: None,
        offset: None,
        include_total: false,
    })
    .unwrap();
    assert_eq!(result3.rows.len(), 1);
    assert_eq!(result3.rows[0].id, id3);
}

/// Reopen: rename, close db, reopen; new name resolves and data is intact.
#[test]
fn test_rename_column_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("reopen.boogy");

    let inserted_id;
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("items", &[ColumnDef::new("a", Type::Text)]).unwrap();
        inserted_id = db.insert("items", &[("a", Value::Text("persist_me".into()))]).unwrap();
        db.rename_column("items", "a", "z").unwrap();

        // Verify before close.
        let row = db.get("items", inserted_id).unwrap().unwrap();
        assert_eq!(row.get("z"), Some(Value::Text("persist_me".into())));
        assert_eq!(row.get("a"), None);
    }

    // Reopen.
    {
        let db = BoogyDb::open(&path).unwrap();
        let row = db.get("items", inserted_id).unwrap().unwrap();
        assert_eq!(
            row.get("z"),
            Some(Value::Text("persist_me".into())),
            "new name must resolve after reopen"
        );
        assert_eq!(row.get("a"), None, "old name must still not resolve after reopen");
    }
}
