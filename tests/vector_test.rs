#![cfg(feature = "vector")]

use boogy_db::*;
use tempfile::TempDir;

fn setup_db() -> (TempDir, BoogyDb) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    db.create_table(
        "items",
        &[
            ColumnDef::new("name", Type::Text),
            ColumnDef::new("category", Type::Text),
        ],
    )
    .unwrap();
    (dir, db)
}

#[test]
fn test_full_lifecycle() {
    let (_dir, db) = setup_db();

    let opts = VectorCollectionOptions::new(3, DistanceMetric::Cosine);
    db.create_vector_collection("items", "embeddings", &opts)
        .unwrap();

    // Insert 3 rows and vectors.
    let id1 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("alpha".into())),
                ("category", Value::Text("A".into())),
            ],
        )
        .unwrap();
    let id2 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("beta".into())),
                ("category", Value::Text("B".into())),
            ],
        )
        .unwrap();
    let id3 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("gamma".into())),
                ("category", Value::Text("A".into())),
            ],
        )
        .unwrap();

    db.vector_insert("items", "embeddings", id1, &[1.0, 0.0, 0.0])
        .unwrap();
    db.vector_insert("items", "embeddings", id2, &[0.0, 1.0, 0.0])
        .unwrap();
    db.vector_insert("items", "embeddings", id3, &[0.9, 0.1, 0.0])
        .unwrap();

    // Search for nearest to [1.0, 0.0, 0.0] -- should be id1.
    let results = db
        .vector_search(
            "items",
            "embeddings",
            &[1.0, 0.0, 0.0],
            &VectorSearchOptions::new(3),
        )
        .unwrap();

    assert!(!results.is_empty());
    assert_eq!(results[0].rowid, id1);

    // Delete id1, search again -- id1 should be gone.
    db.vector_delete("items", "embeddings", id1).unwrap();

    let results = db
        .vector_search(
            "items",
            "embeddings",
            &[1.0, 0.0, 0.0],
            &VectorSearchOptions::new(3),
        )
        .unwrap();

    for r in &results {
        assert_ne!(r.rowid, id1, "deleted vector should not appear in results");
    }

    // Drop the collection.
    db.drop_vector_collection("items", "embeddings").unwrap();
}

#[test]
fn test_batch_insert() {
    let (_dir, db) = setup_db();

    let opts = VectorCollectionOptions::new(4, DistanceMetric::Euclidean);
    db.create_vector_collection("items", "emb", &opts).unwrap();

    // Insert 50 rows.
    let mut rowids = Vec::with_capacity(50);
    for i in 0..50u64 {
        let id = db
            .insert(
                "items",
                &[
                    ("name", Value::Text(format!("item_{i}"))),
                    ("category", Value::Text("X".into())),
                ],
            )
            .unwrap();
        rowids.push(id);
    }

    // Build batch: vectors with linearly increasing first component.
    let entries: Vec<(u64, Vec<f32>)> = rowids
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let v = i as f32 / 49.0;
            (id, vec![v, 0.0, 0.0, 0.0])
        })
        .collect();

    db.vector_insert_batch("items", "emb", &entries).unwrap();

    // Search for nearest to the middle value [0.5, 0.0, 0.0, 0.0].
    let results = db
        .vector_search(
            "items",
            "emb",
            &[0.5, 0.0, 0.0, 0.0],
            &VectorSearchOptions::new(5),
        )
        .unwrap();

    assert!(!results.is_empty());
    // The closest should be around index 24-25 (value ~0.49 or ~0.51).
    let nearest_idx = rowids
        .iter()
        .position(|&id| id == results[0].rowid)
        .unwrap();
    assert!(
        (20..=30).contains(&nearest_idx),
        "nearest should be near the middle, got index {nearest_idx}"
    );
}

#[test]
fn test_filtered_search() {
    let (_dir, db) = setup_db();

    let opts = VectorCollectionOptions::new(3, DistanceMetric::Cosine);
    db.create_vector_collection("items", "emb", &opts).unwrap();

    // Insert 20 items with alternating category A/B.
    let mut ids = Vec::with_capacity(20);
    for i in 0..20u32 {
        let cat = if i % 2 == 0 { "A" } else { "B" };
        let id = db
            .insert(
                "items",
                &[
                    ("name", Value::Text(format!("item_{i}"))),
                    ("category", Value::Text(cat.into())),
                ],
            )
            .unwrap();
        ids.push(id);

        // Spread vectors apart so there are meaningful distances.
        let angle = i as f32 * std::f32::consts::PI / 10.0;
        db.vector_insert("items", "emb", id, &[angle.cos(), angle.sin(), 0.1])
            .unwrap();
    }

    // Search with filter: category = "A".
    let mut search_opts = VectorSearchOptions::new(10);
    search_opts.filter = Some(Filter::eq("category", "A"));

    let results = db
        .vector_search("items", "emb", &[1.0, 0.0, 0.0], &search_opts)
        .unwrap();

    assert!(!results.is_empty(), "filtered search should return results");
    for r in &results {
        let row = db.get("items", r.rowid).unwrap().unwrap();
        let cat = row.get("category").unwrap();
        assert_eq!(
            cat,
            Value::Text("A".into()),
            "all filtered results must have category A, got {:?} for rowid {}",
            cat,
            r.rowid
        );
    }
}

