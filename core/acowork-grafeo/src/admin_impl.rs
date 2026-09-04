//! MemoryAdminService implementation for GrafeoStore.
//!
//! ADR-051 P4: Moves all admin/management business logic (node listing,
//! detail retrieval, stats, CRUD, consolidation trigger, embedding
//! migration) from the Runtime's `memory_query.rs` into the GrafeoStore
//! implementation itself. The Runtime no longer needs to know grafeo
//! graph internals (`db()`, `graph_store()`, `Node`, `Value`).

use std::collections::{BTreeMap, HashMap};

use acowork_core::error::{AcoworkError, Result};
use acowork_memory::admin::{
    AdminConsolidateResult, AdminListNodesOutput, AdminListNodesParams, AdminNodeDetail,
    AdminNodeRecord, AdminStats, MemoryAdminService, RebuildStats,
};
use grafeo_common::types::{NodeId, Value};
use grafeo_core::graph::lpg::Node;

use crate::grafeo::GrafeoStore;
use crate::labels;
use crate::stats;

/// Maximum number of nodes to scan without any filter (keyword or type).
///
/// Queries exceeding this limit are rejected to prevent unbounded memory
/// allocation and excessive CPU usage.
const MAX_UNFILTERED_MEMORY_SCAN: usize = 10_000;

/// All memory labels scanned by list/stats/delete operations.
const MEMORY_LABELS: [&str; 4] = [
    labels::EPISODIC,
    labels::KNOWLEDGE,
    labels::PROCEDURAL,
    labels::AUTOBIOGRAPHICAL,
];

/// Snapshot of node-level embedding health across `MEMORY_LABELS`.
///
/// Produced by [`GrafeoStore::audit_embedding_health`] and surfaced via
/// the `memory_diag` tracing target from `MemoryAdminService::get_stats`
/// whenever `nodes_with_embedding < total_nodes` — i.e. the exact
/// condition that drives the desktop Memory panel's
/// "部分节点缺少向量嵌入" banner (`MemoryPanel.tsx:140`).
///
/// `missing_ids` is bounded by the caller's `limit`; `dim_histogram` is
/// always complete so a stale-dimension regression is visible even when
/// only a handful of nodes are affected.
#[derive(Debug, Default, Clone)]
pub struct EmbeddingAudit {
    /// Up to `limit` `(node_id, label)` tuples for nodes whose
    /// `embedding` property is missing or non-vector, in label /
    /// node-id order.
    pub missing_ids: Vec<(u64, String)>,
    /// Histogram of observed embedding vector dimensions across every
    /// memory-label node. Bucket `0` counts nodes whose `embedding`
    /// property is absent or malformed (see `audit_embedding_health`).
    pub dim_histogram: BTreeMap<usize, u64>,
}

impl GrafeoStore {
    /// Walk every `MEMORY_LABEL` node and report embedding health.
    ///
    /// Differs from [`Self::count_nodes_with_embedding`] in two ways:
    /// 1. Captures up to `limit` node ids that lack a vector `embedding`
    ///    property — needed to pinpoint the "missing 1 node" failure
    ///    mode reported by the desktop Memory panel banner.
    /// 2. Reports the actual dimension distribution so a stale "embedding
    ///    present but wrong dimension" condition (e.g. after a half-finished
    ///    model migration) becomes visible.
    ///
    /// `limit` bounds the `missing_ids` allocation; the histogram is
    /// always complete across all four memory labels.
    pub fn audit_embedding_health(&self, limit: usize) -> EmbeddingAudit {
        let mut audit = EmbeddingAudit::default();
        for label in MEMORY_LABELS {
            let node_ids = self.db().graph_store().nodes_by_label(label);
            for &node_id in &node_ids {
                let Some(node) = self.db().get_node(node_id) else {
                    continue;
                };
                match node.get_property("embedding") {
                    Some(Value::Vector(arc)) => {
                        *audit.dim_histogram.entry(arc.len()).or_insert(0) += 1;
                    }
                    // Either the property is absent (`None`) or holds a
                    // non-vector value (corruption). Both are bucketed as
                    // `0` so the histogram makes the regression obvious.
                    Some(_) | None => {
                        *audit.dim_histogram.entry(0).or_insert(0) += 1;
                        if audit.missing_ids.len() < limit {
                            audit.missing_ids.push((node_id.0, label.to_string()));
                        }
                    }
                }
            }
        }
        audit
    }
}

