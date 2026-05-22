#![cfg(feature = "tokio")]

use boogy_db::*;
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper: open a fresh temp db with ACID mode enabled.
// ---------------------------------------------------------------------------

async fn open_db(dir: &TempDir) -> AsyncBoogyDb {
    let db = AsyncBoogyDb::open(dir.path().join("test.boogy"))
        .await
        .unwrap();
    db.set_acid(true);
    db
}

// ---------------------------------------------------------------------------
// 1. Read-your-writes: insert then get/find inside the tx sees the row BEFORE
//    commit.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tx_reads_its_own_writes() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir).await;
    db.create_table("things", &[ColumnDef::new("name", Type::Text)])
        .await
        .unwrap();

    let mut tx = db.begin_interactive().await.unwrap();

    // Insert is a write — gate is acquired here.
    let id = tx
        .insert("things", &[("name", Value::Text("Alice".into()))])
        .await
        .unwrap();

    // get() reads the tx overlay — should see "Alice" before any commit.
    let row = tx.get("things", id).await.unwrap();
    assert!(row.is_some(), "get() should see own write before commit");
    assert_eq!(
        row.unwrap().get("name").unwrap(),
        Value::Text("Alice".into()),
        "get() should return the just-inserted value"
    );

    // find() also reads the overlay.
    let result = tx
        .find(
            "things",
            FindOptions {
                filters: vec![Filter::eq("name", "Alice")],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        result.rows.len(),
        1,
        "find() should see own write before commit"
    );

    // The db itself should NOT see it yet (read goes to committed state).
    let outside_count = db.count("things", &[]).await.unwrap();
    assert_eq!(
        outside_count, 0,
        "uncommitted write must not be visible outside the tx"
    );

    tx.commit().await.unwrap();

    // Now committed — visible to the db.
    assert_eq!(db.count("things", &[]).await.unwrap(), 1);
}

// ---------------------------------------------------------------------------
// 2. Eager real id + FK footgun fix:
//    - The id returned by tx.insert(parent) is a normal small rowid (not a
//      sentinel like u64::MAX - n).
//    - That id can be used immediately as a FK value in a second insert within
//      the SAME tx.
//    - After commit the child's parent_id column equals pid, and a parent row
//      with id == pid actually exists.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tx_insert_returns_real_id_usable_as_fk() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir).await;

    db.create_table("parents", &[ColumnDef::new("label", Type::Text)])
        .await
        .unwrap();
    db.create_table(
        "children",
        &[
            ColumnDef::new("parent_id", Type::Integer),
            ColumnDef::new("name", Type::Text),
        ],
    )
    .await
    .unwrap();

    let mut tx = db.begin_interactive().await.unwrap();

    // Insert parent — the returned id must be a real, small rowid.
    let pid = tx
        .insert("parents", &[("label", Value::Text("parent-A".into()))])
        .await
        .unwrap();

    // THE FOOTGUN FIX: the id is a genuine, small rowid — not a u64::MAX sentinel.
    assert!(
        pid < 1_000,
        "parent id should be a real small rowid, not a sentinel (got {})",
        pid
    );

    // Use pid immediately as an FK in a child insert — still inside the same tx,
    // before any commit.
    let cid = tx
        .insert(
            "children",
            &[
                ("parent_id", Value::Integer(pid as i64)),
                ("name", Value::Text("child-1".into())),
            ],
        )
        .await
        .unwrap();

    tx.commit().await.unwrap();

    // After commit: parent row with id == pid must exist.
    let parent_row = db.get("parents", pid).await.unwrap();
    assert!(
        parent_row.is_some(),
        "parent row with id {} must exist after commit",
        pid
    );

    // Child's parent_id column must equal pid.
    let child_row = db.get("children", cid).await.unwrap().unwrap();
    let stored_parent_id = match child_row.get("parent_id").unwrap() {
        Value::Integer(n) => n as u64,
        other => panic!("expected Integer for parent_id, got {:?}", other),
    };
    assert_eq!(
        stored_parent_id, pid,
        "child.parent_id ({}) must equal the parent's real rowid ({})",
        stored_parent_id, pid
    );
}

