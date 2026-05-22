//! Benchmark: boogy-db vs SQLite under concurrent load.
//! Tests 1, 2, 4, 8 threads with the same mixed workload.
//! boogy-db is tested at both Durability::None and Durability::Normal.

use std::sync::Arc;
use std::time::{Duration, Instant};
use boogy_db::*;

fn main() {
    let duration = Duration::from_secs(5);

    println!("=== Concurrent Mixed Workload (no index) ===");
    println!("Workload per thread: 30% insert / 30% get / 25% find / 15% count");
    println!("Duration: 5s\n");
    println!("{:>8} {:>14} {:>14} {:>14} {:>8}",
        "threads", "boogy(none)", "boogy(normal)", "sqlite", "ratio");

    for threads in [1, 2, 4, 8] {
        let boogy_none = bench_boogy(threads, duration, false, Durability::None);
        let boogy_normal = bench_boogy(threads, duration, false, Durability::Normal);
        let sqlite = bench_sqlite(threads, duration, false);
        let ratio_none = boogy_none as f64 / sqlite as f64;
        let winner = if ratio_none > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>14} {:>14} {:>14} {:>7.2}x ({winner})",
            threads, boogy_none, boogy_normal, sqlite, ratio_none);
    }

    println!("\n=== Concurrent Mixed Workload (with index) ===\n");
    println!("{:>8} {:>14} {:>14} {:>14} {:>8}",
        "threads", "boogy(none)", "boogy(normal)", "sqlite", "ratio");

    for threads in [1, 2, 4, 8] {
        let boogy_none = bench_boogy(threads, duration, true, Durability::None);
        let boogy_normal = bench_boogy(threads, duration, true, Durability::Normal);
        let sqlite = bench_sqlite(threads, duration, true);
        let ratio_none = boogy_none as f64 / sqlite as f64;
        let winner = if ratio_none > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>14} {:>14} {:>14} {:>7.2}x ({winner})",
            threads, boogy_none, boogy_normal, sqlite, ratio_none);
    }

    bench_write_tx_section();
}

fn bench_boogy(num_threads: usize, duration: Duration, with_index: bool, durability: Durability) -> u64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(BoogyDb::open(dir.path().join("bench.boogy")).unwrap());
    db.set_durability(durability);
    db.create_table("notes", &[
        ColumnDef::new("title", Type::Text),
        ColumnDef::new("body", Type::Text),
        ColumnDef::new("owner", Type::Text),
    ]).unwrap();
    if with_index {
        db.create_index("notes", "idx_owner", "owner").unwrap();
    }

    // Seed
    for i in 0..1000 {
        db.insert("notes", &[
            ("title", Value::Text(format!("note_{i}"))),
            ("body", Value::Text("body content here".into())),
            ("owner", Value::Text(format!("user_{}", i % 10))),
        ]).unwrap();
    }

    let handles: Vec<_> = (0..num_threads).map(|thread_id| {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let mut ops = 0u64;
            let mut rng_state: u64 = 12345 + thread_id as u64 * 99991;
            let mut next_id = 100_000u64 + thread_id as u64 * 1_000_000;

            let start = Instant::now();
            while start.elapsed() < duration {
                rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let op = (rng_state >> 32) % 100;

                match op {
                    0..30 => {
                        next_id += 1;
                        let _ = db.insert("notes", &[
                            ("title", Value::Text(format!("t_{next_id}"))),
                            ("body", Value::Text("body".into())),
                            ("owner", Value::Text(format!("user_{}", next_id % 10))),
                        ]);
                    }
                    30..60 => {
                        let id = (rng_state >> 16) % 1000 + 1;
                        let _ = db.get("notes", id);
                    }
                    60..85 => {
                        let owner = format!("user_{}", (rng_state >> 8) % 10);
                        let _ = db.find("notes", FindOptions {
                            filters: vec![Filter::eq("owner", owner)],
                            limit: Some(20),
                            include_total: false,
                            ..Default::default()
                        });
                    }
                    _ => {
                        let owner = format!("user_{}", (rng_state >> 8) % 10);
                        let _ = db.count("notes", &[Filter::eq("owner", owner)]);
                    }
                }
                ops += 1;
            }
            ops
        })
    }).collect();

    handles.into_iter().map(|h| h.join().unwrap()).sum::<u64>() / duration.as_secs()
}

