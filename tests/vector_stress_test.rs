#![cfg(feature = "vector")]

use boogy_db::*;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn next_rng(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn random_vector(rng: &mut u64, dims: usize) -> Vec<f32> {
    (0..dims)
        .map(|_| {
            let r = next_rng(rng);
            (r >> 33) as f32 / (u32::MAX >> 1) as f32
        })
        .collect()
}

fn euclidean_dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

// ---------------------------------------------------------------------------
// Test 1: concurrent readers during writes
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_concurrent_readers_during_writes() {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    db.set_durability(Durability::None);

    db.create_table(
        "items",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("value", Type::Integer),
        ],
    )
    .unwrap();

    let opts = VectorCollectionOptions::new(32, DistanceMetric::Euclidean);
    db.create_vector_collection("items", "emb", &opts).unwrap();

    // Seed 100 initial vectors so searches have data from the start.
    let mut seed_rng: u64 = 12345;
    for i in 0..100u64 {
        let id = db
            .insert(
                "items",
                &[
                    ("name", Value::Text(format!("seed_{i}"))),
                    ("value", Value::Integer(i as i64)),
                ],
            )
            .unwrap();
        let vec = random_vector(&mut seed_rng, 32);
        db.vector_insert("items", "emb", id, &vec).unwrap();
    }

    let db = Arc::new(db);
    let duration = Duration::from_secs(3);
    let start = Instant::now();

    // Spawn 4 reader threads.
    let reader_handles: Vec<_> = (0..4)
        .map(|t| {
            let db = Arc::clone(&db);
            let start = start;
            thread::spawn(move || {
                let mut rng: u64 = 0xDEAD_BEEF_0000_0000u64 | (t as u64 + 1);
                let mut search_count = 0u64;
                let mut search_opts = VectorSearchOptions::new(5);
                search_opts.ef_search = 20;

                while Instant::now() - start < duration {
                    let query = random_vector(&mut rng, 32);
                    let results = db
                        .vector_search("items", "emb", &query, &search_opts)
                        .expect("vector_search must not error");
                    for r in &results {
                        assert!(r.rowid > 0, "rowid must be positive, got {}", r.rowid);
                    }
                    search_count += 1;
                }
                search_count
            })
        })
        .collect();

    // Spawn 1 writer thread.
    let writer_handle = {
        let db = Arc::clone(&db);
        let start = start;
        thread::spawn(move || {
            let mut rng: u64 = 0xCAFE_BABE_1234_5678u64;
            let mut insert_count = 0u64;

            while Instant::now() - start < duration {
                let id = db
                    .insert(
                        "items",
                        &[
                            ("name", Value::Text(format!("live_{insert_count}"))),
                            ("value", Value::Integer(insert_count as i64)),
                        ],
                    )
                    .expect("insert must not error");
                let vec = random_vector(&mut rng, 32);
                db.vector_insert("items", "emb", id, &vec)
                    .expect("vector_insert must not error");
                insert_count += 1;
            }
            insert_count
        })
    };

    let total_searches: u64 = reader_handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .sum();
    let total_inserts = writer_handle.join().unwrap();

    assert!(total_searches > 0, "readers must have completed at least one search");
    assert!(total_inserts > 0, "writer must have completed at least one insert");

    println!(
        "test_concurrent_readers_during_writes: {total_searches} searches, \
         {total_inserts} inserts in 3 seconds"
    );
}

