//! Benchmark: simulated joins — boogy-db (application-side) vs SQLite (native JOIN).
//!
//! Scenario: a social media app fetching a user profile + their latest 5 posts.
//! boogy-db does two calls (get user + find posts). SQLite does a single JOIN query.
//!
//! Tests both single-thread and concurrent (4 threads) access patterns.
//! boogy-db is tested at both Durability::None and Durability::Normal.

use std::sync::Arc;
use std::time::{Duration, Instant};
use boogy_db::*;

const NUM_USERS: i64 = 500;
const POSTS_PER_USER: i64 = 50;
const DURATION_SECS: u64 = 5;

fn main() {
    let duration = Duration::from_secs(DURATION_SECS);

    println!("=== Join Simulation: Fetch User + Latest 5 Posts ===\n");
    println!("Schema: {} users, {} posts each ({} total posts)", NUM_USERS, POSTS_PER_USER, NUM_USERS * POSTS_PER_USER);
    println!("Query: given a random user ID, fetch user row + 5 most recent posts\n");
    println!("{:<35} {:>14} {:>14} {:>10}",
        "", "boogy(none)", "boogy(normal)", "sqlite");

    // --- boogy-db: without index ---
    let boogy_none = bench_boogy(false, false, 1, duration, Durability::None);
    let boogy_normal = bench_boogy(false, false, 1, duration, Durability::Normal);
    let sqlite = bench_sqlite(false, 1, duration);
    print_row("No index, 1 thread", boogy_none, boogy_normal, sqlite);

    // --- boogy-db: with index on author_id ---
    let boogy_none = bench_boogy(true, false, 1, duration, Durability::None);
    let boogy_normal = bench_boogy(true, false, 1, duration, Durability::Normal);
    let sqlite = bench_sqlite(true, 1, duration);
    print_row("With index, 1 thread", boogy_none, boogy_normal, sqlite);

    // --- boogy-db: with index, no sort (any 5 posts) ---
    let boogy_none = bench_boogy(true, true, 1, duration, Durability::None);
    let boogy_normal = bench_boogy(true, true, 1, duration, Durability::Normal);
    let sqlite = bench_sqlite_nosort(true, 1, duration);
    print_row("With index, no sort, 1 thread", boogy_none, boogy_normal, sqlite);

    println!();

    // --- Concurrent (4 threads) ---
    let boogy_none = bench_boogy(true, false, 4, duration, Durability::None);
    let boogy_normal = bench_boogy(true, false, 4, duration, Durability::Normal);
    let sqlite = bench_sqlite(true, 4, duration);
    print_row("With index, 4 threads", boogy_none, boogy_normal, sqlite);

    let boogy_none = bench_boogy(true, false, 8, duration, Durability::None);
    let boogy_normal = bench_boogy(true, false, 8, duration, Durability::Normal);
    let sqlite = bench_sqlite(true, 8, duration);
    print_row("With index, 8 threads", boogy_none, boogy_normal, sqlite);
}

fn print_row(label: &str, boogy_none: u64, boogy_normal: u64, sqlite: u64) {
    let ratio = boogy_none as f64 / sqlite as f64;
    let winner = if ratio > 1.0 { "boogy" } else { "sqlite" };
    println!("{:<35} {:>10} q/s {:>10} q/s {:>10} q/s  {:.2}x ({winner})",
        label, boogy_none, boogy_normal, sqlite, ratio);
}

fn bench_boogy(with_index: bool, skip_sort: bool, threads: usize, duration: Duration, durability: Durability) -> u64 {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Arc::new(BoogyDb::open(dir.path().join("bench.boogy")).unwrap());
    db.set_durability(durability);

    db.create_table("users", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("bio", Type::Text),
    ]).unwrap();
    db.create_table("posts", &[
        ColumnDef::new("author_id", Type::Integer),
        ColumnDef::new("title", Type::Text),
        ColumnDef::new("body", Type::Text),
        ColumnDef::new("created_at", Type::Integer),
    ]).unwrap();

    if with_index {
        db.create_index("posts", "idx_author", "author_id").unwrap();
    }

    // Seed data
    for u in 0..NUM_USERS {
        db.insert("users", &[
            ("name", Value::Text(format!("User {u}"))),
            ("bio", Value::Text("A short bio about this user.".into())),
        ]).unwrap();
    }
    for u in 0..NUM_USERS {
        let user_id = u + 1; // rowids start at 1
        for p in 0..POSTS_PER_USER {
            let ts = u * 1000 + p; // older users have lower timestamps
            db.insert("posts", &[
                ("author_id", Value::Integer(user_id)),
                ("title", Value::Text(format!("Post {p} by user {u}"))),
                ("body", Value::Text("Lorem ipsum dolor sit amet, consectetur adipiscing elit.".into())),
                ("created_at", Value::Integer(ts)),
            ]).unwrap();
        }
    }

    let handles: Vec<_> = (0..threads).map(|tid| {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let mut ops = 0u64;
            let mut rng: u64 = 12345 + tid as u64 * 77777;
            let start = Instant::now();

            while start.elapsed() < duration {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let user_id = ((rng >> 32) % NUM_USERS as u64) + 1;

                // 1. Fetch user
                let _user = db.get("users", user_id).unwrap();

                // 2. Fetch latest 5 posts by this user
                let _posts = if skip_sort {
                    db.find("posts", FindOptions {
                        filters: vec![Filter::eq("author_id", user_id as i64)],
                        limit: Some(5),
                        include_total: false,
                        ..Default::default()
                    }).unwrap()
                } else {
                    db.find("posts", FindOptions {
                        filters: vec![Filter::eq("author_id", user_id as i64)],
                        sort: vec![Sort::desc("created_at")],
                        limit: Some(5),
                        include_total: false,
                        ..Default::default()
                    }).unwrap()
                };

                ops += 1;
            }
            ops
        })
    }).collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total / DURATION_SECS
}

