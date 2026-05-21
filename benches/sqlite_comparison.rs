//! Benchmark: boogy-db vs SQLite (via rusqlite)
//! Same social feed workload as SpinStack's store-bench.
//! Runs two variants: without indexes and with indexes on the filter column.
//! boogy-db is tested at both Durability::None and Durability::Normal.

use std::time::{Duration, Instant};
use boogy_db::*;

fn main() {
    let duration = Duration::from_secs(5);

    // ======================== WITHOUT INDEX ========================

    println!("=== Without Index ===\n");
    println!("Workload: 30% insert / 30% get / 25% find / 15% count");
    println!("Duration: 5s per engine\n");

    // --- boogy-db (no index, Durability::None) ---
    let boogy_none_no_idx = run_boogy_fresh(Durability::None, false, duration);

    // --- boogy-db (no index, Durability::Normal) ---
    let boogy_normal_no_idx = run_boogy_fresh(Durability::Normal, false, duration);

    // --- SQLite (no index) ---
    let sqlite_no_idx = run_sqlite_fresh(false, duration);

    print_results("Without Index", &boogy_none_no_idx, &boogy_normal_no_idx, &sqlite_no_idx);

    // ======================== WITH INDEX ========================

    println!("\n=== With Index (on 'owner') ===\n");

    // --- boogy-db (with index, Durability::None) ---
    let boogy_none_idx = run_boogy_fresh(Durability::None, true, duration);

    // --- boogy-db (with index, Durability::Normal) ---
    let boogy_normal_idx = run_boogy_fresh(Durability::Normal, true, duration);

    // --- SQLite (with index) ---
    let sqlite_idx = run_sqlite_fresh(true, duration);

    print_results("With Index", &boogy_none_idx, &boogy_normal_idx, &sqlite_idx);
}

/// Create a fresh boogy-db, seed it, and run the workload.
fn run_boogy_fresh(durability: Durability, with_index: bool, duration: Duration) -> Results {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(durability);
    db.create_table("notes", &[
        ColumnDef::new("title", Type::Text),
        ColumnDef::new("body", Type::Text),
        ColumnDef::new("owner", Type::Text),
    ]).unwrap();
    if with_index {
        db.create_index("notes", "idx_owner", "owner").unwrap();
    }

    let mut ids: Vec<u64> = Vec::new();
    for i in 0..1000 {
        ids.push(db.insert("notes", &[
            ("title", Value::Text(format!("note_{i}"))),
            ("body", Value::Text("body content here".into())),
            ("owner", Value::Text(format!("user_{}", i % 10))),
        ]).unwrap());
    }

    let result = run_boogy(&db, &mut ids, duration);
    drop(db);
    drop(dir);
    result
}