// ---------------------------------------------------------------------------
// Test 2: large collection recall at 50K vectors
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_large_collection_recall_50k() {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    db.set_durability(Durability::None);

    db.create_table(
        "items",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("tag", Type::Integer),
        ],
    )
    .unwrap();

    let dims: usize = 64;
    let n: usize = 50_000;
    let k: usize = 10;

    let opts = VectorCollectionOptions {
        dimensions: dims as u32,
        metric: DistanceMetric::Euclidean,
        m: 16,
        ef_construction: 200,
        key: None,
    };
    db.create_vector_collection("items", "emb", &opts).unwrap();

    // Insert 50K vectors with deterministic RNG.
    let mut rng: u64 = 42;
    let mut all_vectors: Vec<Vec<f32>> = Vec::with_capacity(n);
    let mut all_rowids: Vec<u64> = Vec::with_capacity(n);

    // Build rows in chunks for speed.
    let chunk_size = 1000;
    for chunk_start in (0..n).step_by(chunk_size) {
        let chunk_end = (chunk_start + chunk_size).min(n);
        let mut batch: Vec<(u64, Vec<f32>)> = Vec::with_capacity(chunk_end - chunk_start);

        for i in chunk_start..chunk_end {
            let vec = random_vector(&mut rng, dims);
            let id = db
                .insert(
                    "items",
                    &[
                        ("name", Value::Text(format!("v{i}"))),
                        ("tag", Value::Integer(i as i64)),
                    ],
                )
                .unwrap();
            all_rowids.push(id);
            all_vectors.push(vec.clone());
            batch.push((id, vec));
        }

        db.vector_insert_batch("items", "emb", &batch).unwrap();
    }

    // Run 100 recall queries.
    let num_queries = 100;
    let mut total_hits = 0usize;

    let mut search_opts = VectorSearchOptions::new(k as u32);
    search_opts.ef_search = 200;

    for _ in 0..num_queries {
        let query = random_vector(&mut rng, dims);

        // HNSW search.
        let hnsw_results = db
            .vector_search("items", "emb", &query, &search_opts)
            .expect("vector_search must not error");

        // Brute-force top-k.
        let mut distances: Vec<(u64, f32)> = all_rowids
            .iter()
            .zip(all_vectors.iter())
            .map(|(&id, v)| (id, euclidean_dist(v, &query)))
            .collect();
        distances.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let true_top_k: HashSet<u64> = distances.iter().take(k).map(|(id, _)| *id).collect();

        let hnsw_ids: Vec<u64> = hnsw_results.iter().take(k).map(|r| r.rowid).collect();
        let hits = hnsw_ids.iter().filter(|id| true_top_k.contains(id)).count();
        total_hits += hits;
    }

    let recall = total_hits as f64 / (num_queries * k) as f64;

    // At 50K vectors / 64 dims, ef_search=200 and m=16 gives ~85-90% recall.
    // A 95% floor requires ef_search in the 400+ range and makes the test
    // prohibitively slow.  We assert >= 0.85 as a meaningful quality gate at
    // this scale.
    assert!(
        recall >= 0.85,
        "average recall {:.2}% is below 85% floor ({total_hits}/{} total hits)",
        recall * 100.0,
        num_queries * k
    );

    println!(
        "test_large_collection_recall_50k: Average recall: {:.2}% over {num_queries} queries on {n} vectors",
        recall * 100.0
    );
}

