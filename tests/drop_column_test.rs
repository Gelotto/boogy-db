use boogy_db::{BoogyDb, ColumnDef, Filter, FindOptions, Type, Value};
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    (db, dir)
}

fn make_table(db: &BoogyDb) {
    db.create_table(
        "items",
        &[
            ColumnDef::new("a", Type::Text),
            ColumnDef::new("b", Type::Integer),
            ColumnDef::new("c", Type::Integer),
        ],
    )
    .unwrap();
}

/// After drop_column("b"), get() no longer returns a value for "b" and
/// "a"/"c" values are intact.
#[test]
fn test_drop_column_basic_get_invisible() {
    let (db, _dir) = create_db();
    make_table(&db);

    let id = db
        .insert(
            "items",
            &[
                ("a", Value::Text("hello".into())),
                ("b", Value::Integer(99)),
                ("c", Value::Integer(7)),
            ],
        )
        .unwrap();

    db.drop_column("items", "b").unwrap();

    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(
        row.get("b"),
        None,
        "dropped column must not appear in get()"
    );
    assert_eq!(
        row.get("a"),
        Some(Value::Text("hello".into())),
        "other columns must be intact"
    );
    assert_eq!(
        row.get("c"),
        Some(Value::Integer(7)),
        "other columns must be intact"
    );
}

/// After drop_column("b"), columns() no longer includes "b".
#[test]
fn test_drop_column_columns_excludes_dropped() {
    let (db, _dir) = create_db();
    make_table(&db);

    let id = db
        .insert(
            "items",
            &[
                ("a", Value::Text("x".into())),
                ("b", Value::Integer(42)),
                ("c", Value::Integer(1)),
            ],
        )
        .unwrap();

    db.drop_column("items", "b").unwrap();

    let row = db.get("items", id).unwrap().unwrap();
    let cols: std::collections::HashMap<String, Value> = row.columns().into_iter().collect();
    assert!(
        !cols.contains_key("b"),
        "columns() must not include dropped column"
    );
    assert!(cols.contains_key("a"));
    assert!(cols.contains_key("c"));
}

/// After drop_column("b"), find() results exclude "b" from rows and a filter
/// on "b" matches against Null (the column no longer resolves).
#[test]
fn test_drop_column_find_invisible() {
    let (db, _dir) = create_db();
    make_table(&db);

    let id1 = db
        .insert(
            "items",
            &[
                ("a", Value::Text("foo".into())),
                ("b", Value::Integer(10)),
                ("c", Value::Integer(1)),
            ],
        )
        .unwrap();

    let _id2 = db
        .insert(
            "items",
            &[
                ("a", Value::Text("bar".into())),
                ("b", Value::Integer(20)),
                ("c", Value::Integer(2)),
            ],
        )
        .unwrap();

    db.drop_column("items", "b").unwrap();

    let find = |filters: Vec<Filter>| {
        db.find(
            "items",
            FindOptions {
                filters,
                or_groups: vec![],
                sort: vec![],
                limit: None,
                offset: None,
                include_total: false,
            },
        )
        .unwrap()
    };

    // Filtering on dropped column value should match nothing (name unknown → Null).
    let by_b = find(vec![Filter::eq("b", Value::Integer(10))]);
    assert_eq!(
        by_b.rows.len(),
        0,
        "filter on dropped column must match no rows"
    );

    // Filtering on surviving column "a" must still work.
    let by_a = find(vec![Filter::eq("a", Value::Text("foo".into()))]);
    assert_eq!(by_a.rows.len(), 1);
    assert_eq!(by_a.rows[0].id, id1);
    assert_eq!(by_a.rows[0].get("b"), None, "b must be absent from find() rows");
    assert_eq!(by_a.rows[0].get("c"), Some(Value::Integer(1)));
}

/// col_id not reused: after drop_column("b"), add_column("d") gets a fresh slot.
/// Crucially, old "b" data is NOT surfaced under "d".
#[test]
fn test_drop_column_col_id_not_reused() {
    let (db, _dir) = create_db();
    make_table(&db);

    // Insert a row with b = 999 before drop.
    let old_id = db
        .insert(
            "items",
            &[
                ("a", Value::Text("before".into())),
                ("b", Value::Integer(999)),
                ("c", Value::Integer(3)),
            ],
        )
        .unwrap();

    db.drop_column("items", "b").unwrap();

    // Add a fresh column "d" with default 0.
    db.add_column(
        "items",
        ColumnDef::new("d", Type::Integer).default(Value::Integer(0)),
    )
    .unwrap();

    // Old row: "b" gone, "d" reads its default (0), NOT the old b=999.
    let row = db.get("items", old_id).unwrap().unwrap();
    assert_eq!(row.get("b"), None, "b must be invisible");
    assert_eq!(
        row.get("d"),
        Some(Value::Integer(0)),
        "d must read its default for old row"
    );
    // Extra safety: "d" must definitely not return 999 (old b data).
    assert_ne!(
        row.get("d"),
        Some(Value::Integer(999)),
        "old b data must NOT appear as d"
    );

    // New insert: can write d explicitly.
    let new_id = db
        .insert(
            "items",
            &[("a", Value::Text("after".into())), ("c", Value::Integer(4)), ("d", Value::Integer(7))],
        )
        .unwrap();
    let new_row = db.get("items", new_id).unwrap().unwrap();
    assert_eq!(new_row.get("d"), Some(Value::Integer(7)));
    assert_eq!(new_row.get("b"), None);
}

