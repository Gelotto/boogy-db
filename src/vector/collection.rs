use std::collections::HashMap;
use std::path::Path;

use crate::error::{BoogyError, Result};

use super::distance::distance_fn;
use super::hnsw;
use super::mmap::VecFile;
use super::types::{DistanceMetric, VectorCollectionOptions, VectorResult};
use super::wal::{VectorWal, WalEntry};

const NONE_U32: u32 = 0xFFFF_FFFF;

pub struct VectorCollection {
    vecfile: VecFile,
    wal: VectorWal,
    dist_fn: fn(&[f32], &[f32]) -> f32,
    rowid_to_node: HashMap<u64, u32>,
    node_to_rowid: HashMap<u32, u64>,
    rng_state: u64,
}

impl VectorCollection {
    /// Create a new vector collection.
    pub fn create(
        vec_path: impl AsRef<Path>,
        wal_path: impl AsRef<Path>,
        options: &VectorCollectionOptions,
    ) -> Result<Self> {
        if options.dimensions == 0 || options.dimensions > 4096 {
            return Err(BoogyError::VectorError(format!(
                "dimensions must be 1..4096, got {}",
                options.dimensions
            )));
        }
        if options.m == 0 || options.m > 128 {
            return Err(BoogyError::VectorError(format!(
                "m must be 1..128, got {}",
                options.m
            )));
        }

        let vecfile = VecFile::create(
            vec_path,
            options.dimensions,
            options.metric,
            options.m,
            options.ef_construction,
            1024,
        )?;
        let wal = VectorWal::open(wal_path)?;
        let dist = distance_fn(options.metric);

        Ok(VectorCollection {
            vecfile,
            wal,
            dist_fn: dist,
            rowid_to_node: HashMap::new(),
            node_to_rowid: HashMap::new(),
            rng_state: 0x5DEECE66D, // fixed seed
        })
    }

    /// Open an existing vector collection.
    pub fn open(
        vec_path: impl AsRef<Path>,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let mut vecfile = VecFile::open(vec_path)?;
        let mut wal = VectorWal::open(wal_path)?;

        let metric = vecfile.header().metric;
        let dist = distance_fn(metric);

        // Replay committed WAL transactions if non-empty.
        if !wal.is_empty()? {
            let transactions = wal.read_committed()?;
            for entries in &transactions {
                Self::replay_transaction(&mut vecfile, entries)?;
            }
            vecfile.flush()?;
            wal.truncate()?;
        }

        Ok(VectorCollection {
            vecfile,
            wal,
            dist_fn: dist,
            rowid_to_node: HashMap::new(),
            node_to_rowid: HashMap::new(),
            rng_state: 0x5DEECE66D,
        })
    }

    /// Populate rowid<->node mappings from an external source.
    ///
    /// The caller (db.rs) provides these from the boogy-db metadata table.
    pub fn rebuild_mappings(&mut self, mappings: Vec<(u64, u32)>) {
        self.rowid_to_node.clear();
        self.node_to_rowid.clear();
        for (rowid, node_id) in mappings {
            self.rowid_to_node.insert(rowid, node_id);
            self.node_to_rowid.insert(node_id, rowid);
        }
    }

