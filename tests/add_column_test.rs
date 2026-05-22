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

// ── Filter / predicate consistency tests (the "get/find agree" fix) ──────────

/// After add_column with a default, `find(views == 0)` must match old rows
/// (whose raw bytes lack the column), and `find(views IS NULL)` must NOT.
#[test]
fn test_add_column_filter_on_default_matches_old_rows() {
    let (db, _dir) = create_db();
    db.create_table("posts", &[ColumnDef::new("title", Type::Text)]).unwrap();

    // Insert rows BEFORE the column exists (raw bytes will not contain "views").
    let old1 = db.insert("posts", &[("title", Value::Text("p1".into()))]).unwrap();
    let old2 = db.insert("posts", &[("title", Value::Text("p2".into()))]).unwrap();

    // Add column with integer default 0.
    db.add_column(
        "posts",
        ColumnDef::new("views", Type::Integer).default(Value::Integer(0)),
    )
    .unwrap();

    // Insert a new row that explicitly stores Null for views.
    let explicit_null = db
        .insert("posts", &[("title", Value::Text("p3".into())), ("views", Value::Null)])
        .unwrap();

    // Insert a row that explicitly stores views = 42.
    let explicit_42 = db
        .insert("posts", &[("title", Value::Text("p4".into())), ("views", Value::Integer(42))])
        .unwrap();

    let find = |filters: Vec<Filter>| {
        db.find(
            "posts",
            FindOptions { filters, or_groups: vec![], sort: vec![], limit: None, offset: None, include_total: true },
        )
        .unwrap()
    };

    // `views == 0` must return old rows (default) but NOT the explicit-Null or 42 rows.
    let eq0 = find(vec![Filter::eq("views", Value::Integer(0))]);
    let eq0_ids: std::collections::HashSet<u64> = eq0.rows.iter().map(|r| r.id).collect();
    assert!(eq0_ids.contains(&old1), "find(views==0) must include old row 1 (has default 0)");
    assert!(eq0_ids.contains(&old2), "find(views==0) must include old row 2 (has default 0)");
    assert!(!eq0_ids.contains(&explicit_null), "find(views==0) must NOT include explicit-Null row");
    assert!(!eq0_ids.contains(&explicit_42), "find(views==0) must NOT include views=42 row");

    // `views IS NULL` must return the explicit-Null row but NOT old rows (they have default 0).
    let is_null = find(vec![Filter::is_null("views")]);
    let null_ids: std::collections::HashSet<u64> = is_null.rows.iter().map(|r| r.id).collect();
    assert!(null_ids.contains(&explicit_null), "find(views IS NULL) must include explicit-Null row");
    assert!(!null_ids.contains(&old1), "find(views IS NULL) must NOT include old row with default 0");
    assert!(!null_ids.contains(&old2), "find(views IS NULL) must NOT include old row with default 0");

    // `get()` and `find()` must agree for old rows.
    let r1 = db.get("posts", old1).unwrap().unwrap();
    assert_eq!(r1.get("views"), Some(Value::Integer(0)), "get() must also return default 0");
}

/// count() must agree with find() after add_column with a default.
#[test]
fn test_add_column_count_on_default_matches_old_rows() {
    let (db, _dir) = create_db();
    db.create_table("stats", &[ColumnDef::new("key", Type::Text)]).unwrap();

    // Three old rows (no views column yet).
    db.insert("stats", &[("key", Value::Text("a".into()))]).unwrap();
    db.insert("stats", &[("key", Value::Text("b".into()))]).unwrap();
    db.insert("stats", &[("key", Value::Text("c".into()))]).unwrap();

    db.add_column(
        "stats",
        ColumnDef::new("views", Type::Integer).default(Value::Integer(0)),
    )
    .unwrap();

    // One new row with explicit Null.
    db.insert("stats", &[("key", Value::Text("d".into())), ("views", Value::Null)]).unwrap();

    // count(views == 0) must be 3 (the three old rows with default).
    let c_eq0 = db.count("stats", &[Filter::eq("views", Value::Integer(0))]).unwrap();
    assert_eq!(c_eq0, 3, "count(views==0) must count all old rows with default 0");

    // count(views IS NULL) must be 1 (only the explicit-Null row).
    let c_null = db.count("stats", &[Filter::is_null("views")]).unwrap();
    assert_eq!(c_null, 1, "count(views IS NULL) must count only the explicit-Null row");
}

/// After add_column with default, close the DB and reopen it. Old rows must
/// still read the default via get(), AND find()/count() filtering on the
/// default must still match them (proves the stored default survives reopen).
#[test]
fn test_add_column_default_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("reopen.boogy");

    // Phase 1: create, insert, add_column, close.
    let old_id;
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table("items", &[ColumnDef::new("name", Type::Text)]).unwrap();
        old_id = db.insert("items", &[("name", Value::Text("old".into()))]).unwrap();
        db.add_column(
            "items",
            ColumnDef::new("score", Type::Integer).default(Value::Integer(42)),
        )
        .unwrap();
        // Verify before close.
        let r = db.get("items", old_id).unwrap().unwrap();
        assert_eq!(r.get("score"), Some(Value::Integer(42)));
        // DB is dropped here, closing it.
    }

    // Phase 2: reopen and re-verify.
    {
        let db = BoogyDb::open(&path).unwrap();

        // get() must still return the default.
        let r = db.get("items", old_id).unwrap().unwrap();
        assert_eq!(
            r.get("score"),
            Some(Value::Integer(42)),
            "get() must return default after reopen"
        );

        // find(score == 42) must match the old row.
        let result = db
            .find(
                "items",
                FindOptions {
                    filters: vec![Filter::eq("score", Value::Integer(42))],
                    or_groups: vec![],
                    sort: vec![],
                    limit: None,
                    offset: None,
                    include_total: true,
                },
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1, "find(score==42) must return old row after reopen");
        assert_eq!(result.rows[0].id, old_id);

        // count(score == 42) must be 1.
        let c = db.count("items", &[Filter::eq("score", Value::Integer(42))]).unwrap();
        assert_eq!(c, 1, "count(score==42) must be 1 after reopen");

        // find(score IS NULL) must return nothing (old row has default, not Null).
        let r_null = db
            .find(
                "items",
                FindOptions {
                    filters: vec![Filter::is_null("score")],
                    or_groups: vec![],
                    sort: vec![],
                    limit: None,
                    offset: None,
                    include_total: false,
                },
            )
            .unwrap();
        assert_eq!(r_null.rows.len(), 0, "find(score IS NULL) must not match old row with default after reopen");
    }
}
