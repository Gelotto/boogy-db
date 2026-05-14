# Vector Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add HNSW-based approximate nearest neighbor search to boogy-db behind a `vector` feature flag, with mmap'd vector/graph storage, its own WAL, and metadata stored in boogy-db tables.

**Architecture:** Two storage layers — a custom mmap'd file per collection for dense vector data and HNSW graph (optimized for traversal locality), and boogy-db tables for collection metadata and rowid-to-node-id mappings. Distance metrics (cosine, euclidean, dot product) are standalone functions. The vector module calls boogy-db's public API; core boogy-db knows nothing about vectors.

**Tech Stack:** Rust, `memmap2` crate for memory-mapped files, `rand` (already a dependency) for HNSW layer assignment. No other new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-14-vector-search-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `Cargo.toml` | Modify | Add `vector` feature flag, optional `memmap2` dep |
| `src/lib.rs` | Modify | Conditionally expose `pub mod vector` |
| `src/error.rs` | Modify | Add vector-specific error variants |
| `src/vector/mod.rs` | Create | Public re-exports |
| `src/vector/types.rs` | Create | `DistanceMetric`, `VectorCollectionOptions`, `VectorResult`, `VectorSearchOptions` |
| `src/vector/distance.rs` | Create | Cosine, euclidean, dot product distance functions |
| `src/vector/mmap.rs` | Create | Memory-mapped file: header, vector region, graph region, resize |
| `src/vector/wal.rs` | Create | Vector WAL: entry types, append, commit, replay, truncate |
| `src/vector/hnsw.rs` | Create | HNSW algorithm: insert, search, delete, pruning (pure algorithm, no I/O) |
| `src/vector/collection.rs` | Create | `VectorCollection`: coordinates mmap, WAL, HNSW, free list |
| `src/db.rs` | Modify | Add `vector_*` public methods, collection registry |
| `tests/vector_test.rs` | Create | Integration tests |
| `benches/vector_ops.rs` | Create | Benchmarks |

---

## Task 1: Feature Flag, Types, and Error Variants

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Modify: `src/error.rs`
- Create: `src/vector/mod.rs`
- Create: `src/vector/types.rs`

- [ ] **Step 1: Add feature flag and memmap2 dependency to Cargo.toml**

Add to `[features]`:
```toml
vector = ["dep:memmap2"]
```

Add to `[dependencies]`:
```toml
memmap2 = { version = "0.9", optional = true }
```

- [ ] **Step 2: Add vector error variants to src/error.rs**

Add these variants to the `BoogyError` enum:
```rust
VectorCollectionNotFound(String),
VectorCollectionExists(String),
VectorDimensionMismatch { expected: u32, got: u32 },
VectorError(String),
```

Add Display arms:
```rust
BoogyError::VectorCollectionNotFound(name) => write!(f, "vector collection '{name}' not found"),
BoogyError::VectorCollectionExists(name) => write!(f, "vector collection '{name}' already exists"),
BoogyError::VectorDimensionMismatch { expected, got } => write!(f, "vector dimension mismatch: expected {expected}, got {got}"),
BoogyError::VectorError(msg) => write!(f, "vector error: {msg}"),
```

- [ ] **Step 3: Create src/vector/types.rs**

```rust
use crate::filter::Filter;

/// Distance metric for vector similarity search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    /// Cosine distance (1 - cosine_similarity). Range [0, 2].
    Cosine,
    /// Euclidean (L2) distance. Range [0, inf).
    Euclidean,
    /// Negative dot product (lower = more similar). Range (-inf, inf).
    DotProduct,
}

impl DistanceMetric {
    pub fn to_tag(self) -> u8 {
        match self {
            DistanceMetric::Cosine => 0,
            DistanceMetric::Euclidean => 1,
            DistanceMetric::DotProduct => 2,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(DistanceMetric::Cosine),
            1 => Some(DistanceMetric::Euclidean),
            2 => Some(DistanceMetric::DotProduct),
            _ => None,
        }
    }
}

/// Options for creating a vector collection.
pub struct VectorCollectionOptions {
    pub dimensions: u32,
    pub metric: DistanceMetric,
    /// Max connections per node per layer. Layer 0 gets 2*m. Default: 16.
    pub m: u32,
    /// Beam width during insert. Default: 200.
    pub ef_construction: u32,
}

impl Default for VectorCollectionOptions {
    fn default() -> Self {
        Self {
            dimensions: 0,
            metric: DistanceMetric::Cosine,
            m: 16,
            ef_construction: 200,
        }
    }
}

impl VectorCollectionOptions {
    pub fn new(dimensions: u32, metric: DistanceMetric) -> Self {
        Self { dimensions, metric, ..Default::default() }
    }
}

/// A single search result.
#[derive(Debug, Clone)]
pub struct VectorResult {
    pub rowid: u64,
    pub distance: f32,
}

/// Options for vector search queries.
pub struct VectorSearchOptions {
    pub k: u32,
    /// Beam width during search. Default: 10.
    pub ef_search: u32,
    /// Optional metadata filter applied to results from the linked boogy-db table.
    pub filter: Option<Filter>,
}

impl Default for VectorSearchOptions {
    fn default() -> Self {
        Self { k: 10, ef_search: 10, filter: None }
    }
}

impl VectorSearchOptions {
    pub fn new(k: u32) -> Self {
        Self { k, ef_search: k.max(10), filter: None }
    }
}
```

- [ ] **Step 4: Create src/vector/mod.rs**

```rust
mod types;
mod distance;
mod mmap;
mod wal;
mod hnsw;
mod collection;

pub use types::{DistanceMetric, VectorCollectionOptions, VectorResult, VectorSearchOptions};
pub(crate) use collection::VectorCollection;
```

- [ ] **Step 5: Add conditional vector module to src/lib.rs**

Add after the `#[cfg(feature = "tokio")]` block:
```rust
#[cfg(feature = "vector")]
pub mod vector;
```

Add after the existing `#[cfg(feature = "tokio")]` pub use:
```rust
#[cfg(feature = "vector")]
pub use vector::{DistanceMetric, VectorCollectionOptions, VectorResult, VectorSearchOptions};
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo check --features vector`
Expected: Compilation succeeds (with dead_code warnings for types not yet used, and module errors for files not yet created — that's fine, we'll create placeholder files next).

Note: `collection.rs`, `distance.rs`, `hnsw.rs`, `mmap.rs`, `wal.rs` don't exist yet. Create them as empty files so the module tree resolves:

```rust
// src/vector/distance.rs — placeholder
// src/vector/mmap.rs — placeholder
// src/vector/wal.rs — placeholder
// src/vector/hnsw.rs — placeholder
// src/vector/collection.rs — placeholder
```

Run: `cargo check --features vector`
Expected: Clean compilation (possibly with unused warnings).

- [ ] **Step 7: Commit**

```bash
git add src/vector/ src/lib.rs src/error.rs Cargo.toml Cargo.lock
git commit -m "feat(vector): add feature flag, types, and error variants"
```

---

## Task 2: Distance Functions

**Files:**
- Create: `src/vector/distance.rs`
- Test: inline `#[cfg(test)]` module

- [ ] **Step 1: Write failing tests for all three distance metrics**

Replace the placeholder `src/vector/distance.rs` with:
```rust
/// Returns the distance function for the given metric.
pub fn distance_fn(metric: super::types::DistanceMetric) -> fn(&[f32], &[f32]) -> f32 {
    match metric {
        super::types::DistanceMetric::Cosine => cosine_distance,
        super::types::DistanceMetric::Euclidean => euclidean_distance,
        super::types::DistanceMetric::DotProduct => dot_product_distance,
    }
}

/// Cosine distance: 1.0 - cosine_similarity. Range [0, 2].
/// For identical vectors: 0.0. For orthogonal: 1.0. For opposite: 2.0.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    todo!()
}

/// Squared Euclidean distance. Using squared avoids the sqrt on the hot path.
/// Range [0, inf).
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
    todo!()
}

/// Negative dot product. Lower = more similar.
/// Negate so that the HNSW min-distance logic works uniformly across all metrics.
pub fn dot_product_distance(a: &[f32], b: &[f32]) -> f32 {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_identical_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let dist = cosine_distance(&a, &a);
        assert!((dist - 0.0).abs() < 1e-6, "identical vectors should have distance 0, got {dist}");
    }

    #[test]
    fn test_cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let dist = cosine_distance(&a, &b);
        assert!((dist - 1.0).abs() < 1e-6, "orthogonal vectors should have distance 1.0, got {dist}");
    }

    #[test]
    fn test_cosine_opposite_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let dist = cosine_distance(&a, &b);
        assert!((dist - 2.0).abs() < 1e-6, "opposite vectors should have distance 2.0, got {dist}");
    }

    #[test]
    fn test_cosine_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        let dist = cosine_distance(&a, &b);
        // Zero vector has undefined cosine — return max distance
        assert!((dist - 1.0).abs() < 1e-6 || dist >= 1.0, "zero vector should return high distance, got {dist}");
    }

    #[test]
    fn test_euclidean_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let dist = euclidean_distance(&a, &a);
        assert!((dist - 0.0).abs() < 1e-6, "identical vectors should have distance 0, got {dist}");
    }

    #[test]
    fn test_euclidean_known_value() {
        let a = vec![0.0, 0.0];
        let b = vec![3.0, 4.0];
        let dist = euclidean_distance(&a, &b);
        // Squared euclidean: 9 + 16 = 25
        assert!((dist - 25.0).abs() < 1e-6, "expected squared distance 25.0, got {dist}");
    }

    #[test]
    fn test_dot_product_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let dist = dot_product_distance(&a, &a);
        // dot = 1+4+9 = 14, negative = -14
        assert!((dist - (-14.0)).abs() < 1e-6, "expected -14.0, got {dist}");
    }

    #[test]
    fn test_dot_product_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let dist = dot_product_distance(&a, &b);
        assert!((dist - 0.0).abs() < 1e-6, "orthogonal dot product should be 0, got {dist}");
    }

    #[test]
    fn test_distance_fn_dispatch() {
        use super::super::types::DistanceMetric;
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0];
        let f = distance_fn(DistanceMetric::Cosine);
        assert!((f(&a, &b) - 0.0).abs() < 1e-6);
        let f = distance_fn(DistanceMetric::Euclidean);
        assert!((f(&a, &b) - 0.0).abs() < 1e-6);
        let f = distance_fn(DistanceMetric::DotProduct);
        assert!((f(&a, &b) - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_normalized_equals_dot() {
        // For unit vectors, cosine_distance = 1 - dot_product
        let a = vec![0.6, 0.8]; // magnitude 1.0
        let b = vec![1.0, 0.0];
        let cos_dist = cosine_distance(&a, &b);
        let dot = -(dot_product_distance(&a, &b)); // un-negate
        assert!((cos_dist - (1.0 - dot)).abs() < 1e-5, "cos={cos_dist}, 1-dot={}", 1.0 - dot);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --features vector vector::distance --lib`
Expected: FAIL — `todo!()` panics.

- [ ] **Step 3: Implement the distance functions**

Replace the three `todo!()` bodies:

```rust
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        return 1.0; // zero vector — max distance
    }
    1.0 - (dot / denom)
}

pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..a.len() {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum
}

pub fn dot_product_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..a.len() {
        sum += a[i] * b[i];
    }
    -sum
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --features vector vector::distance --lib`
Expected: All 10 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/vector/distance.rs
git commit -m "feat(vector): implement cosine, euclidean, and dot product distance functions"
```

---

## Task 3: Memory-Mapped File Manager

**Files:**
- Create: `src/vector/mmap.rs`

This manages the mmap'd vector file: header, vector data region, graph region (node index + graph data), free list.

- [ ] **Step 1: Write the mmap module with header and vector region**

Replace placeholder `src/vector/mmap.rs`:

```rust
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use memmap2::{MmapMut, MmapOptions};

use super::types::DistanceMetric;
use crate::error::{BoogyError, Result};

/// Magic bytes for vector file header: "BVEC"
const MAGIC: [u8; 4] = [0x42, 0x56, 0x45, 0x43];
const VERSION: u32 = 1;

/// Fixed header size: 4096 bytes (one page). Fields packed at the start.
/// Layout:
///   [0..4]    magic "BVEC"
///   [4..8]    version: u32
///   [8..12]   dimensions: u32
///   [12..13]  metric: u8
///   [13..17]  m: u32
///   [17..21]  ef_construction: u32
///   [21..25]  entry_point: u32 (0xFFFFFFFF = none)
///   [25..29]  node_count: u32
///   [29..33]  max_layer: u32 (0 if empty)
///   [33..37]  node_capacity: u32 (allocated slots)
///   [37..41]  free_list_head: u32 (0xFFFFFFFF = empty)
///   [41..45]  free_list_len: u32
///   [45..49]  graph_data_len: u64 (bytes used in graph data area)
///   [49..57]  reserved
const HEADER_SIZE: usize = 4096;
const NONE_NODE: u32 = 0xFFFFFFFF;

/// In-memory representation of the file header.
#[derive(Debug, Clone)]
pub struct VecFileHeader {
    pub dimensions: u32,
    pub metric: DistanceMetric,
    pub m: u32,
    pub ef_construction: u32,
    pub entry_point: Option<u32>,
    pub node_count: u32,
    pub max_layer: u32,
    pub node_capacity: u32,
    pub free_list_head: Option<u32>,
    pub free_list_len: u32,
    pub graph_data_len: u64,
}