impl MemoryAdminService for GrafeoStore {
    fn list_nodes(&self, params: &AdminListNodesParams) -> AdminListNodesOutput {
        let graph = self.db().graph_store();

        // Parse time_range into a cutoff timestamp (epoch seconds).
        let cutoff: Option<i64> = match params.time_range.as_str() {
            "" | "all" => None,
            "1h" => Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            "1d" => Some(chrono::Utc::now() - chrono::Duration::days(1)),
            "7d" => Some(chrono::Utc::now() - chrono::Duration::days(7)),
            "30d" => Some(chrono::Utc::now() - chrono::Duration::days(30)),
            other => {
                tracing::warn!(
                    time_range = %other,
                    "memory list: unknown time_range value, ignoring filter"
                );
                None
            }
        }
        .map(|ts| ts.timestamp());

        // Reject unfiltered queries when the database is too large.
        let has_filter = !params.keyword.is_empty()
            || !params.node_type.is_empty()
            || !params.sub_type.is_empty()
            || (!params.time_range.is_empty() && params.time_range != "all");
        if !has_filter {
            let total_nodes: usize = MEMORY_LABELS
                .iter()
                .map(|l| graph.nodes_by_label(l).len())
                .sum();
            if total_nodes > MAX_UNFILTERED_MEMORY_SCAN {
                tracing::warn!(
                    total_nodes,
                    max = MAX_UNFILTERED_MEMORY_SCAN,
                    "memory list: rejected unfiltered scan (too many nodes)"
                );
                return AdminListNodesOutput {
                    total: total_nodes as u64,
                    page: params.page,
                    size: params.size,
                    nodes: vec![],
                    rejected_unfiltered: Some(total_nodes as u64),
                };
            }
        }

        let mut all_entries: Vec<AdminNodeRecord> = Vec::new();
        for label in &MEMORY_LABELS {
            if !params.node_type.is_empty() && params.node_type != *label {
                continue;
            }

            let node_ids = graph.nodes_by_label(label);
            let label_node_count = node_ids.len();
            let mut matched = 0usize;
            for id in node_ids {
                if let Some(n) = self.db().get_node(id) {
                    let content = extract_node_content(label, &n);

                    // Keyword filter - case-insensitive substring match.
                    if !params.keyword.is_empty()
                        && !content
                            .to_lowercase()
                            .contains(&params.keyword.to_lowercase())
                    {
                        continue;
                    }

                    // Sub-type filter (Knowledge / Autobiographical only).
                    // Episodic and Procedural labels have no `sub_type`
                    // property, so the filter is *only* applied to labels
                    // that actually carry one. This matches the panel UX:
                    // the sub-filter dropdown is only offered when the
                    // primary type is Knowledge or Autobiographical, and
                    // asking for sub_type=X with `type=Episodic` must NOT
                    // hide Episodic rows.
                    if !params.sub_type.is_empty()
                        && (*label == labels::KNOWLEDGE || *label == labels::AUTOBIOGRAPHICAL)
                    {
                        let node_sub_type = extract_sub_type(label, &n);
                        match node_sub_type {
                            Some(ref s) if s == &params.sub_type => {}
                            _ => continue,
                        }
                    }

                    let created_at = n
                        .get_property("created_at")
                        .and_then(|v| v.as_timestamp())
                        .map(|ts| ts.as_secs())
                        .unwrap_or(0);
                    if let Some(cutoff_ts) = cutoff
                        && created_at < cutoff_ts
                    {
                        continue;
                    }
                    let last_accessed_at = n
                        .get_property("last_accessed_at")
                        .and_then(|v| v.as_timestamp())
                        .map(|ts| ts.as_secs())
                        .unwrap_or(created_at);
                    let access_count = n
                        .get_property("access_count")
                        .and_then(|v| v.as_int64())
                        .unwrap_or(0) as u32;
                    let confidence = n
                        .get_property("confidence")
                        .and_then(|v| v.as_float64())
                        .unwrap_or(0.0);
                    // Raw `importance` property — passed through verbatim.
                    // Node types without this property (Procedural /
                    // Autobiographical) legitimately report 0.0.
                    let importance = n
                        .get_property("importance")
                        .and_then(|v| v.as_float64())
                        .unwrap_or(0.0);
                    let decay_score = n
                        .get_property("decay_score")
                        .and_then(|v| v.as_float64())
                        .unwrap_or(1.0);
                    let status = n
                        .get_property("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Active")
                        .to_string();
                    let sub_type = extract_sub_type(label, &n);
                    all_entries.push(AdminNodeRecord {
                        node_id: id.0,
                        node_type: label.to_string(),
                        sub_type,
                        content,
                        confidence,
                        importance,
                        decay_score,
                        created_at,
                        last_accessed_at,
                        access_count,
                        status,
                    });
                    matched += 1;
                }
            }

            tracing::info!(
                label,
                total_in_label = label_node_count,
                matched,
                "memory list: label scan"
            );
        }

        // Order: most-recent first (created_at DESC). Use node_id DESC as
        // a stable tiebreaker so paginated results stay deterministic.
        all_entries.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.node_id.cmp(&a.node_id))
        });

        let total = all_entries.len() as u64;
        let page = params.page.max(1);
        let size = params.size.clamp(1, 100) as usize;
        let start = ((page - 1) as usize) * size;
        let nodes: Vec<AdminNodeRecord> = if start < all_entries.len() {
            all_entries.into_iter().skip(start).take(size).collect()
        } else {
            vec![]
        };
        tracing::info!(
            total,
            page,
            returned = nodes.len(),
            "memory list: final result"
        );

        AdminListNodesOutput {
            total,
            page,
            size: size as u32,
            nodes,
            rejected_unfiltered: None,
        }
    }

    fn get_node(&self, node_id: u64) -> AdminNodeDetail {
        let target = NodeId(node_id);
        for label in &MEMORY_LABELS {
            let ids = self.db().graph_store().nodes_by_label(label);
            if !ids.contains(&target) {
                continue;
            }
            let n = match self.db().get_node(target) {
                Some(n) => n,
                None => {
                    tracing::warn!(
                        node_id,
                        label,
                        "memory get_node: label index hit but get_node returned None"
                    );
                    continue;
                }
            };

            let content = extract_node_content(label, &n);
            let created_at = n
                .get_property("created_at")
                .and_then(|v| v.as_timestamp())
                .map(|ts| ts.as_secs())
                .unwrap_or(0);
            let last_accessed_at = n
                .get_property("last_accessed_at")
                .and_then(|v| v.as_timestamp())
                .map(|ts| ts.as_secs())
                .unwrap_or(created_at);
            let access_count = n
                .get_property("access_count")
                .and_then(|v| v.as_int64())
                .unwrap_or(0) as u32;
            let confidence = n
                .get_property("confidence")
                .and_then(|v| v.as_float64())
                .unwrap_or(0.0);
            // Raw `importance` property — passed through verbatim.
            // Node types without this property (Procedural /
            // Autobiographical) legitimately report 0.0.
            let importance = n
                .get_property("importance")
                .and_then(|v| v.as_float64())
                .unwrap_or(0.0);
            let decay_score = n
                .get_property("decay_score")
                .and_then(|v| v.as_float64())
                .unwrap_or(1.0);
            let status = n
                .get_property("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Active")
                .to_string();

            // Snapshot every property the engine attached to the node.
            let mut properties: HashMap<String, serde_json::Value> = HashMap::new();
            for (key, value) in n.properties_as_btree() {
                let json_val =
                    serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
                properties.insert(key.as_str().to_string(), json_val);
            }

            return AdminNodeDetail {
                node_id,
                found: true,
                node_type: label.to_string(),
                sub_type: extract_sub_type(label, &n),
                content,
                confidence,
                importance,
                decay_score,
                created_at,
                last_accessed_at,
                access_count,
                status,
                properties,
                message: "ok".to_string(),
            };
        }

        AdminNodeDetail {
            node_id,
            found: false,
            node_type: String::new(),
            sub_type: None,
            content: String::new(),
            confidence: 0.0,
            importance: 0.0,
            decay_score: 0.0,
            created_at: 0,
            last_accessed_at: 0,
            access_count: 0,
            status: String::new(),
            properties: HashMap::new(),
            message: "Node not found".to_string(),
        }
    }

    fn create_node(
        &self,
        label: &str,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<u64> {
        let props = properties
            .iter()
            .map(|(k, v)| (k.as_str(), json_to_grafeo_value(v)));
        let id = self
            .store_node(label, props)
            .map_err(|e| AcoworkError::Memory(e.to_string()))?;
        Ok(id.0)
    }

    fn update_node(
        &self,
        node_id: u64,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        let id = NodeId(node_id);
        if self.get_node(id).is_none() {
            return Err(AcoworkError::Memory(format!(
                "node {} not found",
                node_id
            )));
        }
        let props = properties
            .iter()
            .map(|(k, v)| (k.as_str(), json_to_grafeo_value(v)));
        self.update_node(id, props)
            .map_err(|e| AcoworkError::Memory(e.to_string()))?;
        Ok(())
    }

    fn delete_node(&self, node_id: u64) -> bool {
        let id = NodeId(node_id);
        match GrafeoStore::delete_node(self, id) {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::warn!(node_id = node_id, error = %e, "Failed to delete memory node");
                false
            }
        }
    }

    fn get_stats(&self) -> AdminStats {
        match stats::collect_stats(self) {
            Ok(stats_snapshot) => {
                let aggregate = stats::aggregate_node_status(self, &MEMORY_LABELS)
                    .unwrap_or_default();

                let total_nodes = aggregate.total_nodes;
                let by_type: HashMap<String, u64> = stats_snapshot
                    .label_counts
                    .into_iter()
                    .map(|(k, v)| (k, v as u64))
                    .collect();

                let mut by_status = aggregate.by_status;
                by_status.insert("purged".to_string(), stats_snapshot.purged_count as u64);

                let avg_decay_score = aggregate.avg_decay_score as f64;
                let index_health = "healthy".to_string();
                let stored_dim = self.embedding_dim() as u64;
                let nodes_with_embedding = self.count_nodes_with_embedding();

                // Surface embedding health to logs whenever the runtime's
                // Memory panel would show "部分节点缺少向量嵌入". The cheap
                // path (`nodes_with_embedding == total_nodes`) does no
                // extra work; only the discrepancy case walks every
                // MEMORY_LABEL node to capture the first few missing ids
                // and the embedding-dimension distribution.
                //
                // Grep target: `target: "memory_diag"` (set via
                // RUST_LOG=memory_diag=warn in the runtime).
                if total_nodes > 0 && nodes_with_embedding < total_nodes {
                    let audit = self.audit_embedding_health(10);
                    let dim_summary: Vec<String> = audit
                        .dim_histogram
                        .iter()
                        .map(|(d, c)| format!("{d}:{c}"))
                        .collect();
                    tracing::warn!(
                        target: "memory_diag",
                        total_nodes,
                        nodes_with_embedding,
                        missing = total_nodes - nodes_with_embedding,
                        missing_ids = ?audit.missing_ids,
                        dim_histogram = ?dim_summary,
                        stored_dim,
                        "memory_store: detected nodes without vector embeddings"
                    );
                }

                AdminStats {
                    total_nodes,
                    storage_bytes: 0,
                    by_type,
                    by_status,
                    avg_decay_score,
                    index_health,
                    stored_dim,
                    nodes_with_embedding,
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to collect memory stats");
                AdminStats {
                    total_nodes: 0,
                    storage_bytes: 0,
                    by_type: HashMap::new(),
                    by_status: HashMap::new(),
                    avg_decay_score: 0.0,
                    index_health: format!("error: {}", e),
                    stored_dim: 0,
                    nodes_with_embedding: 0,
                }
            }
        }
    }

    fn consolidate(&self, force: bool) -> AdminConsolidateResult {
        let config = acowork_memory::consolidation::OfflineConsolidationConfig {
            batch_size: 50,
            min_pending_age_hours: if force { 0 } else { 1 },
        };
        match self.run_offline_consolidation(&config) {
            Ok(result) => AdminConsolidateResult {
                upgraded: result.upgraded as u64,
                kept_pending: result.kept_pending as u64,
                marked_dormant: result.marked_dormant as u64,
                triples_extracted: result.triples_extracted as u64,
                procedural_created: result.procedural_created as u64,
                episodic_cleaned: result.episodic_cleaned as u64,
                started: true,
            },
            Err(e) => {
                tracing::warn!(error = %e, "Consolidation failed");
                AdminConsolidateResult::default()
            }
        }
    }

    fn embedding_dim(&self) -> usize {
        GrafeoStore::embedding_dim(self)
    }

    fn count_nodes_with_embedding(&self) -> u64 {
        GrafeoStore::count_nodes_with_embedding(self)
    }

    fn migrate_embedding_dimension(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
    ) -> Result<RebuildStats> {
        let stats = GrafeoStore::migrate_embedding_dimension(self, |text| embed_fn(text), new_dim)
            .map_err(|e| AcoworkError::Memory(e.to_string()))?;
        Ok(RebuildStats {
            total_scanned: stats.total_scanned,
            rebuilt: stats.rebuilt,
            skipped_no_embedding: stats.skipped_no_embedding,
            skipped_no_content: stats.skipped_no_content,
            errors: stats.errors,
        })
    }
}

