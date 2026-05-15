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