#[test]
fn test_vector_update() {
    let (_dir, db) = setup_db();

    let opts = VectorCollectionOptions::new(3, DistanceMetric::Euclidean);
    db.create_vector_collection("items", "emb", &opts).unwrap();

    let id1 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("far".into())),
                ("category", Value::Text("X".into())),
            ],
        )
        .unwrap();
    let id2 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("near".into())),
                ("category", Value::Text("X".into())),
            ],
        )
        .unwrap();

    // Insert far apart: id1 at origin, id2 at [10, 10, 10].
    db.vector_insert("items", "emb", id1, &[0.0, 0.0, 0.0])
        .unwrap();
    db.vector_insert("items", "emb", id2, &[10.0, 10.0, 10.0])
        .unwrap();

    // Search near id2 -- id2 should be closest.
    let results = db
        .vector_search(
            "items",
            "emb",
            &[10.0, 10.0, 10.0],
            &VectorSearchOptions::new(2),
        )
        .unwrap();
    assert_eq!(results[0].rowid, id2);

    // Update id1 to be right next to id2.
    db.vector_update("items", "emb", id1, &[9.9, 9.9, 9.9])
        .unwrap();

    // Now search near [10, 10, 10] -- both should be very close,
    // but id2 is at exactly [10, 10, 10] so it should still be first.
    let results = db
        .vector_search(
            "items",
            "emb",
            &[10.0, 10.0, 10.0],
            &VectorSearchOptions::new(2),
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].rowid, id2);
    // id1 should be the second result (very close now).
    assert_eq!(results[1].rowid, id1);
    // Verify id1's distance is small after the update.
    assert!(
        results[1].distance < 1.0,
        "updated vector should be close, distance = {}",
        results[1].distance
    );
}

#[test]
fn test_multiple_collections() {
    let (_dir, db) = setup_db();

    let opts_cos = VectorCollectionOptions::new(3, DistanceMetric::Cosine);
    let opts_euc = VectorCollectionOptions::new(5, DistanceMetric::Euclidean);
    db.create_vector_collection("items", "coll_a", &opts_cos)
        .unwrap();
    db.create_vector_collection("items", "coll_b", &opts_euc)
        .unwrap();

    let id1 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("one".into())),
                ("category", Value::Text("X".into())),
            ],
        )
        .unwrap();
    let id2 = db
        .insert(
            "items",
            &[
                ("name", Value::Text("two".into())),
                ("category", Value::Text("Y".into())),
            ],
        )
        .unwrap();

    // Insert into coll_a (3 dims).
    db.vector_insert("items", "coll_a", id1, &[1.0, 0.0, 0.0])
        .unwrap();
    db.vector_insert("items", "coll_a", id2, &[0.0, 1.0, 0.0])
        .unwrap();

    // Insert into coll_b (5 dims).
    db.vector_insert("items", "coll_b", id1, &[0.0, 0.0, 0.0, 0.0, 1.0])
        .unwrap();
    db.vector_insert("items", "coll_b", id2, &[0.0, 0.0, 0.0, 0.0, 0.0])
        .unwrap();

    // Search coll_a for [1, 0, 0] -- id1 nearest.
    let res_a = db
        .vector_search(
            "items",
            "coll_a",
            &[1.0, 0.0, 0.0],
            &VectorSearchOptions::new(2),
        )
        .unwrap();
    assert_eq!(res_a[0].rowid, id1);

    // Search coll_b for [0, 0, 0, 0, 1] -- id1 nearest.
    let res_b = db
        .vector_search(
            "items",
            "coll_b",
            &[0.0, 0.0, 0.0, 0.0, 1.0],
            &VectorSearchOptions::new(2),
        )
        .unwrap();
    assert_eq!(res_b[0].rowid, id1);
}

#[test]
fn test_collection_not_found() {
    let (_dir, db) = setup_db();

    let err = db
        .vector_search(
            "items",
            "nonexistent",
            &[1.0, 2.0, 3.0],
            &VectorSearchOptions::new(5),
        )
        .unwrap_err();

    assert!(
        matches!(err, BoogyError::VectorCollectionNotFound(_)),
        "expected VectorCollectionNotFound, got: {err}"
    );
}

#[test]
fn test_duplicate_collection_name() {
    let (_dir, db) = setup_db();

    let opts = VectorCollectionOptions::new(3, DistanceMetric::Cosine);
    db.create_vector_collection("items", "dup", &opts).unwrap();

    let err = db
        .create_vector_collection("items", "dup", &opts)
        .unwrap_err();

    assert!(
        matches!(err, BoogyError::VectorCollectionExists(_)),
        "expected VectorCollectionExists, got: {err}"
    );
}

