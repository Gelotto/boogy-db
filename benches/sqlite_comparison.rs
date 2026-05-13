//! Benchmark: boogy-db vs SQLite (via rusqlite)
//! Same social feed workload as SpinStack's store-bench.

use std::time::{Duration, Instant};
use boogy_db::*;

fn main() {
    println!("=== boogy-db vs SQLite Benchmark ===\n");
    println!("Workload: 30% insert / 30% get / 25% find / 15% count");
    println!("Duration: 5s per engine\n");

    // --- boogy-db ---
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(boogy_db::Durability::None); // Match SQLite's synchronous=NORMAL
    db.create_table("notes", &[
        ColumnDef::new("title", Type::Text),
        ColumnDef::new("body", Type::Text),
        ColumnDef::new("owner", Type::Text),
    ]).unwrap();
    db.create_index("notes", "idx_owner", "owner").unwrap();

    // Seed
    let mut boogy_ids: Vec<String> = Vec::new();
    for i in 0..1000 {
        let id = db.insert("notes", &[
            ("title", Value::Text(format!("note_{i}"))),
            ("body", Value::Text("body content here".into())),
            ("owner", Value::Text(format!("user_{}", i % 10))),
        ]).unwrap();
        boogy_ids.push(id);
    }

    let boogy_results = run_boogy(&db, &mut boogy_ids, Duration::from_secs(5));

    // --- SQLite ---
    let sdir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(sdir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE notes (_id TEXT PRIMARY KEY, title TEXT, body TEXT, owner TEXT)",
        [],
    ).unwrap();
    conn.execute("CREATE INDEX idx_owner ON notes (owner)", []).unwrap();

    let mut sqlite_ids: Vec<String> = Vec::new();
    conn.execute("BEGIN", []).unwrap();
    for i in 0..1000 {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, format!("note_{i}"), "body content here", format!("user_{}", i % 10)],
        ).unwrap();
        sqlite_ids.push(id);
    }
    conn.execute("COMMIT", []).unwrap();

    let sqlite_results = run_sqlite(&conn, &mut sqlite_ids, Duration::from_secs(5));

    // --- Results ---
    println!("{:>20} {:>12} {:>12}", "", "boogy-db", "sqlite");
    println!("{:>20} {:>12.0} {:>12.0}", "ops/sec", boogy_results.ops_sec, sqlite_results.ops_sec);
    println!("{:>20} {:>11} {:>11}", "p50", fmt_us(boogy_results.p50), fmt_us(sqlite_results.p50));
    println!("{:>20} {:>11} {:>11}", "p99", fmt_us(boogy_results.p99), fmt_us(sqlite_results.p99));
    println!();
    println!("{:>20} {:>11}/s {:>11}/s", "insert", fmt_f(boogy_results.insert_count as f64 / 5.0), fmt_f(sqlite_results.insert_count as f64 / 5.0));
    println!("{:>20} {:>11}/s {:>11}/s", "get", fmt_f(boogy_results.get_count as f64 / 5.0), fmt_f(sqlite_results.get_count as f64 / 5.0));
    println!("{:>20} {:>11}/s {:>11}/s", "find", fmt_f(boogy_results.find_count as f64 / 5.0), fmt_f(sqlite_results.find_count as f64 / 5.0));
    println!("{:>20} {:>11}/s {:>11}/s", "count", fmt_f(boogy_results.count_count as f64 / 5.0), fmt_f(sqlite_results.count_count as f64 / 5.0));
}

struct Results {
    ops_sec: f64,
    p50: Duration,
    p99: Duration,
    insert_count: u64,
    get_count: u64,
    find_count: u64,
    count_count: u64,
}

