//! Benchmark: bulk operations — insert_many, update_where, delete_where.
//!
//! Compares boogy-db's bulk API methods against SQLite's equivalent
//! batch INSERT, UPDATE ... WHERE, and DELETE ... WHERE statements.
//! boogy-db is tested at both Durability::None and Durability::Normal.

use std::time::Instant;
use boogy_db::*;

fn main() {
    println!("=== Bulk Operations Benchmark ===\n");

    // --- Bulk Insert ---
    println!("--- Bulk Insert (single transaction) ---\n");
    println!("{:>8} {:>14} {:>14} {:>14} {:>8}",
        "rows", "boogy(none)", "boogy(normal)", "sqlite", "ratio");

    for batch_size in [100, 1_000, 10_000, 50_000] {
        let boogy_none = bench_boogy_insert(batch_size, Durability::None);
        let boogy_normal = bench_boogy_insert(batch_size, Durability::Normal);
        let sqlite = bench_sqlite_insert(batch_size);
        let ratio = boogy_none / sqlite;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>12.0} r/s {:>12.0} r/s {:>12.0} r/s {:>6.2}x ({winner})",
            batch_size, boogy_none, boogy_normal, sqlite, ratio);
    }

    // --- Bulk Update ---
    println!("\n--- Bulk Update (update_where / UPDATE ... WHERE) ---");
    println!("Table: 10,000 rows, 3 columns. Update 1 column on matching rows.\n");
    println!("{:>12} {:>14} {:>14} {:>14} {:>8}",
        "affected", "boogy(none)", "boogy(normal)", "sqlite", "ratio");

    for (filter_val, expected_label) in [("cat_0", "~1,000"), ("cat_0", "~1,000"), ("cat_4", "~2,000")] {
        let boogy_none = bench_boogy_update(filter_val, Durability::None);
        let boogy_normal = bench_boogy_update(filter_val, Durability::Normal);
        let sqlite = bench_sqlite_update(filter_val);
        let ratio = boogy_none / sqlite;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>12} {:>12.0} r/s {:>12.0} r/s {:>12.0} r/s {:>6.2}x ({winner})",
            expected_label, boogy_none, boogy_normal, sqlite, ratio);
    }

    // --- Bulk Delete ---
    println!("\n--- Bulk Delete (delete_where / DELETE ... WHERE) ---");
    println!("Table: 10,000 rows. Delete all rows matching a filter.\n");
    println!("{:>12} {:>14} {:>14} {:>14} {:>8}",
        "deleted", "boogy(none)", "boogy(normal)", "sqlite", "ratio");

    for (pct, filter_desc) in [(10, "~1,000"), (50, "~5,000"), (90, "~9,000")] {
        let boogy_none = bench_boogy_delete(pct, Durability::None);
        let boogy_normal = bench_boogy_delete(pct, Durability::Normal);
        let sqlite = bench_sqlite_delete(pct);
        let ratio = boogy_none / sqlite;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>12} {:>12.0} r/s {:>12.0} r/s {:>12.0} r/s {:>6.2}x ({winner})",
            filter_desc, boogy_none, boogy_normal, sqlite, ratio);
    }

    // --- Bulk Insert with Index ---
    println!("\n--- Bulk Insert with Index ---\n");
    println!("{:>8} {:>14} {:>14} {:>14} {:>8}",
        "rows", "boogy(none)", "boogy(normal)", "sqlite", "ratio");

    for batch_size in [100, 1_000, 10_000, 50_000] {
        let boogy_none = bench_boogy_insert_indexed(batch_size, Durability::None);
        let boogy_normal = bench_boogy_insert_indexed(batch_size, Durability::Normal);
        let sqlite = bench_sqlite_insert_indexed(batch_size);
        let ratio = boogy_none / sqlite;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>12.0} r/s {:>12.0} r/s {:>12.0} r/s {:>6.2}x ({winner})",
            batch_size, boogy_none, boogy_normal, sqlite, ratio);
    }
}

// ---------------------------------------------------------------------------
// Bulk Insert
// ---------------------------------------------------------------------------