#[test]
fn test_dimension_mismatch_at_insert() {
    let (_dir, db) = setup_db();

    let opts = VectorCollectionOptions::new(3, DistanceMetric::Cosine);
    db.create_vector_collection("items", "emb", &opts).unwrap();

    let id = db
        .insert(
            "items",
            &[
                ("name", Value::Text("x".into())),
                ("category", Value::Text("X".into())),
            ],
        )
        .unwrap();

    // Insert a 2-dim vector into a 3-dim collection.
    let err = db
        .vector_insert("items", "emb", id, &[1.0, 2.0])
        .unwrap_err();

    assert!(
        matches!(
            err,
            BoogyError::VectorDimensionMismatch {
                expected: 3,
                got: 2
            }
        ),
        "expected VectorDimensionMismatch, got: {err}"
    );
}

#[test]
fn test_existing_ops_unaffected() {
    let (_dir, db) = setup_db();

    // Normal CRUD should work identically with the vector feature enabled.
    let id = db
        .insert(
            "items",
            &[
                ("name", Value::Text("test".into())),
                ("category", Value::Text("general".into())),
            ],
        )
        .unwrap();

    // Get.
    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(row.get("name").unwrap(), Value::Text("test".into()));

    // Update.
    db.update(
        "items",
        id,
        &[("name", Value::Text("updated".into()))],
    )
    .unwrap();
    let row = db.get("items", id).unwrap().unwrap();
    assert_eq!(row.get("name").unwrap(), Value::Text("updated".into()));

    // Find.
    let find_result = db
        .find(
            "items",
            FindOptions {
                filters: vec![Filter::eq("category", "general")],
                sort: vec![],
                limit: Some(10),
                offset: None,
                include_total: false,
            },
        )
        .unwrap();
    assert_eq!(find_result.rows.len(), 1);
    assert_eq!(find_result.rows[0].id, id);

    // Delete.
    assert!(db.delete("items", id).unwrap());
    assert!(db.get("items", id).unwrap().is_none());
}

#[test]
fn test_recall_against_brute_force() {
    let (_dir, db) = setup_db();

    let dims: usize = 32;
    let n: usize = 1000;
    let k: usize = 10;

    let opts = VectorCollectionOptions::new(dims as u32, DistanceMetric::Euclidean);
    db.create_vector_collection("items", "recall", &opts)
        .unwrap();

    // Generate deterministic pseudo-random vectors using xorshift.
    let mut rng: u64 = 42;
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(n);
    let mut rowids: Vec<u64> = Vec::with_capacity(n);

    for i in 0..n {
        let mut vec = Vec::with_capacity(dims);
        for _ in 0..dims {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            vec.push((rng >> 33) as f32 / (u32::MAX >> 1) as f32);
        }

        let id = db
            .insert(
                "items",
                &[
                    ("name", Value::Text(format!("vec_{i}"))),
                    ("category", Value::Text("recall".into())),
                ],
            )
            .unwrap();

        rowids.push(id);
        vectors.push(vec);
    }

    // Batch insert all vectors.
    let entries: Vec<(u64, Vec<f32>)> = rowids
        .iter()
        .zip(vectors.iter())
        .map(|(&id, v)| (id, v.clone()))
        .collect();
    db.vector_insert_batch("items", "recall", &entries).unwrap();

    // Generate a query vector.
    let mut query = Vec::with_capacity(dims);
    for _ in 0..dims {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        query.push((rng >> 33) as f32 / (u32::MAX >> 1) as f32);
    }

    // Brute-force: compute distances to all vectors and find top-k.
    let mut brute_force: Vec<(u64, f32)> = rowids
        .iter()
        .zip(vectors.iter())
        .map(|(&id, v)| {
            let dist: f32 = v
                .iter()
                .zip(query.iter())
                .map(|(a, b)| (a - b) * (a - b))
                .sum::<f32>()
                .sqrt();
            (id, dist)
        })
        .collect();
    brute_force.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let true_top_k: Vec<u64> = brute_force.iter().take(k).map(|(id, _)| *id).collect();

    // HNSW search.
    let mut search_opts = VectorSearchOptions::new(k as u32);
    search_opts.ef_search = 64; // generous ef_search for good recall
    let results = db
        .vector_search("items", "recall", &query, &search_opts)
        .unwrap();

    assert!(
        results.len() >= k,
        "expected at least {k} results, got {}",
        results.len()
    );

    // Compute recall: fraction of true top-k found in HNSW results.
    let hnsw_ids: Vec<u64> = results.iter().take(k).map(|r| r.rowid).collect();
    let hits = true_top_k.iter().filter(|id| hnsw_ids.contains(id)).count();
    let recall = hits as f64 / k as f64;

    assert!(
        recall >= 0.80,
        "recall {recall:.2} is below 80% floor (hits: {hits}/{k})"
    );
}