fn bench_sqlite(num_threads: usize, duration: Duration, with_index: bool) -> u64 {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bench.db");

    // Setup
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
        conn.execute(
            "CREATE TABLE notes (_id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT, owner TEXT)",
            [],
        ).unwrap();
        if with_index {
            conn.execute("CREATE INDEX idx_owner ON notes(owner)", []).unwrap();
        }
        conn.execute("BEGIN", []).unwrap();
        for i in 0..1000 {
            conn.execute(
                "INSERT INTO notes (title, body, owner) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("note_{i}"), "body content here", format!("user_{}", i % 10)],
            ).unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
    }

    let handles: Vec<_> = (0..num_threads).map(|thread_id| {
        let path = path.clone();
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();

            let mut ops = 0u64;
            let mut rng_state: u64 = 12345 + thread_id as u64 * 99991;

            let start = Instant::now();
            while start.elapsed() < duration {
                rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let op = (rng_state >> 32) % 100;

                match op {
                    0..30 => {
                        let _ = conn.execute(
                            "INSERT INTO notes (title, body, owner) VALUES (?1, ?2, ?3)",
                            rusqlite::params!["title", "body", format!("user_{}", rng_state % 10)],
                        );
                    }
                    30..60 => {
                        let id = (rng_state >> 16) % 1000 + 1;
                        let mut stmt = conn.prepare_cached("SELECT * FROM notes WHERE _id = ?1").unwrap();
                        let _: Vec<(i64, String, String, String)> = stmt.query_map([id as i64], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        }).unwrap().map(|r| r.unwrap()).collect();
                    }
                    60..85 => {
                        let owner = format!("user_{}", (rng_state >> 8) % 10);
                        let mut stmt = conn.prepare_cached(
                            "SELECT * FROM notes WHERE owner = ?1 LIMIT 20"
                        ).unwrap();
                        let _: Vec<(i64, String, String, String)> = stmt.query_map([&owner], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        }).unwrap().map(|r| r.unwrap()).collect();
                    }
                    _ => {
                        let owner = format!("user_{}", (rng_state >> 8) % 10);
                        let mut stmt = conn.prepare_cached(
                            "SELECT COUNT(*) FROM notes WHERE owner = ?1"
                        ).unwrap();
                        let _: i64 = stmt.query_row([&owner], |row| row.get(0)).unwrap();
                    }
                }
                ops += 1;
            }
            ops
        })
    }).collect();

    handles.into_iter().map(|h| h.join().unwrap()).sum::<u64>() / duration.as_secs()
}

// ---------------------------------------------------------------------------
// Concurrent write-transaction throughput (serialize-writers)
// ---------------------------------------------------------------------------
//
// Two scenarios at N concurrent tasks:
//   A) Interactive write-tx: begin_interactive → 2 inserts + 1 upsert_increment → commit
//   B) Non-tx baseline:      same 3 writes as separate AsyncBoogyDb calls
//
// A vs B shows the cost of the serialize-writers gate across the whole tx
// lifetime vs per-op serialization (the gate is still held per-op in B, but
// released immediately after each call).

async fn run_interactive_tx_tasks(db: Arc<AsyncBoogyDb>, tasks: usize, txs_per_task: usize) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(tasks);
    for task_id in 0..tasks {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            for i in 0..txs_per_task {
                let mut tx = db.begin_interactive().await.unwrap();
                tx.insert("t", &[
                    ("name", Value::Text(format!("task{task_id}_tx{i}_a"))),
                    ("v", Value::Integer((task_id * txs_per_task + i) as i64)),
                ]).await.unwrap();
                tx.insert("t", &[
                    ("name", Value::Text(format!("task{task_id}_tx{i}_b"))),
                    ("v", Value::Integer(i as i64 + 1000)),
                ]).await.unwrap();
                tx.upsert_increment(
                    "counters",
                    &[("key", Value::Text(format!("task{task_id}")))],
                    "n",
                    Value::Integer(1),
                    &[],
                ).await.unwrap();
                tx.commit().await.unwrap();
            }
        }));
    }
    for h in handles { h.await.unwrap(); }
    start.elapsed()
}