// ---------------------------------------------------------------------------
// 3. Commit persists; drop-without-commit (rollback) persists nothing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tx_commit_and_rollback() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir).await;
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)])
        .await
        .unwrap();

    // First tx: insert + commit → row persists.
    let mut tx = db.begin_interactive().await.unwrap();
    tx.insert("t", &[("v", Value::Integer(1))]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(db.count("t", &[]).await.unwrap(), 1);

    // Second tx: insert then DROP without commit → row must not persist.
    {
        let mut tx2 = db.begin_interactive().await.unwrap();
        tx2.insert("t", &[("v", Value::Integer(2))]).await.unwrap();
        // tx2 drops here without commit → rollback
    }

    // Only the committed row should be present.
    assert_eq!(
        db.count("t", &[]).await.unwrap(),
        1,
        "rolled-back write must leave the db unchanged"
    );

    // Verify it's the committed row, not the rolled-back one.
    let result = db
        .find(
            "t",
            FindOptions {
                filters: vec![Filter::eq("v", 1i64)],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1, "the committed row (v=1) must be present");

    let result2 = db
        .find(
            "t",
            FindOptions {
                filters: vec![Filter::eq("v", 2i64)],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result2.rows.len(), 0, "the rolled-back row (v=2) must be absent");
}

// ---------------------------------------------------------------------------
// 4. Full op surface: update_where / delete_where / upsert_increment /
//    count / scan_batch all reflect the tx's overlay before commit, and all
//    writes are persisted after commit.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tx_full_ops() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir).await;
    db.create_table(
        "items",
        &[
            ColumnDef::new("label", Type::Text),
            ColumnDef::new("score", Type::Integer),
        ],
    )
    .await
    .unwrap();

    // Seed one committed row outside the tx.
    let seeded_id = db
        .insert("items", &[("label", Value::Text("seed".into())), ("score", Value::Integer(0))])
        .await
        .unwrap();

    let mut tx = db.begin_interactive().await.unwrap();

    // Insert two rows in the tx.
    let id_a = tx
        .insert(
            "items",
            &[("label", Value::Text("alpha".into())), ("score", Value::Integer(10))],
        )
        .await
        .unwrap();
    let _id_b = tx
        .insert(
            "items",
            &[("label", Value::Text("beta".into())), ("score", Value::Integer(20))],
        )
        .await
        .unwrap();

    // count() should see 3 rows: 1 committed seed + 2 from the tx overlay.
    let cnt = tx.count("items", &[]).await.unwrap();
    assert_eq!(cnt, 3, "count() should see the tx's own writes (got {})", cnt);

    // update_where: set score = 99 on rows where label = "alpha".
    let updated = tx
        .update_where(
            "items",
            &[Filter::eq("label", "alpha")],
            &[("score", Value::Integer(99))],
        )
        .await
        .unwrap();
    assert_eq!(updated, 1, "update_where should touch exactly the alpha row");

    // Verify the update is visible in the overlay.
    let alpha_row = tx.get("items", id_a).await.unwrap().unwrap();
    assert_eq!(
        alpha_row.get("score").unwrap(),
        Value::Integer(99),
        "updated score should be visible inside the tx"
    );

    // delete_where: delete rows where label = "beta".
    let deleted = tx
        .delete_where("items", &[Filter::eq("label", "beta")])
        .await
        .unwrap();
    assert_eq!(deleted, 1, "delete_where should remove exactly the beta row");

    // count after delete: should see 2 (seed + alpha).
    let cnt2 = tx.count("items", &[]).await.unwrap();
    assert_eq!(cnt2, 2, "count after delete should be 2 (got {})", cnt2);

    // upsert_increment: increment the seeded row's score by 5.
    let new_score_id = tx
        .upsert_increment(
            "items",
            &[("label", Value::Text("seed".into()))],
            "score",
            Value::Integer(5),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(new_score_id, seeded_id, "upsert_increment must return the existing row's id");

    // scan_batch: scan in primary-key ascending order — should see 2 rows.
    let batch = tx
        .scan_batch(
            "items",
            &[],
            &[],
            ScanOrder::primary_key(SortDir::Asc),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        batch.rows.len(),
        2,
        "scan_batch should see the tx overlay (got {} rows)",
        batch.rows.len()
    );

    tx.commit().await.unwrap();

    // After commit: db sees exactly 2 rows.
    assert_eq!(db.count("items", &[]).await.unwrap(), 2);

    // The seed row's score should be 5 (was 0 + 5 from upsert_increment).
    let seed_row = db.get("items", seeded_id).await.unwrap().unwrap();
    assert_eq!(seed_row.get("score").unwrap(), Value::Integer(5));

    // Alpha's score should be 99.
    let alpha_row = db.get("items", id_a).await.unwrap().unwrap();
    assert_eq!(alpha_row.get("score").unwrap(), Value::Integer(99));
}

// ---------------------------------------------------------------------------
// 5. Concurrency: two concurrent interactive write-txs serialize — no lost
//    update. Both tasks upsert_increment a shared counter by 1; after both
//    join the counter must equal 2, not 1 (which would indicate a lost update).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_write_txs_serialize() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_db(&dir).await);

    db.create_table(
        "counters",
        &[
            ColumnDef::new("key", Type::Text),
            ColumnDef::new("value", Type::Integer),
        ],
    )
    .await
    .unwrap();

    // Initialize the counter to 0.
    db.insert(
        "counters",
        &[
            ("key", Value::Text("hits".into())),
            ("value", Value::Integer(0)),
        ],
    )
    .await
    .unwrap();

    // Number of concurrent tasks. Each increments the counter by 1.
    const TASKS: usize = 5;

    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let db_clone = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            let mut tx = db_clone.begin_interactive().await.unwrap();
            tx.upsert_increment(
                "counters",
                &[("key", Value::Text("hits".into()))],
                "value",
                Value::Integer(1),
                &[],
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // The counter must equal TASKS — no lost updates.
    let result = db
        .find(
            "counters",
            FindOptions {
                filters: vec![Filter::eq("key", "hits")],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1, "counter row must exist");

    let final_value = match result.rows[0].get("value").unwrap() {
        Value::Integer(n) => n,
        other => panic!("expected Integer for counter value, got {:?}", other),
    };
    assert_eq!(
        final_value, TASKS as i64,
        "serialize-writers must prevent lost updates: expected counter={}, got {}",
        TASKS, final_value
    );
}

// ---------------------------------------------------------------------------
// 6. Held across await: the tx survives an async yield between write ops,
//    and find() sees all writes after the yield.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tx_held_across_await() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir).await;
    db.create_table("log", &[ColumnDef::new("msg", Type::Text)])
        .await
        .unwrap();

    let mut tx = db.begin_interactive().await.unwrap();

    // First insert — write gate acquired here.
    let id1 = tx
        .insert("log", &[("msg", Value::Text("before-yield".into()))])
        .await
        .unwrap();

    // Yield to the async runtime — simulates a real await point between ops.
    tokio::task::yield_now().await;

    // Second insert after the yield — tx must still be valid and holding the gate.
    let id2 = tx
        .insert("log", &[("msg", Value::Text("after-yield".into()))])
        .await
        .unwrap();

    // find() should see BOTH rows (both are in the overlay).
    let result = tx
        .find("log", FindOptions { ..Default::default() })
        .await
        .unwrap();
    assert_eq!(
        result.rows.len(),
        2,
        "tx held across await must see all inserts in the overlay (got {})",
        result.rows.len()
    );

    // Verify individual rows via get().
    assert!(
        tx.get("log", id1).await.unwrap().is_some(),
        "first row (before yield) must be visible"
    );
    assert!(
        tx.get("log", id2).await.unwrap().is_some(),
        "second row (after yield) must be visible"
    );

    tx.commit().await.unwrap();

    // Both rows persisted.
    assert_eq!(db.count("log", &[]).await.unwrap(), 2);
}
