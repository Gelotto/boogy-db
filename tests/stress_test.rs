use boogy_db::*;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn create_db() -> (BoogyDb, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("stress.boogy")).unwrap();
    db.set_durability(Durability::None); // speed over durability for stress tests
    (db, dir)
}

/// 8 threads doing mixed insert/get/find/delete on the same table for 2 seconds.
#[test]
fn test_concurrent_mixed_operations() {
    let (db, _dir) = create_db();
    db.create_table(
        "t",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("value", Type::Integer),
        ],
    )
    .unwrap();

    // Seed some data so readers have something to find
    for i in 0..50 {
        db.insert(
            "t",
            &[
                ("name", Value::Text(format!("seed_{i}"))),
                ("value", Value::Integer(i)),
            ],
        )
        .unwrap();
    }

    let db = Arc::new(db);
    let start = Instant::now();
    let duration = Duration::from_secs(2);

    let handles: Vec<_> = (0..8)
        .map(|thread_id| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                let mut ops = 0u64;
                while Instant::now() - start < duration {
                    match ops % 4 {
                        0 => {
                            // Insert
                            let _ = db.insert(
                                "t",
                                &[
                                    ("name", Value::Text(format!("t{thread_id}_r{ops}"))),
                                    ("value", Value::Integer(ops as i64)),
                                ],
                            );
                        }
                        1 => {
                            // Get (may or may not exist)
                            let _ = db.get("t", (ops % 100) + 1);
                        }
                        2 => {
                            // Find
                            let _ = db.find(
                                "t",
                                FindOptions {
                                    filters: vec![Filter::eq("value", (ops % 50) as i64)],
                                    limit: Some(5),
                                    ..Default::default()
                                },
                            );
                        }
                        3 => {
                            // Delete (may or may not exist)
                            let _ = db.delete("t", (ops % 20) + 1);
                        }
                        _ => unreachable!(),
                    }
                    ops += 1;
                }
                ops
            })
        })
        .collect();

    let total_ops: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total_ops > 0, "should have completed some operations");

    // DB should still be consistent -- count should not panic
    let _ = db.count("t", &[]).unwrap();
}

/// Insert 100K rows, verify count, delete half, verify count.
#[test]
fn test_large_dataset_insert_delete() {
    let (db, _dir) = create_db();
    db.create_table("data", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();

    // Batch insert 100K rows using insert_many in chunks
    let chunk_size = 1000;
    let total = 100_000;
    let mut all_ids = Vec::with_capacity(total);

    for chunk_start in (0..total).step_by(chunk_size) {
        let rows: Vec<Vec<(&str, Value)>> = (chunk_start..chunk_start + chunk_size)
            .map(|i| vec![("v", Value::Integer(i as i64))])
            .collect();
        let ids = db.insert_many("data", &rows).unwrap();
        all_ids.extend(ids);
    }

    assert_eq!(db.count("data", &[]).unwrap(), total as u64);

    // Delete the first half
    for &id in &all_ids[..total / 2] {
        db.delete("data", id).unwrap();
    }

    assert_eq!(db.count("data", &[]).unwrap(), (total / 2) as u64);

    // Verify the remaining rows are accessible
    for &id in &all_ids[total / 2..total / 2 + 10] {
        let row = db.get("data", id).unwrap();
        assert!(row.is_some(), "row {id} should still exist");
    }
}

/// Rapid create_table/drop_table cycles.
#[test]
fn test_rapid_create_drop_table() {
    let (db, _dir) = create_db();

    for cycle in 0..50 {
        let name = format!("table_{cycle}");
        db.create_table(&name, &[ColumnDef::new("v", Type::Integer)])
            .unwrap();
        db.insert(&name, &[("v", Value::Integer(cycle as i64))])
            .unwrap();
        assert_eq!(db.count(&name, &[]).unwrap(), 1);
        db.drop_table(&name).unwrap();
        // Table should be gone
        assert!(db.get(&name, 1).is_err());
    }
}

/// Multiple threads create and write to different tables concurrently.
#[test]
fn test_concurrent_table_creation() {
    let (db, _dir) = create_db();
    let db = Arc::new(db);

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                let name = format!("table_{i}");
                db.create_table(&name, &[ColumnDef::new("v", Type::Integer)])
                    .unwrap();
                for j in 0..100 {
                    db.insert(&name, &[("v", Value::Integer(j))]).unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    for i in 0..4 {
        let name = format!("table_{i}");
        assert_eq!(db.count(&name, &[]).unwrap(), 100);
    }
}

/// Concurrent writers to the same table, each inserting and then verifying their rows.
#[test]
fn test_concurrent_same_table_writes() {
    let (db, _dir) = create_db();
    db.create_table(
        "shared",
        &[
            ColumnDef::new("thread", Type::Integer),
            ColumnDef::new("seq", Type::Integer),
        ],
    )
    .unwrap();
    let db = Arc::new(db);

    let per_thread = 100;
    let num_threads = 4;

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                let mut ids = Vec::with_capacity(per_thread);
                for s in 0..per_thread {
                    let id = db
                        .insert(
                            "shared",
                            &[
                                ("thread", Value::Integer(t as i64)),
                                ("seq", Value::Integer(s as i64)),
                            ],
                        )
                        .unwrap();
                    ids.push(id);
                }
                ids
            })
        })
        .collect();

    let all_ids: Vec<Vec<u64>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Total count
    assert_eq!(
        db.count("shared", &[]).unwrap(),
        (num_threads * per_thread) as u64
    );

    // Each thread's rows should be readable
    for thread_ids in &all_ids {
        for &id in thread_ids {
            assert!(db.get("shared", id).unwrap().is_some());
        }
    }
}

/// Stress the index by inserting and querying with many duplicate values.
#[test]
fn test_index_stress_many_duplicates() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("category", Type::Text)])
        .unwrap();
    db.create_index("t", "idx_cat", "category").unwrap();

    let categories = ["electronics", "clothing", "food", "books", "toys"];
    for i in 0..1000 {
        let cat = categories[i % categories.len()];
        db.insert("t", &[("category", Value::Text(cat.into()))])
            .unwrap();
    }

    // Each category should have 200 rows
    for cat in &categories {
        let count = db.count("t", &[Filter::eq("category", *cat)]).unwrap();
        assert_eq!(count, 200, "category '{cat}' should have 200 rows");
    }
}

/// Interleaved insert_many and delete_where.
#[test]
fn test_bulk_insert_and_bulk_delete() {
    let (db, _dir) = create_db();
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .unwrap();

    // Insert 500
    let rows: Vec<Vec<(&str, Value)>> = (0..500)
        .map(|i| vec![("v", Value::Integer(i))])
        .collect();
    db.insert_many("t", &rows).unwrap();
    assert_eq!(db.count("t", &[]).unwrap(), 500);

    // Delete values < 100
    let deleted = db.delete_where("t", &[Filter::lt("v", 100i64)]).unwrap();
    assert_eq!(deleted, 100);
    assert_eq!(db.count("t", &[]).unwrap(), 400);

    // Delete values >= 400
    let deleted = db.delete_where("t", &[Filter::ge("v", 400i64)]).unwrap();
    assert_eq!(deleted, 100);
    assert_eq!(db.count("t", &[]).unwrap(), 300);
}