/// Create a fresh SQLite DB, seed it, and run the workload.
fn run_sqlite_fresh(with_index: bool, duration: Duration) -> Results {
    let sdir = tempfile::TempDir::new().unwrap();
    let conn = rusqlite::Connection::open(sdir.path().join("bench.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    conn.execute(
        "CREATE TABLE notes (_id INTEGER PRIMARY KEY, title TEXT, body TEXT, owner TEXT)",
        [],
    ).unwrap();
    if with_index {
        conn.execute("CREATE INDEX idx_owner ON notes(owner)", []).unwrap();
    }

    let mut ids: Vec<i64> = Vec::new();
    conn.execute("BEGIN", []).unwrap();
    for i in 0..1000i64 {
        conn.execute(
            "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![i, format!("note_{i}"), "body content here", format!("user_{}", i % 10)],
        ).unwrap();
        ids.push(i);
    }
    conn.execute("COMMIT", []).unwrap();

    let result = run_sqlite(&conn, &mut ids, duration);
    drop(conn);
    drop(sdir);
    result
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

fn print_results(_label: &str, boogy_none: &Results, boogy_normal: &Results, sqlite: &Results) {
    let ratio_none = boogy_none.ops_sec / sqlite.ops_sec;
    let ratio_normal = boogy_normal.ops_sec / sqlite.ops_sec;
    println!("{:>20} {:>14} {:>14} {:>12}", "", "boogy(none)", "boogy(normal)", "sqlite");
    println!("{:>20} {:>14.0} {:>14.0} {:>12.0}", "ops/sec",
        boogy_none.ops_sec, boogy_normal.ops_sec, sqlite.ops_sec);
    println!("{:>20} {:>13} {:>13} {:>11}", "p50",
        fmt_us(boogy_none.p50), fmt_us(boogy_normal.p50), fmt_us(sqlite.p50));
    println!("{:>20} {:>13} {:>13} {:>11}", "p99",
        fmt_us(boogy_none.p99), fmt_us(boogy_normal.p99), fmt_us(sqlite.p99));
    println!("{:>20} {:>13.2}x {:>13.2}x", "vs sqlite", ratio_none, ratio_normal);
    println!();
    println!("{:>20} {:>13}/s {:>13}/s {:>11}/s", "insert",
        fmt_f(boogy_none.insert_count as f64 / 5.0),
        fmt_f(boogy_normal.insert_count as f64 / 5.0),
        fmt_f(sqlite.insert_count as f64 / 5.0));
    println!("{:>20} {:>13}/s {:>13}/s {:>11}/s", "get",
        fmt_f(boogy_none.get_count as f64 / 5.0),
        fmt_f(boogy_normal.get_count as f64 / 5.0),
        fmt_f(sqlite.get_count as f64 / 5.0));
    println!("{:>20} {:>13}/s {:>13}/s {:>11}/s", "find",
        fmt_f(boogy_none.find_count as f64 / 5.0),
        fmt_f(boogy_normal.find_count as f64 / 5.0),
        fmt_f(sqlite.find_count as f64 / 5.0));
    println!("{:>20} {:>13}/s {:>13}/s {:>11}/s", "count",
        fmt_f(boogy_none.count_count as f64 / 5.0),
        fmt_f(boogy_normal.count_count as f64 / 5.0),
        fmt_f(sqlite.count_count as f64 / 5.0));
}

fn run_boogy(db: &BoogyDb, ids: &mut Vec<u64>, duration: Duration) -> Results {
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
                let _ = db.get("notes", ids[idx]).unwrap();
                get_count += 1;
            }
            60..85 => {
                let owner = format!("user_{}", (rng_state >> 8) % 10);
                let _ = db.find("notes", FindOptions {
                    filters: vec![Filter::eq("owner", owner)],
                    or_groups: vec![],
                    sort: vec![],
                    limit: Some(20),
                    offset: None,
                    include_total: false,
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

fn run_sqlite(conn: &rusqlite::Connection, ids: &mut Vec<i64>, duration: Duration) -> Results {
    let mut latencies = Vec::with_capacity(100_000);
    let mut rng_state: u64 = 12345;
    let mut insert_count = 0u64;
    let mut get_count = 0u64;
    let mut find_count = 0u64;
    let mut count_count = 0u64;
    let mut next_id = 100_000i64;

    let start = Instant::now();
    while start.elapsed() < duration {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let op = (rng_state >> 32) % 100;
        let t = Instant::now();

        match op {
            0..30 => {
                next_id += 1;
                conn.execute(
                    "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![next_id, format!("new_{next_id}"), "new body", format!("user_{}", next_id % 10)],
                ).unwrap();
                ids.push(next_id);
                insert_count += 1;
            }
            30..60 => {
                let idx = (rng_state >> 16) as usize % ids.len();
                let mut stmt = conn.prepare_cached("SELECT * FROM notes WHERE _id = ?1").unwrap();
                let _: Vec<(i64, String, String, String)> = stmt.query_map([ids[idx]], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                }).unwrap().map(|r| r.unwrap()).collect();
                get_count += 1;
            }
            60..85 => {
                let owner = format!("user_{}", (rng_state >> 8) % 10);
                let mut stmt = conn.prepare_cached(
                    "SELECT * FROM notes WHERE owner = ?1 LIMIT 20"
                ).unwrap();
                let _: Vec<(i64, String, String, String)> = stmt.query_map([&owner], |row| {
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