    /// Insert a vector associated with a rowid.
    pub fn insert(&mut self, rowid: u64, vector: &[f32], fsync: bool) -> Result<u32> {
        let dims = self.vecfile.header().dimensions;
        if vector.len() != dims as usize {
            return Err(BoogyError::VectorDimensionMismatch {
                expected: dims,
                got: vector.len() as u32,
            });
        }
        if self.rowid_to_node.contains_key(&rowid) {
            return Err(BoogyError::VectorError(format!(
                "duplicate rowid {rowid}"
            )));
        }

        // Allocate node.
        let node_id = self.vecfile.allocate_node()?;

        // Assign random layer.
        let rng_val = self.next_rng();
        let m = self.vecfile.header().m;
        let layer = hnsw::assign_layer(m, rng_val);

        // Write vector to mmap.
        self.vecfile.write_vector(node_id, vector);

        // Allocate graph record.
        self.vecfile.allocate_graph_record(node_id, layer);

        // Run HNSW insert to get connections.
        let entry_point = self.vecfile.header().entry_point;
        let current_max_layer = self.vecfile.header().max_layer;
        let ef_construction = self.vecfile.header().ef_construction;
        let dist = self.dist_fn;

        let result = hnsw::insert(
            node_id,
            vector,
            layer,
            entry_point,
            current_max_layer,
            m,
            ef_construction,
            &|a, b| dist(a, b),
            &|id| self.vecfile.read_vector(id).to_vec(),
            &|id, l| self.vecfile.read_neighbors(id, l),
            &|id| self.vecfile.is_deleted(id),
        );

        // Compute new header values.
        let new_ep = result.new_entry_point.unwrap_or_else(|| {
            entry_point.unwrap_or(node_id)
        });
        let new_max = result.new_max_layer.unwrap_or(current_max_layer);

        // Build ALL WAL entries before any mmap mutation of connections/header.
        let mut wal_entries = Vec::new();
        wal_entries.push(WalEntry::InsertVector {
            node_id,
            layer,
            vector: vector.to_vec(),
        });
        for &(nid, l, ref neighbors) in &result.connections {
            wal_entries.push(WalEntry::SetNeighbors {
                node_id: nid,
                layer: l,
                neighbors: neighbors.clone(),
            });
        }
        wal_entries.push(WalEntry::UpdateHeader {
            entry_point: new_ep,
            node_count: self.vecfile.header().node_count,
            max_layer: new_max,
        });

        // Write WAL committed BEFORE applying mutations to mmap.
        self.wal.append_committed(&wal_entries, fsync)?;

        // Now apply connections to mmap.
        for &(nid, l, ref neighbors) in &result.connections {
            self.vecfile.write_neighbors(nid, l, neighbors);
        }

        // Update header in mmap.
        self.vecfile.header_mut().entry_point = Some(new_ep);
        self.vecfile.header_mut().max_layer = new_max;

        // Flush mmap, then truncate WAL.
        self.vecfile.flush()?;
        self.wal.truncate()?;

        // Update mappings.
        self.rowid_to_node.insert(rowid, node_id);
        self.node_to_rowid.insert(node_id, rowid);

        Ok(node_id)
    }

    /// Delete a vector by rowid.
    pub fn delete(&mut self, rowid: u64, fsync: bool) -> Result<()> {
        let node_id = self.rowid_to_node.remove(&rowid).ok_or_else(|| {
            BoogyError::VectorError(format!("rowid {rowid} not found"))
        })?;
        self.node_to_rowid.remove(&node_id);

        // Compute new entry point BEFORE freeing the node (we need to read
        // neighbors while the node is still intact).
        let mut new_ep = self.vecfile.header().entry_point;
        if new_ep == Some(node_id) {
            let neighbors = self.vecfile.read_neighbors(node_id, 0);
            let replacement = neighbors
                .iter()
                .find(|&&n| !self.vecfile.is_deleted(n))
                .copied();
            new_ep = replacement;
        }

        let ep_raw = new_ep.unwrap_or(NONE_U32);
        let wal_entries = vec![
            WalEntry::DeleteNode { node_id },
            WalEntry::UpdateHeader {
                entry_point: ep_raw,
                node_count: self.vecfile.header().node_count,
                max_layer: self.vecfile.header().max_layer,
            },
        ];

        // Write WAL BEFORE mutating mmap.
        self.wal.append_committed(&wal_entries, fsync)?;

        // Now apply mutations to mmap.
        self.vecfile.free_node(node_id);
        self.vecfile.header_mut().entry_point = new_ep;

        // Flush mmap, then truncate WAL.
        self.vecfile.flush()?;
        self.wal.truncate()?;

        Ok(())
    }

    /// Update a vector: delete then re-insert.
    pub fn update(&mut self, rowid: u64, vector: &[f32], fsync: bool) -> Result<u32> {
        self.delete(rowid, fsync)?;
        self.insert(rowid, vector, fsync)
    }

