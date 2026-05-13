use boogy_db::*;
use std::time::Instant;

fn run_point_ops(durability: Durability) -> Vec<(usize, f64, f64)> {
    let mut results = Vec::new();
    for seed_size in [100, 1000, 5000, 10000] {
        let dir = tempfile::TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(durability);
        db.create_table("t", &[ColumnDef::new("v", Type::Integer)]).unwrap();

        let mut ids: Vec<u64> = Vec::new();
        for i in 0..seed_size as i64 {
            ids.push(db.insert("t", &[("v", Value::Integer(i))]).unwrap());
        }

        let n: usize = 20000;

        let t = Instant::now();
        for i in 0..n {
            db.insert("t", &[("v", Value::Integer(i as i64 + 100000))]).unwrap();
        }
        let insert_us = t.elapsed().as_micros() as f64 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let _ = db.get("t", ids[i % ids.len()]).unwrap();
        }
        let get_us = t.elapsed().as_micros() as f64 / n as f64;

        results.push((seed_size, insert_us, get_us));
    }
    results
}

fn main() {
    for (label, durability) in [
        ("Durability::None", Durability::None),
        ("Durability::Normal", Durability::Normal),
    ] {
        println!("=== Point Operations ({label}) ===\n");
        for (seed_size, insert_us, get_us) in run_point_ops(durability) {
            println!(
                "seed={:>5}  insert: {:>6.1} us/op ({:>7.0}/s)  get: {:>5.1} us/op ({:>7.0}/s)",
                seed_size,
                insert_us, 1_000_000.0 / insert_us,
                get_us, 1_000_000.0 / get_us,
            );
        }
        println!();
    }
}