// ── Helper functions (moved from memory_query.rs) ─────────────────────

/// Extract the secondary classification of a memory node, if any.
///
/// - `Knowledge`: reads the `sub_type` property (`Fact` / `Preference` /
///   `Relation` / `Procedure`).
/// - `Autobiographical`: reads the `category` property — the memory panel
///   surfaces this as the `Autobiographical` sub-filter (`Identity` /
///   `Capability` / `Limitation` / `Preference` / `History` / `Relationship`).
/// - `Episodic` / `Procedural`: returns `None` (no secondary classification).
///
/// Returns `None` if the property is missing — older nodes written before
/// the field was tracked will simply lack the sub-filter UI affordance.
fn extract_sub_type(label: &str, n: &Node) -> Option<String> {
    let property_name = match label {
        "Knowledge" => "sub_type",
        "Autobiographical" => "category",
        _ => return None,
    };
    n.get_property(property_name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract a human-readable content string from a Grafeo node.
fn extract_node_content(label: &str, n: &Node) -> String {
    match label {
        "Episodic" => {
            let role = n.get_property("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = n
                .get_property("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("[{}] {}", role, content)
        }
        "Knowledge" => {
            let subject = n
                .get_property("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let predicate = n
                .get_property("predicate")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let object = n
                .get_property("object")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{} {} {}", subject, predicate, object)
        }
        "Procedural" => {
            let name = n
                .get_property("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let action = n
                .get_property("action_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("When {}: {}", name, action)
        }
        "Autobiographical" => {
            let key = n.get_property("key").and_then(|v| v.as_str()).unwrap_or("");
            let value = n
                .get_property("value")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{}: {}", key, value)
        }
        _ => "Unknown".to_string(),
    }
}

/// Convert a `serde_json::Value` into the `grafeo_common::types::Value`
/// representation Grafeo expects for property writes.
fn json_to_grafeo_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int64(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float64(f)
            } else {
                Value::String(n.to_string().into())
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone().into()),
        other => Value::String(other.to_string().into()),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the sub-classification filter (`sub_type`) that powers the
    //! memory panel's secondary drill-down (e.g. "Autobiographical →
    //! Limitation", "Knowledge → Preference"). The filter is the contract
    //! front-end code depends on, so its semantics are exercised here
    //! against a real in-memory GrafeoStore.

    use super::*;
    use crate::grafeo::GrafeoStore;
    use crate::labels;
    use grafeo_common::types::Value;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().expect("in-memory store should open")
    }

    /// Seed two Knowledge nodes with different sub_types, one
    /// Autobiographical Limitation node, and one Episodic node. Returns
    /// the store so each test can run in isolation.
    fn seed_mixed_store() -> GrafeoStore {
        let store = test_store();

        // Knowledge / Fact
        store
            .store_node(
                labels::KNOWLEDGE,
                [
                    ("subject", Value::from("Rust")),
                    ("sub_type", Value::from("Fact")),
                    ("content", Value::from("Rust is a systems language")),
                ],
            )
            .unwrap();

        // Knowledge / Preference
        store
            .store_node(
                labels::KNOWLEDGE,
                [
                    ("subject", Value::from("dark mode")),
                    ("sub_type", Value::from("Preference")),
                    ("content", Value::from("user prefers dark UI")),
                ],
            )
            .unwrap();

        // Autobiographical / Limitation
        store
            .store_node(
                labels::AUTOBIOGRAPHICAL,
                [
                    ("key", Value::from("no-bash-rm")),
                    ("category", Value::from("Limitation")),
                    ("value", Value::from("I do not run destructive shell commands unsupervised")),
                ],
            )
            .unwrap();

        // Episodic — never has a sub_type
        store
            .store_node(
                labels::EPISODIC,
                [
                    ("role", Value::from("user")),
                    ("content", Value::from("hello")),
                ],
            )
            .unwrap();

        store
    }

    #[test]
    fn list_nodes_sub_type_knowledge_preference() {
        let store = seed_mixed_store();
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            node_type: "Knowledge".to_string(),
            sub_type: "Preference".to_string(),
            keyword: String::new(),
            time_range: "all".to_string(),
        });
        assert_eq!(out.total, 1, "only the Preference node should match");
        assert_eq!(out.nodes[0].node_type, "Knowledge");
        assert_eq!(out.nodes[0].sub_type.as_deref(), Some("Preference"));
    }

    #[test]
    fn list_nodes_sub_type_autobiographical_limitation() {
        let store = seed_mixed_store();
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            node_type: "Autobiographical".to_string(),
            sub_type: "Limitation".to_string(),
            keyword: String::new(),
            time_range: "all".to_string(),
        });
        assert_eq!(out.total, 1);
        assert_eq!(out.nodes[0].node_type, "Autobiographical");
        assert_eq!(out.nodes[0].sub_type.as_deref(), Some("Limitation"));
    }

    #[test]
    fn list_nodes_sub_type_returns_empty_when_no_match() {
        let store = seed_mixed_store();
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            node_type: "Autobiographical".to_string(),
            sub_type: "Identity".to_string(), // not seeded
            keyword: String::new(),
            time_range: "all".to_string(),
        });
        assert_eq!(out.total, 0);
        assert!(out.nodes.is_empty());
    }

    #[test]
    fn list_nodes_sub_type_empty_is_no_filter() {
        let store = seed_mixed_store();
        // Empty sub_type must not filter — should return all 4 seeded nodes.
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            node_type: String::new(),
            sub_type: String::new(),
            keyword: String::new(),
            time_range: "all".to_string(),
        });
        assert_eq!(out.total, 4, "empty sub_type must behave as no filter");
    }

    #[test]
    fn list_nodes_sub_type_combines_with_node_type_filter() {
        let store = seed_mixed_store();
        // Asking for sub_type=Limitation but node_type=Knowledge must return
        // nothing — the sub-classification belongs to a different label.
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            node_type: "Knowledge".to_string(),
            sub_type: "Limitation".to_string(),
            keyword: String::new(),
            time_range: "all".to_string(),
        });
        assert_eq!(out.total, 0, "sub_type from another label must not match");
    }

    #[test]
    fn list_nodes_sub_type_against_episodic_is_ignored() {
        let store = seed_mixed_store();
        // Episodic nodes have no sub-classification, so any sub_type filter
        // must let them through — the panel uses sub_type only to drill into
        // Knowledge / Autobiographical, never to filter Episodic out.
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            node_type: "Episodic".to_string(),
            sub_type: "anything".to_string(),
            keyword: String::new(),
            time_range: "all".to_string(),
        });
        assert_eq!(out.total, 1, "sub_type filter must not affect Episodic nodes");
        assert_eq!(out.nodes[0].node_type, "Episodic");
    }

    // ── audit_embedding_health ───────────────────────────────────────
    //
    // Backed by the `memory_diag` warning emitted from `get_stats` when
    // `nodes_with_embedding < total_nodes`. The tests below pin the
    // contract used by the desktop Memory panel's "部分节点缺少向量嵌入"
    // banner to a paper trail in the runtime log.

    use std::sync::Arc;

    /// Create a node under `label` and (optionally) attach an embedding
    /// vector of `dim` floats. Returns the new node id.
    fn make_node(
        store: &GrafeoStore,
        label: &str,
        content: &str,
        embedding_dim: Option<usize>,
    ) -> u64 {
        let id = store
            .store_node(label, [("content", Value::from(content))])
            .expect("store_node should succeed in test store");
        if let Some(dim) = embedding_dim {
            let vec: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.001).collect();
            store
                .db()
                .set_node_property(id, "embedding", Value::Vector(Arc::from(vec.as_slice())));
        }
        id.0
    }

    #[test]
    fn audit_embedding_health_empty_store_reports_no_missing() {
        let store = test_store();
        let audit = store.audit_embedding_health(10);
        assert!(audit.missing_ids.is_empty(), "fresh store has no missing");
        assert!(
            audit.dim_histogram.is_empty(),
            "fresh store has no embedding observations"
        );
    }

    #[test]
    fn audit_embedding_health_lists_missing_node_ids() {
        let store = test_store();
        // The in-memory store's HNSW config defaults to `DEFAULT_EMBEDDING_DIM`
        // (384), so all test embeddings must match that dim to avoid
        // triggering the engine's dimension-mismatch panic.
        const EMB_DIM: usize = acowork_memory::types::DEFAULT_EMBEDDING_DIM;

        let healthy = make_node(&store, labels::EPISODIC, "with emb", Some(EMB_DIM));
        let broken = make_node(&store, labels::EPISODIC, "without emb", None);
        let _autobio =
            make_node(&store, labels::AUTOBIOGRAPHICAL, "self", Some(EMB_DIM));

        let audit = store.audit_embedding_health(10);

        assert_eq!(audit.missing_ids, vec![(broken, labels::EPISODIC.to_string())]);
        assert_eq!(audit.dim_histogram.get(&EMB_DIM), Some(&2));
        // The `0` bucket counts every node without a vector embedding,
        // including the one we deliberately left bare.
        assert_eq!(audit.dim_histogram.get(&0), Some(&1));
        // Sanity: the healthy id must not appear in the missing list.
        assert!(!audit.missing_ids.iter().any(|(id, _)| *id == healthy));
    }

    #[test]
    fn audit_embedding_health_respects_limit() {
        let store = test_store();
        // Five Episodic nodes, none carrying an embedding.
        for i in 0..5 {
            make_node(&store, labels::EPISODIC, &format!("bare-{i}"), None);
        }

        let audit = store.audit_embedding_health(2);
        assert_eq!(
            audit.missing_ids.len(),
            2,
            "limit=2 must cap the reported missing ids"
        );
        assert_eq!(audit.dim_histogram.get(&0), Some(&5));
    }

    // =====================================================================
    // Raw-field passthrough: `confidence` and `importance` are returned
    // verbatim from the node properties — never derived, never remapped by
    // node type. A node type that lacks a property legitimately reports 0.0
    // (that IS the truth, not a bug). This is the contract the Desktop
    // memory panel relies on to render "置信度" vs "重要程度" per type.
    // =====================================================================

    /// Seed one node of each label with the exact `confidence` / `importance`
    /// property values that the real write paths produce:
    ///
    /// | label            | confidence | importance |
    /// |------------------|-----------|-----------|
    /// | Episodic         | (absent)  | 0.7       |  (compaction: record_distilled)
    /// | Knowledge        | 0.7       | 0.5       |  (memory_store tool)
    /// | Procedural       | 0.9       | (absent)  |  (generalization)
    /// | Autobiographical | 1.0       | (absent)  |  (manifest bootstrap)
    fn seed_raw_field_store() -> GrafeoStore {
        let store = test_store();
        store
            .store_node(
                labels::EPISODIC,
                [
                    ("content", Value::from("compacted summary")),
                    ("importance", Value::from(0.7f64)),
                ],
            )
            .unwrap();
        store
            .store_node(
                labels::KNOWLEDGE,
                [
                    ("content", Value::from("user prefers dark mode")),
                    ("confidence", Value::from(0.7f64)),
                    ("importance", Value::from(0.5f64)),
                ],
            )
            .unwrap();
        store
            .store_node(
                labels::PROCEDURAL,
                [
                    ("content", Value::from("when X then Y")),
                    ("confidence", Value::from(0.9f64)),
                ],
            )
            .unwrap();
        store
            .store_node(
                labels::AUTOBIOGRAPHICAL,
                [
                    ("content", Value::from("name: acowork")),
                    ("confidence", Value::from(1.0f64)),
                ],
            )
            .unwrap();
        store
    }

    #[test]
    fn list_nodes_passes_confidence_and_importance_verbatim() {
        let store = seed_raw_field_store();
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            ..Default::default()
        });

        let by_type: std::collections::HashMap<&str, &AdminNodeRecord> = out
            .nodes
            .iter()
            .map(|n| (n.node_type.as_str(), n))
            .collect();

        // Episodic: importance present, confidence absent → 0.0 is the truth.
        let ep = by_type[labels::EPISODIC];
        assert_eq!(ep.confidence, 0.0, "Episodic has no confidence property");
        assert_eq!(ep.importance, 0.7, "Episodic importance must pass through");

        // Knowledge: both present.
        let kn = by_type[labels::KNOWLEDGE];
        assert_eq!(kn.confidence, 0.7);
        assert_eq!(kn.importance, 0.5);

        // Procedural: confidence present, importance absent → 0.0 is the truth.
        let pr = by_type[labels::PROCEDURAL];
        assert_eq!(pr.confidence, 0.9);
        assert_eq!(pr.importance, 0.0, "Procedural has no importance property");

        // Autobiographical: confidence present, importance absent → 0.0.
        let au = by_type[labels::AUTOBIOGRAPHICAL];
        assert_eq!(au.confidence, 1.0);
        assert_eq!(au.importance, 0.0, "Autobiographical has no importance property");
    }

    #[test]
    fn get_node_passes_confidence_and_importance_verbatim() {
        let store = seed_raw_field_store();
        let out = store.list_nodes(&AdminListNodesParams {
            page: 1,
            size: 100,
            ..Default::default()
        });

        for rec in &out.nodes {
            let detail = MemoryAdminService::get_node(&store, rec.node_id);
            assert!(detail.found);
            assert_eq!(
                detail.confidence, rec.confidence,
                "get_node confidence must match list_nodes for {}",
                rec.node_type
            );
            assert_eq!(
                detail.importance, rec.importance,
                "get_node importance must match list_nodes for {}",
                rec.node_type
            );
        }
    }
}
