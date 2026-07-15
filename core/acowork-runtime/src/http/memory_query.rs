//! Shared memory query business logic.
//!
//! ADR-033: Memory endpoints are now served by the Runtime's localhost
//! HTTP server (reverse-proxied by the Gateway). The gRPC query path is
//! being deprecated, but the same business logic is reused by both:
//!
//! - HTTP handlers in [`super::server`] call these functions and
//!   serialise the returned intermediate types into JSON.
//! - The gRPC query path in `crate::cli` calls the same functions and
//!   maps the result into the existing proto response types.
//!
//! This module is the single source of truth for the four memory
//! endpoints (`/memory/nodes`, `/memory/stats`,
//! `/memory/nodes/{id}`, `/memory/consolidate`). It was previously
//! implemented inline in `cli.rs::handle_memory_*_query` and is now
//! promoted to a shared module so the HTTP and gRPC paths cannot drift
//! apart.
//!
//! All entry points take `Option<&Arc<GrafeoStore>>` and report
//! graceful "no store" responses when the store has not been
//! initialised yet (the HTTP server starts before Phase B in the
//! agent boot sequence, so it may briefly see `None` for the
//! memory store).

use std::collections::HashMap;
use std::sync::Arc;

use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::stats;
use grafeo_core::graph::lpg::Node;
use serde::Serialize;

/// Maximum number of nodes to scan without any filter (keyword or type).
///
/// Queries exceeding this limit are rejected to prevent unbounded memory
/// allocation and excessive CPU usage on the Runtime side.
pub(crate) const MAX_UNFILTERED_MEMORY_SCAN: usize = 10_000;

/// All memory labels scanned by list/stats/delete operations.
///
/// Keeping this in one place guarantees the gRPC and HTTP paths agree
/// on what counts as a "memory node".
const MEMORY_LABELS: [&str; 4] = ["Episodic", "Knowledge", "Procedural", "Autobiographical"];

// ── Intermediate types (consumed by both HTTP and gRPC) ───────────────

/// One row in a memory list response.
///
/// Field naming matches the existing proto contract
/// (`MemoryNodeEntry`) so HTTP JSON and gRPC wire formats are
/// interchangeable.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MemoryNodeRecord {
    pub node_id: u64,
    pub node_type: String,
    pub content: String,
    pub confidence: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
}

/// Result of a list-nodes query. Pagination metadata + the page slice.
#[derive(Debug, Clone)]
pub(crate) struct ListNodesOutput {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub nodes: Vec<MemoryNodeRecord>,
    /// When `Some`, the unfiltered scan was rejected because the
    /// database exceeds [`MAX_UNFILTERED_MEMORY_SCAN`]. The HTTP path
    /// surfaces this as a "rejected" hint in the response; the gRPC
    /// path reports the same value as `total`.
    #[allow(dead_code)]
    pub rejected_unfiltered: Option<u64>,
}

/// Result of a stats query.
#[derive(Debug, Clone)]
pub(crate) struct StatsOutput {
    pub total_nodes: u64,
    pub storage_bytes: u64,
    pub by_type: HashMap<String, u64>,
    pub by_status: HashMap<String, u64>,
    pub avg_decay_score: f64,
    pub index_health: String,
    pub stored_dim: u64,
    pub nodes_with_embedding: u64,
    pub model_dim: u64,
}

/// Result of a delete query.
#[derive(Debug, Clone)]
pub(crate) struct DeleteOutput {
    pub node_id: u64,
    pub deleted: bool,
    pub message: String,
}

/// Result of a consolidation query.
#[derive(Debug, Clone)]
pub(crate) struct ConsolidateOutput {
    pub started: bool,
    pub duration_ms: u64,
    pub episodes_consolidated: u64,
    pub knowledge_nodes_generated: u64,
    pub message: String,
}

