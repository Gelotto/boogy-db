//! Benchmark: ACID transactions — boogy-db vs SQLite.
//!
//! Compares multi-operation transactions: insert N rows in one transaction,
//! mixed read/write transactions, and transaction throughput at various sizes.

use std::time::Instant;
use boogy_db::*;

fn main() {
    println!("=== ACID Transaction Benchmarks ===\n");

    // --- Transaction Insert (N rows per transaction) ---
    println!("--- Transaction Insert (N rows per tx, commit once) ---\n");
    println!("{:>8} {:>14} {:>14} {:>14} {:>8}",
        "rows/tx", "boogy(acid)", "boogy(fast)", "sqlite", "ratio");

    for batch in [1, 10, 50, 100, 500] {
        let boogy_acid = bench_boogy_tx_insert(batch, true);
        let boogy_fast = bench_boogy_tx_insert(batch, false);
        let sqlite = bench_sqlite_tx_insert(batch);
        let ratio = boogy_acid / sqlite;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>12.0} r/s {:>12.0} r/s {:>12.0} r/s {:>6.2}x ({winner})",
            batch, boogy_acid, boogy_fast, sqlite, ratio);
    }

    // --- Transaction Mixed (read + write in same tx) ---
    println!("\n--- Mixed Transaction (insert + get + update in one tx) ---");
    println!("Table seeded with 1,000 rows. Each tx: 1 insert + 2 gets + 1 update.\n");
    println!("{:>14} {:>14} {:>14} {:>8}",
        "boogy(acid)", "boogy(fast)", "sqlite", "ratio");

    let boogy_acid = bench_boogy_mixed_tx(true);
    let boogy_fast = bench_boogy_mixed_tx(false);
    let sqlite = bench_sqlite_mixed_tx();
    let ratio = boogy_acid / sqlite;
    let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
    println!("{:>12.0} tx/s {:>12.0} tx/s {:>12.0} tx/s {:>6.2}x ({winner})",
        boogy_acid, boogy_fast, sqlite, ratio);

    // --- Transaction Throughput (small tx, high frequency) ---
    println!("\n--- Transaction Throughput (1 insert per tx, measure tx/s) ---\n");
    println!("{:>14} {:>14} {:>14} {:>8}",
        "boogy(acid)", "boogy(fast)", "sqlite", "ratio");

    let boogy_acid = bench_boogy_tx_throughput(true);
    let boogy_fast = bench_boogy_tx_throughput(false);
    let sqlite = bench_sqlite_tx_throughput();
    let ratio = boogy_acid / sqlite;
    let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
    println!("{:>12.0} tx/s {:>12.0} tx/s {:>12.0} tx/s {:>6.2}x ({winner})",
        boogy_acid, boogy_fast, sqlite, ratio);

    // --- Rollback Cost ---
    println!("\n--- Rollback Cost (begin + 10 inserts + drop, no commit) ---\n");
    println!("{:>14} {:>14}",
        "boogy(acid)", "boogy(fast)");

    let boogy_acid = bench_boogy_rollback(true);
    let boogy_fast = bench_boogy_rollback(false);
    println!("{:>12.0} rb/s {:>12.0} rb/s", boogy_acid, boogy_fast);
}

// ---------------------------------------------------------------------------
// Transaction Insert
// ---------------------------------------------------------------------------

fn bench_boogy_tx_insert(rows_per_tx: usize, acid: bool) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.set_acid(acid);
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    let total_rows = 10_000usize;
    let num_txs = total_rows / rows_per_tx;

    let t = Instant::now();
    for tx_idx in 0..num_txs {
        let mut tx = db.begin().unwrap();
        for i in 0..rows_per_tx {
            let n = tx_idx * rows_per_tx + i;
            tx.insert("t", &[
                ("name", Value::Text(format!("item_{n}"))),
                ("value", Value::Integer(n as i64)),
            ]).unwrap();
        }
        tx.commit().unwrap();
    }
    let elapsed = t.elapsed().as_secs_f64();
    total_rows as f64 / elapsed
}

