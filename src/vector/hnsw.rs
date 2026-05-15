use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

// --- Heap wrappers ---

/// Min-heap candidate: closest-first ordering for BinaryHeap (which is max-heap).
/// We reverse the comparison so BinaryHeap pops the *closest* node first.
#[derive(Clone, Copy)]
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
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: smaller distance = higher priority in BinaryHeap
        other
            .distance
            .partial_cmp(&self.distance)
            .unwrap_or(Ordering::Equal)
            .then(other.node_id.cmp(&self.node_id))
    }
}

/// Max-heap candidate: furthest-first ordering. BinaryHeap pops the *furthest* node.
/// Used for result eviction — when we exceed ef, we evict the worst (furthest) result.
#[derive(Clone, Copy)]
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
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FarCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Natural: larger distance = higher priority in BinaryHeap
        self.distance
            .partial_cmp(&other.distance)
            .unwrap_or(Ordering::Equal)
            .then(self.node_id.cmp(&other.node_id))
    }
}

// --- Result types ---

pub struct InsertResult {
    pub node_id: u32,
    pub level: u32,
    /// Each entry: (node_id, layer, new_neighbor_list) describing the full neighbor
    /// list for that node at that layer after the insert.
    pub connections: Vec<(u32, u32, Vec<u32>)>,
    pub new_entry_point: Option<u32>,
    pub new_max_layer: Option<u32>,
}

pub struct SearchResult {
    /// (node_id, distance), sorted ascending by distance.
    pub neighbors: Vec<(u32, f32)>,
}

// --- Public functions ---

/// Assign a random layer for a new node.
///
/// Formula: floor(-ln(rng_value) * m_l) where m_l = 1/ln(M).
/// rng_value must be in (0, 1).
pub fn assign_layer(m: u32, rng_value: f64) -> u32 {
    let m_l = 1.0 / (m as f64).ln();
    let layer = (-rng_value.ln() * m_l).floor() as u32;
    layer
}

/// Greedy descent through layers above target_layer to find the closest node.
///
/// At each layer, repeatedly pick the closest non-deleted neighbor until no improvement.
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

    for layer in (target_layer..=current_max_layer).rev() {
        if layer <= target_layer {
            break;
        }
        // Greedy walk at this layer
        let mut improved = true;
        while improved {
            improved = false;
            let neighbors = read_neighbors(current, layer);
            for &neighbor in &neighbors {
                if is_deleted(neighbor) {
                    continue;
                }
                let d = distance_fn(query, &read_vector(neighbor));
                if d < current_dist {
                    current = neighbor;
                    current_dist = d;
                    improved = true;
                }
            }
        }
    }

    current
}

/// Beam search at a single layer with beam width ef.
///
/// Uses a min-heap for candidates (closest first to expand) and a max-heap for results
/// (furthest first to evict when exceeding ef). Deleted nodes are traversed through
/// for connectivity but excluded from final results.
///
/// Returns results sorted by distance ascending.
pub fn search_layer(
    entry_points: &[u32],
    query: &[f32],
    ef: u32,
    layer: u32,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    read_neighbors: &dyn Fn(u32, u32) -> Vec<u32>,
    is_deleted: &dyn Fn(u32) -> bool,
    is_allowed: &dyn Fn(u32) -> bool,
) -> Vec<(u32, f32)> {
    let mut visited = HashSet::new();
    let mut candidates: BinaryHeap<Candidate> = BinaryHeap::new();
    let mut results: BinaryHeap<FarCandidate> = BinaryHeap::new();

    for &ep in entry_points {
        if visited.insert(ep) {
            let d = distance_fn(query, &read_vector(ep));
            candidates.push(Candidate { distance: d, node_id: ep });
            if !is_deleted(ep) && is_allowed(ep) {
                results.push(FarCandidate { distance: d, node_id: ep });
            }
        }
    }

    while let Some(closest) = candidates.pop() {
        // If the closest candidate is further than our worst result, stop
        let worst_dist = results.peek().map(|r| r.distance).unwrap_or(f32::MAX);
        if closest.distance > worst_dist && results.len() >= ef as usize {
            break;
        }

        let neighbors = read_neighbors(closest.node_id, layer);
        for &neighbor in &neighbors {
            if !visited.insert(neighbor) {
                continue;
            }
            let d = distance_fn(query, &read_vector(neighbor));

            let worst_dist = results.peek().map(|r| r.distance).unwrap_or(f32::MAX);
            if d < worst_dist || results.len() < ef as usize {
                candidates.push(Candidate { distance: d, node_id: neighbor });
                if !is_deleted(neighbor) && is_allowed(neighbor) {
                    results.push(FarCandidate { distance: d, node_id: neighbor });
                    // Evict furthest if we exceed ef
                    if results.len() > ef as usize {
                        results.pop();
                    }
                }
            }
        }
    }

    let mut out: Vec<(u32, f32)> = results
        .into_iter()
        .map(|r| (r.node_id, r.distance))
        .collect();
    out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    out
}

