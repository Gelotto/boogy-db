use boogy_db::*;
use std::time::Instant;

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

    let mut ids: Vec<String> = Vec::new();
    for i in 0..1000i64 {
        ids.push(db.insert("t", &[("v", Value::Integer(i))]).unwrap());
    }

    // Insert only
    let n: usize = 50000;
    let t = Instant::now();
    for i in 0..n {
        db.insert("t", &[("v", Value::Integer(i as i64 + 10000))]).unwrap();
    }
    let insert_us = t.elapsed().as_micros() as f64 / n as f64;

    // Get only
    let t = Instant::now();
    for i in 0..n {
        let _ = db.get("t", &ids[i % ids.len()]).unwrap();
    }
    let get_us = t.elapsed().as_micros() as f64 / n as f64;

    println!("insert: {insert_us:.1} us/op ({:.0}/s)", 1_000_000.0 / insert_us);
    println!("get:    {get_us:.1} us/op ({:.0}/s)", 1_000_000.0 / get_us);
}
