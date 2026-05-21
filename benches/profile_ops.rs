//! Micro-benchmark: measure each operation type independently at various table sizes.
//! Identifies exactly where boogy-db loses to SQLite.

use std::time::Instant;
use boogy_db::*;

fn main() {
    println!("=== Per-Operation Profiling (no index) ===\n");
    println!("{:>8} {:>10} {:>10} {:>10} {:>10}", "rows", "insert", "get", "find_eq", "count_eq");

    for seed_size in [1000, 2500, 5000] {
        let dir = tempfile::TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("notes", &[
            ColumnDef::new("title", Type::Text),
            ColumnDef::new("body", Type::Text),
            ColumnDef::new("owner", Type::Text),
        ]).unwrap();

        let mut ids: Vec<u64> = Vec::new();
        for i in 0..seed_size {
            ids.push(db.insert("notes", &[
                ("title", Value::Text(format!("note_{i}"))),
                ("body", Value::Text("body content here".into())),
                ("owner", Value::Text(format!("user_{}", i % 10))),
            ]).unwrap());
        }

        let n = 2000usize;

        // Insert
        let t = Instant::now();
        for i in 0..n {
            db.insert("notes", &[
                ("title", Value::Text(format!("new_{i}"))),
                ("body", Value::Text("new body".into())),
                ("owner", Value::Text(format!("user_{}", i % 10))),
            ]).unwrap();
        }
        let insert_us = t.elapsed().as_micros() as f64 / n as f64;

        // Get
        let t = Instant::now();
        for i in 0..n {
            let _ = db.get("notes", ids[i % ids.len()]).unwrap();
        }
        let get_us = t.elapsed().as_micros() as f64 / n as f64;

        // Find (eq filter, limit 20)
        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let _ = db.find("notes", FindOptions {
                filters: vec![Filter::eq("owner", owner)],
                or_groups: vec![],
                sort: vec![],
                limit: Some(20),
                offset: None,
                include_total: false,
            }).unwrap();
        }
        let find_us = t.elapsed().as_micros() as f64 / n as f64;

        // Count (eq filter)
        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let _ = db.count("notes", &[Filter::eq("owner", owner)]).unwrap();
        }
        let count_us = t.elapsed().as_micros() as f64 / n as f64;

        println!("{:>8} {:>9.1}µ {:>9.1}µ {:>9.1}µ {:>9.1}µ",
            seed_size + n, insert_us, get_us, find_us, count_us);
    }

    // Now same with SQLite for comparison
    println!("\n=== SQLite Comparison (no index) ===\n");
    println!("{:>8} {:>10} {:>10} {:>10} {:>10}", "rows", "insert", "get", "find_eq", "count_eq");

    for seed_size in [1000, 2500, 5000] {
        let sdir = tempfile::TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(sdir.path().join("bench.db")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
        conn.execute(
            "CREATE TABLE notes (_id INTEGER PRIMARY KEY, title TEXT, body TEXT, owner TEXT)",
            [],
        ).unwrap();

        let mut ids: Vec<i64> = Vec::new();
        conn.execute("BEGIN", []).unwrap();
        for i in 0..seed_size {
            conn.execute(
                "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![i as i64, format!("note_{i}"), "body content here", format!("user_{}", i % 10)],
            ).unwrap();
            ids.push(i as i64);
        }
        conn.execute("COMMIT", []).unwrap();

        let n = 2000usize;

        // Insert
        let t = Instant::now();
        conn.execute("BEGIN", []).unwrap();
        for i in 0..n {
            let id = (seed_size + i) as i64;
            conn.execute(
                "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, format!("new_{i}"), "new body", format!("user_{}", i % 10)],
            ).unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
        let insert_us = t.elapsed().as_micros() as f64 / n as f64;

        // Get
        let t = Instant::now();
        for i in 0..n {
            let id = ids[i % ids.len()];
            let mut stmt = conn.prepare_cached("SELECT * FROM notes WHERE _id = ?1").unwrap();
            let _: Vec<(i64, String, String, String)> = stmt.query_map([id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            }).unwrap().map(|r| r.unwrap()).collect();
        }
        let get_us = t.elapsed().as_micros() as f64 / n as f64;

        // Find (eq filter, limit 20)
        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let mut stmt = conn.prepare_cached(
                "SELECT * FROM notes WHERE owner = ?1 LIMIT 20"
            ).unwrap();
            let _: Vec<(i64, String, String, String)> = stmt.query_map([&owner], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            }).unwrap().map(|r| r.unwrap()).collect();
        }
        let find_us = t.elapsed().as_micros() as f64 / n as f64;

        // Count (eq filter)
        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let mut stmt = conn.prepare_cached(
                "SELECT COUNT(*) FROM notes WHERE owner = ?1"
            ).unwrap();
            let _: i64 = stmt.query_row([&owner], |row| row.get(0)).unwrap();
        }
        let count_us = t.elapsed().as_micros() as f64 / n as f64;

        println!("{:>8} {:>9.1}µ {:>9.1}µ {:>9.1}µ {:>9.1}µ",
            seed_size + n, insert_us, get_us, find_us, count_us);
    }

    // With indexes
    println!("\n=== boogy-db WITH index ===\n");
    println!("{:>8} {:>10} {:>10} {:>10} {:>10}", "rows", "insert", "get", "find_eq", "count_eq");

    for seed_size in [1000, 2500, 5000] {
        let dir = tempfile::TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("notes", &[
            ColumnDef::new("title", Type::Text),
            ColumnDef::new("body", Type::Text),
            ColumnDef::new("owner", Type::Text),
        ]).unwrap();
        db.create_index("notes", "idx_owner", "owner").unwrap();

        let mut ids: Vec<u64> = Vec::new();
        for i in 0..seed_size {
            ids.push(db.insert("notes", &[
                ("title", Value::Text(format!("note_{i}"))),
                ("body", Value::Text("body content here".into())),
                ("owner", Value::Text(format!("user_{}", i % 10))),
            ]).unwrap());
        }

        let n = 2000usize;

        let t = Instant::now();
        for i in 0..n {
            db.insert("notes", &[
                ("title", Value::Text(format!("new_{i}"))),
                ("body", Value::Text("new body".into())),
                ("owner", Value::Text(format!("user_{}", i % 10))),
            ]).unwrap();
        }
        let insert_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let _ = db.get("notes", ids[i % ids.len()]).unwrap();
        }
        let get_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let _ = db.find("notes", FindOptions {
                filters: vec![Filter::eq("owner", owner)],
                or_groups: vec![],
                sort: vec![],
                limit: Some(20),
                offset: None,
                include_total: false,
            }).unwrap();
        }
        let find_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let _ = db.count("notes", &[Filter::eq("owner", owner)]).unwrap();
        }
        let count_us = t.elapsed().as_micros() as f64 / n as f64;

        println!("{:>8} {:>9.1}µ {:>9.1}µ {:>9.1}µ {:>9.1}µ",
            seed_size + n, insert_us, get_us, find_us, count_us);
    }

    println!("\n=== SQLite WITH index ===\n");
    println!("{:>8} {:>10} {:>10} {:>10} {:>10}", "rows", "insert", "get", "find_eq", "count_eq");

    for seed_size in [1000, 2500, 5000] {
        let sdir = tempfile::TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(sdir.path().join("bench.db")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
        conn.execute(
            "CREATE TABLE notes (_id INTEGER PRIMARY KEY, title TEXT, body TEXT, owner TEXT)",
            [],
        ).unwrap();
        conn.execute("CREATE INDEX idx_owner ON notes(owner)", []).unwrap();

        conn.execute("BEGIN", []).unwrap();
        for i in 0..seed_size {
            conn.execute(
                "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![i as i64, format!("note_{i}"), "body content here", format!("user_{}", i % 10)],
            ).unwrap();
        }
        conn.execute("COMMIT", []).unwrap();

        let n = 2000usize;

        let t = Instant::now();
        conn.execute("BEGIN", []).unwrap();
        for i in 0..n {
            let id = (seed_size + i) as i64;
            conn.execute(
                "INSERT INTO notes (_id, title, body, owner) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, format!("new_{i}"), "new body", format!("user_{}", i % 10)],
            ).unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
        let insert_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let id = (i % seed_size) as i64;
            let mut stmt = conn.prepare_cached("SELECT * FROM notes WHERE _id = ?1").unwrap();
            let _: Vec<(i64, String, String, String)> = stmt.query_map([id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            }).unwrap().map(|r| r.unwrap()).collect();
        }
        let get_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let mut stmt = conn.prepare_cached(
                "SELECT * FROM notes WHERE owner = ?1 LIMIT 20"
            ).unwrap();
            let _: Vec<(i64, String, String, String)> = stmt.query_map([&owner], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            }).unwrap().map(|r| r.unwrap()).collect();
        }
        let find_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let owner = format!("user_{}", i % 10);
            let mut stmt = conn.prepare_cached(
                "SELECT COUNT(*) FROM notes WHERE owner = ?1"
            ).unwrap();
            let _: i64 = stmt.query_row([&owner], |row| row.get(0)).unwrap();
        }
        let count_us = t.elapsed().as_micros() as f64 / n as f64;

        println!("{:>8} {:>9.1}µ {:>9.1}µ {:>9.1}µ {:>9.1}µ",
            seed_size + n, insert_us, get_us, find_us, count_us);
    }
}