/// Simple neighbor selection: take the M closest from sorted candidates.
pub fn select_neighbors(candidates: &[(u32, f32)], m: usize) -> Vec<u32> {
    let mut sorted: Vec<(u32, f32)> = candidates.to_vec();
    sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    sorted.iter().take(m).map(|&(id, _)| id).collect()
}

/// Prune a neighbor list: filter deleted, compute distances from node_id, keep closest max_neighbors.
pub fn prune_neighbors(
    node_id: u32,
    current_neighbors: &[u32],
    max_neighbors: usize,
    distance_fn: &dyn Fn(&[f32], &[f32]) -> f32,
    read_vector: &dyn Fn(u32) -> Vec<f32>,
    is_deleted: &dyn Fn(u32) -> bool,
) -> Vec<u32> {
    let node_vec = read_vector(node_id);
    let mut scored: Vec<(u32, f32)> = current_neighbors
        .iter()
        .filter(|&&n| !is_deleted(n))
        .map(|&n| {
            let d = distance_fn(&node_vec, &read_vector(n));
            (n, d)
        })
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    scored.iter().take(max_neighbors).map(|&(id, _)| id).collect()
}

/// Full k-NN search: descend upper layers, beam search layer 0, return top k.
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
    is_allowed: &dyn Fn(u32) -> bool,
) -> SearchResult {
    // Descend upper layers to find a good entry for layer 0
    let ep = if max_layer > 0 {
        search_upper_layers(
            entry_point,
            query,
            max_layer,
            0,
            distance_fn,
            read_vector,
            read_neighbors,
            is_deleted,
        )
    } else {
        entry_point
    };

    // Beam search at layer 0
    let ef = ef_search.max(k);
    let mut results = search_layer(
        &[ep],
        query,
        ef,
        0,
        distance_fn,
        read_vector,
        read_neighbors,
        is_deleted,
        is_allowed,
    );

    results.truncate(k as usize);
    SearchResult { neighbors: results }
}

