use boogy_db::{BoogyDb, ColumnDef, Filter, FindOptions, Type, Value};
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    (db, dir)
}

/// Existing rows read the stored default for a newly-added Text column.
#[test]
fn test_add_column_existing_rows_read_default() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();

    // Insert two rows before the column is added.
    let id1 = db.insert("items", &[("name", Value::Text("alpha".into()))]).unwrap();
    let id2 = db.insert("items", &[("name", Value::Text("beta".into()))]).unwrap();

    // Add a Text column with a default.
    db.add_column(
        "items",
        ColumnDef::new("note", Type::Text).default(Value::Text("".into())),
    )
    .unwrap();

    // Existing rows should see the default.
    let row1 = db.get("items", id1).unwrap().unwrap();
    assert_eq!(
        row1.get("note"),
        Some(Value::Text("".into())),
        "pre-existing row should read the column default"
    );
    let row2 = db.get("items", id2).unwrap().unwrap();
    assert_eq!(row2.get("note"), Some(Value::Text("".into())));
}

/// Existing rows read None for a newly-added column with no default.
/// (No default means there is nothing to return; behavior mirrors an omitted column.)
#[test]
fn test_add_column_no_default_reads_none() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();
    let id = db.insert("items", &[("name", Value::Text("x".into()))]).unwrap();

    db.add_column("items", ColumnDef::new("n", Type::Integer)).unwrap();

    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(
        row.get("n"),
        None,
        "missing column without default should read None (no default to apply)"
    );
}

/// A new insert that omits the added column reads the default.
#[test]
fn test_add_column_new_insert_omits_column_reads_default() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();

    db.add_column(
        "items",
        ColumnDef::new("note", Type::Text).default(Value::Text("default_note".into())),
    )
    .unwrap();

    // Insert a row that doesn't supply "note".
    let id = db.insert("items", &[("name", Value::Text("charlie".into()))]).unwrap();

    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(
        row.get("note"),
        Some(Value::Text("default_note".into())),
        "new insert omitting the column should read the stored default"
    );
}

/// A new insert that explicitly stores Null overrides the default.
#[test]
fn test_add_column_explicit_null_overrides_default() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();

    db.add_column(
        "items",
        ColumnDef::new("note", Type::Text).default(Value::Text("has_default".into())),
    )
    .unwrap();

    // Insert a row that explicitly stores Null for "note".
    let id = db
        .insert("items", &[("name", Value::Text("dave".into())), ("note", Value::Null)])
        .unwrap();

    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(
        row.get("note"),
        Some(Value::Null),
        "explicit Null should be stored and read back as Null, not the default"
    );
}

/// Adding a NOT-NULL column without a default to a non-empty table returns Err.
#[test]
fn test_add_column_not_null_no_default_nonempty_table_errors() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();
    db.insert("items", &[("name", Value::Text("x".into()))]).unwrap();

    let result = db.add_column("items", ColumnDef::new("must_have", Type::Integer).not_null());
    assert!(
        result.is_err(),
        "NOT-NULL column with no default on non-empty table must fail"
    );
}

/// Adding a NOT-NULL column without a default to an EMPTY table is OK.
#[test]
fn test_add_column_not_null_no_default_empty_table_ok() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();
    // Table is empty — should succeed even though there's no default.
    db.add_column("items", ColumnDef::new("required", Type::Integer).not_null())
        .unwrap();
}

/// Adding a column with a name that matches a live column returns Err.
#[test]
fn test_add_column_duplicate_name_errors() {
    let (db, _dir) = create_db();
    db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();

    let result = db.add_column("items", ColumnDef::new("name", Type::Integer));
    assert!(result.is_err(), "duplicate live column name must fail");
}

/// Row::columns() includes added columns with their defaults.
#[test]
fn test_add_column_columns_method_includes_default() {
    let (db, _dir) = create_db();
    db.create_table("things", &[ColumnDef::new("label", Type::Text)]).unwrap();
    let id = db
        .insert("things", &[("label", Value::Text("foo".into()))])
        .unwrap();

    db.add_column(
        "things",
        ColumnDef::new("count", Type::Integer).default(Value::Integer(0)),
    )
    .unwrap();

    let row = db.get("things", id).unwrap().unwrap();
    let cols: std::collections::HashMap<String, Value> = row.columns().into_iter().collect();
    assert_eq!(cols.get("label"), Some(&Value::Text("foo".into())));
    assert_eq!(
        cols.get("count"),
        Some(&Value::Integer(0)),
        "columns() must include the added column with its default"
    );
}

/// find() returns rows with added-column defaults correctly.
#[test]
fn test_add_column_find_returns_defaults() {
    let (db, _dir) = create_db();
    db.create_table("posts", &[ColumnDef::new("title", Type::Text)]).unwrap();

    db.insert("posts", &[("title", Value::Text("old post".into()))]).unwrap();

    db.add_column(
        "posts",
        ColumnDef::new("views", Type::Integer).default(Value::Integer(0)),
    )
    .unwrap();

    let result = db
        .find(
            "posts",
            FindOptions {
                filters: vec![Filter::eq("title", "old post")],
                or_groups: vec![],
                sort: vec![],
                limit: None,
                offset: None,
                include_total: false,
            },
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("views"), Some(Value::Integer(0)));
}