fn bench_sqlite_tx_insert(rows_per_tx: usize) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE t (name TEXT, value INTEGER)", [],
    ).unwrap();

    let total_rows = 10_000usize;
    let num_txs = total_rows / rows_per_tx;

    let t = Instant::now();
    for tx_idx in 0..num_txs {
        conn.execute("BEGIN", []).unwrap();
        for i in 0..rows_per_tx {
            let n = tx_idx * rows_per_tx + i;
            conn.execute(
                "INSERT INTO t (name, value) VALUES (?1, ?2)",
                rusqlite::params![format!("item_{n}"), n as i64],
            ).unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
    }
    let elapsed = t.elapsed().as_secs_f64();
    total_rows as f64 / elapsed
}

// ---------------------------------------------------------------------------
// Mixed Transaction
// ---------------------------------------------------------------------------

fn bench_boogy_mixed_tx(acid: bool) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.set_acid(acid);
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    // Seed
    for i in 0..1000 {
        db.insert("t", &[
            ("name", Value::Text(format!("item_{i}"))),
            ("value", Value::Integer(i)),
        ]).unwrap();
    }

    let n = 5_000usize;
    let mut rng: u64 = 12345;

    let t = Instant::now();
    for _ in 0..n {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut tx = db.begin().unwrap();
        // Insert
        tx.insert("t", &[
            ("name", Value::Text("new".into())),
            ("value", Value::Integer(rng as i64)),
        ]).unwrap();
        // Two gets
        let id1 = (rng >> 16) % 1000 + 1;
        let id2 = (rng >> 32) % 1000 + 1;
        let _ = tx.get("t", id1);
        let _ = tx.get("t", id2);
        // Update
        let _ = tx.update("t", id1, &[("value", Value::Integer(rng as i64))]);
        tx.commit().unwrap();
    }
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

fn bench_sqlite_mixed_tx() -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE t (_id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, value INTEGER)", [],
    ).unwrap();

    for i in 0..1000 {
        conn.execute(
            "INSERT INTO t (name, value) VALUES (?1, ?2)",
            rusqlite::params![format!("item_{i}"), i],
        ).unwrap();
    }

    let n = 5_000usize;
    let mut rng: u64 = 12345;

    let t = Instant::now();
    for _ in 0..n {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        conn.execute("BEGIN", []).unwrap();
        // Insert
        conn.execute(
            "INSERT INTO t (name, value) VALUES (?1, ?2)",
            rusqlite::params!["new", rng as i64],
        ).unwrap();
        // Two gets
        let id1 = (rng >> 16) % 1000 + 1;
        let id2 = (rng >> 32) % 1000 + 1;
        let mut stmt = conn.prepare_cached("SELECT * FROM t WHERE _id = ?1").unwrap();
        let _: Vec<(i64, String, i64)> = stmt.query_map([id1 as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }).unwrap().map(|r| r.unwrap()).collect();
        let _: Vec<(i64, String, i64)> = stmt.query_map([id2 as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }).unwrap().map(|r| r.unwrap()).collect();
        // Update
        conn.execute(
            "UPDATE t SET value = ?1 WHERE _id = ?2",
            rusqlite::params![rng as i64, id1 as i64],
        ).unwrap();
        conn.execute("COMMIT", []).unwrap();
    }
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

// ---------------------------------------------------------------------------
// Transaction Throughput (1 insert per tx)
// ---------------------------------------------------------------------------

fn bench_boogy_tx_throughput(acid: bool) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.set_acid(acid);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let n = 20_000usize;
    let t = Instant::now();
    for i in 0..n {
        let mut tx = db.begin().unwrap();
        tx.insert("t", &[("v", Value::Integer(i as i64))]).unwrap();
        tx.commit().unwrap();
    }
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

fn bench_sqlite_tx_throughput() -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute("CREATE TABLE t (v INTEGER)", []).unwrap();

    let n = 20_000usize;
    let t = Instant::now();
    for i in 0..n {
        conn.execute("BEGIN", []).unwrap();
        conn.execute("INSERT INTO t (v) VALUES (?1)", [i as i64]).unwrap();
        conn.execute("COMMIT", []).unwrap();
    }
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}

// ---------------------------------------------------------------------------
// Rollback Cost
// ---------------------------------------------------------------------------

fn bench_boogy_rollback(acid: bool) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.set_acid(acid);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let n = 10_000usize;
    let t = Instant::now();
    for _ in 0..n {
        let mut tx = db.begin().unwrap();
        for i in 0..10 {
            tx.insert("t", &[("v", Value::Integer(i))]).unwrap();
        }
        // Drop without commit — rollback
        drop(tx);
    }
    let elapsed = t.elapsed().as_secs_f64();
    n as f64 / elapsed
}
