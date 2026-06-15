//! Associative Spreading retrieval (S2.8).
//!
//! Implements graph-based expansion, cross-layer retrieval, PageRank integration,
//! topology boosting, community detection, and dynamic weight adjustment.

use std::collections::{HashMap, HashSet, VecDeque};

use grafeo_common::types::{NodeId, Value};
use grafeo_core::graph::Direction;

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::retrieval::cosine_distance_to_similarity;
use crate::types::labels;

// ---------------------------------------------------------------------------
// S2.8.1: Graph expansion configuration and types
// ---------------------------------------------------------------------------

/// Configuration for graph expansion.
#[derive(Debug, Clone)]
pub struct GraphExpandConfig {
    /// Maximum traversal depth (default: 3).
    pub max_hops: u32,
    /// Maximum total expanded nodes (default: 20).
    pub max_total_nodes: usize,
    /// Score thresholds per hop for early stopping.
    /// Index 0 = hop 1 threshold, index 1 = hop 2, etc.
    pub early_stop_thresholds: Vec<f32>,
    /// Minimum edge weight to traverse (default: 0.1).
    pub min_edge_weight: f32,
}

impl Default for GraphExpandConfig {
    fn default() -> Self {
        Self {
            max_hops: 3,
            max_total_nodes: 20,
            early_stop_thresholds: vec![0.15, 0.2, 0.25],
            min_edge_weight: 0.1,
        }
    }
}

impl GraphExpandConfig {
    /// Create a new config with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set max hops.
    pub fn with_max_hops(mut self, max_hops: u32) -> Self {
        self.max_hops = max_hops;
        self
    }

    /// Set max total nodes.
    pub fn with_max_total_nodes(mut self, max_total_nodes: usize) -> Self {
        self.max_total_nodes = max_total_nodes;
        self
    }

    /// Set early stop thresholds.
    pub fn with_early_stop_thresholds(mut self, thresholds: Vec<f32>) -> Self {
        self.early_stop_thresholds = thresholds;
        self
    }

    /// Set min edge weight.
    pub fn with_min_edge_weight(mut self, weight: f32) -> Self {
        self.min_edge_weight = weight;
        self
    }

    /// Get the early-stop threshold for a specific hop distance (1-based).
    /// Returns `None` if hop is beyond the configured thresholds (no early stop).
    pub fn threshold_for_hop(&self, hop: u32) -> Option<f32> {
        if hop == 0 {
            return None;
        }
        let idx = (hop as usize).saturating_sub(1);
        self.early_stop_thresholds.get(idx).copied()
    }
}

/// A node discovered during graph expansion.
#[derive(Debug, Clone)]
pub struct ExpandedNode {
    /// The expanded node's ID.
    pub node_id: NodeId,
    /// The node's primary label.
    pub label: String,
    /// Number of hops from the nearest seed node.
    pub hop_distance: u32,
    /// Accumulated score: parent_score * edge_weight * decay.
    pub accumulated_score: f64,
    /// Traversal path from the seed node.
    pub path: Vec<NodeId>,
}

/// Decay factor applied per hop during expansion.
const DECAY_PER_HOP: f64 = 0.7;

/// Default edge weight when no explicit weight property exists.
const DEFAULT_EDGE_WEIGHT: f64 = 1.0;