fn bench_sqlite(with_index: bool, threads: usize, duration: Duration) -> u64 {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bench.db");

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
        conn.execute(
            "CREATE TABLE users (_id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, bio TEXT)",
            [],
        ).unwrap();
        conn.execute(
            "CREATE TABLE posts (_id INTEGER PRIMARY KEY AUTOINCREMENT, author_id INTEGER, title TEXT, body TEXT, created_at INTEGER)",
            [],
        ).unwrap();
        if with_index {
            conn.execute("CREATE INDEX idx_author ON posts(author_id)", []).unwrap();
        }

        conn.execute("BEGIN", []).unwrap();
        for u in 0..NUM_USERS {
            conn.execute(
                "INSERT INTO users (name, bio) VALUES (?1, ?2)",
                rusqlite::params![format!("User {u}"), "A short bio about this user."],
            ).unwrap();
        }
        for u in 0..NUM_USERS {
            let user_id = u + 1;
            for p in 0..POSTS_PER_USER {
                let ts = u * 1000 + p;
                conn.execute(
                    "INSERT INTO posts (author_id, title, body, created_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![user_id, format!("Post {p} by user {u}"),
                        "Lorem ipsum dolor sit amet, consectetur adipiscing elit.", ts],
                ).unwrap();
            }
        }
        conn.execute("COMMIT", []).unwrap();
    }

    let handles: Vec<_> = (0..threads).map(|tid| {
        let path = path.clone();
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();

            let mut ops = 0u64;
            let mut rng: u64 = 12345 + tid as u64 * 77777;
            let start = Instant::now();

            while start.elapsed() < duration {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let user_id = ((rng >> 32) % NUM_USERS as u64) + 1;

                // Single JOIN query: fetch user + latest 5 posts
                let mut stmt = conn.prepare_cached(
                    "SELECT u.name, u.bio, p.title, p.body, p.created_at \
                     FROM users u \
                     JOIN posts p ON p.author_id = u._id \
                     WHERE u._id = ?1 \
                     ORDER BY p.created_at DESC \
                     LIMIT 5"
                ).unwrap();
                let _rows: Vec<(String, String, String, String, i64)> = stmt
                    .query_map([user_id as i64], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                    })
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect();

                ops += 1;
            }
            ops
        })
    }).collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total / DURATION_SECS
}

fn bench_sqlite_nosort(with_index: bool, threads: usize, duration: Duration) -> u64 {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bench.db");

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
        conn.execute(
            "CREATE TABLE users (_id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, bio TEXT)",
            [],
        ).unwrap();
        conn.execute(
            "CREATE TABLE posts (_id INTEGER PRIMARY KEY AUTOINCREMENT, author_id INTEGER, title TEXT, body TEXT, created_at INTEGER)",
            [],
        ).unwrap();
        if with_index {
            conn.execute("CREATE INDEX idx_author ON posts(author_id)", []).unwrap();
        }

        conn.execute("BEGIN", []).unwrap();
        for u in 0..NUM_USERS {
            conn.execute(
                "INSERT INTO users (name, bio) VALUES (?1, ?2)",
                rusqlite::params![format!("User {u}"), "A short bio about this user."],
            ).unwrap();
        }
        for u in 0..NUM_USERS {
            let user_id = u + 1;
            for p in 0..POSTS_PER_USER {
                let ts = u * 1000 + p;
                conn.execute(
                    "INSERT INTO posts (author_id, title, body, created_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![user_id, format!("Post {p} by user {u}"),
                        "Lorem ipsum dolor sit amet, consectetur adipiscing elit.", ts],
                ).unwrap();
            }
        }
        conn.execute("COMMIT", []).unwrap();
    }

    let handles: Vec<_> = (0..threads).map(|tid| {
        let path = path.clone();
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();

            let mut ops = 0u64;
            let mut rng: u64 = 12345 + tid as u64 * 77777;
            let start = Instant::now();

            while start.elapsed() < duration {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let user_id = ((rng >> 32) % NUM_USERS as u64) + 1;

                // Two separate queries, no sort — closer to boogy-db's approach
                let mut stmt = conn.prepare_cached(
                    "SELECT name, bio FROM users WHERE _id = ?1"
                ).unwrap();
                let _: Vec<(String, String)> = stmt
                    .query_map([user_id as i64], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap().map(|r| r.unwrap()).collect();

                let mut stmt = conn.prepare_cached(
                    "SELECT title, body, created_at FROM posts WHERE author_id = ?1 LIMIT 5"
                ).unwrap();
                let _: Vec<(String, String, i64)> = stmt
                    .query_map([user_id as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .unwrap().map(|r| r.unwrap()).collect();

                ops += 1;
            }
            ops
        })
    }).collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total / DURATION_SECS
}
