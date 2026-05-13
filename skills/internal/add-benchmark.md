# Adding a New Benchmark

Benchmarks live in `benches/` with custom harness (`harness = false`), a `main()` function, and `std::time::Instant` for timing. No criterion dependency.

### 1. Register in Cargo.toml

Add a new `[[bench]]` section:

```toml
[[bench]]
name = "my_benchmark"
harness = false
```

### 2. Create the benchmark file

Create `benches/my_benchmark.rs`. Follow the existing pattern from `benches/point_ops.rs`:

```rust
use boogy_db::*;
use std::time::Instant;

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None); // or Normal for realistic mode

    // Setup: create table, seed data
    db.create_table("t", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ]).unwrap();

    for i in 0..1000 {
        db.insert("t", &[
            ("name", Value::Text(format!("item_{}", i))),
            ("value", Value::Integer(i)),
        ]).unwrap();
    }

    // Benchmark
    let n = 10_000;
    let start = Instant::now();
    for i in 0..n {
        // ... operation under test ...
    }
    let elapsed = start.elapsed();

    let ops_per_sec = n as f64 / elapsed.as_secs_f64();
    let us_per_op = elapsed.as_micros() as f64 / n as f64;
    println!("ops/sec: {ops_per_sec:.0}, us/op: {us_per_op:.1}");
}
```

### 3. Compare against SQLite (optional)

If comparing against SQLite, add it as a dev-dependency (already in Cargo.toml as `rusqlite`):

```rust
use rusqlite::Connection;

fn bench_sqlite() {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = Connection::open(dir.path().join("bench.sqlite")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    // ... equivalent SQLite operations ...
}
```

### 4. Test both durability modes

Run the benchmark at `Durability::None` and `Durability::Normal` to show the WAL overhead:

```rust
for (label, durability) in [
    ("None", Durability::None),
    ("Normal", Durability::Normal),
] {
    // ... run benchmark with this durability ...
    println!("[{label}] ops/sec: {ops_per_sec:.0}");
}
```

### 5. Run

```bash
cargo bench --bench my_benchmark   # single benchmark
cargo bench                        # all benchmarks
```

## Checklist

- [ ] `[[bench]]` entry added to `Cargo.toml` with `harness = false`
- [ ] Benchmark file created in `benches/`
- [ ] Uses `Instant` for timing (no criterion)
- [ ] Tests both `Durability::None` and `Durability::Normal`
- [ ] Uses `tempfile::TempDir` for isolation
- [ ] Prints results to stdout
- [ ] `cargo bench --bench my_benchmark` runs successfully