    /// Search for the k nearest vectors to a query.
    ///
    /// When `row_loader` and `filter` are both provided, filtering happens inline
    /// during HNSW graph traversal (pre-filtering). Nodes that fail the filter are
    /// still expanded for graph connectivity but excluded from results.
    pub fn search(
        &self,
        query: &[f32],
        k: u32,
        ef_search: u32,
        row_loader: Option<&dyn Fn(u64) -> Option<Vec<(String, crate::value::Value)>>>,
        filter: Option<&crate::filter::Filter>,
    ) -> Result<Vec<VectorResult>> {
        let dims = self.vecfile.header().dimensions;
        if query.len() != dims as usize {
            return Err(BoogyError::VectorDimensionMismatch {
                expected: dims,
                got: query.len() as u32,
            });
        }

        let entry_point = match self.vecfile.header().entry_point {
            Some(ep) => ep,
            None => return Ok(Vec::new()),
        };

        let max_layer = self.vecfile.header().max_layer;
        let dist = self.dist_fn;

        // Build the is_allowed closure. When both row_loader and filter are
        // present, load the row and evaluate the filter. Cache results so each
        // node is checked at most once.
        use std::cell::RefCell;
        let cache: RefCell<std::collections::HashMap<u32, bool>> =
            RefCell::new(std::collections::HashMap::new());

        let has_filter = row_loader.is_some() && filter.is_some();

        let is_allowed = |node_id: u32| -> bool {
            if !has_filter {
                return true;
            }
            if let Some(&cached) = cache.borrow().get(&node_id) {
                return cached;
            }
            let result = match self.node_to_rowid.get(&node_id) {
                Some(&rowid) => {
                    match row_loader.unwrap()(rowid) {
                        Some(columns) => {
                            let f = filter.unwrap();
                            let val = columns
                                .iter()
                                .find(|(name, _)| *name == f.column)
                                .map(|(_, v)| v.clone())
                                .unwrap_or(crate::value::Value::Null);
                            f.matches(&val)
                        }
                        None => false,
                    }
                }
                None => false,
            };
            cache.borrow_mut().insert(node_id, result);
            result
        };

        let result = hnsw::search(
            query,
            k,
            ef_search,
            entry_point,
            max_layer,
            &|a, b| dist(a, b),
            &|id| self.vecfile.read_vector(id).to_vec(),
            &|id, l| self.vecfile.read_neighbors(id, l),
            &|id| self.vecfile.is_deleted(id),
            &is_allowed,
        );

        let results = result
            .neighbors
            .into_iter()
            .filter_map(|(node_id, distance)| {
                self.node_to_rowid.get(&node_id).map(|&rowid| VectorResult {
                    rowid,
                    distance,
                })
            })
            .collect();

        Ok(results)
    }

    /// Number of active (non-deleted) vectors in the collection.
    pub fn len(&self) -> usize {
        self.rowid_to_node.len()
    }

    /// Whether the collection is empty.
    pub fn is_empty(&self) -> bool {
        self.rowid_to_node.is_empty()
    }

    /// Dimensions of vectors in this collection.
    pub fn dimensions(&self) -> u32 {
        self.vecfile.header().dimensions
    }

    /// Distance metric used by this collection.
    pub fn metric(&self) -> DistanceMetric {
        self.vecfile.header().metric
    }

    /// Look up the node_id for a given rowid.
    pub fn node_id_for_rowid(&self, rowid: u64) -> Option<u32> {
        self.rowid_to_node.get(&rowid).copied()
    }

    // --- Private helpers ---

    /// Replay a single committed WAL transaction onto the vecfile.
    fn replay_transaction(vecfile: &mut VecFile, entries: &[WalEntry]) -> Result<()> {
        for entry in entries {
            match entry {
                WalEntry::InsertVector { node_id, layer, vector } => {
                    // Allocate node slot if needed (grow to accommodate).
                    while vecfile.header().node_count <= *node_id {
                        vecfile.allocate_node()?;
                    }
                    vecfile.write_vector(*node_id, vector);
                    vecfile.allocate_graph_record(*node_id, *layer);
                    vecfile.set_deleted(*node_id, false);
                }
                WalEntry::SetNeighbors { node_id, layer, neighbors } => {
                    vecfile.write_neighbors(*node_id, *layer, neighbors);
                }
                WalEntry::DeleteNode { node_id } => {
                    vecfile.free_node(*node_id);
                }
                WalEntry::UpdateHeader { entry_point, node_count, max_layer } => {
                    vecfile.header_mut().entry_point = if *entry_point == NONE_U32 {
                        None
                    } else {
                        Some(*entry_point)
                    };
                    vecfile.header_mut().node_count = *node_count;
                    vecfile.header_mut().max_layer = *max_layer;
                }
                WalEntry::Commit => {
                    // Should not appear in transaction entries, but harmless.
                }
            }
        }
        Ok(())
    }