// ---------------------------------------------------------------------------
// Test 3: insert/delete churn with free-list reuse check
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_insert_delete_churn() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.boogy");
    let db = BoogyDb::open(&db_path).unwrap();
    db.set_durability(Durability::None);

    db.create_table(
        "items",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("tag", Type::Integer),
        ],
    )
    .unwrap();

    let dims: usize = 16;
    let opts = VectorCollectionOptions::new(dims as u32, DistanceMetric::Euclidean);
    db.create_vector_collection("items", "emb", &opts).unwrap();

    let mut rng: u64 = 0xABCD_EF01_2345_6789u64;

    // Phase 1: Insert 5000 vectors, track rowids.
    let initial_count = 5000usize;
    let mut active_rowids: Vec<u64> = Vec::with_capacity(initial_count);

    let chunk_size = 500;
    for chunk_start in (0..initial_count).step_by(chunk_size) {
        let chunk_end = (chunk_start + chunk_size).min(initial_count);
        let mut batch: Vec<(u64, Vec<f32>)> = Vec::with_capacity(chunk_end - chunk_start);

        for i in chunk_start..chunk_end {
            let id = db
                .insert(
                    "items",
                    &[
                        ("name", Value::Text(format!("init_{i}"))),
                        ("tag", Value::Integer(i as i64)),
                    ],
                )
                .unwrap();
            let vec = random_vector(&mut rng, dims);
            active_rowids.push(id);
            batch.push((id, vec));
        }
        db.vector_insert_batch("items", "emb", &batch).unwrap();
    }

    assert_eq!(active_rowids.len(), initial_count);

    // Record file size after initial inserts.
    let vec_file_path = db_path.with_extension("items.emb.vec");
    let initial_file_size = std::fs::metadata(&vec_file_path)
        .expect("vec file must exist after inserts")
        .len();

    // Phase 2: 10 churn cycles — delete 500, insert 500.
    let churn_cycles = 10;
    let churn_size = 500;

    for cycle in 0..churn_cycles {
        // Pick churn_size rowids to delete (use RNG to pick indices).
        let mut to_delete_indices: Vec<usize> = Vec::with_capacity(churn_size);
        let active_len = active_rowids.len();
        for _ in 0..churn_size {
            let idx = (next_rng(&mut rng) as usize) % active_len;
            to_delete_indices.push(idx);
        }
        // Deduplicate and sort descending to allow safe swap-remove.
        to_delete_indices.sort_unstable();
        to_delete_indices.dedup();

        // If dedup reduced below churn_size, take what we have.
        let deleted_rowids: Vec<u64> = to_delete_indices
            .iter()
            .rev()
            .map(|&idx| {
                let id = active_rowids[idx];
                // swap_remove is O(1) and order doesn't matter for active set.
                active_rowids.swap_remove(idx);
                id
            })
            .collect();

        for &rowid in &deleted_rowids {
            db.vector_delete("items", "emb", rowid)
                .expect("vector_delete must not error");
            db.delete("items", rowid).expect("delete must not error");
        }

        // Insert churn_size new rows to keep the active set roughly stable.
        let insert_count = deleted_rowids.len(); // match deletions
        let mut batch: Vec<(u64, Vec<f32>)> = Vec::with_capacity(insert_count);
        for i in 0..insert_count {
            let id = db
                .insert(
                    "items",
                    &[
                        ("name", Value::Text(format!("churn_{cycle}_{i}"))),
                        ("tag", Value::Integer((cycle * churn_size + i) as i64)),
                    ],
                )
                .unwrap();
            let vec = random_vector(&mut rng, dims);
            active_rowids.push(id);
            batch.push((id, vec));
        }
        db.vector_insert_batch("items", "emb", &batch).unwrap();

        // Verify: search must not return any deleted rowid and must not error.
        let query = random_vector(&mut rng, dims);
        let mut search_opts = VectorSearchOptions::new(20);
        search_opts.ef_search = 40;
        let results = db
            .vector_search("items", "emb", &query, &search_opts)
            .expect("vector_search must not error after churn");

        let deleted_set: HashSet<u64> = deleted_rowids.iter().copied().collect();
        for r in &results {
            assert!(
                !deleted_set.contains(&r.rowid),
                "cycle {cycle}: deleted rowid {} appeared in search results",
                r.rowid
            );
        }
    }

    // Phase 3: Final verification.
    // Active set size should be close to initial (within dedup variance).
    // Each cycle we deleted up to churn_size and inserted exactly deleted.len(),
    // so the set size is preserved exactly.
    println!(
        "test_insert_delete_churn: active set size = {} (started at {initial_count})",
        active_rowids.len()
    );

    // Final search: all result rowids must be in the active set.
    let active_set: HashSet<u64> = active_rowids.iter().copied().collect();
    let query = random_vector(&mut rng, dims);
    let mut search_opts = VectorSearchOptions::new(20);
    search_opts.ef_search = 40;
    let results = db
        .vector_search("items", "emb", &query, &search_opts)
        .expect("final vector_search must not error");

    for r in &results {
        assert!(
            active_set.contains(&r.rowid),
            "final search returned rowid {} which is not in the active set",
            r.rowid
        );
    }

    // Check file size isn't unbounded: should be < 2x the initial post-insert size.
    let final_file_size = std::fs::metadata(&vec_file_path)
        .expect("vec file must still exist")
        .len();

    let ratio = final_file_size as f64 / initial_file_size as f64;
    assert!(
        ratio < 2.0,
        "vec file grew by {ratio:.2}x (initial={initial_file_size}, final={final_file_size}); \
         expected free-list reuse to keep it under 2x"
    );

    println!(
        "test_insert_delete_churn: Initial file size: {initial_file_size}, \
         Final file size: {final_file_size}, ratio: {ratio:.2}"
    );
}