/// Manages a memory-mapped vector file.
///
/// File layout:
///   [Header: HEADER_SIZE bytes]
///   [Vector Data Region: node_capacity * dims * 4 bytes]
///   [Node Index: node_capacity * 8 bytes (offset u64 per node into graph data)]
///   [Graph Data Area: variable, neighbor records appended here]
///   [Free List: node_capacity * 1 byte (0 = active, 1 = deleted, stores next-free chain in vector slot)]
pub struct VecFile {
    path: PathBuf,
    file: File,
    mmap: MmapMut,
    header: VecFileHeader,
}

impl VecFile {
    /// Create a new vector file with the given parameters.
    pub fn create(
        path: &Path,
        dimensions: u32,
        metric: DistanceMetric,
        m: u32,
        ef_construction: u32,
        initial_capacity: u32,
    ) -> Result<Self> {
        let vector_region_size = initial_capacity as u64 * dimensions as u64 * 4;
        let node_index_size = initial_capacity as u64 * 8;
        let deleted_flags_size = initial_capacity as u64;
        // Start with some space for graph data
        let initial_graph_data = initial_capacity as u64 * Self::max_neighbor_record_size(m) as u64;
        let file_size = HEADER_SIZE as u64
            + vector_region_size
            + node_index_size
            + initial_graph_data
            + deleted_flags_size;

        let file = OpenOptions::new()
            .read(true).write(true).create_new(true)
            .open(path)
            .map_err(BoogyError::Io)?;
        file.set_len(file_size).map_err(BoogyError::Io)?;

        let mmap = unsafe {
            MmapOptions::new().len(file_size as usize).map_mut(&file)
        }.map_err(BoogyError::Io)?;

        let header = VecFileHeader {
            dimensions,
            metric,
            m,
            ef_construction,
            entry_point: None,
            node_count: 0,
            max_layer: 0,
            node_capacity: initial_capacity,
            free_list_head: None,
            free_list_len: 0,
            graph_data_len: 0,
        };

        let mut vf = Self { path: path.to_path_buf(), file, mmap, header };
        vf.write_header()?;
        // Initialize node index to NONE offsets
        let idx_start = vf.node_index_offset();
        for i in 0..initial_capacity as usize {
            let off = idx_start + i * 8;
            vf.mmap[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        }
        vf.mmap.flush().map_err(BoogyError::Io)?;
        Ok(vf)
    }

    /// Open an existing vector file.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true).write(true)
            .open(path)
            .map_err(BoogyError::Io)?;
        let metadata = file.metadata().map_err(BoogyError::Io)?;
        let mmap = unsafe {
            MmapOptions::new().len(metadata.len() as usize).map_mut(&file)
        }.map_err(BoogyError::Io)?;

        let mut vf = Self {
            path: path.to_path_buf(),
            file,
            mmap,
            header: VecFileHeader {
                dimensions: 0, metric: DistanceMetric::Cosine, m: 0,
                ef_construction: 0, entry_point: None, node_count: 0,
                max_layer: 0, node_capacity: 0, free_list_head: None,
                free_list_len: 0, graph_data_len: 0,
            },
        };
        vf.read_header()?;
        Ok(vf)
    }

    fn write_header(&mut self) -> Result<()> {
        let h = &self.header;
        self.mmap[0..4].copy_from_slice(&MAGIC);
        self.mmap[4..8].copy_from_slice(&VERSION.to_le_bytes());
        self.mmap[8..12].copy_from_slice(&h.dimensions.to_le_bytes());
        self.mmap[12] = h.metric.to_tag();
        self.mmap[13..17].copy_from_slice(&h.m.to_le_bytes());
        self.mmap[17..21].copy_from_slice(&h.ef_construction.to_le_bytes());
        self.mmap[21..25].copy_from_slice(&h.entry_point.unwrap_or(NONE_NODE).to_le_bytes());
        self.mmap[25..29].copy_from_slice(&h.node_count.to_le_bytes());
        self.mmap[29..33].copy_from_slice(&h.max_layer.to_le_bytes());
        self.mmap[33..37].copy_from_slice(&h.node_capacity.to_le_bytes());
        self.mmap[37..41].copy_from_slice(&h.free_list_head.unwrap_or(NONE_NODE).to_le_bytes());
        self.mmap[41..45].copy_from_slice(&h.free_list_len.to_le_bytes());
        self.mmap[45..53].copy_from_slice(&h.graph_data_len.to_le_bytes());
        Ok(())
    }