/// Result of a single-node GET query (`GET /memory/nodes/{nid}`).
///
/// ADR-034 §11.2 #12: returns the full node detail when `found`, plus
/// every property on the Grafeo node so the desktop UI can render any
/// custom fields the engine attached to the node. The `properties`
/// snapshot is intentionally `HashMap<String, serde_json::Value>` (not
/// strongly typed) — schema is per-label and varies over time, so we
/// ferry the keys through verbatim instead of guessing.
#[derive(Debug, Clone)]
pub(crate) struct GetNodeOutput {
    pub node_id: u64,
    pub found: bool,
    pub node_type: String,
    pub content: String,
    pub confidence: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
    /// All Grafeo node properties, serialised as JSON values.
    /// Empty when `found == false` or the store is unavailable.
    pub properties: HashMap<String, serde_json::Value>,
    pub message: String,
}

/// Parameters for [`list_nodes`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ListNodesParams {
    pub page: u32,
    pub size: u32,
    /// Filter by node type ("Episodic" / "Knowledge" / "Procedural" / "Autobiographical").
    /// Empty string = no filter.
    pub node_type: String,
    /// Case-insensitive substring filter applied to the rendered content.
    /// Empty string = no filter.
    pub keyword: String,
    /// Time-range bucket. Supported values: "1h", "1d", "7d", "30d", "all", "".
    pub time_range: String,
}

// ── Conversion helpers (intermediate → wire format) ────────────────────

/// Convert a [`ListNodesOutput`] to a JSON-friendly [`serde_json::Value`]
/// matching the response shape documented in
/// `docs/zh/protocols/http.md` §7.7.
pub(crate) fn list_output_to_json(out: &ListNodesOutput) -> serde_json::Value {
    serde_json::json!({
        "total": out.total,
        "page": out.page,
        "size": out.size,
        "nodes": out.nodes,
    })
}

/// Convert a [`StatsOutput`] to JSON.
pub(crate) fn stats_output_to_json(out: &StatsOutput) -> serde_json::Value {
    serde_json::json!({
        "total_nodes": out.total_nodes,
        "storage_bytes": out.storage_bytes,
        "by_type": out.by_type,
        "by_status": out.by_status,
        "avg_decay_score": out.avg_decay_score,
        "index_health": out.index_health,
        "stored_dim": out.stored_dim,
        "nodes_with_embedding": out.nodes_with_embedding,
        "model_dim": out.model_dim,
    })
}

/// Convert a [`DeleteOutput`] to JSON.
pub(crate) fn delete_output_to_json(out: &DeleteOutput) -> serde_json::Value {
    serde_json::json!({
        "node_id": out.node_id,
        "deleted": out.deleted,
        "message": out.message,
    })
}

/// Convert a [`ConsolidateOutput`] to JSON.
pub(crate) fn consolidate_output_to_json(out: &ConsolidateOutput) -> serde_json::Value {
    serde_json::json!({
        "started": out.started,
        "duration_ms": out.duration_ms,
        "episodes_consolidated": out.episodes_consolidated,
        "knowledge_nodes_generated": out.knowledge_nodes_generated,
        "message": out.message,
    })
}

/// Convert a [`GetNodeOutput`] to JSON.
///
/// Schema mirrors [`GetNodeOutput`] 1:1 (`found` flag leads so the UI can
/// distinguish "missing" from "missing fields"; `properties` is included
/// as a nested object whenever `found == true`, otherwise as `{}`).
pub(crate) fn get_output_to_json(out: &GetNodeOutput) -> serde_json::Value {
    serde_json::json!({
        "node_id": out.node_id,
        "found": out.found,
        "node_type": out.node_type,
        "content": out.content,
        "confidence": out.confidence,
        "decay_score": out.decay_score,
        "created_at": out.created_at,
        "last_accessed_at": out.last_accessed_at,
        "access_count": out.access_count,
        "status": out.status,
        "properties": out.properties,
        "message": out.message,
    })
}

// ── Core business logic ───────────────────────────────────────────────

fn empty_list_output(page: u32, size: u32) -> ListNodesOutput {
    ListNodesOutput {
        total: 0,
        page,
        size,
        nodes: vec![],
        rejected_unfiltered: None,
    }
}