fn bench_boogy_insert(n: usize, durability: Durability) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(durability);
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("category", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    let rows: Vec<Vec<(&str, Value)>> = (0..n).map(|i| vec![
        ("name", Value::Text(format!("item_{i}"))),
        ("category", Value::Text(format!("cat_{}", i % 10))),
        ("value", Value::Integer(i as i64)),
    ]).collect();

    let t = Instant::now();
    db.insert_many("t", &rows).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

fn bench_sqlite_insert(n: usize) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE t (name TEXT, category TEXT, value INTEGER)",
        [],
    ).unwrap();

    let t = Instant::now();
    conn.execute("BEGIN", []).unwrap();
    for i in 0..n {
        conn.execute(
            "INSERT INTO t (name, category, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![format!("item_{i}"), format!("cat_{}", i % 10), i as i64],
        ).unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

// ---------------------------------------------------------------------------
// Bulk Insert with Index
// ---------------------------------------------------------------------------

fn bench_boogy_insert_indexed(n: usize, durability: Durability) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(durability);
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("category", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();
    db.create_index("t", "idx_cat", "category").unwrap();

    let rows: Vec<Vec<(&str, Value)>> = (0..n).map(|i| vec![
        ("name", Value::Text(format!("item_{i}"))),
        ("category", Value::Text(format!("cat_{}", i % 10))),
        ("value", Value::Integer(i as i64)),
    ]).collect();

    let t = Instant::now();
    db.insert_many("t", &rows).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

fn bench_sqlite_insert_indexed(n: usize) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE t (name TEXT, category TEXT, value INTEGER)",
        [],
    ).unwrap();
    conn.execute("CREATE INDEX idx_cat ON t(category)", []).unwrap();

    let t = Instant::now();
    conn.execute("BEGIN", []).unwrap();
    for i in 0..n {
        conn.execute(
            "INSERT INTO t (name, category, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![format!("item_{i}"), format!("cat_{}", i % 10), i as i64],
        ).unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

// ---------------------------------------------------------------------------
// Bulk Update
// ---------------------------------------------------------------------------

fn seed_boogy_10k(durability: Durability) -> (BoogyDb, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(durability);
    db.create_table("t", &[
        ColumnDef::new("category", Type::Text),
        ColumnDef::new("status", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    let rows: Vec<Vec<(&str, Value)>> = (0..10_000).map(|i| vec![
        ("category", Value::Text(format!("cat_{}", i % 10))),
        ("status", Value::Text("active".into())),
        ("value", Value::Integer(i)),
    ]).collect();
    db.insert_many("t", &rows).unwrap();
    (db, dir)
}

fn seed_sqlite_10k() -> (rusqlite::Connection, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE t (category TEXT, status TEXT, value INTEGER)",
        [],
    ).unwrap();

    conn.execute("BEGIN", []).unwrap();
    for i in 0..10_000i64 {
        conn.execute(
            "INSERT INTO t (category, status, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![format!("cat_{}", i % 10), "active", i],
        ).unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    (conn, dir)
}

fn bench_boogy_update(filter_val: &str, durability: Durability) -> f64 {
    let (db, _dir) = seed_boogy_10k(durability);

    let t = Instant::now();
    let updated = db.update_where(
        "t",
        &[Filter::eq("category", filter_val)],
        &[("status", Value::Text("archived".into()))],
    ).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    updated as f64 / elapsed
}

fn bench_sqlite_update(filter_val: &str) -> f64 {
    let (conn, _dir) = seed_sqlite_10k();

    let t = Instant::now();
    let updated = conn.execute(
        "UPDATE t SET status = 'archived' WHERE category = ?1",
        [filter_val],
    ).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    updated as f64 / elapsed
}

// ---------------------------------------------------------------------------
// Bulk Delete
// ---------------------------------------------------------------------------

fn bench_boogy_delete(pct: usize, durability: Durability) -> f64 {
    let (db, _dir) = seed_boogy_10k(durability);
    // Delete rows where value < threshold (pct% of 10,000)
    let threshold = (10_000 * pct / 100) as i64;

    let t = Instant::now();
    let deleted = db.delete_where(
        "t",
        &[Filter::lt("value", threshold)],
    ).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    deleted as f64 / elapsed
}

fn bench_sqlite_delete(pct: usize) -> f64 {
    let (conn, _dir) = seed_sqlite_10k();
    let threshold = (10_000 * pct / 100) as i64;

    let t = Instant::now();
    let deleted = conn.execute(
        "DELETE FROM t WHERE value < ?1",
        [threshold],
    ).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    deleted as f64 / elapsed
}
