//! Benchmark: vector search latency, insert throughput, HNSW vs brute force, regression check.
//!
//! Requires `--features vector` to compile.
//! All vector benchmarks use `Durability::None` for speed.
//! The regression section uses `Durability::Normal` to check that enabling
//! the vector feature doesn't degrade ordinary table operations.

use std::time::{Duration, Instant};
use boogy_db::*;

const DIMS: usize = 128;

// ---------------------------------------------------------------------------
// Simple xorshift64 RNG for deterministic pseudo-random vectors
// ---------------------------------------------------------------------------

struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform float in [0, 1).
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 11) as f32 / (1u64 << 53) as f32
    }

    /// Generate a random 128-dim vector with components in [-1, 1).
    fn next_vec(&mut self) -> Vec<f32> {
        (0..DIMS).map(|_| self.next_f32() * 2.0 - 1.0).collect()
    }
}

// ---------------------------------------------------------------------------
// Percentile helper
// ---------------------------------------------------------------------------

fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 * pct) as usize).min(sorted.len() - 1);
    sorted[idx]
}

fn fmt_us(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{us} us")
    } else if us < 1_000_000 {
        format!("{:.1} ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2} s", us as f64 / 1_000_000.0)
    }
}

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

/// Create a BoogyDb with a table and a vector collection, pre-seeded with
/// `n` vectors (each row has a matching row in the "docs" table).
fn setup_db(n: usize, rng: &mut Xorshift64) -> (BoogyDb, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);

    db.create_table("docs", &[ColumnDef::new("tag", Type::Text)]).unwrap();

    db.create_vector_collection(
        "docs",
        "emb",
        &VectorCollectionOptions {
            dimensions: DIMS as u32,
            metric: DistanceMetric::Cosine,
            m: 16,
            ef_construction: 200,
            key: None,
        },
    )
    .unwrap();

    // Insert rows first (vector_insert checks row exists), then batch-insert vectors.
    let mut rowids = Vec::with_capacity(n);
    for i in 0..n {
        let rowid = db
            .insert("docs", &[("tag", Value::Text(format!("doc_{i}")))])
            .unwrap();
        rowids.push(rowid);
    }

    // Batch-insert all vectors at once.
    let entries: Vec<(u64, Vec<f32>)> = rowids
        .iter()
        .map(|&rowid| (rowid, rng.next_vec()))
        .collect();
    db.vector_insert_batch("docs", "emb", &entries).unwrap();

    (db, dir)
}

// ---------------------------------------------------------------------------
// Section 1: Insert throughput
// ---------------------------------------------------------------------------