async fn run_nontx_tasks(db: Arc<AsyncBoogyDb>, tasks: usize, txs_per_task: usize) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(tasks);
    for task_id in 0..tasks {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            for i in 0..txs_per_task {
                db.insert("t", &[
                    ("name", Value::Text(format!("task{task_id}_tx{i}_a"))),
                    ("v", Value::Integer((task_id * txs_per_task + i) as i64)),
                ]).await.unwrap();
                db.insert("t", &[
                    ("name", Value::Text(format!("task{task_id}_tx{i}_b"))),
                    ("v", Value::Integer(i as i64 + 1000)),
                ]).await.unwrap();
                db.upsert_increment(
                    "counters",
                    &[("key", Value::Text(format!("task{task_id}")))],
                    "n",
                    Value::Integer(1),
                    &[],
                ).await.unwrap();
            }
        }));
    }
    for h in handles { h.await.unwrap(); }
    start.elapsed()
}

fn open_async_db(acid: bool) -> Arc<AsyncBoogyDb> {
    // Single-thread runtime just for setup; the bench uses multi-thread.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Leak the TempDir so the DB file outlives the setup runtime.
    let dir = Box::leak(Box::new(tempfile::TempDir::new().unwrap()));
    let db = rt.block_on(async {
        let db = AsyncBoogyDb::open(dir.path().join("bench.boogy")).await.unwrap();
        db.set_acid(acid);
        db.set_durability(Durability::None);
        db.create_table("t", &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("v", Type::Integer),
        ]).await.unwrap();
        db.create_table("counters", &[
            ColumnDef::new("key", Type::Text),
            ColumnDef::new("n", Type::Integer),
        ]).await.unwrap();
        db.create_index("counters", "idx_key", "key").await.unwrap();
        db
    });
    Arc::new(db)
}

fn bench_concurrent_write_tx(tasks: usize, txs_per_task: usize) -> (f64, f64) {
    let total_txs = tasks * txs_per_task;

    // Scenario A: interactive write-tx (serialize-writers across tx lifetime)
    let db_a = open_async_db(true);
    let elapsed_a = {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(tasks.max(2))
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_interactive_tx_tasks(db_a, tasks, txs_per_task))
    };
    let tx_per_sec_a = total_txs as f64 / elapsed_a.as_secs_f64();

    // Scenario B: non-tx (per-op gate, released between calls)
    let db_b = open_async_db(false);
    let elapsed_b = {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(tasks.max(2))
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_nontx_tasks(db_b, tasks, txs_per_task))
    };
    let tx_per_sec_b = total_txs as f64 / elapsed_b.as_secs_f64();

    (tx_per_sec_a, tx_per_sec_b)
}

fn bench_write_tx_section() {
    let txs_per_task = 500;

    println!("\n=== Concurrent Write-Transaction Throughput (serialize-writers) ===");
    println!("Each 'tx' = begin_interactive → 2 inserts + 1 upsert_increment → commit");
    println!("Non-tx baseline: same 3 ops as separate AsyncBoogyDb calls (per-op gate)");
    println!("{txs_per_task} tx/task, Durability::None, ACID mode on\n");
    println!("{:>8}  {:>18}  {:>16}  {:>10}",
        "tasks", "interactive (tx/s)", "non-tx (tx/s)", "overhead");

    for tasks in [1, 2, 4, 8] {
        let (interactive, nontx) = bench_concurrent_write_tx(tasks, txs_per_task);
        let overhead_pct = (nontx - interactive) / nontx * 100.0;
        println!("{:>8}  {:>18.0}  {:>16.0}  {:>9.1}%",
            tasks, interactive, nontx, overhead_pct);
    }
}