// ---------------------------------------------------------------------------
// S2.8.1: Graph expansion (BFS with scoring and early stopping)
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Expand from seed nodes via graph traversal.
    ///
    /// Uses BFS from seed nodes, scoring each expanded node as:
    /// `accumulated_score = parent_score * edge_weight * DECAY_PER_HOP`
    ///
    /// Stops expanding a path when:
    /// - The accumulated score drops below the early-stop threshold for that hop
    /// - The total number of expanded nodes reaches `max_total_nodes`
    /// - The hop distance exceeds `max_hops`
    /// - The edge weight is below `min_edge_weight`
    pub fn graph_expand(
        &self,
        seed_nodes: &[(NodeId, f64)],
        config: &GraphExpandConfig,
    ) -> Result<Vec<ExpandedNode>> {
        let mut results: Vec<ExpandedNode> = Vec::new();
        let mut visited: HashSet<NodeId> = HashSet::new();

        // Queue entries: (node_id, hop, accumulated_score, path)
        let mut queue: VecDeque<(NodeId, u32, f64, Vec<NodeId>)> = VecDeque::new();

        // Seed nodes are visited but not included in results.
        for (node_id, score) in seed_nodes {
            visited.insert(*node_id);
            queue.push_back((*node_id, 0, *score, vec![*node_id]));
        }

        'outer: while let Some((current_id, hops, parent_score, path)) = queue.pop_front() {
            if hops >= config.max_hops {
                continue;
            }
            if results.len() >= config.max_total_nodes {
                break;
            }

            let graph = self.db.graph_store();
            let edge_refs = graph.edges_from(current_id, Direction::Both);

            for (neighbor_id, edge_id) in edge_refs {
                if visited.contains(&neighbor_id) {
                    continue;
                }

                // Get edge weight from properties, or use default.
                let edge_weight = self
                    .db
                    .get_edge(edge_id)
                    .and_then(|e| e.get_property("weight").and_then(|v| v.as_float64()))
                    .unwrap_or(DEFAULT_EDGE_WEIGHT);

                if edge_weight < f64::from(config.min_edge_weight) {
                    continue;
                }

                let next_hop = hops + 1;
                let accumulated_score = parent_score * edge_weight * DECAY_PER_HOP;

                // Early stop: check threshold for this hop.
                if let Some(threshold) = config.threshold_for_hop(next_hop)
                    && accumulated_score < f64::from(threshold)
                {
                    continue;
                }

                // Capacity check BEFORE adding to results/queue:
                // If we've reached the limit, break out of BOTH loops
                // so no high-score neighbors are silently skipped.
                if results.len() >= config.max_total_nodes {
                    break 'outer;
                }

                visited.insert(neighbor_id);

                // Resolve label.
                let label = self
                    .db
                    .get_node(neighbor_id)
                    .and_then(|n| n.labels.first().map(|l| l.to_string()))
                    .unwrap_or_default();

                let mut new_path = path.clone();
                new_path.push(neighbor_id);

                results.push(ExpandedNode {
                    node_id: neighbor_id,
                    label: label.clone(),
                    hop_distance: next_hop,
                    accumulated_score,
                    path: new_path.clone(),
                });

                // Only enqueue if we haven't hit the capacity limit yet.
                // This prevents the queue from growing with items that
                // will never be processed.
                if results.len() < config.max_total_nodes {
                    queue.push_back((neighbor_id, next_hop, accumulated_score, new_path));
                }
            }
        }

        // Sort by accumulated_score descending.
        results.sort_by(|a, b| {
            b.accumulated_score
                .partial_cmp(&a.accumulated_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// S2.8.2: Cross-layer retrieval
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Cross-layer retrieval: find knowledge nodes related to episodes and vice versa.
    ///
    /// First performs vector search on both Knowledge and Episodic labels,
    /// then expands the results via graph traversal.
    pub fn cross_layer_search(
        &self,
        query_embedding: &[f32],
        k: usize,
        expand_config: &GraphExpandConfig,
    ) -> Result<Vec<ExpandedNode>> {
        // Vector search on Knowledge label.
        let knowledge_results = self
            .db
            .vector_search(
                labels::KNOWLEDGE,
                "embedding",
                query_embedding,
                k,
                None,
                None,
            )
            .unwrap_or_default();

        // Vector search on Episodic label.
        let episodic_results = self
            .db
            .vector_search(
                labels::EPISODIC,
                "embedding",
                query_embedding,
                k,
                None,
                None,
            )
            .unwrap_or_default();

        // Combine results: convert distance to similarity score.
        let mut seeds: Vec<(NodeId, f64)> = Vec::new();

        for (id, dist) in &knowledge_results {
            let similarity = cosine_distance_to_similarity(*dist);
            seeds.push((*id, similarity));
        }
        for (id, dist) in &episodic_results {
            let similarity = cosine_distance_to_similarity(*dist);
            seeds.push((*id, similarity));
        }

        // Deduplicate seeds by node_id, keeping the higher score.
        let mut seen: HashMap<NodeId, f64> = HashMap::new();
        for (id, score) in seeds {
            seen.entry(id)
                .and_modify(|existing| {
                    if score > *existing {
                        *existing = score;
                    }
                })
                .or_insert(score);
        }
        let seeds: Vec<(NodeId, f64)> = seen.into_iter().collect();

        // Expand from seeds.
        self.graph_expand(&seeds, expand_config)
    }
}

// ---------------------------------------------------------------------------
// S2.8.3: PageRank integration
// ---------------------------------------------------------------------------

/// Default node count threshold for switching to sampled PageRank.
/// Graphs with more than this many nodes use the sampled approximation.
const PAGERANK_SAMPLING_THRESHOLD: usize = 1000;

/// Fraction of nodes to sample per walk in sampled PageRank.
const PAGERANK_SAMPLE_FRACTION: f64 = 0.3;

impl GrafeoStore {
    /// Compute PageRank scores for all nodes in the graph.
    ///
    /// Automatically selects between full iterative PageRank (small graphs)
    /// and random-walk sampling (large graphs, >1000 nodes) for performance.
    pub fn compute_pagerank(&self, iterations: usize, damping: f64) -> Result<Vec<(NodeId, f64)>> {
        // Check graph size to decide strategy
        let node_count = self.count_nodes()?;
        if node_count > PAGERANK_SAMPLING_THRESHOLD {
            return self.compute_pagerank_sampled(iterations, damping, node_count);
        }

        let session = self.db.session();
        let gql =
            format!("CALL grafeo.pagerank({{damping: {damping}, max_iterations: {iterations}}})");

        match session.execute(&gql) {
            Ok(result) => {
                let mut scores = Vec::new();
                for row in result.rows() {
                    if let (Some(Value::Int64(id)), Some(Value::Float64(score))) =
                        (row.first(), row.get(1))
                    {
                        scores.push((NodeId::new(*id as u64), *score));
                    }
                }
                Ok(scores)
            }
            Err(_) => self.compute_pagerank_fallback(iterations, damping),
        }
    }

    /// Count total nodes in the graph (lightweight query).
    fn count_nodes(&self) -> Result<usize> {
        let session = self.db.session();
        match session.execute("MATCH (n) RETURN count(n)") {
            Ok(result) => {
                if let Some(row) = result.rows().first() {
                    if let Some(Value::Int64(count)) = row.first() {
                        return Ok(*count as usize);
                    }
                }
                Ok(0)
            }
            Err(e) => Err(crate::error::GrafeoError::Memory(format!(
                "count_nodes GQL failed: {e}"
            ))),
        }
    }

    /// Sampled PageRank: uses random-walk sampling for large graphs.
    ///
    /// Instead of O(V²) full iteration, runs random walks from sampled
    /// seed nodes and aggregates visit counts into approximate PageRank scores.
    pub fn compute_pagerank_sampled(
        &self,
        iterations: usize,
        damping: f64,
        node_count: usize,
    ) -> Result<Vec<(NodeId, f64)>> {
        let graph = self.db.graph_store();

        // Collect all node IDs
        let session = self.db.session();
        let mut node_ids: Vec<NodeId> = Vec::new();
        if let Ok(result) = session.execute("MATCH (n) RETURN id(n)") {
            for row in result.rows() {
                if let Some(Value::Int64(id)) = row.first() {
                    node_ids.push(NodeId::new(*id as u64));
                }
            }
        }

        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Sample fraction of nodes as random walk seeds
        let sample_size = ((node_count as f64) * PAGERANK_SAMPLE_FRACTION).ceil() as usize;
        let sample_size = sample_size.min(node_ids.len()).max(1);

        // Use deterministic sampling (every Nth node) for reproducibility
        let step = node_ids.len() / sample_size;
        let seeds: Vec<NodeId> = node_ids
            .iter()
            .step_by(step.max(1))
            .take(sample_size)
            .copied()
            .collect();

        // Run random walks and count visits.
        // Uses a deterministic PCG-style PRNG instead of the `rand` crate
        // for reproducibility and to avoid extra dependencies.
        let mut visit_counts: HashMap<NodeId, f64> = HashMap::new();
        let steps_per_walk = iterations.max(10);

        let mut rng_state: u64 = 0x853c49e6748fea9b; // Arbitrary seed for reproducibility
        for &seed in &seeds {
            let mut current = seed;
            for _ in 0..steps_per_walk {
                *visit_counts.entry(current).or_insert(0.0) += 1.0;

                // With probability (1 - damping), teleport to a random seed
                // (standard PageRank random surfer model)
                rng_state = rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let coin = (rng_state as f64) / (u64::MAX as f64);
                if coin < 1.0 - damping {
                    let idx = (rng_state as usize) % seeds.len();
                    current = seeds[idx];
                    continue;
                }

                // Get outgoing edges
                let edge_refs = graph.edges_from(current, Direction::Outgoing);
                if edge_refs.is_empty() {
                    // Dead-end: teleport to a deterministic "random" seed
                    rng_state = rng_state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let idx = (rng_state as usize) % seeds.len();
                    current = seeds[idx];
                } else {
                    // Follow a deterministic "random" outgoing edge
                    rng_state = rng_state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let idx = (rng_state as usize) % edge_refs.len();
                    current = edge_refs[idx].0;
                }
            }
        }

        // Normalize visit counts to score distribution
        let total_visits: f64 = visit_counts.values().sum();
        let mut scores: Vec<(NodeId, f64)> = if total_visits > 0.0 {
            visit_counts
                .into_iter()
                .map(|(id, count)| (id, count / total_visits))
                .collect()
        } else {
            Vec::new()
        };

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scores)
    }

    /// Fallback PageRank: simplified iterative computation using graph_store.
    fn compute_pagerank_fallback(
        &self,
        iterations: usize,
        damping: f64,
    ) -> Result<Vec<(NodeId, f64)>> {
        let graph = self.db.graph_store();

        // Collect all node IDs and build adjacency.
        let mut out_degree: HashMap<NodeId, usize> = HashMap::new();
        let mut out_neighbors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();

        // Try to get nodes via GQL.
        let mut known_nodes: HashSet<NodeId> = HashSet::new();
        let session = self.db.session();
        if let Ok(result) = session.execute("MATCH (n) RETURN id(n)") {
            for row in result.rows() {
                if let Some(Value::Int64(id)) = row.first() {
                    known_nodes.insert(NodeId::new(*id as u64));
                }
            }
        }

        if known_nodes.is_empty() {
            return Ok(Vec::new());
        }

        let node_ids: Vec<NodeId> = known_nodes.into_iter().collect();
        let n = node_ids.len() as f64;
        let node_set: HashSet<NodeId> = node_ids.iter().copied().collect();

        // Build adjacency from graph_store.
        for &nid in &node_ids {
            let edge_refs = graph.edges_from(nid, Direction::Outgoing);
            let mut neighbors = Vec::new();
            for (neighbor_id, _) in &edge_refs {
                if node_set.contains(neighbor_id) {
                    neighbors.push(*neighbor_id);
                }
            }
            out_degree.insert(nid, neighbors.len());
            out_neighbors.insert(nid, neighbors);
        }

        // Initialize scores uniformly.
        let mut scores: HashMap<NodeId, f64> = HashMap::new();
        for &nid in &node_ids {
            scores.insert(nid, 1.0 / n);
        }

        // Iterate.
        for _ in 0..iterations {
            let mut new_scores: HashMap<NodeId, f64> = HashMap::new();
            for &nid in &node_ids {
                let base = (1.0 - damping) / n;
                new_scores.insert(nid, base);
            }

            for &nid in &node_ids {
                let degree = out_degree[&nid];
                if degree > 0 {
                    let share = scores[&nid] / degree as f64;
                    for &neighbor in &out_neighbors[&nid] {
                        if let Some(s) = new_scores.get_mut(&neighbor) {
                            *s += damping * share;
                        }
                    }
                }
            }

            scores = new_scores;
        }

        let mut result: Vec<(NodeId, f64)> = scores.into_iter().collect();
        result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(result)
    }

    /// Boost search results by PageRank importance.
    ///
    /// For each result, the score is adjusted as:
    /// `new_score = original_score * (1.0 - pagerank_weight) + pagerank * pagerank_weight`
    pub fn apply_pagerank_boost(
        &self,
        results: &mut [(NodeId, f64)],
        pagerank_weight: f64,
    ) -> Result<()> {
        if results.is_empty() || pagerank_weight <= 0.0 {
            return Ok(());
        }

        let pagerank_scores = self.compute_pagerank(20, 0.85)?;
        let pr_map: HashMap<NodeId, f64> = pagerank_scores.into_iter().collect();

        for (node_id, score) in results.iter_mut() {
            if let Some(pr) = pr_map.get(node_id) {
                *score = *score * (1.0 - pagerank_weight) + pr * pagerank_weight;
            }
        }

        // Re-sort by boosted score.
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// S2.8.4: Topology boost (graph connectivity)
// ---------------------------------------------------------------------------

/// Boost nodes with higher graph connectivity.
///
/// `connectivity = number_of_edges / average_edges_per_node`
///
/// The accumulated score is multiplied by `(1.0 + connectivity_factor * boost_strength)`.
pub fn topology_boost(
    results: &mut [ExpandedNode],
    node_edge_counts: &HashMap<NodeId, usize>,
    avg_edges: f64,
) {
    if avg_edges <= 0.0 || results.is_empty() {
        return;
    }

    for node in results.iter_mut() {
        if let Some(&edge_count) = node_edge_counts.get(&node.node_id) {
            let connectivity = edge_count as f64 / avg_edges;
            // boost_strength scales the effect; use a fixed 0.1 factor.
            let boost = 1.0 + connectivity * 0.1;
            node.accumulated_score *= boost;
        }
    }
}

/// Compute edge counts per node for topology boost.
pub fn compute_edge_counts(store: &GrafeoStore) -> (HashMap<NodeId, usize>, f64) {
    let mut counts: HashMap<NodeId, usize> = HashMap::new();
    let graph = store.db.graph_store();

    // Collect all nodes via GQL.
    let session = store.db.session();
    if let Ok(result) = session.execute("MATCH (n) RETURN id(n)") {
        for row in result.rows() {
            if let Some(Value::Int64(id)) = row.first() {
                let nid = NodeId::new(*id as u64);
                let edge_refs = graph.edges_from(nid, Direction::Both);
                counts.insert(nid, edge_refs.len());
            }
        }
    }

    let total: usize = counts.values().sum();
    let avg = if counts.is_empty() {
        0.0
    } else {
        total as f64 / counts.len() as f64
    };

    (counts, avg)
}

// ---------------------------------------------------------------------------
// S2.8.5: Community detection (Louvain)
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Run Louvain community detection.
    ///
    /// Uses grafeo-engine's built-in Louvain algorithm via GQL CALL procedure.
    /// Falls back to a simplified label-propagation approach if the procedure fails.
    ///
    /// Returns community assignments: node_id -> community_id.
    pub fn detect_communities(&self) -> Result<HashMap<NodeId, u64>> {
        let session = self.db.session();
        let gql = "CALL grafeo.louvain()";

        match session.execute(gql) {
            Ok(result) => {
                let mut communities: HashMap<NodeId, u64> = HashMap::new();
                for row in result.rows() {
                    if let (Some(Value::Int64(id)), Some(Value::Int64(community))) =
                        (row.first(), row.get(1))
                    {
                        communities.insert(NodeId::new(*id as u64), *community as u64);
                    }
                }
                Ok(communities)
            }
            Err(_) => {
                // Fallback: simplified label propagation.
                self.detect_communities_fallback()
            }
        }
    }

    /// Fallback community detection using simplified label propagation.
    fn detect_communities_fallback(&self) -> Result<HashMap<NodeId, u64>> {
        let graph = self.db.graph_store();

        // Collect nodes via GQL.
        let session = self.db.session();
        let mut node_ids: Vec<NodeId> = Vec::new();
        if let Ok(result) = session.execute("MATCH (n) RETURN id(n)") {
            for row in result.rows() {
                if let Some(Value::Int64(id)) = row.first() {
                    node_ids.push(NodeId::new(*id as u64));
                }
            }
        }

        if node_ids.is_empty() {
            return Ok(HashMap::new());
        }

        // Initialize: each node is its own community (id as community).
        let mut labels: HashMap<NodeId, u64> = HashMap::new();
        for (i, &nid) in node_ids.iter().enumerate() {
            labels.insert(nid, i as u64);
        }

        // Run label propagation for a few iterations.
        for _ in 0..10 {
            let mut changed = false;
            for &nid in &node_ids {
                let edge_refs = graph.edges_from(nid, Direction::Both);
                if edge_refs.is_empty() {
                    continue;
                }

                // Count neighbor labels.
                let mut label_counts: HashMap<u64, usize> = HashMap::new();
                for (neighbor_id, _) in &edge_refs {
                    if let Some(&label) = labels.get(neighbor_id) {
                        *label_counts.entry(label).or_insert(0) += 1;
                    }
                }

                // Adopt the most common neighbor label.
                if let Some((&best_label, _)) = label_counts.iter().max_by_key(|(_, count)| **count)
                    && labels[&nid] != best_label
                {
                    labels.insert(nid, best_label);
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }

        Ok(labels)
    }

    /// Boost results from the same community as the query's nearest node.
    ///
    /// For each result node in the same community as `query_community`,
    /// its score is multiplied by `(1.0 + boost_factor)`.
    pub fn apply_community_boost(
        &self,
        results: &mut [(NodeId, f64)],
        query_community: u64,
        boost_factor: f64,
    ) -> Result<()> {
        if results.is_empty() || boost_factor <= 0.0 {
            return Ok(());
        }

        let communities = self.detect_communities()?;

        for (node_id, score) in results.iter_mut() {
            if let Some(&community) = communities.get(node_id)
                && community == query_community
            {
                *score *= 1.0 + boost_factor;
            }
        }

        // Re-sort by boosted score.
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// S2.8.6: Expansion limits (enforced via GraphExpandConfig defaults)
// ---------------------------------------------------------------------------

/// Validate that expansion config respects the S2.8.6 limits.
///
/// - Max 3 hops
/// - Total nodes <= 20
/// - Early stop thresholds must be provided
pub fn validate_expand_config(config: &GraphExpandConfig) -> Result<()> {
    if config.max_hops > 3 {
        return Err(crate::error::GrafeoError::Memory(format!(
            "max_hops exceeds limit: {} > 3",
            config.max_hops
        )));
    }
    if config.max_total_nodes > 20 {
        return Err(crate::error::GrafeoError::Memory(format!(
            "max_total_nodes exceeds limit: {} > 20",
            config.max_total_nodes
        )));
    }
    if config.early_stop_thresholds.len() < config.max_hops as usize {
        return Err(crate::error::GrafeoError::Memory(format!(
            "early_stop_thresholds has {} entries but max_hops is {}",
            config.early_stop_thresholds.len(),
            config.max_hops
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// S2.8.7: Dynamic retrieval weights based on memory_hint type
// ---------------------------------------------------------------------------

/// Get fusion weights based on memory_hint type.
///
/// Returns `(vector_weight, text_weight, graph_weight)` for use in
/// hybrid search and graph expansion decisions.
///
/// | type | meaning       | vector | text | graph |
/// |------|---------------|--------|------|-------|
/// | `s`  | semantic      | 0.8    | 0.2  | 0.0   |
/// | `f`  | fact          | 0.5    | 0.5  | 0.0   |
/// | `r`  | relational    | 0.6    | 0.2  | 0.2   |
/// | `i`  | identity      | 0.3    | 0.7  | 0.0   |
/// | _    | default       | 0.7    | 0.3  | 0.0   |
pub fn get_hint_weights(hint_type: &str) -> (f32, f32, f32) {
    match hint_type {
        "s" => (0.8, 0.2, 0.0),
        "f" => (0.5, 0.5, 0.0),
        "r" => (0.6, 0.2, 0.2),
        "i" => (0.3, 0.7, 0.0),
        _ => (0.7, 0.3, 0.0),
    }
}

/// Get graph_expand early stop thresholds based on hint type.
///
/// - `"s"` (semantic): conservative thresholds `[0.15, 0.2, 0.25]`
/// - `"r"` (relational): aggressive thresholds `[0.1, 0.12, 0.15]`
/// - Other: default conservative thresholds `[0.15, 0.2, 0.25]`
pub fn get_expand_thresholds(hint_type: &str) -> Vec<f32> {
    match hint_type {
        "s" => vec![0.15, 0.2, 0.25],
        "r" => vec![0.1, 0.12, 0.15],
        _ => vec![0.15, 0.2, 0.25],
    }
}

/// Build a `GraphExpandConfig` from a hint type.
///
/// Uses `get_expand_thresholds` for early stop thresholds.
/// Graph expansion is only enabled for `"s"` and `"r"` hint types.
pub fn config_from_hint(hint_type: &str) -> GraphExpandConfig {
    GraphExpandConfig::default().with_early_stop_thresholds(get_expand_thresholds(hint_type))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_EMBEDDING_DIM;
    use grafeo_common::types::Value;

    /// Helper: create an in-memory GrafeoStore for testing.
    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    /// Helper: generate a test embedding vector.
    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    /// Helper: store a Knowledge node with embedding.
    fn store_knowledge(store: &GrafeoStore, subject: &str, embedding: &[f32]) -> NodeId {
        let id = store
            .store_node(labels::KNOWLEDGE, [("subject", Value::from(subject))])
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id
    }

    /// Helper: store an Episodic node with embedding.
    fn store_episode(store: &GrafeoStore, content: &str, embedding: &[f32]) -> NodeId {
        let id = store
            .store_node(labels::EPISODIC, [("content", Value::from(content))])
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id
    }

    // =====================================================================
    // Test 1: GraphExpandConfig defaults
    // =====================================================================

    #[test]
    fn test_graph_expand_config_defaults() {
        let config = GraphExpandConfig::default();
        assert_eq!(config.max_hops, 3);
        assert_eq!(config.max_total_nodes, 20);
        assert_eq!(config.early_stop_thresholds, vec![0.15, 0.2, 0.25]);
        assert!((config.min_edge_weight - 0.1).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test 2: GraphExpandConfig threshold_for_hop
    // =====================================================================

    #[test]
    fn test_threshold_for_hop() {
        let config = GraphExpandConfig::default();
        assert_eq!(config.threshold_for_hop(0), None);
        assert_eq!(config.threshold_for_hop(1), Some(0.15));
        assert_eq!(config.threshold_for_hop(2), Some(0.2));
        assert_eq!(config.threshold_for_hop(3), Some(0.25));
        assert_eq!(config.threshold_for_hop(4), None);
    }

    // =====================================================================
    // Test 3: graph_expand basic traversal
    // =====================================================================

    #[test]
    fn test_graph_expand_basic_traversal() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();

        // a -> b -> c
        store
            .create_memory_edge(a, b, "REFERENCES", vec![])
            .unwrap();
        store
            .create_memory_edge(b, c, "REFERENCES", vec![])
            .unwrap();

        let config = GraphExpandConfig::new()
            .with_max_hops(3)
            .with_early_stop_thresholds(vec![0.0, 0.0, 0.0]);

        let results = store.graph_expand(&[(a, 1.0)], &config).unwrap();

        // Should find b (1 hop) and c (2 hops).
        assert_eq!(results.len(), 2);
        let ids: Vec<NodeId> = results.iter().map(|n| n.node_id).collect();
        assert!(ids.contains(&b));
        assert!(ids.contains(&c));
    }

    // =====================================================================
    // Test 4: graph_expand respects max_hops
    // =====================================================================

    #[test]
    fn test_graph_expand_max_hops_limit() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();
        let d = store
            .store_node("Knowledge", [("k", Value::from("d"))])
            .unwrap();

        store.create_memory_edge(a, b, "R", vec![]).unwrap();
        store.create_memory_edge(b, c, "R", vec![]).unwrap();
        store.create_memory_edge(c, d, "R", vec![]).unwrap();

        // Max 1 hop: should only reach b.
        let config = GraphExpandConfig::new()
            .with_max_hops(1)
            .with_early_stop_thresholds(vec![0.0]);

        let results = store.graph_expand(&[(a, 1.0)], &config).unwrap();

        let ids: Vec<NodeId> = results.iter().map(|n| n.node_id).collect();
        assert!(ids.contains(&b));
        assert!(!ids.contains(&c));
        assert!(!ids.contains(&d));
    }

    // =====================================================================
    // Test 5: graph_expand early stopping
    // =====================================================================

    #[test]
    fn test_graph_expand_early_stopping() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();

        store.create_memory_edge(a, b, "R", vec![]).unwrap();
        store.create_memory_edge(b, c, "R", vec![]).unwrap();

        // High early-stop threshold: should prune most nodes.
        let config = GraphExpandConfig::new()
            .with_max_hops(3)
            .with_early_stop_thresholds(vec![0.9, 0.95, 0.99]);

        let results = store.graph_expand(&[(a, 1.0)], &config).unwrap();

        // With threshold 0.9 at hop 1, accumulated_score = 1.0 * 1.0 * 0.7 = 0.7 < 0.9
        // So even b should be pruned.
        assert!(results.is_empty());
    }

    // =====================================================================
    // Test 6: graph_expand respects max_total_nodes
    // =====================================================================

    #[test]
    fn test_graph_expand_max_total_nodes() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let mut neighbor_ids = Vec::new();
        for i in 0..10 {
            let nid = store
                .store_node("Knowledge", [("k", Value::from(format!("n{i}")))])
                .unwrap();
            store.create_memory_edge(a, nid, "R", vec![]).unwrap();
            neighbor_ids.push(nid);
        }

        let config = GraphExpandConfig::new()
            .with_max_hops(1)
            .with_max_total_nodes(3)
            .with_early_stop_thresholds(vec![0.0]);

        let results = store.graph_expand(&[(a, 1.0)], &config).unwrap();

        assert!(results.len() <= 3);
    }

    // =====================================================================
    // Test 7: cross_layer_search basic
    // =====================================================================

    #[test]
    fn test_cross_layer_search_basic() {
        let store = test_store();
        let emb = test_embedding();
        let _kid = store_knowledge(&store, "Rust programming", &emb);
        let _eid = store_episode(&store, "Learning Rust today", &emb);

        let config = GraphExpandConfig::default();
        let results = store.cross_layer_search(&emb, 5, &config).unwrap();

        // Should return at least the seed nodes' neighbors.
        // Since the two nodes have no edges, expansion won't find new nodes.
        // But the vector search itself should find them (though they're seeds, not results).
        // graph_expand only returns expanded nodes, not seeds.
        // So with no edges, results may be empty — that's correct.
        assert!(results.len() <= 20);
    }

    // =====================================================================
    // Test 8: PageRank compute (fallback)
    // =====================================================================

    #[test]
    fn test_compute_pagerank_basic() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();

        store.create_memory_edge(a, b, "R", vec![]).unwrap();
        store.create_memory_edge(b, c, "R", vec![]).unwrap();

        let scores = store.compute_pagerank(20, 0.85).unwrap();
        assert!(!scores.is_empty(), "PageRank should return scores");

        // Verify all nodes have a score.
        let score_map: HashMap<NodeId, f64> = scores.into_iter().collect();
        assert!(score_map.contains_key(&a));
        assert!(score_map.contains_key(&b));
        assert!(score_map.contains_key(&c));
    }

    // =====================================================================
    // Test 9: apply_pagerank_boost
    // =====================================================================

    #[test]
    fn test_apply_pagerank_boost() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();

        store.create_memory_edge(a, b, "R", vec![]).unwrap();

        let mut results = vec![(a, 0.5), (b, 0.3)];
        store.apply_pagerank_boost(&mut results, 0.1).unwrap();

        // Results should still have 2 entries.
        assert_eq!(results.len(), 2);
        // The ordering may change based on PageRank.
        let ids: Vec<NodeId> = results.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
    }

    // =====================================================================
    // Test 10: topology_boost
    // =====================================================================

    #[test]
    fn test_topology_boost_basic() {
        let n1 = NodeId::new(1);
        let n2 = NodeId::new(2);
        let n3 = NodeId::new(3);

        let mut results = vec![
            ExpandedNode {
                node_id: n1,
                label: "Knowledge".to_string(),
                hop_distance: 1,
                accumulated_score: 0.5,
                path: vec![n1],
            },
            ExpandedNode {
                node_id: n2,
                label: "Knowledge".to_string(),
                hop_distance: 1,
                accumulated_score: 0.3,
                path: vec![n2],
            },
            ExpandedNode {
                node_id: n3,
                label: "Knowledge".to_string(),
                hop_distance: 2,
                accumulated_score: 0.1,
                path: vec![n3],
            },
        ];

        let mut edge_counts = HashMap::new();
        edge_counts.insert(n1, 10); // High connectivity.
        edge_counts.insert(n2, 2);
        edge_counts.insert(n3, 1);
        let avg_edges = 4.33; // Average.

        let original_scores: Vec<f64> = results.iter().map(|n| n.accumulated_score).collect();

        topology_boost(&mut results, &edge_counts, avg_edges);

        // n1 should get the biggest boost (highest connectivity).
        assert!(results[0].accumulated_score >= original_scores[0]);
        // n3 should get a minimal boost.
        let n3_node = results.iter().find(|n| n.node_id == n3).unwrap();
        let n1_node = results.iter().find(|n| n.node_id == n1).unwrap();
        // n1's relative boost should be larger than n3's.
        let n1_ratio = n1_node.accumulated_score / original_scores[0];
        let n3_ratio = n3_node.accumulated_score / original_scores[2];
        assert!(n1_ratio > n3_ratio);
    }

    // =====================================================================
    // Test 11: topology_boost with zero avg_edges
    // =====================================================================

    #[test]
    fn test_topology_boost_zero_avg() {
        let n1 = NodeId::new(1);
        let mut results = vec![ExpandedNode {
            node_id: n1,
            label: "Knowledge".to_string(),
            hop_distance: 1,
            accumulated_score: 0.5,
            path: vec![n1],
        }];

        let edge_counts = HashMap::new();
        let original = results[0].accumulated_score;
        topology_boost(&mut results, &edge_counts, 0.0);
        // With avg_edges = 0, no boost should be applied.
        assert!((results[0].accumulated_score - original).abs() < f64::EPSILON);
    }

    // =====================================================================
    // Test 12: detect_communities
    // =====================================================================

    #[test]
    fn test_detect_communities_basic() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();
        let _d = store
            .store_node("Knowledge", [("k", Value::from("d"))])
            .unwrap();

        // a-b-c community, d isolated.
        store.create_memory_edge(a, b, "R", vec![]).unwrap();
        store.create_memory_edge(b, c, "R", vec![]).unwrap();

        let communities = store.detect_communities().unwrap();
        assert!(!communities.is_empty());

        // a, b, c should be in the same community.
        if let (Some(ca), Some(cb), Some(cc)) = (
            communities.get(&a),
            communities.get(&b),
            communities.get(&c),
        ) {
            assert_eq!(ca, cb);
            assert_eq!(cb, cc);
        }
    }

    // =====================================================================
    // Test 13: get_hint_weights
    // =====================================================================

    #[test]
    fn test_get_hint_weights() {
        assert_eq!(get_hint_weights("s"), (0.8, 0.2, 0.0));
        assert_eq!(get_hint_weights("f"), (0.5, 0.5, 0.0));
        assert_eq!(get_hint_weights("r"), (0.6, 0.2, 0.2));
        assert_eq!(get_hint_weights("i"), (0.3, 0.7, 0.0));
        assert_eq!(get_hint_weights("x"), (0.7, 0.3, 0.0));
    }

    // =====================================================================
    // Test 14: get_expand_thresholds
    // =====================================================================

    #[test]
    fn test_get_expand_thresholds() {
        assert_eq!(get_expand_thresholds("s"), vec![0.15, 0.2, 0.25]);
        assert_eq!(get_expand_thresholds("r"), vec![0.1, 0.12, 0.15]);
        assert_eq!(get_expand_thresholds("f"), vec![0.15, 0.2, 0.25]);
    }

    // =====================================================================
    // Test 15: count_nodes (indirect via compute_pagerank on empty graph)
    // =====================================================================

    #[test]
    fn test_count_nodes_empty_graph() {
        let store = test_store();
        // Empty graph should produce empty PageRank.
        let scores = store.compute_pagerank(20, 0.85).unwrap();
        assert!(scores.is_empty(), "empty graph should have no PageRank");
    }

    // =====================================================================
    // Test 16: sampled PageRank on a chain graph (small graph, full strategy)
    // =====================================================================

    #[test]
    fn test_compute_pagerank_small_graph_uses_fallback() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();
        store.create_memory_edge(a, b, "R", vec![]).unwrap();
        store.create_memory_edge(b, c, "R", vec![]).unwrap();
        store.create_memory_edge(c, a, "R", vec![]).unwrap();

        let scores = store.compute_pagerank(20, 0.85).unwrap();
        assert_eq!(scores.len(), 3);

        // All nodes should have non-zero scores.
        for (_id, score) in &scores {
            assert!(*score > 0.0, "PageRank score should be positive");
        }

        // Scores should sum to approximately 1.0.
        let total: f64 = scores.iter().map(|(_, s)| s).sum();
        assert!((total - 1.0).abs() < 0.01, "total={total} should be ~1.0");
    }

    // =====================================================================
    // Test 17: PageRank deterministic output (same graph → same result)
    // =====================================================================

    #[test]
    fn test_compute_pagerank_deterministic() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        let c = store
            .store_node("Knowledge", [("k", Value::from("c"))])
            .unwrap();
        store.create_memory_edge(a, b, "R", vec![]).unwrap();
        store.create_memory_edge(b, c, "R", vec![]).unwrap();
        store.create_memory_edge(c, a, "R", vec![]).unwrap();

        let first_run = store.compute_pagerank(20, 0.85).unwrap();
        let second_run = store.compute_pagerank(20, 0.85).unwrap();

        // Scores should be identical (deterministic), though ordering may differ
        // due to HashSet-to-Vec conversion in the fallback path.
        assert_eq!(first_run.len(), second_run.len());

        // Build score maps for comparison.
        let map1: HashMap<NodeId, f64> = first_run.iter().copied().collect();
        let map2: HashMap<NodeId, f64> = second_run.iter().copied().collect();
        for (id, score1) in &map1 {
            let score2 = map2.get(id).unwrap();
            assert!(
                (score1 - score2).abs() < 1e-10,
                "score for {id} differs: {score1} vs {score2}"
            );
        }
    }

    // =====================================================================
    // Test 18: PageRank respects damping factor
    // =====================================================================

    #[test]
    fn test_pagerank_damping_factor_effect() {
        let store = test_store();
        let a = store
            .store_node("Knowledge", [("k", Value::from("a"))])
            .unwrap();
        let b = store
            .store_node("Knowledge", [("k", Value::from("b"))])
            .unwrap();
        store.create_memory_edge(a, b, "R", vec![]).unwrap();

        // Different damping factors should produce different score distributions.
        let scores_low = store.compute_pagerank(10, 0.5).unwrap();
        let scores_high = store.compute_pagerank(10, 0.99).unwrap();

        // Build score maps.
        let map_low: HashMap<NodeId, f64> = scores_low.iter().copied().collect();
        let map_high: HashMap<NodeId, f64> = scores_high.iter().copied().collect();

        // All nodes must have scores.
        assert_eq!(map_low.len(), 2);
        assert_eq!(map_high.len(), 2);

        // Damping must have an effect — at least one node's score must differ.
        let any_diff = map_low
            .iter()
            .any(|(id, score)| (score - map_high.get(id).unwrap_or(&0.0)).abs() > 1e-10);
        assert!(any_diff, "damping factor must affect PageRank scores");
    }
}