fn empty_stats_output(embed_provider_dim: u64) -> StatsOutput {
    StatsOutput {
        total_nodes: 0,
        storage_bytes: 0,
        by_type: HashMap::new(),
        by_status: HashMap::new(),
        avg_decay_score: 0.0,
        index_health: "no_store".to_string(),
        stored_dim: 0,
        nodes_with_embedding: 0,
        model_dim: embed_provider_dim,
    }
}

/// List memory nodes with pagination, filtering, and search.
pub(crate) fn list_nodes(
    memory_store: Option<&Arc<GrafeoStore>>,
    params: ListNodesParams,
) -> ListNodesOutput {
    let store = match memory_store {
        Some(s) => s,
        None => {
            tracing::warn!("memory list: no Grafeo store available");
            return empty_list_output(params.page, params.size);
        }
    };
    let graph = store.db().graph_store();

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

    // P0: Reject unfiltered queries when the database is too large.
    let has_filter = !params.keyword.is_empty()
        || !params.node_type.is_empty()
        || (!params.time_range.is_empty() && params.time_range != "all");
    if !has_filter {
        let total_nodes: usize = MEMORY_LABELS.iter().map(|l| graph.nodes_by_label(l).len()).sum();
        if total_nodes > MAX_UNFILTERED_MEMORY_SCAN {
            tracing::warn!(
                total_nodes,
                max = MAX_UNFILTERED_MEMORY_SCAN,
                "memory list: rejected unfiltered scan (too many nodes)"
            );
            return ListNodesOutput {
                total: total_nodes as u64,
                page: params.page,
                size: params.size,
                nodes: vec![],
                rejected_unfiltered: Some(total_nodes as u64),
            };
        }
    }

    let mut all_entries: Vec<MemoryNodeRecord> = Vec::new();
    for label in &MEMORY_LABELS {
        if !params.node_type.is_empty() && params.node_type != *label {
            continue;
        }

        let node_ids = graph.nodes_by_label(label);
        let label_node_count = node_ids.len();
        let mut matched = 0usize;
        for id in node_ids {
            if let Some(n) = store.db().get_node(id) {
                let content = extract_node_content(label, &n);

                // Keyword filter — case-insensitive substring match.
                if !params.keyword.is_empty()
                    && !content
                        .to_lowercase()
                        .contains(&params.keyword.to_lowercase())
                {
                    continue;
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
                let decay_score = n
                    .get_property("decay_score")
                    .and_then(|v| v.as_float64())
                    .unwrap_or(1.0);
                let status = n
                    .get_property("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Active")
                    .to_string();
                all_entries.push(MemoryNodeRecord {
                    node_id: id.0,
                    node_type: label.to_string(),
                    content,
                    confidence,
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

    // Order: most-recent first (created_at DESC). Use node_id DESC as a
    // stable tiebreaker so paginated results stay deterministic when
    // timestamps collide (or are missing).
    all_entries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.node_id.cmp(&a.node_id))
    });

    let total = all_entries.len() as u64;
    let page = params.page.max(1);
    let size = params.size.clamp(1, 100) as usize;
    let start = ((page - 1) as usize) * size;
    let nodes: Vec<MemoryNodeRecord> = if start < all_entries.len() {
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

    ListNodesOutput {
        total,
        page,
        size: size as u32,
        nodes,
        rejected_unfiltered: None,
    }
}

/// Extract a human-readable content string from a Grafeo node.
///
/// Mirrors the formatting used in the gRPC path so the desktop app
/// sees consistent previews regardless of whether the data came in
/// over the legacy gRPC channel or the new HTTP reverse-proxy.
pub(crate) fn extract_node_content(label: &str, n: &Node) -> String {
    match label {
        "Episodic" => {
            let role = n
                .get_property("role")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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

/// Collect memory statistics, including vector-index diagnostics.
pub(crate) fn get_stats(
    memory_store: Option<&Arc<GrafeoStore>>,
    embed_provider_dim: u64,
) -> StatsOutput {
    let store = match memory_store {
        Some(s) => s,
        None => return empty_stats_output(embed_provider_dim),
    };
    match stats::collect_stats(store) {
        Ok(stats_snapshot) => {
            // Node-level aggregation (status breakdown + decay score).
            // Covers all user-visible memory labels so that every node
            // contributes to the Active/Dormant/Pending counts.
            let aggregate = stats::aggregate_node_status(
                store,
                &[
                    acowork_grafeo::labels::EPISODIC,
                    acowork_grafeo::labels::KNOWLEDGE,
                    acowork_grafeo::labels::PROCEDURAL,
                    acowork_grafeo::labels::AUTOBIOGRAPHICAL,
                ],
            )
            .unwrap_or_default();

            let total_nodes = aggregate.total_nodes;
            let by_type: HashMap<String, u64> = stats_snapshot
                .label_counts
                .into_iter()
                .map(|(k, v)| (k, v as u64))
                .collect();

            // by_status comes from the per-node aggregate, augmented with
            // the purged-node count from the global stats snapshot.
            let mut by_status = aggregate.by_status;
            by_status.insert("purged".to_string(), stats_snapshot.purged_count as u64);

            let avg_decay_score = aggregate.avg_decay_score as f64;
            let index_health = "healthy".to_string();

            // Vector-index diagnostics used by the desktop "Rebuild Index" banner.
            let stored_dim = store.embedding_dim() as u64;
            let nodes_with_embedding = store.count_nodes_with_embedding();

            StatsOutput {
                total_nodes,
                storage_bytes: 0, // TODO P3: track file size in StatsCollector
                by_type,
                by_status,
                avg_decay_score,
                index_health,
                stored_dim,
                nodes_with_embedding,
                model_dim: embed_provider_dim,
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to collect memory stats");
            StatsOutput {
                total_nodes: 0,
                storage_bytes: 0,
                by_type: HashMap::new(),
                by_status: HashMap::new(),
                avg_decay_score: 0.0,
                index_health: format!("error: {}", e),
                stored_dim: 0,
                nodes_with_embedding: 0,
                model_dim: embed_provider_dim,
            }
        }
    }
}

/// Look up a single memory node by numeric ID.
///
/// Scans every [`MEMORY_LABELS`] entry and returns the first node whose
/// `NodeId` matches. The lookup is intentionally cheap (`O(n)` over
/// labels, then `O(1)` once the right label is identified via
/// `get_node`) — the alternative (`graph.get_node_by_id`) is not part of
/// the public API and we don't want to depend on internals.
///
/// When the store is unavailable the result reports `"found": false`
/// with `node_id` echoed back so the HTTP handler can distinguish
/// "store cold" (503) from "no such node" (404) — this matches the
/// pattern already used by `delete_node` / `trigger_consolidate`.
pub(crate) fn get_node(
    memory_store: Option<&Arc<GrafeoStore>>,
    node_id: u64,
) -> GetNodeOutput {
    let not_available = GetNodeOutput {
        node_id,
        found: false,
        node_type: String::new(),
        content: String::new(),
        confidence: 0.0,
        decay_score: 0.0,
        created_at: 0,
        last_accessed_at: 0,
        access_count: 0,
        status: String::new(),
        properties: HashMap::new(),
        message: "Memory store not available".to_string(),
    };
    let store = match memory_store {
        Some(s) => s,
        None => {
            tracing::warn!("memory get_node: no Grafeo store available");
            return not_available;
        }
    };
    let target = grafeo_common::types::NodeId(node_id);
    for label in &MEMORY_LABELS {
        let ids = store.db().graph_store().nodes_by_label(label);
        if !ids.contains(&target) {
            continue;
        }
        let n = match store.db().get_node(target) {
            Some(n) => n,
            None => {
                // Index says the node exists but the lookup failed —
                // should not happen in practice, but treat as "not found".
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
        let decay_score = n
            .get_property("decay_score")
            .and_then(|v| v.as_float64())
            .unwrap_or(1.0);
        let status = n
            .get_property("status")
            .and_then(|v| v.as_str())
            .unwrap_or("Active")
            .to_string();

        // Snapshot every property the engine attached to the node so the
        // desktop UI can render detail views without us having to know
        // the per-label schema in advance. Reuse the `Serialize`
        // impl on `grafeo_common::types::Value` (it already covers
        // every variant — Int64/Float64/String/Bool/Timestamp/List/
        // Map/Vector/Bytes/... ) so we don't have to enumerate them
        // ourselves and risk drifting from upstream.
        let mut properties: HashMap<String, serde_json::Value> = HashMap::new();
        for (key, value) in n.properties_as_btree() {
            let json_val = serde_json::to_value(&value).unwrap_or(serde_json::Value::Null);
            properties.insert(key.as_str().to_string(), json_val);
        }

        return GetNodeOutput {
            node_id,
            found: true,
            node_type: label.to_string(),
            content,
            confidence,
            decay_score,
            created_at,
            last_accessed_at,
            access_count,
            status,
            properties,
            message: "ok".to_string(),
        };
    }

    GetNodeOutput {
        node_id,
        found: false,
        node_type: String::new(),
        content: String::new(),
        confidence: 0.0,
        decay_score: 0.0,
        created_at: 0,
        last_accessed_at: 0,
        access_count: 0,
        status: String::new(),
        properties: HashMap::new(),
        message: "Node not found".to_string(),
    }
}

/// Delete a memory node by ID.
pub(crate) fn delete_node(
    memory_store: Option<&Arc<GrafeoStore>>,
    node_id: u64,
) -> DeleteOutput {
    let store = match memory_store {
        Some(s) => s,
        None => {
            return DeleteOutput {
                node_id,
                deleted: false,
                message: "Memory store not available".to_string(),
            };
        }
    };
    let id = grafeo_common::types::NodeId(node_id);
    match store.delete_node(id) {
        Ok(deleted) => DeleteOutput {
            node_id,
            deleted,
            message: if deleted {
                "Node deleted".to_string()
            } else {
                "Node not found".to_string()
            },
        },
        Err(e) => DeleteOutput {
            node_id,
            deleted: false,
            message: format!("Error: {}", e),
        },
    }
}

/// Trigger offline memory consolidation.
///
/// `force = true` short-circuits the `min_pending_age_hours` guard so
/// the operator can flush Pending nodes immediately. `retention_days`
/// is accepted for API compatibility but currently has no effect on
/// Phase 2 consolidation (only used by episodic cleanup, which is
/// scheduled separately and not driven by this endpoint).
pub(crate) fn trigger_consolidate(
    memory_store: Option<&Arc<GrafeoStore>>,
    force: bool,
    _retention_days: u32,
) -> ConsolidateOutput {
    let store = match memory_store {
        Some(s) => s,
        None => {
            return ConsolidateOutput {
                started: false,
                duration_ms: 0,
                episodes_consolidated: 0,
                knowledge_nodes_generated: 0,
                message: "Memory store not available".to_string(),
            };
        }
    };
    let config = acowork_grafeo::consolidation::OfflineConsolidationConfig {
        batch_size: 50,
        min_pending_age_hours: if force { 0 } else { 1 },
    };
    let start = std::time::Instant::now();
    match store.run_offline_consolidation(&config) {
        Ok(result) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            ConsolidateOutput {
                started: true,
                duration_ms,
                episodes_consolidated: result.upgraded as u64,
                knowledge_nodes_generated: 0, // Phase 2 doesn't generate new nodes
                message: format!(
                    "Upgraded: {}, Kept pending: {}, Marked dormant: {}",
                    result.upgraded, result.kept_pending, result.marked_dormant
                ),
            }
        }
        Err(e) => ConsolidateOutput {
            started: false,
            duration_ms: 0,
            episodes_consolidated: 0,
            knowledge_nodes_generated: 0,
            message: format!("Consolidation error: {}", e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().expect("in-memory store should open")
    }

    #[test]
    fn list_nodes_returns_empty_when_no_store() {
        let out = list_nodes(None, ListNodesParams::default());
        assert_eq!(out.total, 0);
        assert_eq!(out.page, 0);
        assert_eq!(out.size, 0);
        assert!(out.nodes.is_empty());
    }

    #[test]
    fn get_stats_returns_zero_when_no_store() {
        let out = get_stats(None, 512);
        assert_eq!(out.total_nodes, 0);
        assert_eq!(out.model_dim, 512);
        assert_eq!(out.index_health, "no_store");
    }

    #[test]
    fn delete_node_reports_unavailable_without_store() {
        let out = delete_node(None, 42);
        assert_eq!(out.node_id, 42);
        assert!(!out.deleted);
        assert_eq!(out.message, "Memory store not available");
    }

    #[test]
    fn trigger_consolidate_reports_unavailable_without_store() {
        let out = trigger_consolidate(None, true, 30);
        assert!(!out.started);
        assert_eq!(out.episodes_consolidated, 0);
    }

    #[test]
    fn delete_node_reports_not_found_for_missing_id() {
        let store = Arc::new(make_store());
        let out = delete_node(Some(&store), 9_999_999);
        assert_eq!(out.node_id, 9_999_999);
        assert!(!out.deleted);
        assert_eq!(out.message, "Node not found");
    }

    #[test]
    fn get_node_reports_unavailable_without_store() {
        let out = get_node(None, 42);
        assert_eq!(out.node_id, 42);
        assert!(!out.found);
        assert_eq!(out.message, "Memory store not available");
        assert!(out.properties.is_empty());
    }

    #[test]
    fn get_node_reports_not_found_for_missing_id() {
        let store = Arc::new(make_store());
        let out = get_node(Some(&store), 9_999_999);
        assert_eq!(out.node_id, 9_999_999);
        assert!(!out.found);
        assert_eq!(out.message, "Node not found");
        assert!(out.node_type.is_empty());
        assert!(out.properties.is_empty());
    }

    #[test]
    fn get_output_to_json_includes_found_flag_and_properties() {
        let out = GetNodeOutput {
            node_id: 7,
            found: true,
            node_type: "Episodic".to_string(),
            content: "[user] hi".to_string(),
            confidence: 0.5,
            decay_score: 0.9,
            created_at: 100,
            last_accessed_at: 100,
            access_count: 0,
            status: "Active".to_string(),
            properties: HashMap::new(),
            message: "ok".to_string(),
        };
        let v = get_output_to_json(&out);
        assert_eq!(v["node_id"], 7);
        assert_eq!(v["found"], true);
        assert_eq!(v["node_type"], "Episodic");
        assert_eq!(v["message"], "ok");
    }

    #[test]
    fn list_nodes_handles_unfiltered_scan_against_real_store() {
        let store = Arc::new(make_store());
        let out = list_nodes(Some(&store), ListNodesParams::default());
        // Empty store → empty result, well-formed shape.
        assert_eq!(out.total, 0);
        assert!(out.nodes.is_empty());
    }

    #[test]
    fn get_stats_against_real_store_reports_zero_total() {
        let store = Arc::new(make_store());
        let out = get_stats(Some(&store), 384);
        assert_eq!(out.total_nodes, 0);
        assert_eq!(out.model_dim, 384);
        assert_eq!(out.index_health, "healthy");
    }

    #[test]
    fn list_output_to_json_serialises_pagination() {
        let out = ListNodesOutput {
            total: 1,
            page: 1,
            size: 20,
            nodes: vec![MemoryNodeRecord {
                node_id: 7,
                node_type: "Episodic".to_string(),
                content: "[user] hi".to_string(),
                confidence: 0.5,
                decay_score: 0.9,
                created_at: 100,
                last_accessed_at: 100,
                access_count: 0,
                status: "Active".to_string(),
            }],
            rejected_unfiltered: None,
        };
        let v = list_output_to_json(&out);
        assert_eq!(v["total"], 1);
        assert_eq!(v["page"], 1);
        assert_eq!(v["size"], 20);
        assert_eq!(v["nodes"][0]["node_id"], 7);
        assert_eq!(v["nodes"][0]["node_type"], "Episodic");
    }
}
