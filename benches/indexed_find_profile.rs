//! Micro-profile the indexed find path to identify where time goes.

use std::time::Instant;
use boogy_db::*;

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("notes", &[
        ColumnDef::new("title", Type::Text),
        ColumnDef::new("body", Type::Text),
        ColumnDef::new("owner", Type::Text),
    ]).unwrap();
    db.create_index("notes", "idx_owner", "owner").unwrap();

    for i in 0..3000 {
        db.insert("notes", &[
            ("title", Value::Text(format!("note_{i}"))),
            ("body", Value::Text("body content here".into())),
            ("owner", Value::Text(format!("user_{}", i % 10))),
        ]).unwrap();
    }

    let n = 5000;

    // Indexed find (what we're profiling)
    let t = Instant::now();
    for i in 0..n {
        let owner = format!("user_{}", i % 10);
        let _ = db.find("notes", FindOptions {
            filters: vec![Filter::eq("owner", owner)],
            limit: Some(20),
            include_total: false,
            ..Default::default()
        }).unwrap();
    }
    let find_idx_us = t.elapsed().as_micros() as f64 / n as f64;

    // Indexed count
    let t = Instant::now();
    for i in 0..n {
        let owner = format!("user_{}", i % 10);
        let _ = db.count("notes", &[Filter::eq("owner", owner)]).unwrap();
    }
    let count_idx_us = t.elapsed().as_micros() as f64 / n as f64;

    // Just the insert overhead with index
    let t = Instant::now();
    for i in 0..n {
        db.insert("notes", &[
            ("title", Value::Text(format!("bench_{i}"))),
            ("body", Value::Text("x".into())),
            ("owner", Value::Text(format!("user_{}", i % 10))),
        ]).unwrap();
    }
    let insert_idx_us = t.elapsed().as_micros() as f64 / n as f64;

    // Bare get for comparison
    let t = Instant::now();
    for i in 0..n {
        let _ = db.get("notes", (i % 3000) as u64 + 1).unwrap();
    }
    let get_us = t.elapsed().as_micros() as f64 / n as f64;

    println!("=== Indexed Operation Costs (3K rows) ===");
    println!("  find (limit 20): {find_idx_us:.1}µs");
    println!("  count:           {count_idx_us:.1}µs");
    println!("  insert:          {insert_idx_us:.1}µs");
    println!("  get:             {get_us:.1}µs");

    // Now measure just the format!() overhead
    let t = Instant::now();
    for i in 0..n {
        let _ = format!("user_{}", i % 10);
    }
    let fmt_us = t.elapsed().as_micros() as f64 / n as f64;
    println!("\n  format!() alone: {fmt_us:.2}µs");

    // Measure Value::Text allocation overhead
    let t = Instant::now();
    for i in 0..n {
        let owner = format!("user_{}", i % 10);
        let _ = Value::Text(owner);
    }
    let alloc_us = t.elapsed().as_micros() as f64 / n as f64;
    println!("  Value::Text():   {alloc_us:.2}µs");
}