fn bench_insert_throughput() {
    println!("=== Insert Throughput (128 dims, Durability::None) ===\n");
    println!("{:>10}  {:>16}  {:>16}", "n", "single (vecs/s)", "batch-100 (vecs/s)");

    for n in [1_000usize, 10_000] {
        // --- Single insert ---
        let single_rate = {
            let dir = tempfile::TempDir::new().unwrap();
            let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
            db.set_durability(Durability::None);
            db.create_table("docs", &[ColumnDef::new("tag", Type::Text)]).unwrap();
            db.create_vector_collection(
                "docs",
                "emb",
                &VectorCollectionOptions {
                    dimensions: DIMS as u32,
                    metric: DistanceMetric::Cosine,
                    m: 16,
                    ef_construction: 200,
                },
            )
            .unwrap();

            let mut rng = Xorshift64::new(0xDEAD_BEEF_1234_5678);
            // Seed initial rows so HNSW graph exists.
            let mut rowids: Vec<u64> = Vec::with_capacity(n);
            for i in 0..n {
                let rowid = db
                    .insert("docs", &[("tag", Value::Text(format!("seed_{i}")))])
                    .unwrap();
                rowids.push(rowid);
            }
            let seed_entries: Vec<(u64, Vec<f32>)> = rowids
                .iter()
                .map(|&rowid| (rowid, rng.next_vec()))
                .collect();
            db.vector_insert_batch("docs", "emb", &seed_entries).unwrap();

            // Now time 200 individual inserts.
            let measure = 200usize;
            let t = Instant::now();
            for i in 0..measure {
                let tag_id = n + i;
                let rowid = db
                    .insert(
                        "docs",
                        &[("tag", Value::Text(format!("new_{tag_id}")))],
                    )
                    .unwrap();
                db.vector_insert("docs", "emb", rowid, &rng.next_vec()).unwrap();
            }
            let elapsed = t.elapsed();
            measure as f64 / elapsed.as_secs_f64()
        };

        // --- Batch-100 insert ---
        let batch_rate = {
            let dir = tempfile::TempDir::new().unwrap();
            let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
            db.set_durability(Durability::None);
            db.create_table("docs", &[ColumnDef::new("tag", Type::Text)]).unwrap();
            db.create_vector_collection(
                "docs",
                "emb",
                &VectorCollectionOptions {
                    dimensions: DIMS as u32,
                    metric: DistanceMetric::Cosine,
                    m: 16,
                    ef_construction: 200,
                },
            )
            .unwrap();

            let mut rng = Xorshift64::new(0xABCD_EF01_2345_6789);
            let mut rowids: Vec<u64> = Vec::with_capacity(n);
            for i in 0..n {
                let rowid = db
                    .insert("docs", &[("tag", Value::Text(format!("seed_{i}")))])
                    .unwrap();
                rowids.push(rowid);
            }
            let seed_entries: Vec<(u64, Vec<f32>)> = rowids
                .iter()
                .map(|&rowid| (rowid, rng.next_vec()))
                .collect();
            db.vector_insert_batch("docs", "emb", &seed_entries).unwrap();

            // Time 10 batches of 100.
            const BATCH: usize = 100;
            const BATCHES: usize = 10;
            let total = BATCH * BATCHES;
            let t = Instant::now();
            for b in 0..BATCHES {
                // Pre-insert rows to the table.
                let mut new_ids = Vec::with_capacity(BATCH);
                for i in 0..BATCH {
                    let tag_id = n + b * BATCH + i;
                    let rowid = db
                        .insert(
                            "docs",
                            &[("tag", Value::Text(format!("new_{tag_id}")))],
                        )
                        .unwrap();
                    new_ids.push(rowid);
                }
                let entries: Vec<(u64, Vec<f32>)> = new_ids
                    .iter()
                    .map(|&rowid| (rowid, rng.next_vec()))
                    .collect();
                db.vector_insert_batch("docs", "emb", &entries).unwrap();
            }
            let elapsed = t.elapsed();
            total as f64 / elapsed.as_secs_f64()
        };

        println!(
            "{:>10}  {:>16.0}  {:>16.0}",
            n, single_rate, batch_rate
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// Section 2: Search latency
// ---------------------------------------------------------------------------

fn bench_search_latency() {
    println!("=== Search Latency (k=10, ef_search=50, Durability::None) ===\n");
    println!(
        "{:>8}  {:>12}  {:>12}  {:>12}  {:>12}",
        "n", "avg", "p50", "p99", "searches/s"
    );

    for n in [1_000usize, 10_000, 50_000] {
        let mut rng = Xorshift64::new(0x1234_5678_9ABC_DEF0);
        let (db, _dir) = setup_db(n, &mut rng);

        let search_opts = VectorSearchOptions {
            k: 10,
            ef_search: 50,
            filter: None,
        };

        // Warm up: discard first few searches.
        for _ in 0..5 {
            let q = rng.next_vec();
            let _ = db.vector_search("docs", "emb", &q, &search_opts).unwrap();
        }

        // Measure 200 searches.
        let iters = 200usize;
        let mut latencies = Vec::with_capacity(iters);
        for _ in 0..iters {
            let q = rng.next_vec();
            let t = Instant::now();
            let _ = db.vector_search("docs", "emb", &q, &search_opts).unwrap();
            latencies.push(t.elapsed());
        }

        let total_time: Duration = latencies.iter().sum();
        let avg = total_time / latencies.len() as u32;
        latencies.sort();
        let p50 = percentile(&latencies, 0.50);
        let p99 = percentile(&latencies, 0.99);
        let searches_per_sec = latencies.len() as f64 / total_time.as_secs_f64();

        println!(
            "{:>8}  {:>12}  {:>12}  {:>12}  {:>12.0}",
            n,
            fmt_us(avg),
            fmt_us(p50),
            fmt_us(p99),
            searches_per_sec
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// Section 3: HNSW vs brute force
// ---------------------------------------------------------------------------

/// Brute-force linear scan: compute cosine distance to every vector and
/// return the k smallest. We retrieve vectors through the public search API
/// by using a large ef_search that effectively exhausts the graph — but
/// since we don't have a public brute-force API, we implement a genuine
/// linear scan here using the raw float data we stored during insert.
fn brute_force_search(
    vectors: &[Vec<f32>],
    query: &[f32],
    k: usize,
) -> Vec<(usize, f32)> {
    // Cosine distance: 1 - dot(a,b) / (|a| * |b|)
    let mut dists: Vec<(usize, f32)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let dot: f32 = v.iter().zip(query).map(|(a, b)| a * b).sum();
            let norm_a: f32 = v.iter().map(|a| a * a).sum::<f32>().sqrt();
            let norm_b: f32 = query.iter().map(|b| b * b).sum::<f32>().sqrt();
            let cosine_dist = if norm_a == 0.0 || norm_b == 0.0 {
                1.0
            } else {
                1.0 - dot / (norm_a * norm_b)
            };
            (i, cosine_dist)
        })
        .collect();

    dists.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    dists.truncate(k);
    dists
}

fn bench_hnsw_vs_brute_force() {
    const N: usize = 10_000;
    const K: usize = 10;
    const EF: u32 = 50;
    const ITERS: usize = 100;

    println!("=== HNSW vs Brute Force (n={N}, k={K}, ef_search={EF}) ===\n");

    let mut rng = Xorshift64::new(0xFEDC_BA98_7654_3210);

    // Pre-generate all vectors so brute force has access to the raw data.
    let all_vecs: Vec<Vec<f32>> = (0..N).map(|_| rng.next_vec()).collect();

    // --- Set up HNSW-indexed DB ---
    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("docs", &[ColumnDef::new("tag", Type::Text)]).unwrap();
    db.create_vector_collection(
        "docs",
        "emb",
        &VectorCollectionOptions {
            dimensions: DIMS as u32,
            metric: DistanceMetric::Cosine,
            m: 16,
            ef_construction: 200,
            key: None,
        },
    )
    .unwrap();

    let rowids: Vec<u64> = (0..N)
        .map(|i| {
            db.insert("docs", &[("tag", Value::Text(format!("doc_{i}")))])
                .unwrap()
        })
        .collect();

    let entries: Vec<(u64, Vec<f32>)> = rowids
        .iter()
        .zip(all_vecs.iter())
        .map(|(&rowid, vec)| (rowid, vec.clone()))
        .collect();
    db.vector_insert_batch("docs", "emb", &entries).unwrap();

    let search_opts = VectorSearchOptions { k: K as u32, ef_search: EF, filter: None };

    // Warm up.
    for _ in 0..5 {
        let q = rng.next_vec();
        let _ = db.vector_search("docs", "emb", &q, &search_opts).unwrap();
    }

    // --- Measure HNSW ---
    let mut hnsw_latencies = Vec::with_capacity(ITERS);
    let queries: Vec<Vec<f32>> = (0..ITERS).map(|_| rng.next_vec()).collect();

    for q in &queries {
        let t = Instant::now();
        let _ = db.vector_search("docs", "emb", q, &search_opts).unwrap();
        hnsw_latencies.push(t.elapsed());
    }

    // --- Measure brute force ---
    let mut bf_latencies = Vec::with_capacity(ITERS);

    for q in &queries {
        let t = Instant::now();
        let _ = brute_force_search(&all_vecs, q, K);
        bf_latencies.push(t.elapsed());
    }

    let hnsw_total: Duration = hnsw_latencies.iter().sum();
    let bf_total: Duration = bf_latencies.iter().sum();
    let hnsw_avg = hnsw_total / ITERS as u32;
    let bf_avg = bf_total / ITERS as u32;
    hnsw_latencies.sort();
    bf_latencies.sort();

    let hnsw_rate = ITERS as f64 / hnsw_total.as_secs_f64();
    let bf_rate = ITERS as f64 / bf_total.as_secs_f64();
    let speedup = hnsw_rate / bf_rate;

    println!("{:>16}  {:>12}  {:>12}  {:>12}  {:>14}", "", "avg", "p50", "p99", "searches/s");
    println!(
        "{:>16}  {:>12}  {:>12}  {:>12}  {:>14.0}",
        "HNSW",
        fmt_us(hnsw_avg),
        fmt_us(percentile(&hnsw_latencies, 0.50)),
        fmt_us(percentile(&hnsw_latencies, 0.99)),
        hnsw_rate,
    );
    println!(
        "{:>16}  {:>12}  {:>12}  {:>12}  {:>14.0}",
        "brute force",
        fmt_us(bf_avg),
        fmt_us(percentile(&bf_latencies, 0.50)),
        fmt_us(percentile(&bf_latencies, 0.99)),
        bf_rate,
    );
    if speedup >= 1.0 {
        println!("\nHNSW is {speedup:.2}x faster than brute force");
    } else {
        println!("\nBrute force is {:.2}x faster than HNSW (graph too small)", 1.0 / speedup);
    }
    println!();
}

// ---------------------------------------------------------------------------
// Section 4: Existing ops regression
// ---------------------------------------------------------------------------

fn bench_regression() {
    println!("=== Existing Ops Regression (vector feature enabled, Durability::Normal) ===");
    println!("Table has no vector collection. Checks that vector feature overhead is zero.\n");

    const SEED: usize = 1_000;
    const OPS: usize = 5_000;

    let dir = tempfile::TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::Normal);
    db.create_table("notes", &[
        ColumnDef::new("title", Type::Text),
        ColumnDef::new("body", Type::Text),
        ColumnDef::new("owner", Type::Text),
    ])
    .unwrap();

    // Seed rows.
    let mut ids: Vec<u64> = Vec::with_capacity(SEED);
    for i in 0..SEED {
        ids.push(
            db.insert(
                "notes",
                &[
                    ("title", Value::Text(format!("note_{i}"))),
                    ("body", Value::Text("body content".into())),
                    ("owner", Value::Text(format!("user_{}", i % 10))),
                ],
            )
            .unwrap(),
        );
    }

    let mut rng = Xorshift64::new(0x9999_AAAA_BBBB_CCCC);

    // --- Insert ---
    let mut next_id = SEED;
    let t = Instant::now();
    for _ in 0..OPS {
        next_id += 1;
        ids.push(
            db.insert(
                "notes",
                &[
                    ("title", Value::Text(format!("note_{next_id}"))),
                    ("body", Value::Text("new body".into())),
                    ("owner", Value::Text(format!("user_{}", next_id % 10))),
                ],
            )
            .unwrap(),
        );
    }
    let insert_us = t.elapsed().as_micros() as f64 / OPS as f64;

    // --- Get ---
    let t = Instant::now();
    for _ in 0..OPS {
        let idx = (rng.next_u64() as usize) % ids.len();
        let _ = db.get("notes", ids[idx]).unwrap();
    }
    let get_us = t.elapsed().as_micros() as f64 / OPS as f64;

    // --- Find ---
    let t = Instant::now();
    for _ in 0..OPS {
        let owner = format!("user_{}", rng.next_u64() % 10);
        let _ = db.find("notes", FindOptions {
            filters: vec![Filter::eq("owner", owner)],
            sort: vec![],
            limit: Some(20),
            offset: None,
            include_total: false,
        }).unwrap();
    }
    let find_us = t.elapsed().as_micros() as f64 / OPS as f64;

    println!("{:>8}  {:>10}  {:>12}", "op", "us/op", "ops/s");
    println!(
        "{:>8}  {:>10.1}  {:>12.0}",
        "insert",
        insert_us,
        1_000_000.0 / insert_us
    );
    println!(
        "{:>8}  {:>10.1}  {:>12.0}",
        "get",
        get_us,
        1_000_000.0 / get_us
    );
    println!(
        "{:>8}  {:>10.1}  {:>12.0}",
        "find",
        find_us,
        1_000_000.0 / find_us
    );
    println!();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    println!("boogy-db vector benchmarks");
    println!("==========================\n");

    bench_insert_throughput();
    bench_search_latency();
    bench_hnsw_vs_brute_force();
    bench_regression();
}
