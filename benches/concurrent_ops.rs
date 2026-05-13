//! Benchmark: boogy-db vs SQLite under concurrent load.
//! Tests 1, 2, 4, 8 threads with the same mixed workload.

use std::sync::Arc;
use std::time::{Duration, Instant};
use boogy_db::*;

fn main() {
    let duration = Duration::from_secs(5);

    println!("=== Concurrent Mixed Workload (no index) ===");
    println!("Workload per thread: 30% insert / 30% get / 25% find / 15% count");
    println!("Duration: 5s\n");
    println!("{:>8} {:>14} {:>14} {:>8}", "threads", "boogy ops/s", "sqlite ops/s", "ratio");

    for threads in [1, 2, 4, 8] {
        let boogy = bench_boogy(threads, duration, false);
        let sqlite = bench_sqlite(threads, duration, false);
        let ratio = boogy as f64 / sqlite as f64;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>14} {:>14} {:>7.2}x ({winner})", threads, boogy, sqlite, ratio);
    }

    println!("\n=== Concurrent Mixed Workload (with index) ===\n");
    println!("{:>8} {:>14} {:>14} {:>8}", "threads", "boogy ops/s", "sqlite ops/s", "ratio");

    for threads in [1, 2, 4, 8] {
        let boogy = bench_boogy(threads, duration, true);
        let sqlite = bench_sqlite(threads, duration, true);
        let ratio = boogy as f64 / sqlite as f64;
        let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
        println!("{:>8} {:>14} {:>14} {:>7.2}x ({winner})", threads, boogy, sqlite, ratio);
    }
}

fn bench_boogy(num_threads: usize, duration: Duration, with_index: bool) -> u64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(BoogyDb::open(dir.path().join("bench.boogy")).unwrap());
    db.set_durability(Durability::None);
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
