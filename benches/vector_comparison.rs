//! Benchmark: boogy-db vector search vs usearch vs hnswlib-rs
//!
//! Compares insert throughput, search latency, and recall across three
//! HNSW implementations at matching parameters (M=16, ef_construction=200).
//!
//! Run: cargo bench --features vector --bench vector_comparison

use std::time::Instant;

use boogy_db::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared config
// ---------------------------------------------------------------------------

const DIMS: usize = 128;
const M: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const EF_SEARCH: usize = 50;
const K: usize = 10;

// ---------------------------------------------------------------------------
// Deterministic RNG
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self { Self(seed) }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn next_f32(&mut self) -> f32 {
        (self.next() >> 33) as f32 / (u32::MAX >> 1) as f32
    }

    fn random_vector(&mut self, dims: usize) -> Vec<f32> {
        (0..dims).map(|_| self.next_f32()).collect()
    }
}

// ---------------------------------------------------------------------------
// Brute-force baseline (for recall measurement)
// ---------------------------------------------------------------------------

fn brute_force_topk(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut dists: Vec<(usize, f32)> = vectors.iter().enumerate()
        .map(|(i, v)| {
            let d: f32 = v.iter().zip(query).map(|(a, b)| (a - b) * (a - b)).sum();
            (i, d)
        })
        .collect();
    dists.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    dists.iter().take(k).map(|(i, _)| *i).collect()
}

// ---------------------------------------------------------------------------
// boogy-db
// ---------------------------------------------------------------------------

fn bench_boogy(n: usize, vectors: &[Vec<f32>], queries: &[Vec<f32>]) -> (f64, f64, f64) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("items", &[ColumnDef::new("i", Type::Integer)]).unwrap();
    db.create_vector_collection("items", "emb", &VectorCollectionOptions {
        dimensions: DIMS as u32,
        metric: DistanceMetric::Euclidean,
        m: M as u32,
        ef_construction: EF_CONSTRUCTION as u32,
        key: None,
    }).unwrap();

    // Insert
    let start = Instant::now();
    for (i, vec) in vectors.iter().enumerate() {
        let rid = db.insert("items", &[("i", Value::Integer(i as i64))]).unwrap();
        db.vector_insert("items", "emb", rid, vec).unwrap();
    }
    let insert_secs = start.elapsed().as_secs_f64();
    let insert_rate = n as f64 / insert_secs;

    // Search
    let mut latencies = Vec::with_capacity(queries.len());
    let mut all_results: Vec<Vec<u64>> = Vec::new();
    for query in queries {
        let t = Instant::now();
        let results = db.vector_search("items", "emb", query, &VectorSearchOptions {
            k: K as u32,
            ef_search: EF_SEARCH as u32,
            filter: None,
        }).unwrap();
        latencies.push(t.elapsed());
        // rowids are 1-based, convert to 0-based index
        all_results.push(results.iter().map(|r| r.rowid - 1).collect());
    }
    latencies.sort();
    let avg_us = latencies.iter().map(|d| d.as_nanos()).sum::<u128>() as f64
        / latencies.len() as f64 / 1000.0;

    // Recall
    let mut total_recall = 0.0;
    for (i, query) in queries.iter().enumerate() {
        let truth = brute_force_topk(vectors, query, K);
        let hits = all_results[i].iter()
            .filter(|&&r| truth.contains(&(r as usize)))
            .count();
        total_recall += hits as f64 / K as f64;
    }
    let avg_recall = total_recall / queries.len() as f64;

    (insert_rate, avg_us, avg_recall)
}

// ---------------------------------------------------------------------------
// usearch
// ---------------------------------------------------------------------------

fn bench_usearch(n: usize, vectors: &[Vec<f32>], queries: &[Vec<f32>]) -> (f64, f64, f64) {
    use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

    let options = IndexOptions {
        dimensions: DIMS,
        metric: MetricKind::L2sq,
        quantization: ScalarKind::F32,
        connectivity: M,
        expansion_add: EF_CONSTRUCTION,
        expansion_search: EF_SEARCH,
        multi: false,
    };
    let index = Index::new(&options).unwrap();
    index.reserve(n).unwrap();

    // Insert
    let start = Instant::now();
    for (i, vec) in vectors.iter().enumerate() {
        index.add(i as u64, vec).unwrap();
    }
    let insert_secs = start.elapsed().as_secs_f64();
    let insert_rate = n as f64 / insert_secs;

    // Search
    index.change_expansion_search(EF_SEARCH);
    let mut latencies = Vec::with_capacity(queries.len());
    let mut all_results: Vec<Vec<u64>> = Vec::new();
    for query in queries {
        let t = Instant::now();
        let results = index.search(query, K).unwrap();
        latencies.push(t.elapsed());
        all_results.push(results.keys.clone());
    }
    latencies.sort();
    let avg_us = latencies.iter().map(|d| d.as_nanos()).sum::<u128>() as f64
        / latencies.len() as f64 / 1000.0;

    // Recall
    let mut total_recall = 0.0;
    for (i, query) in queries.iter().enumerate() {
        let truth = brute_force_topk(vectors, query, K);
        let hits = all_results[i].iter()
            .filter(|&&r| truth.contains(&(r as usize)))
            .count();
        total_recall += hits as f64 / K as f64;
    }
    let avg_recall = total_recall / queries.len() as f64;

    (insert_rate, avg_us, avg_recall)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("=== Vector Search Comparison: boogy-db vs usearch ===");
    println!("dims={DIMS}, M={M}, ef_construction={EF_CONSTRUCTION}, ef_search={EF_SEARCH}, k={K}");
    println!("(hnswlib-rs excluded — requires nightly Rust)\n");

    for &n in &[10_000, 50_000] {
        println!("--- {n} vectors ---\n");

        // Generate shared test data
        let mut rng = Rng::new(42);
        let vectors: Vec<Vec<f32>> = (0..n).map(|_| rng.random_vector(DIMS)).collect();
        let queries: Vec<Vec<f32>> = (0..100).map(|_| rng.random_vector(DIMS)).collect();

        println!("{:<15} {:>14} {:>14} {:>10}", "", "insert (v/s)", "search (µs)", "recall");

        let (ins, search, recall) = bench_boogy(n, &vectors, &queries);
        println!("{:<15} {:>11.0} v/s {:>11.1} µs {:>8.1}%", "boogy-db", ins, search, recall * 100.0);

        let (ins, search, recall) = bench_usearch(n, &vectors, &queries);
        println!("{:<15} {:>11.0} v/s {:>11.1} µs {:>8.1}%", "usearch", ins, search, recall * 100.0);

        println!();
    }
}