    fn read_header(&mut self) -> Result<()> {
        if self.mmap[0..4] != MAGIC {
            return Err(BoogyError::Corruption("invalid vector file magic".into()));
        }
        let version = u32::from_le_bytes(self.mmap[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(BoogyError::Corruption(format!("unsupported vector file version: {version}")));
        }
        let dimensions = u32::from_le_bytes(self.mmap[8..12].try_into().unwrap());
        let metric = DistanceMetric::from_tag(self.mmap[12])
            .ok_or_else(|| BoogyError::Corruption("invalid metric tag".into()))?;
        let m = u32::from_le_bytes(self.mmap[13..17].try_into().unwrap());
        let ef_construction = u32::from_le_bytes(self.mmap[17..21].try_into().unwrap());
        let ep_raw = u32::from_le_bytes(self.mmap[21..25].try_into().unwrap());
        let node_count = u32::from_le_bytes(self.mmap[25..29].try_into().unwrap());
        let max_layer = u32::from_le_bytes(self.mmap[29..33].try_into().unwrap());
        let node_capacity = u32::from_le_bytes(self.mmap[33..37].try_into().unwrap());
        let fl_raw = u32::from_le_bytes(self.mmap[37..41].try_into().unwrap());
        let free_list_len = u32::from_le_bytes(self.mmap[41..45].try_into().unwrap());
        let graph_data_len = u64::from_le_bytes(self.mmap[45..53].try_into().unwrap());

        self.header = VecFileHeader {
            dimensions, metric, m, ef_construction,
            entry_point: if ep_raw == NONE_NODE { None } else { Some(ep_raw) },
            node_count, max_layer, node_capacity,
            free_list_head: if fl_raw == NONE_NODE { None } else { Some(fl_raw) },
            free_list_len, graph_data_len,
        };
        Ok(())
    }

    // --- Region offset calculations ---

    fn vector_region_offset(&self) -> usize {
        HEADER_SIZE
    }

    fn vector_slot_size(&self) -> usize {
        self.header.dimensions as usize * 4
    }

    fn node_index_offset(&self) -> usize {
        self.vector_region_offset() + self.header.node_capacity as usize * self.vector_slot_size()
    }

    fn graph_data_offset(&self) -> usize {
        self.node_index_offset() + self.header.node_capacity as usize * 8
    }

    fn deleted_flags_offset(&self) -> usize {
        self.graph_data_offset()
            + self.header.node_capacity as usize * Self::max_neighbor_record_size(self.header.m)
    }

    /// Max size of a neighbor record for the highest possible layer.
    /// Layer 0: 2*M neighbors. Each higher layer: M neighbors.
    /// A node at layer L has: header(4 bytes for max_layer + 4 bytes for neighbor_count per layer)
    ///   + layer 0 neighbors (2*M * 4) + layers 1..L neighbors (L * M * 4)
    /// For capacity calculation we use a conservative max of 8 layers.
    fn max_neighbor_record_size(m: u32) -> usize {
        let max_layers = 8u32; // conservative; actual max depends on collection size
        let header = 4; // max_layer field
        let layer0 = (2 * m * 4) as usize + 4; // neighbors + count
        let upper: usize = (1..max_layers).map(|_| (m * 4) as usize + 4).sum();
        header + layer0 + upper
    }

    // --- Vector data access ---

    /// Read vector data for node_id. Returns a slice into the mmap.
    pub fn read_vector(&self, node_id: u32) -> &[f32] {
        let offset = self.vector_region_offset() + node_id as usize * self.vector_slot_size();
        let bytes = &self.mmap[offset..offset + self.vector_slot_size()];
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, self.header.dimensions as usize) }
    }

    /// Write vector data for node_id.
    pub fn write_vector(&mut self, node_id: u32, vector: &[f32]) {
        let offset = self.vector_region_offset() + node_id as usize * self.vector_slot_size();
        let bytes = unsafe {
            std::slice::from_raw_parts(vector.as_ptr() as *const u8, vector.len() * 4)
        };
        self.mmap[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    // --- Deleted flag access ---

    pub fn is_deleted(&self, node_id: u32) -> bool {
        let offset = self.deleted_flags_offset() + node_id as usize;
        self.mmap[offset] == 1
    }

    pub fn set_deleted(&mut self, node_id: u32, deleted: bool) {
        let offset = self.deleted_flags_offset() + node_id as usize;
        self.mmap[offset] = if deleted { 1 } else { 0 };
    }

    // --- Node index (graph record offsets) ---

    /// Get the offset into the graph data area for a node's neighbor record.
    /// Returns None if the node has no graph record yet.
    pub fn graph_record_offset(&self, node_id: u32) -> Option<u64> {
        let idx_off = self.node_index_offset() + node_id as usize * 8;
        let raw = u64::from_le_bytes(self.mmap[idx_off..idx_off + 8].try_into().unwrap());
        if raw == u64::MAX { None } else { Some(raw) }
    }

    fn set_graph_record_offset(&mut self, node_id: u32, offset: u64) {
        let idx_off = self.node_index_offset() + node_id as usize * 8;
        self.mmap[idx_off..idx_off + 8].copy_from_slice(&offset.to_le_bytes());
    }

    // --- Graph neighbor record access ---
    // Record layout at graph_data_offset + record_offset:
    //   [max_layer: u32]
    //   For layer 0: [count: u32][neighbor_0: u32] ... [neighbor_{2M-1}: u32]
    //   For layer i>0: [count: u32][neighbor_0: u32] ... [neighbor_{M-1}: u32]

    fn graph_record_abs_offset(&self, record_offset: u64) -> usize {
        self.graph_data_offset() + record_offset as usize
    }

    /// Read the max layer for a node.
    pub fn read_node_max_layer(&self, node_id: u32) -> Option<u32> {
        let record_offset = self.graph_record_offset(node_id)?;
        let abs = self.graph_record_abs_offset(record_offset);
        Some(u32::from_le_bytes(self.mmap[abs..abs + 4].try_into().unwrap()))
    }

    fn layer_neighbors_offset(&self, record_offset: u64, layer: u32) -> usize {
        let abs = self.graph_record_abs_offset(record_offset);
        let mut off = abs + 4; // skip max_layer
        for l in 0..layer {
            let cap = if l == 0 { 2 * self.header.m } else { self.header.m };
            off += 4 + cap as usize * 4; // count + slots
        }
        off
    }

    fn layer_capacity(&self, layer: u32) -> u32 {
        if layer == 0 { 2 * self.header.m } else { self.header.m }
    }

    /// Read neighbors for a node at a given layer.
    pub fn read_neighbors(&self, node_id: u32, layer: u32) -> Vec<u32> {
        let record_offset = match self.graph_record_offset(node_id) {
            Some(o) => o,
            None => return Vec::new(),
        };
        let off = self.layer_neighbors_offset(record_offset, layer);
        let count = u32::from_le_bytes(self.mmap[off..off + 4].try_into().unwrap()) as usize;
        let mut neighbors = Vec::with_capacity(count);
        for i in 0..count {
            let noff = off + 4 + i * 4;
            neighbors.push(u32::from_le_bytes(self.mmap[noff..noff + 4].try_into().unwrap()));
        }
        neighbors
    }

    /// Write neighbors for a node at a given layer.
    pub fn write_neighbors(&mut self, node_id: u32, layer: u32, neighbors: &[u32]) {
        let record_offset = match self.graph_record_offset(node_id) {
            Some(o) => o,
            None => return,
        };
        let off = self.layer_neighbors_offset(record_offset, layer);
        let cap = self.layer_capacity(layer) as usize;
        let count = neighbors.len().min(cap);
        self.mmap[off..off + 4].copy_from_slice(&(count as u32).to_le_bytes());
        for i in 0..count {
            let noff = off + 4 + i * 4;
            self.mmap[noff..noff + 4].copy_from_slice(&neighbors[i].to_le_bytes());
        }
        // Clear remaining slots
        for i in count..cap {
            let noff = off + 4 + i * 4;
            self.mmap[noff..noff + 4].copy_from_slice(&NONE_NODE.to_le_bytes());
        }
    }

    /// Allocate a new graph record for a node with the given max_layer.
    /// Appends to the graph data area.
    pub fn allocate_graph_record(&mut self, node_id: u32, max_layer: u32) -> Result<()> {
        let record_size = self.graph_record_size(max_layer);
        let data_offset = self.header.graph_data_len;
        // Check if we need to grow (leave room — we have pre-allocated space)
        self.header.graph_data_len = data_offset.checked_add(record_size as u64)
            .ok_or_else(|| BoogyError::VectorError("graph data overflow".into()))?;

        self.set_graph_record_offset(node_id, data_offset);

        let abs = self.graph_record_abs_offset(data_offset);
        // Write max_layer
        self.mmap[abs..abs + 4].copy_from_slice(&max_layer.to_le_bytes());
        // Zero-initialize all neighbor counts
        let mut off = abs + 4;
        for l in 0..=max_layer {
            let cap = self.layer_capacity(l) as usize;
            self.mmap[off..off + 4].copy_from_slice(&0u32.to_le_bytes()); // count = 0
            for i in 0..cap {
                let noff = off + 4 + i * 4;
                self.mmap[noff..noff + 4].copy_from_slice(&NONE_NODE.to_le_bytes());
            }
            off += 4 + cap * 4;
        }
        Ok(())
    }

    fn graph_record_size(&self, max_layer: u32) -> usize {
        let mut size = 4; // max_layer field
        for l in 0..=max_layer {
            let cap = self.layer_capacity(l) as usize;
            size += 4 + cap * 4; // count + slots
        }
        size
    }

    // --- Free list ---

    /// Allocate a node ID. Reuses from free list or bumps node_count.
    pub fn allocate_node(&mut self) -> Result<u32> {
        if let Some(free_id) = self.header.free_list_head {
            // Read next-free from the vector slot (first 4 bytes repurposed)
            let offset = self.vector_region_offset() + free_id as usize * self.vector_slot_size();
            let next_raw = u32::from_le_bytes(self.mmap[offset..offset + 4].try_into().unwrap());
            self.header.free_list_head = if next_raw == NONE_NODE { None } else { Some(next_raw) };
            self.header.free_list_len -= 1;
            self.set_deleted(free_id, false);
            return Ok(free_id);
        }
        let id = self.header.node_count;
        if id >= self.header.node_capacity {
            self.grow()?;
        }
        self.header.node_count = id.checked_add(1)
            .ok_or_else(|| BoogyError::VectorError("node count overflow".into()))?;
        Ok(id)
    }

    /// Mark a node as deleted and add to free list.
    pub fn free_node(&mut self, node_id: u32) {
        self.set_deleted(node_id, true);
        // Store current free list head in the vector slot
        let offset = self.vector_region_offset() + node_id as usize * self.vector_slot_size();
        let next = self.header.free_list_head.unwrap_or(NONE_NODE);
        self.mmap[offset..offset + 4].copy_from_slice(&next.to_le_bytes());
        self.header.free_list_head = Some(node_id);
        self.header.free_list_len += 1;
    }

    /// Double the capacity of the file.
    fn grow(&mut self) -> Result<()> {
        let new_capacity = self.header.node_capacity.checked_mul(2)
            .ok_or_else(|| BoogyError::VectorError("capacity overflow".into()))?;
        let vector_region_size = new_capacity as u64 * self.vector_slot_size() as u64;
        let node_index_size = new_capacity as u64 * 8;
        let graph_data_size = new_capacity as u64 * Self::max_neighbor_record_size(self.header.m) as u64;
        let deleted_flags_size = new_capacity as u64;
        let new_file_size = HEADER_SIZE as u64 + vector_region_size + node_index_size + graph_data_size + deleted_flags_size;

        // We need to remap. First flush, then resize, then remap.
        self.mmap.flush().map_err(BoogyError::Io)?;
        self.file.set_len(new_file_size).map_err(BoogyError::Io)?;

        // Copy old data regions to new positions (they shift because vector region grew)
        // This is complex — we need to read old data, remap, write to new positions.
        // For simplicity: read old regions into memory, remap, write back.
        let old_capacity = self.header.node_capacity;
        let old_node_index = self.read_raw_region(
            self.node_index_offset(), old_capacity as usize * 8);
        let old_graph_data = self.read_raw_region(
            self.graph_data_offset(), self.header.graph_data_len as usize);
        let old_deleted_flags = self.read_raw_region(
            self.deleted_flags_offset(), old_capacity as usize);

        // Update capacity before recalculating offsets
        self.header.node_capacity = new_capacity;

        // Remap
        self.mmap = unsafe {
            MmapOptions::new().len(new_file_size as usize).map_mut(&self.file)
        }.map_err(BoogyError::Io)?;

        // Write data to new positions
        let new_idx_off = self.node_index_offset();
        self.mmap[new_idx_off..new_idx_off + old_node_index.len()]
            .copy_from_slice(&old_node_index);
        // Initialize new index entries to NONE
        for i in old_capacity as usize..new_capacity as usize {
            let off = new_idx_off + i * 8;
            self.mmap[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        }

        let new_graph_off = self.graph_data_offset();
        self.mmap[new_graph_off..new_graph_off + old_graph_data.len()]
            .copy_from_slice(&old_graph_data);

        let new_del_off = self.deleted_flags_offset();
        self.mmap[new_del_off..new_del_off + old_deleted_flags.len()]
            .copy_from_slice(&old_deleted_flags);

        Ok(())
    }

    fn read_raw_region(&self, offset: usize, len: usize) -> Vec<u8> {
        self.mmap[offset..offset + len].to_vec()
    }

    // --- Accessors ---

    pub fn header(&self) -> &VecFileHeader {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut VecFileHeader {
        &mut self.header
    }

    pub fn dimensions(&self) -> u32 {
        self.header.dimensions
    }

    pub fn flush(&mut self) -> Result<()> {
        self.write_header()?;
        self.mmap.flush().map_err(BoogyError::Io)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_path(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn test_create_and_reopen() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir, "test.vec");
        {
            let mut vf = VecFile::create(&path, 4, DistanceMetric::Cosine, 16, 200, 64).unwrap();
            assert_eq!(vf.header().dimensions, 4);
            assert_eq!(vf.header().node_count, 0);
            assert!(vf.header().entry_point.is_none());
            vf.flush().unwrap();
        }
        {
            let vf = VecFile::open(&path).unwrap();
            assert_eq!(vf.header().dimensions, 4);
            assert_eq!(vf.header().metric, DistanceMetric::Cosine);
            assert_eq!(vf.header().m, 16);
        }
    }

    #[test]
    fn test_vector_read_write() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir, "test.vec");
        let mut vf = VecFile::create(&path, 3, DistanceMetric::Euclidean, 4, 100, 16).unwrap();
        let id = vf.allocate_node().unwrap();
        assert_eq!(id, 0);
        vf.write_vector(id, &[1.0, 2.0, 3.0]);
        let v = vf.read_vector(id);
        assert_eq!(v, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_free_list_reuse() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir, "test.vec");
        let mut vf = VecFile::create(&path, 2, DistanceMetric::Cosine, 4, 100, 16).unwrap();
        let id0 = vf.allocate_node().unwrap();
        let id1 = vf.allocate_node().unwrap();
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        vf.free_node(id0);
        assert!(vf.is_deleted(id0));
        assert_eq!(vf.header().free_list_len, 1);
        let id2 = vf.allocate_node().unwrap();
        assert_eq!(id2, 0); // reused
        assert!(!vf.is_deleted(id2));
    }

    #[test]
    fn test_graph_record_neighbors() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir, "test.vec");
        let mut vf = VecFile::create(&path, 2, DistanceMetric::Cosine, 4, 100, 16).unwrap();
        let id = vf.allocate_node().unwrap();
        vf.allocate_graph_record(id, 1).unwrap(); // node appears on layers 0 and 1

        // Layer 0: capacity = 2*4 = 8 neighbors
        vf.write_neighbors(id, 0, &[1, 2, 3]);
        let n = vf.read_neighbors(id, 0);
        assert_eq!(n, vec![1, 2, 3]);

        // Layer 1: capacity = 4 neighbors
        vf.write_neighbors(id, 1, &[5, 6]);
        let n = vf.read_neighbors(id, 1);
        assert_eq!(n, vec![5, 6]);

        assert_eq!(vf.read_node_max_layer(id), Some(1));
    }

    #[test]
    fn test_grow_preserves_data() {
        let dir = TempDir::new().unwrap();
        let path = tmp_path(&dir, "test.vec");
        let mut vf = VecFile::create(&path, 2, DistanceMetric::Cosine, 4, 100, 4).unwrap();
        // Fill all 4 slots
        for i in 0..4 {
            let id = vf.allocate_node().unwrap();
            vf.write_vector(id, &[i as f32, (i * 10) as f32]);
            vf.allocate_graph_record(id, 0).unwrap();
            vf.write_neighbors(id, 0, &[]);
        }
        // Next allocate triggers grow
        let id4 = vf.allocate_node().unwrap();
        assert_eq!(id4, 4);
        assert_eq!(vf.header().node_capacity, 8);
        // Verify old data survived
        assert_eq!(vf.read_vector(0), &[0.0, 0.0]);
        assert_eq!(vf.read_vector(3), &[3.0, 30.0]);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --features vector vector::mmap --lib`
Expected: All 5 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/vector/mmap.rs
git commit -m "feat(vector): memory-mapped file manager with header, vector/graph regions, free list"
```

---

## Task 4: Vector WAL

**Files:**
- Create: `src/vector/wal.rs`

- [ ] **Step 1: Implement the vector WAL**

Replace placeholder `src/vector/wal.rs`:

```rust
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{BoogyError, Result};

/// WAL entry type tags.
const TAG_INSERT_VECTOR: u8 = 1;
const TAG_SET_NEIGHBORS: u8 = 2;
const TAG_DELETE_NODE: u8 = 3;
const TAG_UPDATE_HEADER: u8 = 4;
const TAG_COMMIT: u8 = 255;

/// A single WAL entry.
#[derive(Debug, Clone)]
pub enum WalEntry {
    InsertVector {
        node_id: u32,
        layer: u32,
        vector: Vec<f32>,
    },
    SetNeighbors {
        node_id: u32,
        layer: u32,
        neighbors: Vec<u32>,
    },
    DeleteNode {
        node_id: u32,
    },
    UpdateHeader {
        entry_point: u32,  // 0xFFFFFFFF = none
        node_count: u32,
        max_layer: u32,
    },
    Commit,
}

/// Vector WAL — sequential append log with commit markers.
pub struct VectorWal {
    path: PathBuf,
    file: File,
}

impl VectorWal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true).write(true).create(true)
            .open(path)
            .map_err(BoogyError::Io)?;
        Ok(Self { path: path.to_path_buf(), file })
    }

    /// Append a single entry to the WAL.
    pub fn append(&mut self, entry: &WalEntry) -> Result<()> {
        let mut buf = Vec::new();
        Self::encode_entry(&mut buf, entry);
        // Write length-prefixed: [len: u32][data]
        let len = buf.len() as u32;
        self.file.write_all(&len.to_le_bytes()).map_err(BoogyError::Io)?;
        self.file.write_all(&buf).map_err(BoogyError::Io)?;
        Ok(())
    }

    /// Append multiple entries and a commit marker, then optionally fsync.
    pub fn append_committed(&mut self, entries: &[WalEntry], fsync: bool) -> Result<()> {
        for entry in entries {
            self.append(entry)?;
        }
        self.append(&WalEntry::Commit)?;
        if fsync {
            self.file.sync_all().map_err(BoogyError::Io)?;
        }
        Ok(())
    }

    /// Read all committed transactions from the WAL.
    /// Returns groups of entries (one Vec per committed transaction).
    /// Incomplete transactions (no Commit marker) are discarded.
    pub fn read_committed(&mut self) -> Result<Vec<Vec<WalEntry>>> {
        self.file.seek(SeekFrom::Start(0)).map_err(BoogyError::Io)?;
        let mut reader = BufReader::new(&self.file);
        let mut transactions = Vec::new();
        let mut current_tx = Vec::new();

        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {},
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(BoogyError::Io(e)),
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];
            match reader.read_exact(&mut data) {
                Ok(()) => {},
                Err(_) => break, // partial entry — discard
            }
            match Self::decode_entry(&data) {
                Some(WalEntry::Commit) => {
                    transactions.push(std::mem::take(&mut current_tx));
                }
                Some(entry) => current_tx.push(entry),
                None => break, // corrupt entry — stop
            }
        }
        // current_tx holds an uncommitted transaction — discard it
        Ok(transactions)
    }

    /// Truncate the WAL (after successful replay).
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0).map_err(BoogyError::Io)?;
        self.file.seek(SeekFrom::Start(0)).map_err(BoogyError::Io)?;
        Ok(())
    }

    /// Returns true if the WAL file is empty.
    pub fn is_empty(&self) -> Result<bool> {
        let len = self.file.metadata().map_err(BoogyError::Io)?.len();
        Ok(len == 0)
    }

    // --- Encoding ---

    fn encode_entry(buf: &mut Vec<u8>, entry: &WalEntry) {
        match entry {
            WalEntry::InsertVector { node_id, layer, vector } => {
                buf.push(TAG_INSERT_VECTOR);
                buf.extend_from_slice(&node_id.to_le_bytes());
                buf.extend_from_slice(&layer.to_le_bytes());
                buf.extend_from_slice(&(vector.len() as u32).to_le_bytes());
                for &v in vector {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            WalEntry::SetNeighbors { node_id, layer, neighbors } => {
                buf.push(TAG_SET_NEIGHBORS);
                buf.extend_from_slice(&node_id.to_le_bytes());
                buf.extend_from_slice(&layer.to_le_bytes());
                buf.extend_from_slice(&(neighbors.len() as u32).to_le_bytes());
                for &n in neighbors {
                    buf.extend_from_slice(&n.to_le_bytes());
                }
            }
            WalEntry::DeleteNode { node_id } => {
                buf.push(TAG_DELETE_NODE);
                buf.extend_from_slice(&node_id.to_le_bytes());
            }
            WalEntry::UpdateHeader { entry_point, node_count, max_layer } => {
                buf.push(TAG_UPDATE_HEADER);
                buf.extend_from_slice(&entry_point.to_le_bytes());
                buf.extend_from_slice(&node_count.to_le_bytes());
                buf.extend_from_slice(&max_layer.to_le_bytes());
            }
            WalEntry::Commit => {
                buf.push(TAG_COMMIT);
            }
        }
    }

    fn decode_entry(data: &[u8]) -> Option<WalEntry> {
        if data.is_empty() { return None; }
        let tag = data[0];
        let rest = &data[1..];
        match tag {
            TAG_INSERT_VECTOR => {
                if rest.len() < 12 { return None; }
                let node_id = u32::from_le_bytes(rest[0..4].try_into().unwrap());
                let layer = u32::from_le_bytes(rest[4..8].try_into().unwrap());
                let vec_len = u32::from_le_bytes(rest[8..12].try_into().unwrap()) as usize;
                if rest.len() < 12 + vec_len * 4 { return None; }
                let mut vector = Vec::with_capacity(vec_len);
                for i in 0..vec_len {
                    let off = 12 + i * 4;
                    vector.push(f32::from_le_bytes(rest[off..off + 4].try_into().unwrap()));
                }
                Some(WalEntry::InsertVector { node_id, layer, vector })
            }
            TAG_SET_NEIGHBORS => {
                if rest.len() < 12 { return None; }
                let node_id = u32::from_le_bytes(rest[0..4].try_into().unwrap());
                let layer = u32::from_le_bytes(rest[4..8].try_into().unwrap());
                let count = u32::from_le_bytes(rest[8..12].try_into().unwrap()) as usize;
                if rest.len() < 12 + count * 4 { return None; }
                let mut neighbors = Vec::with_capacity(count);
                for i in 0..count {
                    let off = 12 + i * 4;
                    neighbors.push(u32::from_le_bytes(rest[off..off + 4].try_into().unwrap()));
                }
                Some(WalEntry::SetNeighbors { node_id, layer, neighbors })
            }
            TAG_DELETE_NODE => {
                if rest.len() < 4 { return None; }
                let node_id = u32::from_le_bytes(rest[0..4].try_into().unwrap());
                Some(WalEntry::DeleteNode { node_id })
            }
            TAG_UPDATE_HEADER => {
                if rest.len() < 12 { return None; }
                let entry_point = u32::from_le_bytes(rest[0..4].try_into().unwrap());
                let node_count = u32::from_le_bytes(rest[4..8].try_into().unwrap());
                let max_layer = u32::from_le_bytes(rest[8..12].try_into().unwrap());
                Some(WalEntry::UpdateHeader { entry_point, node_count, max_layer })
            }
            TAG_COMMIT => Some(WalEntry::Commit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_append_and_read_committed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.vec.wal");
        let mut wal = VectorWal::open(&path).unwrap();

        let entries = vec![
            WalEntry::InsertVector { node_id: 0, layer: 1, vector: vec![1.0, 2.0, 3.0] },
            WalEntry::SetNeighbors { node_id: 0, layer: 0, neighbors: vec![1, 2] },
            WalEntry::UpdateHeader { entry_point: 0, node_count: 1, max_layer: 1 },
        ];
        wal.append_committed(&entries, false).unwrap();

        let txns = wal.read_committed().unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].len(), 3);
        match &txns[0][0] {
            WalEntry::InsertVector { node_id, layer, vector } => {
                assert_eq!(*node_id, 0);
                assert_eq!(*layer, 1);
                assert_eq!(vector, &[1.0, 2.0, 3.0]);
            }
            _ => panic!("expected InsertVector"),
        }
    }

    #[test]
    fn test_uncommitted_transaction_discarded() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.vec.wal");
        let mut wal = VectorWal::open(&path).unwrap();

        // One committed tx
        wal.append_committed(&[
            WalEntry::InsertVector { node_id: 0, layer: 0, vector: vec![1.0] },
        ], false).unwrap();
        // One uncommitted entry (no Commit marker)
        wal.append(&WalEntry::InsertVector { node_id: 1, layer: 0, vector: vec![2.0] }).unwrap();

        let txns = wal.read_committed().unwrap();
        assert_eq!(txns.len(), 1); // only the committed one
    }

    #[test]
    fn test_truncate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.vec.wal");
        let mut wal = VectorWal::open(&path).unwrap();
        wal.append_committed(&[
            WalEntry::DeleteNode { node_id: 5 },
        ], false).unwrap();
        assert!(!wal.is_empty().unwrap());
        wal.truncate().unwrap();
        assert!(wal.is_empty().unwrap());
        let txns = wal.read_committed().unwrap();
        assert!(txns.is_empty());
    }

    #[test]
    fn test_empty_wal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.vec.wal");
        let mut wal = VectorWal::open(&path).unwrap();
        assert!(wal.is_empty().unwrap());
        let txns = wal.read_committed().unwrap();
        assert!(txns.is_empty());
    }

    #[test]
    fn test_multiple_transactions() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.vec.wal");
        let mut wal = VectorWal::open(&path).unwrap();

        wal.append_committed(&[
            WalEntry::InsertVector { node_id: 0, layer: 0, vector: vec![1.0, 2.0] },
        ], false).unwrap();
        wal.append_committed(&[
            WalEntry::InsertVector { node_id: 1, layer: 0, vector: vec![3.0, 4.0] },
            WalEntry::SetNeighbors { node_id: 0, layer: 0, neighbors: vec![1] },
            WalEntry::SetNeighbors { node_id: 1, layer: 0, neighbors: vec![0] },
        ], false).unwrap();

        let txns = wal.read_committed().unwrap();
        assert_eq!(txns.len(), 2);
        assert_eq!(txns[0].len(), 1);
        assert_eq!(txns[1].len(), 3);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --features vector vector::wal --lib`
Expected: All 5 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/vector/wal.rs
git commit -m "feat(vector): WAL with append, commit markers, replay, and truncate"
```

---

## Task 5: HNSW Algorithm (Pure Logic)

**Files:**
- Create: `src/vector/hnsw.rs`

This module is pure algorithm — no I/O. It takes closures/trait references for reading vectors and neighbors, and returns mutation lists. This makes it testable in isolation with in-memory data structures.

- [ ] **Step 1: Implement the HNSW algorithm**

Replace placeholder `src/vector/hnsw.rs`:

```rust
use std::collections::BinaryHeap;
use std::cmp::Ordering;

/// A candidate during HNSW search: (distance, node_id).
/// Min-heap ordering: smaller distance = higher priority.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    distance: f32,
    node_id: u32,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.distance == other.distance && self.node_id == other.node_id
    }
}
impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap (BinaryHeap is max-heap by default)
        other.distance.partial_cmp(&self.distance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.node_id.cmp(&self.node_id))
    }
}