/// Full HNSW insert per Malkov & Yashunin (2018).
///
/// Pure algorithm: returns an InsertResult describing all mutations (connections to apply).
/// The caller is responsible for persisting these mutations.
pub fn insert(
    node_id: u32,
    _vector: &[f32],
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
    let mut connections: Vec<(u32, u32, Vec<u32>)> = Vec::new();

    // First node: no connections needed, just set it as entry point
    let ep = match entry_point {
        None => {
            // Record this node with empty neighbor lists for each layer
            for l in 0..=level {
                connections.push((node_id, l, Vec::new()));
            }
            return InsertResult {
                node_id,
                level,
                connections,
                new_entry_point: Some(node_id),
                new_max_layer: Some(level),
            };
        }
        Some(ep) => ep,
    };

    let query = read_vector(node_id);

    // Descend through upper layers (above this node's level) to find nearest entry
    let mut current_ep = ep;
    if current_max_layer > level {
        current_ep = search_upper_layers(
            ep,
            &query,
            current_max_layer,
            level,
            distance_fn,
            read_vector,
            read_neighbors,
            is_deleted,
        );
    }

    // At each layer from min(level, current_max_layer) down to 0: find neighbors and connect
    let top_layer = level.min(current_max_layer);
    for layer in (0..=top_layer).rev() {
        let max_neighbors = if layer == 0 { 2 * m } else { m } as usize;

        // Search for nearest neighbors at this layer
        let candidates = search_layer(
            &[current_ep],
            &query,
            ef_construction,
            layer,
            distance_fn,
            read_vector,
            read_neighbors,
            is_deleted,
            &|_| true,
        );

        // Select M closest as neighbors for the new node
        let selected = select_neighbors(&candidates, max_neighbors.min(m as usize));

        // Record the new node's neighbor list at this layer
        connections.push((node_id, layer, selected.clone()));

        // Bidirectional connections: add node_id as neighbor of each selected node
        for &neighbor in &selected {
            let mut neighbor_list = read_neighbors(neighbor, layer);
            if !neighbor_list.contains(&node_id) {
                neighbor_list.push(node_id);
            }

            // Prune if exceeding max neighbors for this layer
            if neighbor_list.len() > max_neighbors {
                neighbor_list = prune_neighbors(
                    neighbor,
                    &neighbor_list,
                    max_neighbors,
                    distance_fn,
                    read_vector,
                    is_deleted,
                );
            }

            connections.push((neighbor, layer, neighbor_list));
        }

        // Update entry point for next layer down: use the closest candidate found
        if let Some(&(closest_id, _)) = candidates.first() {
            current_ep = closest_id;
        }
    }

    // If this node's level exceeds the current max, it becomes the new entry point
    let new_entry_point = if level > current_max_layer {
        Some(node_id)
    } else {
        None
    };
    let new_max_layer = if level > current_max_layer {
        Some(level)
    } else {
        None
    };

    InsertResult {
        node_id,
        level,
        connections,
        new_entry_point,
        new_max_layer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn euclidean(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }

    struct TestGraph {
        vectors: Vec<Vec<f32>>,
        /// neighbors[node][layer] -> neighbor ids
        neighbors: Vec<Vec<Vec<u32>>>,
        deleted: Vec<bool>,
    }

    impl TestGraph {
        fn new() -> Self {
            TestGraph {
                vectors: Vec::new(),
                neighbors: Vec::new(),
                deleted: Vec::new(),
            }
        }

        fn add_node(&mut self, vector: Vec<f32>, num_layers: u32) -> u32 {
            let id = self.vectors.len() as u32;
            self.vectors.push(vector);
            let mut layers = Vec::new();
            for _ in 0..=num_layers {
                layers.push(Vec::new());
            }
            self.neighbors.push(layers);
            self.deleted.push(false);
            id
        }

        fn set_neighbors(&mut self, node: u32, layer: u32, neighbors: Vec<u32>) {
            let n = node as usize;
            let l = layer as usize;
            // Grow layer vec if needed
            while self.neighbors[n].len() <= l {
                self.neighbors[n].push(Vec::new());
            }
            self.neighbors[n][l] = neighbors;
        }

        fn apply_insert(&mut self, result: &InsertResult) {
            for &(node_id, layer, ref neighbor_list) in &result.connections {
                let n = node_id as usize;
                let l = layer as usize;
                while self.neighbors[n].len() <= l {
                    self.neighbors[n].push(Vec::new());
                }
                self.neighbors[n][l] = neighbor_list.clone();
            }
        }

        fn read_vector(&self, id: u32) -> Vec<f32> {
            self.vectors[id as usize].clone()
        }

        fn read_neighbors(&self, id: u32, layer: u32) -> Vec<u32> {
            let n = id as usize;
            let l = layer as usize;
            if n < self.neighbors.len() && l < self.neighbors[n].len() {
                self.neighbors[n][l].clone()
            } else {
                Vec::new()
            }
        }

        fn is_deleted(&self, id: u32) -> bool {
            self.deleted.get(id as usize).copied().unwrap_or(false)
        }
    }

    #[test]
    fn test_assign_layer() {
        // rng 0.99 -> close to 0, should give layer 0
        let layer = assign_layer(16, 0.99);
        assert_eq!(layer, 0, "rng=0.99 should produce layer 0");

        // Very small rng -> high layer
        let layer = assign_layer(16, 0.001);
        assert!(layer >= 2, "rng=0.001 should produce a high layer, got {layer}");

        // Check formula: m=16, m_l = 1/ln(16) ~ 0.3607
        // rng=0.001 -> -ln(0.001) ~ 6.9078 -> 6.9078 * 0.3607 ~ 2.49 -> floor = 2
        let layer = assign_layer(16, 0.001);
        assert_eq!(layer, 2, "rng=0.001 with m=16 should give layer 2");
    }

    #[test]
    fn test_select_neighbors() {
        let candidates = vec![
            (0, 5.0),
            (1, 1.0),
            (2, 3.0),
            (3, 0.5),
            (4, 2.0),
        ];
        let selected = select_neighbors(&candidates, 3);
        // Should pick the 3 closest: node 3 (0.5), node 1 (1.0), node 4 (2.0)
        assert_eq!(selected, vec![3, 1, 4]);
    }

    #[test]
    fn test_search_layer_basic() {
        // 5 nodes on a line: [0,0], [1,0], [2,0], [3,0], [4,0]
        // Connected as a chain: 0-1-2-3-4
        let mut graph = TestGraph::new();
        for i in 0..5 {
            graph.add_node(vec![i as f32, 0.0], 0);
        }
        graph.set_neighbors(0, 0, vec![1]);
        graph.set_neighbors(1, 0, vec![0, 2]);
        graph.set_neighbors(2, 0, vec![1, 3]);
        graph.set_neighbors(3, 0, vec![2, 4]);
        graph.set_neighbors(4, 0, vec![3]);

        let query = vec![2.5, 0.0];
        let results = search_layer(
            &[0],
            &query,
            10,
            0,
            &euclidean,
            &|id| graph.read_vector(id),
            &|id, layer| graph.read_neighbors(id, layer),
            &|id| graph.is_deleted(id),
            &|_| true,
        );

        // Should find nodes 2 and 3 as closest (distance 0.25 each)
        assert!(results.len() >= 2);
        let ids: Vec<u32> = results.iter().map(|r| r.0).collect();
        assert!(ids.contains(&2), "results should contain node 2: {ids:?}");
        assert!(ids.contains(&3), "results should contain node 3: {ids:?}");
        // Verify sorted by distance
        for w in results.windows(2) {
            assert!(w[0].1 <= w[1].1, "results not sorted: {:?}", results);
        }
    }

    #[test]
    fn test_insert_first_node() {
        let graph = TestGraph::new();
        let result = insert(
            0,
            &[1.0, 2.0],
            0,
            None, // no entry point yet
            0,
            16,
            200,
            &euclidean,
            &|id| graph.read_vector(id),
            &|id, layer| graph.read_neighbors(id, layer),
            &|id| graph.is_deleted(id),
        );
        assert_eq!(result.new_entry_point, Some(0));
        assert_eq!(result.node_id, 0);
    }

    #[test]
    fn test_insert_and_search_small_graph() {
        let mut graph = TestGraph::new();
        let m = 4u32;
        let ef_construction = 16u32;

        // Pre-add all 20 node vectors (on a line: [0,0], [1,0], ..., [19,0])
        for i in 0..20 {
            graph.add_node(vec![i as f32, 0.0], 0);
        }

        let mut entry_point: Option<u32> = None;
        let mut max_layer = 0u32;

        for i in 0..20u32 {
            let result = insert(
                i,
                &graph.vectors[i as usize].clone(),
                0, // all at layer 0 for simplicity
                entry_point,
                max_layer,
                m,
                ef_construction,
                &euclidean,
                &|id| graph.read_vector(id),
                &|id, layer| graph.read_neighbors(id, layer),
                &|id| graph.is_deleted(id),
            );
            graph.apply_insert(&result);
            if let Some(ep) = result.new_entry_point {
                entry_point = Some(ep);
            }
            if let Some(ml) = result.new_max_layer {
                max_layer = ml;
            }
            // Ensure entry_point is set after first insert
            if entry_point.is_none() {
                entry_point = Some(i);
            }
        }

        // Search for [10.5, 0]
        let query = vec![10.5, 0.0];
        let result = search(
            &query,
            5,
            16,
            entry_point.unwrap(),
            max_layer,
            &euclidean,
            &|id| graph.read_vector(id),
            &|id, layer| graph.read_neighbors(id, layer),
            &|id| graph.is_deleted(id),
            &|_| true,
        );

        let ids: Vec<u32> = result.neighbors.iter().map(|r| r.0).collect();
        assert!(
            ids.contains(&10),
            "search near [10.5, 0] should find node 10, got: {ids:?}"
        );
        assert!(
            ids.contains(&11),
            "search near [10.5, 0] should find node 11, got: {ids:?}"
        );
    }

    #[test]
    fn test_deleted_nodes_skipped() {
        let mut graph = TestGraph::new();
        let m = 4u32;
        let ef_construction = 16u32;

        // 10 nodes on a line
        for i in 0..10 {
            graph.add_node(vec![i as f32, 0.0], 0);
        }

        let mut entry_point: Option<u32> = None;
        let mut max_layer = 0u32;

        for i in 0..10u32 {
            let result = insert(
                i,
                &graph.vectors[i as usize].clone(),
                0,
                entry_point,
                max_layer,
                m,
                ef_construction,
                &euclidean,
                &|id| graph.read_vector(id),
                &|id, layer| graph.read_neighbors(id, layer),
                &|id| graph.is_deleted(id),
            );
            graph.apply_insert(&result);
            if let Some(ep) = result.new_entry_point {
                entry_point = Some(ep);
            }
            if let Some(ml) = result.new_max_layer {
                max_layer = ml;
            }
            if entry_point.is_none() {
                entry_point = Some(i);
            }
        }

        // Delete node 5
        graph.deleted[5] = true;

        // Search near [5, 0]
        let query = vec![5.0, 0.0];
        let result = search(
            &query,
            3,
            16,
            entry_point.unwrap(),
            max_layer,
            &euclidean,
            &|id| graph.read_vector(id),
            &|id, layer| graph.read_neighbors(id, layer),
            &|id| graph.is_deleted(id),
            &|_| true,
        );

        let ids: Vec<u32> = result.neighbors.iter().map(|r| r.0).collect();
        assert!(
            !ids.contains(&5),
            "deleted node 5 should not appear in results: {ids:?}"
        );
        // Should still find neighbors of node 5
        assert!(!ids.is_empty(), "should still return some results");
    }
}