    /// Xorshift64 RNG. Returns a value in (0, 1).
    fn next_rng(&mut self) -> f64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        // Map to (0, 1): use upper bits, divide by u64::MAX
        (x as f64) / (u64::MAX as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_options(dims: u32) -> VectorCollectionOptions {
        VectorCollectionOptions {
            dimensions: dims,
            metric: DistanceMetric::Euclidean,
            m: 8,
            ef_construction: 50,
        }
    }

    fn make_collection(dir: &TempDir, dims: u32) -> VectorCollection {
        let vec_path = dir.path().join("test.bvec");
        let wal_path = dir.path().join("test.bwal");
        VectorCollection::create(vec_path, wal_path, &test_options(dims)).unwrap()
    }

    #[test]
    fn test_create_insert_search() {
        let dir = TempDir::new().unwrap();
        let mut col = make_collection(&dir, 3);

        // Insert 3 orthogonal vectors.
        col.insert(100, &[1.0, 0.0, 0.0], false).unwrap();
        col.insert(200, &[0.0, 1.0, 0.0], false).unwrap();
        col.insert(300, &[0.0, 0.0, 1.0], false).unwrap();

        // Search near [1, 0.1, 0].
        let results = col.search(&[1.0, 0.1, 0.0], 3, 50, None, None).unwrap();
        assert!(!results.is_empty(), "search should return results");
        assert_eq!(
            results[0].rowid, 100,
            "closest to [1,0.1,0] should be rowid 100, got {}",
            results[0].rowid
        );
    }

    #[test]
    fn test_delete_and_search() {
        let dir = TempDir::new().unwrap();
        let mut col = make_collection(&dir, 3);

        col.insert(100, &[1.0, 0.0, 0.0], false).unwrap();
        col.insert(200, &[0.0, 1.0, 0.0], false).unwrap();
        col.insert(300, &[0.0, 0.0, 1.0], false).unwrap();

        // Delete middle vector.
        col.delete(200, false).unwrap();

        let results = col.search(&[0.0, 1.0, 0.0], 3, 50, None, None).unwrap();
        let rowids: Vec<u64> = results.iter().map(|r| r.rowid).collect();
        assert!(
            !rowids.contains(&200),
            "deleted rowid 200 should not appear: {rowids:?}"
        );
    }

    #[test]
    fn test_update_vector() {
        let dir = TempDir::new().unwrap();
        let mut col = make_collection(&dir, 3);

        // Two vectors far apart.
        col.insert(100, &[1.0, 0.0, 0.0], false).unwrap();
        col.insert(200, &[0.0, 0.0, 1.0], false).unwrap();

        // Update rowid 200 to be near rowid 100.
        col.update(200, &[0.9, 0.0, 0.0], false).unwrap();

        let results = col.search(&[1.0, 0.0, 0.0], 2, 50, None, None).unwrap();
        assert_eq!(results.len(), 2);
        let rowids: Vec<u64> = results.iter().map(|r| r.rowid).collect();
        assert!(rowids.contains(&100));
        assert!(rowids.contains(&200));
        // Both should be close to query.
        for r in &results {
            assert!(r.distance < 0.02, "distance {} too large after update", r.distance);
        }
    }

    #[test]
    fn test_dimension_mismatch() {
        let dir = TempDir::new().unwrap();
        let mut col = make_collection(&dir, 3);

        let err = col.insert(100, &[1.0, 2.0], false).unwrap_err();
        match err {
            BoogyError::VectorDimensionMismatch { expected: 3, got: 2 } => {}
            other => panic!("expected VectorDimensionMismatch, got: {other}"),
        }
    }

    #[test]
    fn test_persistence_and_wal_replay() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("persist.bvec");
        let wal_path = dir.path().join("persist.bwal");

        // Create and insert.
        {
            let mut col = VectorCollection::create(
                &vec_path,
                &wal_path,
                &test_options(3),
            )
            .unwrap();
            col.insert(100, &[1.0, 0.0, 0.0], false).unwrap();
            col.insert(200, &[0.0, 1.0, 0.0], false).unwrap();
            col.insert(300, &[0.0, 0.0, 1.0], false).unwrap();
        }

        // Reopen and rebuild mappings.
        {
            let mut col = VectorCollection::open(&vec_path, &wal_path).unwrap();
            col.rebuild_mappings(vec![(100, 0), (200, 1), (300, 2)]);

            let results = col.search(&[1.0, 0.1, 0.0], 3, 50, None, None).unwrap();
            assert!(!results.is_empty(), "search after reopen should return results");
            assert_eq!(
                results[0].rowid, 100,
                "closest should be rowid 100 after reopen"
            );
        }
    }

    #[test]
    fn test_empty_search() {
        let dir = TempDir::new().unwrap();
        let col = make_collection(&dir, 3);

        let results = col.search(&[1.0, 0.0, 0.0], 5, 50, None, None).unwrap();
        assert!(results.is_empty(), "empty collection search should return empty vec");
    }
}