/// Re-add a dropped name: after drop_column("b"), add_column("b") succeeds
/// and the new "b" reads its default for old rows (old b values are gone).
#[test]
fn test_drop_column_re_add_dropped_name() {
    let (db, _dir) = create_db();
    make_table(&db);

    let old_id = db
        .insert(
            "items",
            &[
                ("a", Value::Text("x".into())),
                ("b", Value::Integer(42)),
                ("c", Value::Integer(1)),
            ],
        )
        .unwrap();

    db.drop_column("items", "b").unwrap();

    // Re-adding the same name must succeed (name freed on drop).
    db.add_column(
        "items",
        ColumnDef::new("b", Type::Text).default(Value::Text("fresh".into())),
    )
    .unwrap();

    // Old row: the new "b" reads its default, NOT the old integer 42.
    let row = db.get("items", old_id).unwrap().unwrap();
    assert_eq!(
        row.get("b"),
        Some(Value::Text("fresh".into())),
        "re-added b must read the new default for old rows"
    );
    assert_ne!(
        row.get("b"),
        Some(Value::Integer(42)),
        "old b data must NOT appear under the re-added b"
    );

    // New insert with the fresh "b".
    let new_id = db
        .insert(
            "items",
            &[("a", Value::Text("y".into())), ("c", Value::Integer(2)), ("b", Value::Text("explicit".into()))],
        )
        .unwrap();
    let new_row = db.get("items", new_id).unwrap().unwrap();
    assert_eq!(
        new_row.get("b"),
        Some(Value::Text("explicit".into()))
    );
}

/// drop_column rejects dropping a column referenced by an index.
/// drop_index first, then drop_column succeeds.
#[test]
fn test_drop_column_rejected_if_indexed() {
    let (db, _dir) = create_db();
    make_table(&db);

    db.create_index("items", "idx_c", "c").unwrap();

    // Attempt to drop "c" while indexed → must fail.
    let result = db.drop_column("items", "c");
    assert!(result.is_err(), "drop_column must fail when column is indexed");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("idx_c"),
        "error must name the blocking index; got: {err_msg}"
    );

    // Drop the index, then drop_column("c") must succeed.
    db.drop_index("items", "idx_c").unwrap();
    db.drop_column("items", "c").unwrap();

    // Confirm c is gone.
    let id = db
        .insert("items", &[("a", Value::Text("t".into())), ("b", Value::Integer(1))])
        .unwrap();
    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(row.get("c"), None, "c must be invisible after drop");
}

/// drop_column on an already-dropped or nonexistent column → Err.
#[test]
fn test_drop_column_nonexistent_errors() {
    let (db, _dir) = create_db();
    make_table(&db);

    let result = db.drop_column("items", "missing");
    assert!(result.is_err(), "dropping a nonexistent column must fail");
}

/// drop_column twice → second attempt must fail (name no longer live).
#[test]
fn test_drop_column_already_dropped_errors() {
    let (db, _dir) = create_db();
    make_table(&db);

    db.drop_column("items", "b").unwrap();
    let result = db.drop_column("items", "b");
    assert!(
        result.is_err(),
        "dropping an already-dropped column must fail"
    );
}

/// Tombstone persists across reopen: drop a column, close, reopen — still gone.
#[test]
fn test_drop_column_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("reopen.boogy");

    let id_before;
    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table(
            "items",
            &[
                ColumnDef::new("a", Type::Text),
                ColumnDef::new("b", Type::Integer),
                ColumnDef::new("c", Type::Integer),
            ],
        )
        .unwrap();
        id_before = db
            .insert(
                "items",
                &[
                    ("a", Value::Text("hello".into())),
                    ("b", Value::Integer(77)),
                    ("c", Value::Integer(3)),
                ],
            )
            .unwrap();
        db.drop_column("items", "b").unwrap();

        // Confirm before close.
        let row = db.get("items", id_before).unwrap().unwrap();
        assert_eq!(row.get("b"), None);
        assert_eq!(row.get("a"), Some(Value::Text("hello".into())));
    }

    // Reopen.
    {
        let db = BoogyDb::open(&path).unwrap();
        let row = db.get("items", id_before).unwrap().unwrap();
        assert_eq!(row.get("b"), None, "dropped column must still be invisible after reopen");
        assert_eq!(
            row.get("a"),
            Some(Value::Text("hello".into())),
            "other columns must be intact after reopen"
        );
        assert_eq!(row.get("c"), Some(Value::Integer(3)));

        // Attempting to drop "b" again must fail (still marked dropped, name not live).
        let result = db.drop_column("items", "b");
        assert!(result.is_err(), "double-drop must fail after reopen");

        // add_column("d") after reopen must get a fresh slot (col_id > 2).
        db.add_column(
            "items",
            ColumnDef::new("d", Type::Integer).default(Value::Integer(55)),
        )
        .unwrap();
        let row2 = db.get("items", id_before).unwrap().unwrap();
        assert_eq!(row2.get("d"), Some(Value::Integer(55)));
        assert_eq!(row2.get("b"), None);
    }
}
