//! Benchmark: schema-evolution O(1) proof + default-at-read read-throughput.
//!
//! Part 1 — O(1) ops: time add_column / rename_column / drop_column at 1k, 10k, 100k rows.
//!   Expectation: ~constant across sizes (metadata-only, no row scan).
//!
//! Part 2 — default-at-read read regression: compare point-get and find throughput on
//!   (A) table with column physically present in every row vs
//!   (B) same table where the column was add_column'd *after* the rows existed.

use boogy_db::*;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Seed N rows into table `t` with columns `k` (int) + `v` (int).
fn seed_table(db: &BoogyDb, n: usize) -> Vec<u64> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n as i64 {
        ids.push(
            db.insert("t", &[
                ("k", Value::Integer(i)),
                ("v", Value::Integer(i * 2)),
            ]).unwrap(),
        );
    }
    ids
}

// ---------------------------------------------------------------------------
// Part 1: O(1) schema ops
// ---------------------------------------------------------------------------

struct SchemaOpTiming {
    #[allow(dead_code)]
    size: usize,
    add_us: f64,
    rename_us: f64,
    drop_us: f64,
}

fn bench_schema_ops_at_size(n: usize) -> SchemaOpTiming {
    // add_column -----------------------------------------------------------
    let add_us = {
        let dir = tempfile::TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[
            ColumnDef::new("k", Type::Integer),
            ColumnDef::new("v", Type::Integer),
        ]).unwrap();
        seed_table(&db, n);
        // Warm the cache — a simple get
        let _ = db.get("t", 1);

        let col = ColumnDef::new("extra", Type::Text).default(Value::Text("default".into()));
        let t = Instant::now();
        db.add_column("t", col).unwrap();
        t.elapsed().as_micros() as f64
    };

    // rename_column --------------------------------------------------------
    let rename_us = {
        let dir = tempfile::TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[
            ColumnDef::new("k", Type::Integer),
            ColumnDef::new("v", Type::Integer),
        ]).unwrap();
        seed_table(&db, n);
        let _ = db.get("t", 1);

        let t = Instant::now();
        db.rename_column("t", "v", "value").unwrap();
        t.elapsed().as_micros() as f64
    };

    // drop_column ----------------------------------------------------------
    let drop_us = {
        let dir = tempfile::TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("t", &[
            ColumnDef::new("k", Type::Integer),
            ColumnDef::new("v", Type::Integer),
        ]).unwrap();
        seed_table(&db, n);
        let _ = db.get("t", 1);

        let t = Instant::now();
        db.drop_column("t", "v").unwrap();
        t.elapsed().as_micros() as f64
    };

    SchemaOpTiming { size: n, add_us, rename_us, drop_us }
}

// ---------------------------------------------------------------------------
// Part 2: default-at-read read throughput
// ---------------------------------------------------------------------------

struct ReadThroughput {
    label: &'static str,
    get_ops_per_sec: f64,
    find_ops_per_sec: f64,
}

/// Table A: column `x` is physically present in every row (created with the table).
fn bench_physical_read(n: usize) -> ReadThroughput {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("t", &[
        ColumnDef::new("k", Type::Integer),
        ColumnDef::new("x", Type::Integer),
    ]).unwrap();

    let mut ids = Vec::with_capacity(n);
    for i in 0..n as i64 {
        ids.push(
            db.insert("t", &[
                ("k", Value::Integer(i)),
                ("x", Value::Integer(42)),
            ]).unwrap(),
        );
    }

    let iters = 10_000usize;

    // point-get throughput
    let t = Instant::now();
    for i in 0..iters {
        let id = ids[i % ids.len()];
        let row = db.get("t", id).unwrap().unwrap();
        let _ = row.get("x");
    }
    let get_secs = t.elapsed().as_secs_f64();

    // find throughput (equality filter on x)
    let t = Instant::now();
    for _ in 0..iters {
        let _ = db.find("t", FindOptions {
            filters: vec![Filter::eq("x", 42i64)],
            limit: Some(10),
            ..Default::default()
        }).unwrap();
    }
    let find_secs = t.elapsed().as_secs_f64();

    ReadThroughput {
        label: "A (physical x)",
        get_ops_per_sec: iters as f64 / get_secs,
        find_ops_per_sec: iters as f64 / find_secs,
    }
}