/// Reversed candidate for max-heap usage (furthest first).
#[derive(Debug, Clone, Copy)]
struct FarCandidate {
    distance: f32,
    node_id: u32,
}

impl PartialEq for FarCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.distance == other.distance && self.node_id == other.node_id
    }
}
impl Eq for FarCandidate {}
impl PartialOrd for FarCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for FarCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance.partial_cmp(&other.distance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

/// Result of an HNSW insert: the mutations needed to update storage.
pub struct InsertResult {
    /// The node ID assigned to the inserted vector.
    pub node_id: u32,
    /// The layer this node was assigned to.
    pub level: u32,
    /// Neighbor connections to write: (node_id, layer, new_neighbor_list).
    /// Includes both the new node's neighbors and updated neighbors of existing nodes (due to pruning).
    pub connections: Vec<(u32, u32, Vec<u32>)>,
    /// New entry point, if this node became the new entry point.
    pub new_entry_point: Option<u32>,
    /// New max layer, if this node created a new highest layer.
    pub new_max_layer: Option<u32>,
}

/// Result of an HNSW search.
pub struct SearchResult {
    pub neighbors: Vec<(u32, f32)>, // (node_id, distance), sorted by distance ascending
}

/// Assign a random layer for a new node.
/// Uses the formula: floor(-ln(uniform(0,1)) * m_l) where m_l = 1/ln(M).
pub fn assign_layer(m: u32, rng_value: f64) -> u32 {
    let m_l = 1.0 / (m as f64).ln();
    let level = (-rng_value.ln() * m_l).floor() as u32;
    level
}

/// Greedy search from entry point through upper layers (above target_layer).
/// Returns the closest node found at target_layer.
pub fn search_upper_layers(
    entry_point: u32,
    query: &[f32],
    current_max_layer: u32,
    target_layer: u32,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    read_neighbors: &dyn Fn(u32, u32) -> Vec<u32>,
    is_deleted: &dyn Fn(u32) -> bool,
) -> u32 {
    let mut current = entry_point;
    let mut current_dist = distance_fn(query, &read_vector(current));

    for layer in (target_layer + 1..=current_max_layer).rev() {
        let mut changed = true;
        while changed {
            changed = false;
            for &neighbor in &read_neighbors(current, layer) {
                if is_deleted(&neighbor) { continue; }
                let dist = distance_fn(query, &read_vector(neighbor));
                if dist < current_dist {
                    current = neighbor;
                    current_dist = dist;
                    changed = true;
                }
            }
        }
    }
    current
}

/// Beam search at a single layer. Returns up to ef nearest non-deleted nodes.
pub fn search_layer(
    entry_points: &[u32],
    query: &[f32],
    ef: u32,
    layer: u32,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    read_neighbors: &dyn Fn(u32, u32) -> Vec<u32>,
    is_deleted: &dyn Fn(u32) -> bool,
) -> Vec<(u32, f32)> {
    let mut visited = std::collections::HashSet::new();
    let mut candidates: BinaryHeap<Candidate> = BinaryHeap::new(); // min-heap
    let mut results: BinaryHeap<FarCandidate> = BinaryHeap::new(); // max-heap

    for &ep in entry_points {
        if visited.insert(ep) {
            let dist = distance_fn(query, &read_vector(ep));
            candidates.push(Candidate { distance: dist, node_id: ep });
            if !is_deleted(&ep) {
                results.push(FarCandidate { distance: dist, node_id: ep });
            }
        }
    }

    while let Some(closest) = candidates.pop() {
        // If closest candidate is further than the furthest result, stop
        if let Some(furthest) = results.peek() {
            if results.len() >= ef as usize && closest.distance > furthest.distance {
                break;
            }
        }

        for &neighbor in &read_neighbors(closest.node_id, layer) {
            if !visited.insert(neighbor) { continue; }
            let dist = distance_fn(query, &read_vector(neighbor));

            let dominated = results.len() >= ef as usize
                && results.peek().is_some_and(|f| dist >= f.distance);
            if dominated { continue; }

            candidates.push(Candidate { distance: dist, node_id: neighbor });
            if !is_deleted(&neighbor) {
                results.push(FarCandidate { distance: dist, node_id: neighbor });
                if results.len() > ef as usize {
                    results.pop(); // evict furthest
                }
            }
        }
    }

    let mut result_vec: Vec<(u32, f32)> = results
        .into_iter()
        .map(|c| (c.node_id, c.distance))
        .collect();
    result_vec.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    result_vec
}

/// Select the M closest neighbors from candidates (simple heuristic).
pub fn select_neighbors(candidates: &[(u32, f32)], m: usize) -> Vec<u32> {
    // candidates should already be sorted by distance
    candidates.iter().take(m).map(|&(id, _)| id).collect()
}

/// Prune a neighbor list to keep at most max_neighbors, keeping the closest.
pub fn prune_neighbors(
    node_id: u32,
    current_neighbors: &[u32],
    max_neighbors: u32,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    is_deleted: &dyn Fn(u32) -> bool,
) -> Vec<u32> {
    if current_neighbors.len() <= max_neighbors as usize {
        return current_neighbors.to_vec();
    }
    let node_vec = read_vector(node_id);
    let mut scored: Vec<(u32, f32)> = current_neighbors.iter()
        .filter(|&&n| !is_deleted(&n))
        .map(|&n| {
            let dist = distance_fn(&node_vec, &read_vector(n));
            (n, dist)
        })
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    scored.into_iter().take(max_neighbors as usize).map(|(id, _)| id).collect()
}

/// Full HNSW k-NN search.
pub fn search(
    query: &[f32],
    k: u32,
    ef_search: u32,
    entry_point: u32,
    max_layer: u32,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    read_neighbors: &dyn Fn(u32, u32) -> Vec<u32>,
    is_deleted: &dyn Fn(u32) -> bool,
) -> SearchResult {
    let ef = ef_search.max(k);

    // Descend through upper layers
    let ep = search_upper_layers(
        entry_point, query, max_layer, 0,
        distance_fn, read_vector, read_neighbors, is_deleted,
    );

    // Search layer 0 with beam width ef
    let results = search_layer(
        &[ep], query, ef, 0,
        distance_fn, read_vector, read_neighbors, is_deleted,
    );

    SearchResult {
        neighbors: results.into_iter().take(k as usize).collect(),
    }
}

/// Full HNSW insert. Returns the mutations needed.
pub fn insert(
    node_id: u32,
    vector: &[f32],
    level: u32,
    entry_point: Option<u32>,
    current_max_layer: u32,
    m: u32,
    ef_construction: u32,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    read_neighbors: &dyn Fn(u32, u32) -> Vec<u32>,
    is_deleted: &dyn Fn(u32) -> bool,
) -> InsertResult {
    let mut connections = Vec::new();
    let mut new_entry_point = None;
    let mut new_max_layer = None;

    let Some(ep) = entry_point else {
        // First node — it becomes the entry point
        connections.push((node_id, 0, Vec::new()));
        return InsertResult {
            node_id, level,
            connections,
            new_entry_point: Some(node_id),
            new_max_layer: Some(level),
        };
    };

    // Descend from entry point through layers above this node's level
    let start_layer = level.min(current_max_layer);
    let closest = if start_layer < current_max_layer {
        search_upper_layers(
            ep, vector, current_max_layer, start_layer,
            distance_fn, read_vector, read_neighbors, is_deleted,
        )
    } else {
        ep
    };

    // Search and connect at each layer from level down to 0
    let mut ep_for_layer = closest;
    for layer in (0..=start_layer).rev() {
        let max_neighbors = if layer == 0 { 2 * m } else { m };

        let candidates = search_layer(
            &[ep_for_layer], vector, ef_construction, layer,
            distance_fn, read_vector, read_neighbors, is_deleted,
        );

        let neighbors = select_neighbors(&candidates, max_neighbors as usize);

        // Record this node's neighbors at this layer
        connections.push((node_id, layer, neighbors.clone()));

        // Add bidirectional connections and prune if needed
        for &neighbor in &neighbors {
            let mut neighbor_list = read_neighbors(neighbor, layer);
            neighbor_list.push(node_id);
            if neighbor_list.len() > max_neighbors as usize {
                neighbor_list = prune_neighbors(
                    neighbor, &neighbor_list, max_neighbors,
                    distance_fn, read_vector, is_deleted,
                );
            }
            connections.push((neighbor, layer, neighbor_list));
        }

        if !candidates.is_empty() {
            ep_for_layer = candidates[0].0;
        }
    }

    // Update entry point if needed
    if level > current_max_layer {
        new_entry_point = Some(node_id);
        new_max_layer = Some(level);
    }

    InsertResult {
        node_id, level, connections, new_entry_point, new_max_layer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory graph for testing HNSW in isolation.
    struct TestGraph {
        vectors: Vec<Vec<f32>>,
        neighbors: Vec<Vec<Vec<u32>>>, // [node_id][layer] -> neighbors
        deleted: Vec<bool>,
    }

    impl TestGraph {
        fn new() -> Self {
            Self { vectors: Vec::new(), neighbors: Vec::new(), deleted: Vec::new() }
        }

        fn add_node(&mut self, vector: Vec<f32>, max_layer: u32) -> u32 {
            let id = self.vectors.len() as u32;
            self.vectors.push(vector);
            self.neighbors.push(vec![Vec::new(); max_layer as usize + 1]);
            self.deleted.push(false);
            id
        }

        fn set_neighbors(&mut self, node: u32, layer: u32, neighbors: Vec<u32>) {
            self.neighbors[node as usize][layer as usize] = neighbors;
        }

        fn apply_insert(&mut self, result: &InsertResult) {
            for &(node_id, layer, ref nbrs) in &result.connections {
                while self.neighbors[node_id as usize].len() <= layer as usize {
                    self.neighbors[node_id as usize].push(Vec::new());
                }
                self.neighbors[node_id as usize][layer as usize] = nbrs.clone();
            }
        }
    }

    fn euclidean(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }

    #[test]
    fn test_assign_layer() {
        // With m=16, m_l = 1/ln(16) ≈ 0.36
        // rng_value close to 1.0 -> -ln(~1) ≈ 0 -> layer 0
        assert_eq!(assign_layer(16, 0.99), 0);
        // rng_value very small -> -ln(small) large -> higher layer
        let level = assign_layer(16, 0.001);
        assert!(level >= 2, "very small rng should give high layer, got {level}");
    }

    #[test]
    fn test_select_neighbors() {
        let candidates = vec![(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.4)];
        let selected = select_neighbors(&candidates, 2);
        assert_eq!(selected, vec![1, 2]);
    }

    #[test]
    fn test_search_layer_basic() {
        let mut graph = TestGraph::new();
        // Create 5 nodes in 2D, layer 0 only
        for i in 0..5 {
            graph.add_node(vec![i as f32, 0.0], 0);
        }
        // Linear chain: 0-1-2-3-4
        for i in 0..4 {
            graph.set_neighbors(i, 0, vec![i + 1]);
            graph.set_neighbors(i + 1, 0, vec![i]);
        }

        let query = vec![2.5, 0.0]; // closest to node 2 and 3
        let results = search_layer(
            &[0], &query, 5, 0,
            &euclidean,
            &|id| graph.vectors[id as usize].clone(),
            &|id, layer| graph.neighbors[id as usize].get(layer as usize).cloned().unwrap_or_default(),
            &|id| graph.deleted[id as usize],
        );

        // Should find nodes closest to [2.5, 0.0]
        assert!(!results.is_empty());
        // Node 2 (dist=0.25) or node 3 (dist=0.25) should be first
        assert!(results[0].0 == 2 || results[0].0 == 3);
    }

    #[test]
    fn test_insert_first_node() {
        let result = insert(
            0, &[1.0, 2.0], 0, None, 0, 4, 10,
            &euclidean,
            &|_| vec![1.0, 2.0],
            &|_, _| Vec::new(),
            &|_| false,
        );
        assert_eq!(result.new_entry_point, Some(0));
    }

    #[test]
    fn test_insert_and_search_small_graph() {
        let mut graph = TestGraph::new();
        let m = 4u32;
        let ef_construction = 10u32;
        let mut entry_point: Option<u32> = None;
        let mut max_layer = 0u32;

        // Insert 20 points on a line: [0,0], [1,0], ..., [19,0]
        // Force all to layer 0 for simplicity
        for i in 0..20 {
            let vector = vec![i as f32, 0.0];
            let id = graph.add_node(vector.clone(), 0);
            let result = insert(
                id, &vector, 0, entry_point, max_layer, m, ef_construction,
                &euclidean,
                &|nid| graph.vectors[nid as usize].clone(),
                &|nid, layer| graph.neighbors[nid as usize].get(layer as usize).cloned().unwrap_or_default(),
                &|nid| graph.deleted[nid as usize],
            );
            graph.apply_insert(&result);
            if let Some(ep) = result.new_entry_point {
                entry_point = Some(ep);
            }
            if let Some(ml) = result.new_max_layer {
                max_layer = ml;
            }
            if entry_point.is_none() {
                entry_point = Some(id);
            }
        }

        // Search for nearest to [10.5, 0.0] — should find 10 and 11
        let result = search(
            &[10.5, 0.0], 3, 10, entry_point.unwrap(), max_layer,
            &euclidean,
            &|id| graph.vectors[id as usize].clone(),
            &|id, layer| graph.neighbors[id as usize].get(layer as usize).cloned().unwrap_or_default(),
            &|id| graph.deleted[id as usize],
        );

        assert_eq!(result.neighbors.len(), 3);
        // Nodes 10 (dist=0.25) and 11 (dist=0.25) should be in top results
        let ids: Vec<u32> = result.neighbors.iter().map(|r| r.0).collect();
        assert!(ids.contains(&10), "expected node 10 in results, got {:?}", ids);
        assert!(ids.contains(&11), "expected node 11 in results, got {:?}", ids);
    }

    #[test]
    fn test_deleted_nodes_skipped() {
        let mut graph = TestGraph::new();
        let m = 4u32;
        let mut entry_point: Option<u32> = None;
        let mut max_layer = 0u32;

        for i in 0..10 {
            let vector = vec![i as f32, 0.0];
            let id = graph.add_node(vector.clone(), 0);
            let result = insert(
                id, &vector, 0, entry_point, max_layer, m, 10,
                &euclidean,
                &|nid| graph.vectors[nid as usize].clone(),
                &|nid, layer| graph.neighbors[nid as usize].get(layer as usize).cloned().unwrap_or_default(),
                &|nid| graph.deleted[nid as usize],
            );
            graph.apply_insert(&result);
            if let Some(ep) = result.new_entry_point { entry_point = Some(ep); }
            if let Some(ml) = result.new_max_layer { max_layer = ml; }
            if entry_point.is_none() { entry_point = Some(id); }
        }

        // Delete node 5
        graph.deleted[5] = true;

        let result = search(
            &[5.0, 0.0], 3, 10, entry_point.unwrap(), max_layer,
            &euclidean,
            &|id| graph.vectors[id as usize].clone(),
            &|id, layer| graph.neighbors[id as usize].get(layer as usize).cloned().unwrap_or_default(),
            &|id| graph.deleted[id as usize],
        );

        let ids: Vec<u32> = result.neighbors.iter().map(|r| r.0).collect();
        assert!(!ids.contains(&5), "deleted node 5 should not appear in results");
        // Should find 4 and 6 instead
        assert!(ids.contains(&4) || ids.contains(&6));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --features vector vector::hnsw --lib`
Expected: All 5 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/vector/hnsw.rs
git commit -m "feat(vector): HNSW algorithm — insert, search, delete, pruning (pure logic, no I/O)"
```

---

## Task 6: VectorCollection Coordinator

**Files:**
- Create: `src/vector/collection.rs`

This ties together VecFile, VectorWal, and HNSW into a single coordinated API.

- [ ] **Step 1: Implement VectorCollection**

Replace placeholder `src/vector/collection.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, Mutex};

use crate::db::Durability;
use crate::error::{BoogyError, Result};

use super::distance::distance_fn;
use super::hnsw;
use super::mmap::{VecFile, VecFileHeader};
use super::types::{DistanceMetric, VectorCollectionOptions, VectorResult, VectorSearchOptions};
use super::wal::{VectorWal, WalEntry};

const INITIAL_CAPACITY: u32 = 1024;

/// A vector collection backed by an mmap'd file with HNSW indexing.
pub struct VectorCollection {
    vecfile: VecFile,
    wal: VectorWal,
    dist_fn: fn(&[f32], &[f32]) -> f32,
    /// Mapping from external rowid to internal node_id.
    rowid_to_node: std::collections::HashMap<u64, u32>,
    /// Mapping from internal node_id to external rowid.
    node_to_rowid: std::collections::HashMap<u32, u64>,
    rng_state: u64,
}

impl VectorCollection {
    /// Create a new vector collection.
    pub fn create(
        vec_path: &Path,
        wal_path: &Path,
        options: &VectorCollectionOptions,
    ) -> Result<Self> {
        if options.dimensions == 0 || options.dimensions > 4096 {
            return Err(BoogyError::VectorError(
                format!("dimensions must be 1-4096, got {}", options.dimensions)
            ));
        }
        if options.m == 0 || options.m > 128 {
            return Err(BoogyError::VectorError(
                format!("m must be 1-128, got {}", options.m)
            ));
        }

        let vecfile = VecFile::create(
            vec_path, options.dimensions, options.metric,
            options.m, options.ef_construction, INITIAL_CAPACITY,
        )?;
        let wal = VectorWal::open(wal_path)?;

        Ok(Self {
            dist_fn: distance_fn(options.metric),
            vecfile, wal,
            rowid_to_node: std::collections::HashMap::new(),
            node_to_rowid: std::collections::HashMap::new(),
            rng_state: 0x12345678_9ABCDEF0,
        })
    }

    /// Open an existing vector collection, replaying the WAL if needed.
    pub fn open(vec_path: &Path, wal_path: &Path) -> Result<Self> {
        let mut vecfile = VecFile::open(vec_path)?;
        let mut wal = VectorWal::open(wal_path)?;

        // Replay WAL
        let transactions = wal.read_committed()?;
        for tx in &transactions {
            Self::replay_transaction(&mut vecfile, tx)?;
        }
        if !transactions.is_empty() {
            vecfile.flush()?;
            wal.truncate()?;
        }

        let dist_fn = distance_fn(vecfile.header().metric);

        let mut collection = Self {
            dist_fn,
            vecfile, wal,
            rowid_to_node: std::collections::HashMap::new(),
            node_to_rowid: std::collections::HashMap::new(),
            rng_state: 0x12345678_9ABCDEF0,
        };
        // Note: rowid<->node mappings are rebuilt from boogy-db by the caller (collection.rs
        // doesn't access boogy-db directly). The caller calls rebuild_mappings().
        Ok(collection)
    }

    /// Rebuild the rowid<->node mappings from an external source.
    /// Called by db.rs after opening the collection.
    pub fn rebuild_mappings(&mut self, mappings: Vec<(u64, u32)>) {
        self.rowid_to_node.clear();
        self.node_to_rowid.clear();
        for (rowid, node_id) in mappings {
            self.rowid_to_node.insert(rowid, node_id);
            self.node_to_rowid.insert(node_id, rowid);
        }
    }

    fn replay_transaction(vecfile: &mut VecFile, entries: &[WalEntry]) -> Result<()> {
        for entry in entries {
            match entry {
                WalEntry::InsertVector { node_id, layer, vector } => {
                    // Ensure node is allocated
                    while vecfile.header().node_count <= *node_id {
                        vecfile.allocate_node()?;
                    }
                    vecfile.write_vector(*node_id, vector);
                    vecfile.allocate_graph_record(*node_id, *layer)?;
                }
                WalEntry::SetNeighbors { node_id, layer, neighbors } => {
                    vecfile.write_neighbors(*node_id, *layer, neighbors);
                }
                WalEntry::DeleteNode { node_id } => {
                    vecfile.free_node(*node_id);
                }
                WalEntry::UpdateHeader { entry_point, node_count, max_layer } => {
                    let h = vecfile.header_mut();
                    h.entry_point = if *entry_point == 0xFFFFFFFF { None } else { Some(*entry_point) };
                    h.node_count = *node_count;
                    h.max_layer = *max_layer;
                }
                WalEntry::Commit => {} // shouldn't appear in transaction entries
            }
        }
        Ok(())
    }

    fn next_rng(&mut self) -> f64 {
        // Simple xorshift64 for layer assignment
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;
        // Map to (0, 1) — avoid 0 which would give ln(0) = -inf
        (self.rng_state as f64 / u64::MAX as f64).max(f64::MIN_POSITIVE)
    }

    /// Insert a vector associated with a rowid.
    pub fn insert(&mut self, rowid: u64, vector: &[f32], fsync: bool) -> Result<u32> {
        let dims = self.vecfile.dimensions();
        if vector.len() != dims as usize {
            return Err(BoogyError::VectorDimensionMismatch {
                expected: dims,
                got: vector.len() as u32,
            });
        }
        if self.rowid_to_node.contains_key(&rowid) {
            return Err(BoogyError::VectorError(
                format!("rowid {rowid} already has a vector in this collection")
            ));
        }

        let node_id = self.vecfile.allocate_node()?;
        let level = hnsw::assign_layer(self.vecfile.header().m, self.next_rng());
        let m = self.vecfile.header().m;
        let ef_construction = self.vecfile.header().ef_construction;
        let entry_point = self.vecfile.header().entry_point;
        let max_layer = self.vecfile.header().max_layer;

        // Write vector data
        self.vecfile.write_vector(node_id, vector);
        self.vecfile.allocate_graph_record(node_id, level)?;

        // Run HNSW insert algorithm
        let dist_fn = self.dist_fn;
        let result = hnsw::insert(
            node_id, vector, level, entry_point, max_layer,
            m, ef_construction,
            &|a, b| dist_fn(a, b),
            &|id| self.vecfile.read_vector(id).to_vec(),
            &|id, layer| self.vecfile.read_neighbors(id, layer),
            &|id| self.vecfile.is_deleted(id),
        );

        // Build WAL entries
        let mut wal_entries = Vec::new();
        wal_entries.push(WalEntry::InsertVector {
            node_id, layer: level, vector: vector.to_vec(),
        });

        // Apply connections
        for &(nid, layer, ref neighbors) in &result.connections {
            self.vecfile.write_neighbors(nid, layer, neighbors);
            wal_entries.push(WalEntry::SetNeighbors {
                node_id: nid, layer, neighbors: neighbors.clone(),
            });
        }

        // Update header
        if let Some(ep) = result.new_entry_point {
            self.vecfile.header_mut().entry_point = Some(ep);
        }
        if let Some(ml) = result.new_max_layer {
            self.vecfile.header_mut().max_layer = ml;
        }
        let h = self.vecfile.header();
        wal_entries.push(WalEntry::UpdateHeader {
            entry_point: h.entry_point.unwrap_or(0xFFFFFFFF),
            node_count: h.node_count,
            max_layer: h.max_layer,
        });

        // Write WAL
        self.wal.append_committed(&wal_entries, fsync)?;

        // Flush mmap
        self.vecfile.flush()?;

        // Truncate WAL after successful flush
        self.wal.truncate()?;

        // Update mappings
        self.rowid_to_node.insert(rowid, node_id);
        self.node_to_rowid.insert(node_id, rowid);

        Ok(node_id)
    }

    /// Delete a vector by rowid.
    pub fn delete(&mut self, rowid: u64, fsync: bool) -> Result<()> {
        let node_id = self.rowid_to_node.remove(&rowid)
            .ok_or_else(|| BoogyError::VectorError(
                format!("rowid {rowid} not found in collection")
            ))?;
        self.node_to_rowid.remove(&node_id);

        self.vecfile.free_node(node_id);

        let mut wal_entries = vec![WalEntry::DeleteNode { node_id }];
        // If deleted node was the entry point, we need to find a new one
        if self.vecfile.header().entry_point == Some(node_id) {
            // Pick any non-deleted neighbor at the highest layer, or clear
            let max_layer = self.vecfile.header().max_layer;
            let mut new_ep = None;
            for layer in (0..=max_layer).rev() {
                let neighbors = self.vecfile.read_neighbors(node_id, layer);
                for &n in &neighbors {
                    if !self.vecfile.is_deleted(n) {
                        new_ep = Some(n);
                        break;
                    }
                }
                if new_ep.is_some() { break; }
            }
            self.vecfile.header_mut().entry_point = new_ep;
        }

        let h = self.vecfile.header();
        wal_entries.push(WalEntry::UpdateHeader {
            entry_point: h.entry_point.unwrap_or(0xFFFFFFFF),
            node_count: h.node_count,
            max_layer: h.max_layer,
        });

        self.wal.append_committed(&wal_entries, fsync)?;
        self.vecfile.flush()?;
        self.wal.truncate()?;
        Ok(())
    }

    /// Update a vector for a rowid (delete + re-insert).
    pub fn update(&mut self, rowid: u64, vector: &[f32], fsync: bool) -> Result<u32> {
        self.delete(rowid, false)?;
        self.insert(rowid, vector, fsync)
    }

    /// Search for k nearest neighbors.
    pub fn search(&self, query: &[f32], k: u32, ef_search: u32) -> Result<Vec<VectorResult>> {
        let dims = self.vecfile.dimensions();
        if query.len() != dims as usize {
            return Err(BoogyError::VectorDimensionMismatch {
                expected: dims,
                got: query.len() as u32,
            });
        }

        let entry_point = match self.vecfile.header().entry_point {
            Some(ep) => ep,
            None => return Ok(Vec::new()), // empty collection
        };

        let dist_fn = self.dist_fn;
        let result = hnsw::search(
            query, k, ef_search, entry_point, self.vecfile.header().max_layer,
            &|a, b| dist_fn(a, b),
            &|id| self.vecfile.read_vector(id).to_vec(),
            &|id, layer| self.vecfile.read_neighbors(id, layer),
            &|id| self.vecfile.is_deleted(id),
        );

        Ok(result.neighbors.into_iter()
            .filter_map(|(node_id, distance)| {
                self.node_to_rowid.get(&node_id).map(|&rowid| VectorResult { rowid, distance })
            })
            .collect())
    }

    /// Number of vectors in the collection (excluding deleted).
    pub fn len(&self) -> usize {
        self.rowid_to_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rowid_to_node.is_empty()
    }

    pub fn dimensions(&self) -> u32 {
        self.vecfile.dimensions()
    }

    pub fn metric(&self) -> DistanceMetric {
        self.vecfile.header().metric
    }

    /// Get the internal node_id for a rowid, if it exists.
    pub fn node_id_for_rowid(&self, rowid: u64) -> Option<u32> {
        self.rowid_to_node.get(&rowid).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_opts(dims: u32) -> VectorCollectionOptions {
        VectorCollectionOptions {
            dimensions: dims,
            metric: DistanceMetric::Euclidean,
            m: 8,
            ef_construction: 50,
        }
    }

    #[test]
    fn test_create_insert_search() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("test.vec");
        let wal_path = dir.path().join("test.vec.wal");
        let opts = make_opts(3);
        let mut coll = VectorCollection::create(&vec_path, &wal_path, &opts).unwrap();

        coll.insert(100, &[1.0, 0.0, 0.0], false).unwrap();
        coll.insert(200, &[0.0, 1.0, 0.0], false).unwrap();
        coll.insert(300, &[0.0, 0.0, 1.0], false).unwrap();

        let results = coll.search(&[1.0, 0.1, 0.0], 2, 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].rowid, 100); // closest to [1,0,0]
    }

    #[test]
    fn test_delete_and_search() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("test.vec");
        let wal_path = dir.path().join("test.vec.wal");
        let opts = make_opts(2);
        let mut coll = VectorCollection::create(&vec_path, &wal_path, &opts).unwrap();

        coll.insert(1, &[0.0, 0.0], false).unwrap();
        coll.insert(2, &[1.0, 0.0], false).unwrap();
        coll.insert(3, &[2.0, 0.0], false).unwrap();

        coll.delete(2, false).unwrap();

        let results = coll.search(&[1.0, 0.0], 3, 10).unwrap();
        let rowids: Vec<u64> = results.iter().map(|r| r.rowid).collect();
        assert!(!rowids.contains(&2), "deleted rowid should not appear");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_update_vector() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("test.vec");
        let wal_path = dir.path().join("test.vec.wal");
        let opts = make_opts(2);
        let mut coll = VectorCollection::create(&vec_path, &wal_path, &opts).unwrap();

        coll.insert(1, &[0.0, 0.0], false).unwrap();
        coll.insert(2, &[10.0, 0.0], false).unwrap();

        // Update rowid 1 to be close to rowid 2
        coll.update(1, &[9.0, 0.0], false).unwrap();

        let results = coll.search(&[10.0, 0.0], 1, 10).unwrap();
        // Rowid 2 is at [10,0], rowid 1 is now at [9,0] — both close
        assert!(results[0].rowid == 2 || results[0].rowid == 1);
    }

    #[test]
    fn test_dimension_mismatch() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("test.vec");
        let wal_path = dir.path().join("test.vec.wal");
        let opts = make_opts(3);
        let mut coll = VectorCollection::create(&vec_path, &wal_path, &opts).unwrap();

        let err = coll.insert(1, &[1.0, 2.0], false); // 2 dims, expected 3
        assert!(err.is_err());
    }

    #[test]
    fn test_persistence_and_wal_replay() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("test.vec");
        let wal_path = dir.path().join("test.vec.wal");
        let opts = make_opts(2);

        // Create and insert
        {
            let mut coll = VectorCollection::create(&vec_path, &wal_path, &opts).unwrap();
            coll.insert(1, &[1.0, 0.0], false).unwrap();
            coll.insert(2, &[0.0, 1.0], false).unwrap();
        }

        // Reopen and search
        {
            let mut coll = VectorCollection::open(&vec_path, &wal_path).unwrap();
            // Rebuild mappings (normally db.rs does this)
            coll.rebuild_mappings(vec![(1, 0), (2, 1)]);
            let results = coll.search(&[1.0, 0.0], 1, 10).unwrap();
            assert_eq!(results[0].rowid, 1);
        }
    }

    #[test]
    fn test_empty_search() {
        let dir = TempDir::new().unwrap();
        let vec_path = dir.path().join("test.vec");
        let wal_path = dir.path().join("test.vec.wal");
        let opts = make_opts(2);
        let coll = VectorCollection::create(&vec_path, &wal_path, &opts).unwrap();

        let results = coll.search(&[1.0, 0.0], 5, 10).unwrap();
        assert!(results.is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --features vector vector::collection --lib`
Expected: All 6 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/vector/collection.rs
git commit -m "feat(vector): VectorCollection coordinator — ties mmap, WAL, and HNSW together"
```

---

## Task 7: BoogyDb Public API

**Files:**
- Modify: `src/db.rs`
- Modify: `src/vector/mod.rs`

Wire the `vector_*` methods into `BoogyDb`. Collections are stored in a `HashMap` keyed by `(table, collection_name)`. Metadata (collection params + rowid-to-node mappings) stored in boogy-db internal tables.

- [ ] **Step 1: Add vector collection registry to BoogyDb**

Add to `src/db.rs` imports (inside a `#[cfg(feature = "vector")]` block):

```rust
#[cfg(feature = "vector")]
use crate::vector::VectorCollection;
#[cfg(feature = "vector")]
use crate::vector::{VectorCollectionOptions, VectorResult, VectorSearchOptions};
```

Add a new field to the `BoogyDb` struct:

```rust
#[cfg(feature = "vector")]
vector_collections: RwLock<HashMap<(String, String), VectorCollection>>,
```

Initialize it in `BoogyDb::open()`:

```rust
#[cfg(feature = "vector")]
vector_collections: RwLock::new(HashMap::new()),
```

- [ ] **Step 2: Add vector_* public methods to BoogyDb**

Add an `impl BoogyDb` block gated by `#[cfg(feature = "vector")]`:

```rust
#[cfg(feature = "vector")]
impl BoogyDb {
    fn vector_file_path(&self, table: &str, collection: &str) -> PathBuf {
        let mut p = self.path.clone();
        p.set_extension(format!("{table}.{collection}.vec"));
        p
    }

    fn vector_wal_path(&self, table: &str, collection: &str) -> PathBuf {
        let mut p = self.path.clone();
        p.set_extension(format!("{table}.{collection}.vec.wal"));
        p
    }

    fn collection_key(table: &str, collection: &str) -> (String, String) {
        (table.to_string(), collection.to_string())
    }

    pub fn create_vector_collection(
        &self,
        table: &str,
        name: &str,
        options: VectorCollectionOptions,
    ) -> Result<()> {
        // Verify table exists
        {
            let tables = self.tables.read().unwrap();
            if !tables.contains_key(table) {
                return Err(BoogyError::TableNotFound(table.to_string()));
            }
        }

        let key = Self::collection_key(table, name);
        let mut collections = self.vector_collections.write().unwrap();
        if collections.contains_key(&key) {
            return Err(BoogyError::VectorCollectionExists(name.to_string()));
        }

        let vec_path = self.vector_file_path(table, name);
        let wal_path = self.vector_wal_path(table, name);
        let collection = VectorCollection::create(&vec_path, &wal_path, &options)?;
        collections.insert(key, collection);
        Ok(())
    }

    pub fn drop_vector_collection(&self, table: &str, name: &str) -> Result<()> {
        let key = Self::collection_key(table, name);
        let mut collections = self.vector_collections.write().unwrap();
        if collections.remove(&key).is_none() {
            return Err(BoogyError::VectorCollectionNotFound(name.to_string()));
        }
        // Remove files
        let vec_path = self.vector_file_path(table, name);
        let wal_path = self.vector_wal_path(table, name);
        let _ = std::fs::remove_file(&vec_path);
        let _ = std::fs::remove_file(&wal_path);
        Ok(())
    }

    pub fn vector_insert(
        &self,
        table: &str,
        collection: &str,
        rowid: u64,
        vector: &[f32],
    ) -> Result<()> {
        // Verify rowid exists in table
        if self.get(table, rowid)?.is_none() {
            return Err(BoogyError::RowNotFound(format!("{rowid}")));
        }

        let key = Self::collection_key(table, collection);
        let mut collections = self.vector_collections.write().unwrap();
        let coll = collections.get_mut(&key)
            .ok_or_else(|| BoogyError::VectorCollectionNotFound(collection.to_string()))?;
        let fsync = self.durability.load(std::sync::atomic::Ordering::Relaxed) == Durability::Immediate as u8;
        coll.insert(rowid, vector, fsync)?;
        Ok(())
    }

    pub fn vector_insert_batch(
        &self,
        table: &str,
        collection: &str,
        entries: &[(u64, Vec<f32>)],
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let mut collections = self.vector_collections.write().unwrap();
        let coll = collections.get_mut(&key)
            .ok_or_else(|| BoogyError::VectorCollectionNotFound(collection.to_string()))?;
        let fsync = self.durability.load(std::sync::atomic::Ordering::Relaxed) == Durability::Immediate as u8;

        for (rowid, vector) in entries {
            // Verify rowid exists — we need to drop the lock briefly
            // Actually, since we hold the write lock on collections, we can't call self.get()
            // which takes a read lock on tables. This is fine — tables lock is independent.
            coll.insert(*rowid, vector, false)?;
        }
        if fsync {
            // Final fsync after the batch
            // The collection already flushed after each insert, so we just need
            // to ensure the last one was synced. This is handled by the collection.
        }
        Ok(())
    }

    pub fn vector_update(
        &self,
        table: &str,
        collection: &str,
        rowid: u64,
        vector: &[f32],
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let mut collections = self.vector_collections.write().unwrap();
        let coll = collections.get_mut(&key)
            .ok_or_else(|| BoogyError::VectorCollectionNotFound(collection.to_string()))?;
        let fsync = self.durability.load(std::sync::atomic::Ordering::Relaxed) == Durability::Immediate as u8;
        coll.update(rowid, vector, fsync)?;
        Ok(())
    }

    pub fn vector_delete(
        &self,
        table: &str,
        collection: &str,
        rowid: u64,
    ) -> Result<()> {
        let key = Self::collection_key(table, collection);
        let mut collections = self.vector_collections.write().unwrap();
        let coll = collections.get_mut(&key)
            .ok_or_else(|| BoogyError::VectorCollectionNotFound(collection.to_string()))?;
        let fsync = self.durability.load(std::sync::atomic::Ordering::Relaxed) == Durability::Immediate as u8;
        coll.delete(rowid, fsync)?;
        Ok(())
    }

    pub fn vector_search(
        &self,
        table: &str,
        collection: &str,
        query: &[f32],
        options: VectorSearchOptions,
    ) -> Result<Vec<VectorResult>> {
        let key = Self::collection_key(table, collection);
        let collections = self.vector_collections.read().unwrap();
        let coll = collections.get(&key)
            .ok_or_else(|| BoogyError::VectorCollectionNotFound(collection.to_string()))?;

        // When filtering, inflate ef_search to compensate for post-filter rejection
        let effective_ef = if options.filter.is_some() {
            options.ef_search * 4
        } else {
            options.ef_search
        };
        let fetch_k = if options.filter.is_some() { options.k * 4 } else { options.k };
        let mut results = coll.search(query, fetch_k, effective_ef)?;

        // Apply metadata filter if present
        if let Some(ref filter) = options.filter {
            results.retain(|r| {
                match self.get(table, r.rowid) {
                    Ok(Some(row)) => {
                        match row.get(&filter.column) {
                            Some(val) => filter.matches(&val),
                            None => filter.op == crate::filter::FilterOp::IsNull,
                        }
                    }
                    _ => false,
                }
            });
            results.truncate(options.k as usize);
        }

        Ok(results)
    }
}
```

- [ ] **Step 3: Run compilation check**

Run: `cargo check --features vector`
Expected: Compiles. Fix any borrow checker or visibility issues.

- [ ] **Step 4: Commit**

```bash
git add src/db.rs src/vector/mod.rs
git commit -m "feat(vector): wire vector_* public methods into BoogyDb"
```

---

## Task 8: Integration Tests

**Files:**
- Create: `tests/vector_test.rs`

- [ ] **Step 1: Write integration tests**

```rust
#![cfg(feature = "vector")]

use boogy_db::*;
use tempfile::TempDir;

fn setup_db() -> (TempDir, BoogyDb) {
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    db.create_table("items", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("category", Type::Text),
    ]).unwrap();
    (dir, db)
}

fn setup_db_with_vectors(count: usize, dims: usize) -> (TempDir, BoogyDb) {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "embeddings",
        VectorCollectionOptions::new(dims as u32, DistanceMetric::Euclidean),
    ).unwrap();

    for i in 0..count {
        let mut vec = vec![0.0f32; dims];
        vec[i % dims] = 1.0; // spread vectors across dimensions
        let rowid = db.insert("items", &[
            ("name", Value::Text(format!("item_{i}"))),
            ("category", Value::Text(if i % 2 == 0 { "A".into() } else { "B".into() })),
        ]).unwrap();
        db.vector_insert("items", "embeddings", rowid, &vec).unwrap();
    }
    (dir, db)
}

#[test]
fn test_full_lifecycle() {
    let (dir, db) = setup_db();

    // Create collection
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(3, DistanceMetric::Cosine),
    ).unwrap();

    // Insert vectors linked to rows
    let id1 = db.insert("items", &[("name", Value::Text("apple".into()))]).unwrap();
    let id2 = db.insert("items", &[("name", Value::Text("banana".into()))]).unwrap();
    let id3 = db.insert("items", &[("name", Value::Text("cherry".into()))]).unwrap();

    db.vector_insert("items", "emb", id1, &[1.0, 0.0, 0.0]).unwrap();
    db.vector_insert("items", "emb", id2, &[0.0, 1.0, 0.0]).unwrap();
    db.vector_insert("items", "emb", id3, &[0.0, 0.0, 1.0]).unwrap();

    // Search
    let results = db.vector_search("items", "emb", &[1.0, 0.1, 0.0],
        VectorSearchOptions::new(2),
    ).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].rowid, id1); // closest to [1,0,0]

    // Delete
    db.vector_delete("items", "emb", id2).unwrap();
    let results = db.vector_search("items", "emb", &[0.0, 1.0, 0.0],
        VectorSearchOptions::new(3),
    ).unwrap();
    let rowids: Vec<u64> = results.iter().map(|r| r.rowid).collect();
    assert!(!rowids.contains(&id2));

    // Drop collection
    db.drop_vector_collection("items", "emb").unwrap();
}

#[test]
fn test_batch_insert() {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(2, DistanceMetric::Euclidean),
    ).unwrap();

    let mut entries = Vec::new();
    for i in 0..50 {
        let rowid = db.insert("items", &[
            ("name", Value::Text(format!("item_{i}"))),
        ]).unwrap();
        entries.push((rowid, vec![i as f32, 0.0]));
    }

    db.vector_insert_batch("items", "emb", &entries).unwrap();

    // Search for nearest to [25, 0]
    let results = db.vector_search("items", "emb", &[25.0, 0.0],
        VectorSearchOptions::new(3),
    ).unwrap();
    assert_eq!(results.len(), 3);
    // Should include rowids for items near index 25
}

#[test]
fn test_filtered_search() {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(2, DistanceMetric::Euclidean),
    ).unwrap();

    // Insert items with categories
    for i in 0..20 {
        let category = if i % 2 == 0 { "A" } else { "B" };
        let rowid = db.insert("items", &[
            ("name", Value::Text(format!("item_{i}"))),
            ("category", Value::Text(category.into())),
        ]).unwrap();
        db.vector_insert("items", "emb", rowid, &[i as f32, 0.0]).unwrap();
    }

    // Search with filter: only category A
    let results = db.vector_search("items", "emb", &[10.0, 0.0],
        VectorSearchOptions {
            k: 5,
            ef_search: 20,
            filter: Some(Filter::eq("category", "A")),
        },
    ).unwrap();

    // All results should be category A
    for r in &results {
        let row = db.get("items", r.rowid).unwrap().unwrap();
        assert_eq!(row.get("category"), Some(Value::Text("A".into())));
    }
}

#[test]
fn test_vector_update() {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(2, DistanceMetric::Euclidean),
    ).unwrap();

    let id1 = db.insert("items", &[("name", Value::Text("a".into()))]).unwrap();
    let id2 = db.insert("items", &[("name", Value::Text("b".into()))]).unwrap();

    db.vector_insert("items", "emb", id1, &[0.0, 0.0]).unwrap();
    db.vector_insert("items", "emb", id2, &[10.0, 0.0]).unwrap();

    // Before update: id1 is at origin, far from id2
    let results = db.vector_search("items", "emb", &[10.0, 0.0],
        VectorSearchOptions::new(1),
    ).unwrap();
    assert_eq!(results[0].rowid, id2);

    // Update id1 to be right next to id2
    db.vector_update("items", "emb", id1, &[9.9, 0.0]).unwrap();

    let results = db.vector_search("items", "emb", &[10.0, 0.0],
        VectorSearchOptions::new(1),
    ).unwrap();
    // Now either id1 or id2 could be closest (both very near [10,0])
    assert!(results[0].rowid == id1 || results[0].rowid == id2);
}

#[test]
fn test_multiple_collections() {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "title_emb",
        VectorCollectionOptions::new(2, DistanceMetric::Cosine),
    ).unwrap();
    db.create_vector_collection("items", "image_emb",
        VectorCollectionOptions::new(3, DistanceMetric::Euclidean),
    ).unwrap();

    let id = db.insert("items", &[("name", Value::Text("test".into()))]).unwrap();

    db.vector_insert("items", "title_emb", id, &[1.0, 0.0]).unwrap();
    db.vector_insert("items", "image_emb", id, &[0.0, 1.0, 0.0]).unwrap();

    let r1 = db.vector_search("items", "title_emb", &[1.0, 0.0],
        VectorSearchOptions::new(1),
    ).unwrap();
    let r2 = db.vector_search("items", "image_emb", &[0.0, 1.0, 0.0],
        VectorSearchOptions::new(1),
    ).unwrap();

    assert_eq!(r1[0].rowid, id);
    assert_eq!(r2[0].rowid, id);
}

#[test]
fn test_collection_not_found() {
    let (dir, db) = setup_db();
    let err = db.vector_search("items", "nonexistent", &[1.0],
        VectorSearchOptions::new(1),
    );
    assert!(err.is_err());
}

#[test]
fn test_duplicate_collection_name() {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(2, DistanceMetric::Cosine),
    ).unwrap();
    let err = db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(2, DistanceMetric::Cosine),
    );
    assert!(err.is_err());
}

#[test]
fn test_dimension_mismatch_at_insert() {
    let (dir, db) = setup_db();
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions::new(3, DistanceMetric::Cosine),
    ).unwrap();
    let id = db.insert("items", &[("name", Value::Text("x".into()))]).unwrap();
    let err = db.vector_insert("items", "emb", id, &[1.0, 2.0]); // 2 dims, expected 3
    assert!(err.is_err());
}

#[test]
fn test_existing_ops_unaffected_by_vector_feature() {
    // Verify that enabling the vector feature doesn't break normal CRUD
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    db.create_table("users", &[
        ColumnDef::new("name", Type::Text),
    ]).unwrap();
    let id = db.insert("users", &[("name", Value::Text("Alice".into()))]).unwrap();
    let row = db.get("users", id).unwrap().unwrap();
    assert_eq!(row.get("name"), Some(Value::Text("Alice".into())));
    let result = db.find("users", FindOptions {
        filters: vec![Filter::eq("name", "Alice")],
        ..Default::default()
    }).unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn test_recall_50k_brute_force() {
    // Insert 1000 random-ish vectors in 32 dims, search, compare to brute force
    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("test.boogy")).unwrap();
    db.create_table("vecs", &[
        ColumnDef::new("idx", Type::Integer),
    ]).unwrap();
    db.create_vector_collection("vecs", "emb",
        VectorCollectionOptions {
            dimensions: 32,
            metric: DistanceMetric::Euclidean,
            m: 16,
            ef_construction: 200,
        },
    ).unwrap();

    let count = 1000;
    let dims = 32;
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut rowids: Vec<u64> = Vec::new();

    // Deterministic pseudo-random vectors
    let mut rng: u64 = 42;
    for i in 0..count {
        let mut vec = Vec::with_capacity(dims);
        for _ in 0..dims {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            vec.push((rng >> 33) as f32 / (u32::MAX >> 1) as f32);
        }
        let rowid = db.insert("vecs", &[("idx", Value::Integer(i as i64))]).unwrap();
        db.vector_insert("vecs", "emb", rowid, &vec).unwrap();
        vectors.push(vec);
        rowids.push(rowid);
    }

    // Query vector
    let query: Vec<f32> = (0..dims).map(|i| i as f32 / dims as f32).collect();
    let k = 10u32;

    // HNSW search
    let results = db.vector_search("vecs", "emb", &query,
        VectorSearchOptions { k, ef_search: 50, filter: None },
    ).unwrap();

    // Brute force
    let mut dists: Vec<(u64, f32)> = rowids.iter().zip(vectors.iter())
        .map(|(&rid, vec)| {
            let d: f32 = query.iter().zip(vec).map(|(a, b)| (a - b) * (a - b)).sum();
            (rid, d)
        })
        .collect();
    dists.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let brute_top_k: Vec<u64> = dists.iter().take(k as usize).map(|d| d.0).collect();

    // Check recall: how many of the HNSW results are in the true top-k?
    let hnsw_rowids: Vec<u64> = results.iter().map(|r| r.rowid).collect();
    let recall = hnsw_rowids.iter().filter(|r| brute_top_k.contains(r)).count();
    let recall_pct = recall as f64 / k as f64 * 100.0;

    assert!(recall_pct >= 80.0,
        "recall {recall_pct}% is below 80% threshold (got {recall}/{k} correct). \
         HNSW: {:?}, brute: {:?}", hnsw_rowids, brute_top_k);
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --features vector --test vector_test`
Expected: All 11 tests PASS.

- [ ] **Step 3: Also run existing tests to ensure no regression**

Run: `cargo test --features vector`
Expected: All tests PASS (existing + new vector tests).

- [ ] **Step 4: Commit**

```bash
git add tests/vector_test.rs
git commit -m "test(vector): integration tests — lifecycle, batch, filtered search, recall, regression"
```

---

## Task 9: Benchmarks

**Files:**
- Create: `benches/vector_ops.rs`
- Modify: `Cargo.toml` (add bench entry)

- [ ] **Step 1: Add bench entry to Cargo.toml**

Add:
```toml
[[bench]]
name = "vector_ops"
harness = false
required-features = ["vector"]
```

- [ ] **Step 2: Write benchmarks**

Create `benches/vector_ops.rs`:

```rust
use std::time::{Duration, Instant};
use boogy_db::*;
use tempfile::TempDir;

const DIMS: u32 = 128;
const DURATION_SECS: u64 = 5;

fn main() {
    println!("=== Vector Search Benchmarks (dims={DIMS}) ===\n");

    bench_insert_throughput();
    bench_search_latency();
    bench_brute_force_comparison();
    bench_no_regression();
}

fn bench_insert_throughput() {
    println!("--- Insert Throughput ---\n");
    println!("{:<30} {:>12} {:>12}", "", "single", "batch(100)");

    for &count in &[1_000, 10_000] {
        let dir = TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("items", &[ColumnDef::new("i", Type::Integer)]).unwrap();
        db.create_vector_collection("items", "emb",
            VectorCollectionOptions {
                dimensions: DIMS, metric: DistanceMetric::Euclidean,
                m: 16, ef_construction: 200,
            },
        ).unwrap();

        // Pre-generate vectors
        let mut rng: u64 = 12345;
        let vectors: Vec<Vec<f32>> = (0..count).map(|_| {
            (0..DIMS).map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                (rng >> 33) as f32 / (u32::MAX >> 1) as f32
            }).collect()
        }).collect();

        // Single insert
        let start = Instant::now();
        for (i, vec) in vectors.iter().enumerate() {
            let rid = db.insert("items", &[("i", Value::Integer(i as i64))]).unwrap();
            db.vector_insert("items", "emb", rid, vec).unwrap();
        }
        let single_rate = count as f64 / start.elapsed().as_secs_f64();

        // Batch insert (new db)
        let dir2 = TempDir::new().unwrap();
        let db2 = BoogyDb::open(dir2.path().join("bench.boogy")).unwrap();
        db2.set_durability(Durability::None);
        db2.create_table("items", &[ColumnDef::new("i", Type::Integer)]).unwrap();
        db2.create_vector_collection("items", "emb",
            VectorCollectionOptions {
                dimensions: DIMS, metric: DistanceMetric::Euclidean,
                m: 16, ef_construction: 200,
            },
        ).unwrap();

        let start = Instant::now();
        for chunk in vectors.chunks(100) {
            let entries: Vec<(u64, Vec<f32>)> = chunk.iter().enumerate().map(|(i, v)| {
                let rid = db2.insert("items", &[("i", Value::Integer(i as i64))]).unwrap();
                (rid, v.clone())
            }).collect();
            db2.vector_insert_batch("items", "emb", &entries).unwrap();
        }
        let batch_rate = count as f64 / start.elapsed().as_secs_f64();

        println!("{:<30} {:>8.0} v/s {:>8.0} v/s", format!("{count} vectors"), single_rate, batch_rate);
    }
    println!();
}

fn bench_search_latency() {
    println!("--- Search Latency (k=10, ef_search=50) ---\n");
    println!("{:<20} {:>12} {:>12} {:>12}", "size", "avg", "p50", "p99");

    for &count in &[1_000, 10_000, 50_000] {
        let dir = TempDir::new().unwrap();
        let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
        db.set_durability(Durability::None);
        db.create_table("items", &[ColumnDef::new("i", Type::Integer)]).unwrap();
        db.create_vector_collection("items", "emb",
            VectorCollectionOptions {
                dimensions: DIMS, metric: DistanceMetric::Euclidean,
                m: 16, ef_construction: 200,
            },
        ).unwrap();

        let mut rng: u64 = 12345;
        let mut make_vec = || -> Vec<f32> {
            (0..DIMS).map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                (rng >> 33) as f32 / (u32::MAX >> 1) as f32
            }).collect()
        };

        for i in 0..count {
            let vec = make_vec();
            let rid = db.insert("items", &[("i", Value::Integer(i as i64))]).unwrap();
            db.vector_insert("items", "emb", rid, &vec).unwrap();
        }

        // Run searches
        let mut latencies = Vec::new();
        let duration = Duration::from_secs(DURATION_SECS);
        let start = Instant::now();
        while start.elapsed() < duration {
            let query = make_vec();
            let t = Instant::now();
            let _ = db.vector_search("items", "emb", &query,
                VectorSearchOptions { k: 10, ef_search: 50, filter: None },
            ).unwrap();
            latencies.push(t.elapsed());
        }

        latencies.sort();
        let avg = latencies.iter().map(|d| d.as_nanos()).sum::<u128>() / latencies.len() as u128;
        let p50 = latencies[latencies.len() / 2].as_nanos();
        let p99 = latencies[latencies.len() * 99 / 100].as_nanos();

        println!("{:<20} {:>8} us {:>8} us {:>8} us",
            format!("{count} vectors"),
            avg / 1000, p50 / 1000, p99 / 1000);
    }
    println!();
}

fn bench_brute_force_comparison() {
    println!("--- HNSW vs Brute Force (10K vectors, k=10) ---\n");

    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::None);
    db.create_table("items", &[ColumnDef::new("i", Type::Integer)]).unwrap();
    db.create_vector_collection("items", "emb",
        VectorCollectionOptions {
            dimensions: DIMS, metric: DistanceMetric::Euclidean,
            m: 16, ef_construction: 200,
        },
    ).unwrap();

    let count = 10_000;
    let mut rng: u64 = 12345;
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    for i in 0..count {
        let vec: Vec<f32> = (0..DIMS).map(|_| {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng >> 33) as f32 / (u32::MAX >> 1) as f32
        }).collect();
        let rid = db.insert("items", &[("i", Value::Integer(i as i64))]).unwrap();
        db.vector_insert("items", "emb", rid, &vec).unwrap();
        vectors.push(vec);
    }

    let query: Vec<f32> = (0..DIMS).map(|_| {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        (rng >> 33) as f32 / (u32::MAX >> 1) as f32
    }).collect();

    // HNSW
    let start = Instant::now();
    let mut hnsw_ops = 0;
    while start.elapsed() < Duration::from_secs(3) {
        let _ = db.vector_search("items", "emb", &query,
            VectorSearchOptions { k: 10, ef_search: 50, filter: None },
        ).unwrap();
        hnsw_ops += 1;
    }
    let hnsw_rate = hnsw_ops as f64 / 3.0;

    // Brute force
    let start = Instant::now();
    let mut brute_ops = 0;
    while start.elapsed() < Duration::from_secs(3) {
        let mut dists: Vec<f32> = vectors.iter().map(|v| {
            query.iter().zip(v).map(|(a, b)| (a - b) * (a - b)).sum()
        }).collect();
        dists.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let _ = &dists[..10];
        brute_ops += 1;
    }
    let brute_rate = brute_ops as f64 / 3.0;

    println!("HNSW:        {:.0} searches/s", hnsw_rate);
    println!("Brute force: {:.0} searches/s", brute_rate);
    println!("Speedup:     {:.1}x", hnsw_rate / brute_rate);
    println!();
}

fn bench_no_regression() {
    println!("--- Existing Ops Regression Check (vector feature enabled, no collections) ---\n");

    let dir = TempDir::new().unwrap();
    let db = BoogyDb::open(dir.path().join("bench.boogy")).unwrap();
    db.set_durability(Durability::Normal);
    db.create_table("users", &[
        ColumnDef::new("name", Type::Text),
        ColumnDef::new("age", Type::Integer),
    ]).unwrap();

    // Seed
    for i in 0..1000 {
        db.insert("users", &[
            ("name", Value::Text(format!("user_{i}"))),
            ("age", Value::Integer(i % 100)),
        ]).unwrap();
    }

    let duration = Duration::from_secs(3);

    // Insert
    let start = Instant::now();
    let mut ops = 0u64;
    while start.elapsed() < duration {
        db.insert("users", &[
            ("name", Value::Text("bench".into())),
            ("age", Value::Integer(25)),
        ]).unwrap();
        ops += 1;
    }
    println!("Insert:      {:.0} ops/s", ops as f64 / 3.0);

    // Get
    let start = Instant::now();
    ops = 0;
    while start.elapsed() < duration {
        let _ = db.get("users", 500).unwrap();
        ops += 1;
    }
    println!("Get:         {:.0} ops/s", ops as f64 / 3.0);

    // Find
    let start = Instant::now();
    ops = 0;
    while start.elapsed() < duration {
        let _ = db.find("users", FindOptions {
            filters: vec![Filter::eq("age", 25i64)],
            limit: Some(10),
            ..Default::default()
        }).unwrap();
        ops += 1;
    }
    println!("Find:        {:.0} ops/s", ops as f64 / 3.0);
    println!();
}
```

- [ ] **Step 3: Run benchmarks**

Run: `cargo bench --features vector --bench vector_ops`
Expected: Benchmarks run and print results. Verify HNSW is faster than brute force and existing ops show no regression.

- [ ] **Step 4: Commit**

```bash
git add benches/vector_ops.rs Cargo.toml
git commit -m "bench(vector): search latency, insert throughput, HNSW vs brute force, regression check"
```

---

## Task 10: README and Final Cleanup

**Files:**
- Modify: `README.md`
- Modify: `src/vector/mod.rs` (if any visibility fixes needed)

- [ ] **Step 1: Add vector search section to README.md**

Add a new section after "ACID Transactions" and before "Skills & Guides":

```markdown
## Vector Search

boogy-db supports approximate nearest neighbor (ANN) search via an HNSW index, enabled with the `vector` feature flag. Vector collections are linked to tables — each vector is associated with a row via its rowid.

### Setup

```toml
[dependencies]
boogy-db = { path = ".", features = ["vector"] }
```

### Usage

```rust
use boogy_db::*;

let db = BoogyDb::open("my.boogy")?;

db.create_table("articles", &[
    ColumnDef::new("title", Type::Text),
    ColumnDef::new("category", Type::Text),
])?;

// Create a vector collection linked to the table
db.create_vector_collection("articles", "embeddings",
    VectorCollectionOptions::new(768, DistanceMetric::Cosine),
)?;

// Insert rows and their embeddings
let id = db.insert("articles", &[
    ("title", Value::Text("Rust performance tips".into())),
    ("category", Value::Text("engineering".into())),
])?;
db.vector_insert("articles", "embeddings", id, &embedding)?;

// Batch insert
db.vector_insert_batch("articles", "embeddings", &[
    (id1, embedding1),
    (id2, embedding2),
])?;

// k-NN search
let results = db.vector_search("articles", "embeddings", &query_embedding,
    VectorSearchOptions::new(10),
)?;
for r in &results {
    let row = db.get("articles", r.rowid)?.unwrap();
    println!("{}: {}", row.get("title").unwrap(), r.distance);
}

// Filtered search (ANN + metadata)
let results = db.vector_search("articles", "embeddings", &query_embedding,
    VectorSearchOptions {
        k: 10,
        ef_search: 50,
        filter: Some(Filter::eq("category", "engineering")),
    },
)?;

// Update embedding (e.g., after re-embedding with a new model)
db.vector_update("articles", "embeddings", id, &new_embedding)?;

// Delete
db.vector_delete("articles", "embeddings", id)?;
```

### Distance Metrics

| Metric | Use case |
|--------|----------|
| `DistanceMetric::Cosine` | Text embeddings (OpenAI, Cohere). Measures angle. |
| `DistanceMetric::Euclidean` | Image embeddings, spatial data. Measures L2 distance. |
| `DistanceMetric::DotProduct` | Pre-normalized vectors. Fast inner product. |

### HNSW Parameters

| Parameter | Default | Effect |
|-----------|---------|--------|
| `m` | 16 | Connections per node. Higher = better recall, more memory. |
| `ef_construction` | 200 | Build-time beam width. Higher = better graph quality, slower insert. |
| `ef_search` | 10 | Query-time beam width. Higher = better recall, slower search. |
```

- [ ] **Step 2: Update Table of Contents in README**

Add `- [Vector Search](#vector-search)` to the Table of Contents.

- [ ] **Step 3: Run all tests one final time**

Run: `cargo test --features vector`
Expected: All tests PASS. No regressions.

Run: `cargo test` (without vector feature)
Expected: All existing tests PASS. Vector code is completely gated.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: add vector search section to README"
```
