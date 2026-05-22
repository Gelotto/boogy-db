//! Reopen/persistence integration test for schema-evolution async wrappers.
//!
//! Opens a real on-disk database (not in-memory), exercises `add_column`,
//! `rename_column`, and `drop_column` through the async `AsyncBoogyDb` API,
//! then drops the handle and reopens the database from the same path, asserting
//! that all schema mutations survived the round-trip.

#![cfg(feature = "tokio")]

use boogy_db::{AsyncBoogyDb, ColumnDef, Filter, FindOptions, Type, Value};
use tempfile::TempDir;

/// Full reopen/persistence integration test exercising all three schema-evolution
/// async wrappers. The database is a real temp file so the close+reopen is genuine.
#[tokio::test]
async fn test_schema_evolution_async_reopen_persistence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("schema_evo.boogy");

    // ── Phase 1: setup + schema evolution ─────────────────────────────────────
    let old_id1;
    let old_id2;
    {
        let db = AsyncBoogyDb::open(&path).await.unwrap();

        db.create_table(
            "events",
            &[
                ColumnDef::new("kind", Type::Text),
                ColumnDef::new("score", Type::Integer),
                ColumnDef::new("temp_col", Type::Integer),
            ],
        )
        .await
        .unwrap();

        // Insert rows BEFORE the schema changes.
        old_id1 = db
            .insert(
                "events",
                &[
                    ("kind", Value::Text("click".into())),
                    ("score", Value::Integer(10)),
                    ("temp_col", Value::Integer(99)),
                ],
            )
            .await
            .unwrap();
        old_id2 = db
            .insert(
                "events",
                &[
                    ("kind", Value::Text("view".into())),
                    ("score", Value::Integer(20)),
                    ("temp_col", Value::Integer(88)),
                ],
            )
            .await
            .unwrap();

        // (1) add_column with a default — old rows will read the default on reopen.
        db.add_column(
            "events",
            ColumnDef::new("tag", Type::Text).default(Value::Text("untagged".into())),
        )
        .await
        .unwrap();

        // (2) rename_column: "score" → "rating".
        db.rename_column("events", "score", "rating").await.unwrap();

        // (3) drop_column: "temp_col" tombstoned, its col_id never reused.
        db.drop_column("events", "temp_col").await.unwrap();

        // Verify the state before close (sanity check — main assertions are post-reopen).
        let row = db.get("events", old_id1).await.unwrap().unwrap();
        assert_eq!(row.get("tag"), Some(Value::Text("untagged".into())));
        assert_eq!(row.get("rating"), Some(Value::Integer(10)));
        assert_eq!(row.get("score"), None);
        assert_eq!(row.get("temp_col"), None);

        // db is dropped here — the handle is closed.
    }

    // ── Phase 2: reopen and assert all mutations persisted ────────────────────
    {
        let db = AsyncBoogyDb::open(&path).await.unwrap();

        // ── Assertion 1: added column still reads its default for old rows ────
        // (proves the stored default survived the system-page round-trip)
        let row1 = db.get("events", old_id1).await.unwrap().unwrap();
        assert_eq!(
            row1.get("tag"),
            Some(Value::Text("untagged".into())),
            "get(): added column must return default for old rows after reopen"
        );
        let row2 = db.get("events", old_id2).await.unwrap().unwrap();
        assert_eq!(
            row2.get("tag"),
            Some(Value::Text("untagged".into())),
            "get(): added column default must hold for second old row after reopen"
        );

        // find() filtering on the default must match the old rows.
        let found_default = db
            .find(
                "events",
                FindOptions {
                    filters: vec![Filter::eq("tag", Value::Text("untagged".into()))],
                    or_groups: vec![],
                    sort: vec![],
                    limit: None,
                    offset: None,
                    include_total: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            found_default.rows.len(),
            2,
            "find(tag == 'untagged') must match both old rows after reopen (default-at-read persisted)"
        );
        let found_ids: std::collections::HashSet<u64> =
            found_default.rows.iter().map(|r| r.id).collect();
        assert!(found_ids.contains(&old_id1));
        assert!(found_ids.contains(&old_id2));

        // count on the default must also be 2.
        let cnt = db
            .count(
                "events",
                &[Filter::eq("tag", Value::Text("untagged".into()))],
            )
            .await
            .unwrap();
        assert_eq!(cnt, 2, "count(tag == 'untagged') must be 2 after reopen");

        // ── Assertion 2: renamed column resolves under its new name ───────────
        let row1_r = db.get("events", old_id1).await.unwrap().unwrap();
        assert_eq!(
            row1_r.get("rating"),
            Some(Value::Integer(10)),
            "renamed column must resolve under 'rating' with original data after reopen"
        );
        let row2_r = db.get("events", old_id2).await.unwrap().unwrap();
        assert_eq!(
            row2_r.get("rating"),
            Some(Value::Integer(20)),
            "renamed column must resolve under 'rating' for second row after reopen"
        );
        // Old name must be gone.
        assert_eq!(
            row1_r.get("score"),
            None,
            "old column name 'score' must not resolve after reopen"
        );

        // find by new name must work.
        let found_rating = db
            .find(
                "events",
                FindOptions {
                    filters: vec![Filter::eq("rating", Value::Integer(10))],
                    or_groups: vec![],
                    sort: vec![],
                    limit: None,
                    offset: None,
                    include_total: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            found_rating.rows.len(),
            1,
            "find(rating == 10) must return exactly one row after reopen"
        );
        assert_eq!(found_rating.rows[0].id, old_id1);

        // ── Assertion 3: dropped column is still gone (tombstone persisted) ───
        let row1_d = db.get("events", old_id1).await.unwrap().unwrap();
        assert_eq!(
            row1_d.get("temp_col"),
            None,
            "dropped column 'temp_col' must still be invisible after reopen"
        );

        // Attempting to drop it again must fail (name not live).
        let second_drop = db.drop_column("events", "temp_col").await;
        assert!(
            second_drop.is_err(),
            "double-drop of 'temp_col' must fail after reopen"
        );

        // ── Assertion 4: add_column after reopen gets a fresh col_id ─────────
        // (the dropped slot must NOT be reused; inserting+reading the new column works)
        db.add_column(
            "events",
            ColumnDef::new("new_field", Type::Integer).default(Value::Integer(0)),
        )
        .await
        .unwrap();

        // Old row reads the new column's default (not temp_col's old data).
        let row1_new = db.get("events", old_id1).await.unwrap().unwrap();
        assert_eq!(
            row1_new.get("new_field"),
            Some(Value::Integer(0)),
            "new_field must read its default for pre-existing rows"
        );
        assert_ne!(
            row1_new.get("new_field"),
            Some(Value::Integer(99)),
            "new_field must NOT surface old temp_col data (col_id not reused)"
        );

        // A fresh insert that writes the new column explicitly must round-trip.
        let new_id = db
            .insert(
                "events",
                &[
                    ("kind", Value::Text("tap".into())),
                    ("rating", Value::Integer(5)),
                    ("tag", Value::Text("fresh".into())),
                    ("new_field", Value::Integer(777)),
                ],
            )
            .await
            .unwrap();
        let new_row = db.get("events", new_id).await.unwrap().unwrap();
        assert_eq!(
            new_row.get("new_field"),
            Some(Value::Integer(777)),
            "explicitly inserted new_field value must round-trip"
        );
        assert_eq!(
            new_row.get("temp_col"),
            None,
            "temp_col must still be invisible in new inserts"
        );
    }
}