/// Table B: column `x` was add_column'd AFTER the rows existed (default-at-read branch).
fn bench_default_at_read(n: usize) -> ReadThroughput {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("t", &[
        ColumnDef::new("k", Type::Integer),
    ]).unwrap();

    let mut ids = Vec::with_capacity(n);
    for i in 0..n as i64 {
        ids.push(
            db.insert("t", &[
                ("k", Value::Integer(i)),
            ]).unwrap(),
        );
    }

    // Add column AFTER rows exist — all existing rows will use default on read
    db.add_column("t", ColumnDef::new("x", Type::Integer).default(Value::Integer(42))).unwrap();

    let iters = 10_000usize;

    // point-get throughput
    let t = Instant::now();
    for i in 0..iters {
        let id = ids[i % ids.len()];
        let row = db.get("t", id).unwrap().unwrap();
        let _ = row.get("x");
    }
    let get_secs = t.elapsed().as_secs_f64();

    // find throughput (equality filter on x — hits default-at-read path)
    let t = Instant::now();
    for _ in 0..iters {
        let _ = db.find("t", FindOptions {
            filters: vec![Filter::eq("x", 42i64)],
            limit: Some(10),
            ..Default::default()
        }).unwrap();
    }
    let find_secs = t.elapsed().as_secs_f64();

    ReadThroughput {
        label: "B (default-at-read x)",
        get_ops_per_sec: iters as f64 / get_secs,
        find_ops_per_sec: iters as f64 / find_secs,
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    // -----------------------------------------------------------------------
    // Part 1: O(1) schema ops
    // -----------------------------------------------------------------------
    println!("=== Schema Op Timing (O(1) proof) ===\n");
    println!(
        "{:>10}  {:>14}  {:>14}  {:>14}",
        "rows", "add_column", "rename_column", "drop_column"
    );

    // Use 1k / 10k / 100k.  1M is impractical for CI (seed time dominates); stated in results.
    let sizes = [1_000usize, 10_000, 100_000];
    let mut timings = Vec::new();
    for &n in &sizes {
        eprint!("  seeding {}k rows... ", n / 1000);
        let t = bench_schema_ops_at_size(n);
        eprintln!("done");
        println!(
            "{:>10}  {:>12.0} us  {:>12.0} us  {:>12.0} us",
            n, t.add_us, t.rename_us, t.drop_us
        );
        timings.push(t);
    }

    // Compute max/min ratio to assess constancy
    let max_add = timings.iter().map(|t| t.add_us as u64).max().unwrap() as f64;
    let min_add = timings.iter().map(|t| t.add_us as u64).min().unwrap().max(1) as f64;
    let max_ren = timings.iter().map(|t| t.rename_us as u64).max().unwrap() as f64;
    let min_ren = timings.iter().map(|t| t.rename_us as u64).min().unwrap().max(1) as f64;
    let max_drp = timings.iter().map(|t| t.drop_us as u64).max().unwrap() as f64;
    let min_drp = timings.iter().map(|t| t.drop_us as u64).min().unwrap().max(1) as f64;

    println!();
    println!(
        "max/min variation (lower = more O(1)):  add={:.1}x  rename={:.1}x  drop={:.1}x",
        max_add / min_add, max_ren / min_ren, max_drp / min_drp,
    );
    let verdict = if max_add / min_add < 5.0 && max_ren / min_ren < 5.0 && max_drp / min_drp < 5.0 {
        "PASS — all ops are O(1) (variation < 5x across 100x table size)"
    } else {
        "CONCERN — at least one op shows significant growth with table size"
    };
    println!("Verdict: {verdict}");
    println!();

    // -----------------------------------------------------------------------
    // Part 2: default-at-read read throughput
    // -----------------------------------------------------------------------
    println!("=== Default-at-Read Read Throughput (N=10,000 rows) ===\n");
    println!(
        "{:<25}  {:>16}  {:>16}",
        "table variant", "point-get (ops/s)", "find limit=10 (ops/s)"
    );

    let seed_n = 10_000;
    eprint!("  benching A (physical)...      ");
    let a = bench_physical_read(seed_n);
    eprintln!("done");
    eprint!("  benching B (default-at-read)...");
    let b = bench_default_at_read(seed_n);
    eprintln!("done");

    println!("{:<25}  {:>14.0}/s  {:>14.0}/s", a.label, a.get_ops_per_sec, a.find_ops_per_sec);
    println!("{:<25}  {:>14.0}/s  {:>14.0}/s", b.label, b.get_ops_per_sec, b.find_ops_per_sec);
    println!();

    let get_ratio  = b.get_ops_per_sec  / a.get_ops_per_sec;
    let find_ratio = b.find_ops_per_sec / a.find_ops_per_sec;
    println!(
        "B vs A:  get={:.2}x ({})  find={:.2}x ({})",
        get_ratio,
        pct_change(get_ratio),
        find_ratio,
        pct_change(find_ratio),
    );

    let regression = get_ratio < 0.80 || find_ratio < 0.80;
    let read_verdict = if regression {
        "CONCERN — default-at-read branch causes a measurable read regression (>20% drop)"
    } else {
        "PASS — no meaningful read regression from default-at-read branch"
    };
    println!("Verdict: {read_verdict}");
}

fn pct_change(ratio: f64) -> String {
    let pct = (ratio - 1.0) * 100.0;
    if pct >= 0.0 {
        format!("+{:.1}%", pct)
    } else {
        format!("{:.1}%", pct)
    }
}