fn run_boogy(db: &BoogyDb, ids: &mut Vec<String>, duration: Duration) -> Results {
    let mut latencies = Vec::with_capacity(100_000);
    let mut rng_state: u64 = 12345;
    let mut insert_count = 0u64;
    let mut get_count = 0u64;
    let mut find_count = 0u64;
    let mut count_count = 0u64;
    let mut next_id = 100_000u64;

    let start = Instant::now();
    while start.elapsed() < duration {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let op = (rng_state >> 32) % 100;
        let t = Instant::now();

        match op {
            0..30 => {
                next_id += 1;
                let id = db.insert("notes", &[
                    ("title", Value::Text(format!("new_{next_id}"))),
                    ("body", Value::Text("new body".into())),
                    ("owner", Value::Text(format!("user_{}", next_id % 10))),
                ]).unwrap();
                ids.push(id);
                insert_count += 1;
            }
            30..60 => {
                let idx = (rng_state >> 16) as usize % ids.len();
                let _ = db.get("notes", &ids[idx]).unwrap();
                get_count += 1;
            }
            60..85 => {
                let owner = format!("user_{}", (rng_state >> 8) % 10);
                let _ = db.find("notes", FindOptions {
                    filters: vec![Filter::eq("owner", owner)],
                    sort: vec![],
                    limit: Some(20),
                    offset: None,
                }).unwrap();
                find_count += 1;
            }
            _ => {
                let owner = format!("user_{}", (rng_state >> 8) % 10);
                let _ = db.count("notes", &[Filter::eq("owner", owner)]).unwrap();
                count_count += 1;
            }
        }

        latencies.push(t.elapsed());
    }

    let total = latencies.len() as f64;
    latencies.sort();
    Results {
        ops_sec: total / duration.as_secs_f64(),
        p50: latencies[(total * 0.50) as usize],
        p99: latencies[(total * 0.99) as usize],
        insert_count,
        get_count,
        find_count,
        count_count,
    }
}

fn run_sqlite(conn: &rusqlite::Connection, ids: &mut Vec<String>, duration: Duration) -> Results {
    let mut latencies = Vec::with_capacity(100_000);
    let mut rng_state: u64 = 12345;
    let mut insert_count = 0u64;
    let mut get_count = 0u64;
    let mut find_count = 0u64;
    let mut count_count = 0u64;
    let mut next_id = 100_000u64;

    let start = Instant::now();
    while start.elapsed() < duration {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let op = (rng_state >> 32) % 100;
        let t = Instant::now();

        match op {
            0..30 => {
                next_id += 1;
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![id, format!("new_{next_id}"), "new body", format!("user_{}", next_id % 10)],
                ).unwrap();
                ids.push(id);
                insert_count += 1;
            }
            30..60 => {
                let idx = (rng_state >> 16) as usize % ids.len();
                let mut stmt = conn.prepare_cached("SELECT * FROM notes WHERE _id = ?1").unwrap();
                let _: Vec<(String, String, String, String)> = stmt.query_map([&ids[idx]], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                }).unwrap().map(|r| r.unwrap()).collect();
                get_count += 1;
            }
            60..85 => {
                let owner = format!("user_{}", (rng_state >> 8) % 10);
                let mut stmt = conn.prepare_cached(
                    "SELECT * FROM notes WHERE owner = ?1 LIMIT 20"
                ).unwrap();
                let _: Vec<(String, String, String, String)> = stmt.query_map([&owner], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                }).unwrap().map(|r| r.unwrap()).collect();
                find_count += 1;
            }
            _ => {
                let owner = format!("user_{}", (rng_state >> 8) % 10);
                let mut stmt = conn.prepare_cached(
                    "SELECT COUNT(*) FROM notes WHERE owner = ?1"
                ).unwrap();
                let _: i64 = stmt.query_row([&owner], |row| row.get(0)).unwrap();
                count_count += 1;
            }
        }

        latencies.push(t.elapsed());
    }

    let total = latencies.len() as f64;
    latencies.sort();
    Results {
        ops_sec: total / duration.as_secs_f64(),
        p50: latencies[(total * 0.50) as usize],
        p99: latencies[(total * 0.99) as usize],
        insert_count,
        get_count,
        find_count,
        count_count,
    }
}

fn fmt_us(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1000 { format!("{us} us") }
    else if us < 1_000_000 { format!("{:.1} ms", us as f64 / 1000.0) }
    else { format!("{:.2} s", us as f64 / 1_000_000.0) }
}

fn fmt_f(f: f64) -> String {
    if f > 1000.0 { format!("{:.0}", f) } else { format!("{:.1}", f) }
}
