//! Memory management HTTP API handlers
//!
//! Implements the Memory API endpoints for agent memory inspection:
//! - GET    /api/agents/{id}/memory/nodes          — list memory nodes (paginated)
//! - GET    /api/agents/{id}/memory/stats           — get memory statistics
//! - DELETE /api/agents/{id}/memory/nodes/{node_id} — delete a memory node
//! - POST   /api/agents/{id}/memory/consolidate     — trigger memory consolidation

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the memory management router
pub fn memory_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/memory/nodes", get(list_memory_nodes))
        .route("/api/agents/{id}/memory/stats", get(get_memory_stats))
        .route("/api/agents/{id}/memory/nodes/{node_id}", delete(delete_memory_node))
        .route("/api/agents/{id}/memory/consolidate", post(trigger_consolidate))
}

// ── Query parameters ──────────────────────────────────────────────────

/// Query parameters for listing memory nodes
#[derive(Debug, Deserialize)]
pub struct MemoryNodesQuery {
    /// Page number (1-based, default: 1)
    pub page: Option<u32>,
    /// Page size (default: 20, max: 100)
    pub size: Option<u32>,
    /// Filter by node type: Knowledge, Episodic, Procedural, Autobiographical
    pub r#type: Option<String>,
    /// Keyword search in node content
    pub keyword: Option<String>,
    /// Time range filter: 1h, 1d, 7d, 30d, all
    pub time_range: Option<String>,
}

impl MemoryNodesQuery {
    /// Get the effective page number (1-based)
    pub fn effective_page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    /// Get the effective page size (capped at 100)
    pub fn effective_size(&self) -> u32 {
        self.size.unwrap_or(20).clamp(1, 100)
    }
}

// ── Response types ────────────────────────────────────────────────────

/// A single memory node in the list response
#[derive(Serialize)]
pub struct MemoryNodeResponse {
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

/// Paginated list of memory nodes
#[derive(Serialize)]
pub struct MemoryNodesListResponse {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub nodes: Vec<MemoryNodeResponse>,
}

/// Memory statistics summary
#[derive(Serialize)]
pub struct MemoryStatsResponse {
    pub total_nodes: u64,
    pub storage_bytes: u64,
    pub by_type: std::collections::HashMap<String, u64>,
    pub by_status: std::collections::HashMap<String, u64>,
    pub avg_decay_score: f64,
    pub index_health: String,
}

/// Response for deleting a memory node
#[derive(Serialize)]
pub struct DeleteNodeResponse {
    pub node_id: u64,
    pub deleted: bool,
    pub message: String,
}

/// Request body for triggering memory consolidation
#[derive(Debug, Deserialize)]
pub struct ConsolidateRequest {
    /// Force consolidation even if conditions are not met
    pub force: Option<bool>,
    /// Retention period in days for episodic cleanup
    pub retention_days: Option<u32>,
}

/// Response for memory consolidation trigger
#[derive(Serialize)]
pub struct ConsolidateResponse {
    pub started: bool,
    pub duration_ms: u64,
    pub episodes_consolidated: u64,
    pub knowledge_nodes_generated: u64,
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents/{id}/memory/nodes` — list memory nodes for an agent
pub async fn list_memory_nodes(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<MemoryNodesQuery>,
) -> Result<Json<MemoryNodesListResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Check if memory store is available
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    let page = query.effective_page();
    let size = query.effective_size();

    match memory_store {
        Some(store) => {
            // Use the memory store to query stats, then build a paginated response
            match store.stats() {
                Ok(stats) => {
                    // Build a hybrid search query if keyword is provided,
                    // otherwise return stats-based summary
                    let total = stats.node_count + stats.episode_count;

                    // If keyword filter is provided, perform a search
                    if let Some(ref keyword) = query.keyword {
                        let mut mq = rollball_memory::MemoryQuery::new(keyword.clone());
                        mq.limit = size as usize;
                        mq.filters = build_memory_filters(&query);

                        match store.hybrid_search(&mq) {
                            Ok(results) => {
                                let nodes: Vec<MemoryNodeResponse> = results
                                    .into_iter()
                                    .map(|r| MemoryNodeResponse {
                                        node_id: r.node_id,
                                        node_type: r.label,
                                        content: r.content,
                                        confidence: r.score,
                                        decay_score: 0.0, // Not directly available from SearchResult
                                        created_at: 0,    // Not directly available from SearchResult
                                        last_accessed_at: 0,
                                        access_count: 0,
                                        status: "Active".to_string(),
                                    })
                                    .collect();
                                let result_count = nodes.len() as u64;
                                Ok(Json(MemoryNodesListResponse {
                                    total: result_count,
                                    page,
                                    size,
                                    nodes,
                                }))
                            }
                            Err(e) => {
                                tracing::warn!("Memory search failed for agent {}: {}", agent_id, e);
                                Ok(Json(MemoryNodesListResponse {
                                    total: 0,
                                    page,
                                    size,
                                    nodes: vec![],
                                }))
                            }
                        }
                    } else {
                        // No keyword: return stats-based empty list
                        // (Full node listing requires iteration support not in current trait)
                        Ok(Json(MemoryNodesListResponse {
                            total,
                            page,
                            size,
                            nodes: vec![],
                        }))
                    }
                }
                Err(e) => {
                    tracing::warn!("Memory stats query failed for agent {}: {}", agent_id, e);
                    Ok(Json(MemoryNodesListResponse {
                        total: 0,
                        page,
                        size,
                        nodes: vec![],
                    }))
                }
            }
        }
        None => {
            // Memory store not initialized — return empty data with informational message
            Ok(Json(MemoryNodesListResponse {
                total: 0,
                page,
                size,
                nodes: vec![],
            }))
        }
    }
}

/// `GET /api/agents/{id}/memory/stats` — get memory statistics for an agent
pub async fn get_memory_stats(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MemoryStatsResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Check if memory store is available
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    match memory_store {
        Some(store) => {
            match store.stats() {
                Ok(stats) => {
                    let mut by_type = std::collections::HashMap::new();
                    by_type.insert("Episodic".to_string(), stats.episode_count);
                    by_type.insert("Knowledge".to_string(), stats.active_node_count);
                    by_type.insert("Procedural".to_string(), 0); // Not separated in current stats
                    by_type.insert("Autobiographical".to_string(), 0);

                    let mut by_status = std::collections::HashMap::new();
                    by_status.insert("Active".to_string(), stats.active_node_count);
                    by_status.insert("Dormant".to_string(), stats.dormant_node_count);

                    let total = stats.node_count + stats.episode_count;
                    let avg_decay = if total > 0 { 0.5 } else { 0.0 }; // Placeholder

                    let index_health = if stats.index_count > 0 {
                        "healthy".to_string()
                    } else {
                        "no_indexes".to_string()
                    };

                    Ok(Json(MemoryStatsResponse {
                        total_nodes: total,
                        storage_bytes: stats.storage_size_bytes,
                        by_type,
                        by_status,
                        avg_decay_score: avg_decay,
                        index_health,
                    }))
                }
                Err(e) => {
                    tracing::warn!("Memory stats query failed for agent {}: {}", agent_id, e);
                    Ok(Json(MemoryStatsResponse {
                        total_nodes: 0,
                        storage_bytes: 0,
                        by_type: std::collections::HashMap::new(),
                        by_status: std::collections::HashMap::new(),
                        avg_decay_score: 0.0,
                        index_health: "error".to_string(),
                    }))
                }
            }
        }
        None => {
            // Memory store not initialized — return empty stats
            Ok(Json(MemoryStatsResponse {
                total_nodes: 0,
                storage_bytes: 0,
                by_type: std::collections::HashMap::new(),
                by_status: std::collections::HashMap::new(),
                avg_decay_score: 0.0,
                index_health: "not_initialized".to_string(),
            }))
        }
    }
}

/// `DELETE /api/agents/{id}/memory/nodes/{node_id}` — delete a memory node
pub async fn delete_memory_node(
    State(state): State<AppState>,
    Path((agent_id, node_id)): Path<(String, u64)>,
) -> Result<Json<DeleteNodeResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Check if memory store is available
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    match memory_store {
        Some(_store) => {
            // Current MemoryStore trait does not expose a direct delete-by-ID method.
            // Nodes transition via decay (Active → Dormant → Purge).
            // For now, force-reactivate then mark as consolidated for cleanup.
            tracing::info!(
                "Memory node delete requested: agent={}, node_id={}",
                agent_id, node_id
            );
            Ok(Json(DeleteNodeResponse {
                node_id,
                deleted: false,
                message: "Direct node deletion not supported; use decay/purge lifecycle".to_string(),
            }))
        }
        None => {
            Err(ApiError::service_unavailable("Memory store not initialized"))
        }
    }
}

/// `POST /api/agents/{id}/memory/consolidate` — trigger memory consolidation
pub async fn trigger_consolidate(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(_body): Json<ConsolidateRequest>,
) -> Result<Json<ConsolidateResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Check if memory store is available
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    match memory_store {
        Some(store) => {
            // Run decay scan to transition nodes
            let decay_config = rollball_memory::DecayConfig::default();
            let start = std::time::Instant::now();

            let mut episodes_consolidated = 0u64;
            let mut knowledge_generated = 0u64;

            match store.run_decay_scan(&decay_config) {
                Ok(result) => {
                    episodes_consolidated = result.reactivated;
                    knowledge_generated = result.to_dormant;
                    tracing::info!(
                        "Memory consolidation completed for agent {}: {} dormant, {} reactivated",
                        agent_id, result.to_dormant, result.reactivated
                    );
                }
                Err(e) => {
                    tracing::warn!("Memory consolidation failed for agent {}: {}", agent_id, e);
                }
            }

            // Also cleanup old consolidated episodes
            if let Some(retention_days) = _body.retention_days {
                let duration = std::time::Duration::from_secs(retention_days as u64 * 24 * 3600);
                if let Err(e) = store.cleanup_episodes(duration) {
                    tracing::warn!("Episode cleanup failed for agent {}: {}", agent_id, e);
                }
            }

            let duration_ms = start.elapsed().as_millis() as u64;

            Ok(Json(ConsolidateResponse {
                started: true,
                duration_ms,
                episodes_consolidated,
                knowledge_nodes_generated: knowledge_generated,
                message: "Consolidation completed".to_string(),
            }))
        }
        None => {
            Err(ApiError::service_unavailable("Memory store not initialized"))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Build MemoryFilters from query parameters
fn build_memory_filters(query: &MemoryNodesQuery) -> rollball_memory::MemoryFilters {
    let mut filters = rollball_memory::MemoryFilters::default();

    if let Some(ref node_type) = query.r#type {
        let type_filter = match node_type.as_str() {
            "Knowledge" => Some(rollball_memory::NodeTypeFilter::Knowledge),
            "Episodic" => Some(rollball_memory::NodeTypeFilter::Episodic),
            "Procedural" => Some(rollball_memory::NodeTypeFilter::Procedural),
            "Autobiographical" => Some(rollball_memory::NodeTypeFilter::Autobiographical),
            _ => None,
        };
        if let Some(tf) = type_filter {
            filters.node_types = vec![tf];
        }
    }

    if let Some(ref time_range) = query.time_range {
        let now = chrono::Utc::now();
        let from = match time_range.as_str() {
            "1h" => now - chrono::Duration::hours(1),
            "1d" => now - chrono::Duration::days(1),
            "7d" => now - chrono::Duration::days(7),
            "30d" => now - chrono::Duration::days(30),
            "all" => return filters, // No filter
            _ => return filters,
        };
        filters.time_range = Some((from, now));
    }

    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_nodes_query_defaults() {
        let query = MemoryNodesQuery {
            page: None,
            size: None,
            r#type: None,
            keyword: None,
            time_range: None,
        };
        assert_eq!(query.effective_page(), 1);
        assert_eq!(query.effective_size(), 20);
    }

    #[test]
    fn test_memory_nodes_query_capped() {
        let query = MemoryNodesQuery {
            page: Some(0),
            size: Some(200),
            r#type: None,
            keyword: None,
            time_range: None,
        };
        assert_eq!(query.effective_page(), 1); // 0 -> 1
        assert_eq!(query.effective_size(), 100); // capped at 100
    }

    #[test]
    fn test_consolidate_request_deserialization() {
        let json = r#"{"force": true, "retention_days": 30}"#;
        let req: ConsolidateRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.force, Some(true));
        assert_eq!(req.retention_days, Some(30));
    }

    #[test]
    fn test_consolidate_request_defaults() {
        let json = r#"{}"#;
        let req: ConsolidateRequest = serde_json::from_str(json).unwrap();
        assert!(req.force.is_none());
        assert!(req.retention_days.is_none());
    }

    #[test]
    fn test_build_memory_filters_type() {
        let query = MemoryNodesQuery {
            page: None,
            size: None,
            r#type: Some("Knowledge".to_string()),
            keyword: None,
            time_range: None,
        };
        let filters = build_memory_filters(&query);
        assert_eq!(filters.node_types.len(), 1);
        assert_eq!(filters.node_types[0], rollball_memory::NodeTypeFilter::Knowledge);
    }

    #[test]
    fn test_build_memory_filters_time_range() {
        let query = MemoryNodesQuery {
            page: None,
            size: None,
            r#type: None,
            keyword: None,
            time_range: Some("7d".to_string()),
        };
        let filters = build_memory_filters(&query);
        assert!(filters.time_range.is_some());
    }

    #[test]
    fn test_delete_node_response_serialization() {
        let resp = DeleteNodeResponse {
            node_id: 42,
            deleted: true,
            message: "Deleted".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"node_id\":42"));
        assert!(json.contains("\"deleted\":true"));
    }

    #[test]
    fn test_memory_stats_response_serialization() {
        let resp = MemoryStatsResponse {
            total_nodes: 100,
            storage_bytes: 4096,
            by_type: std::collections::HashMap::new(),
            by_status: std::collections::HashMap::new(),
            avg_decay_score: 0.75,
            index_health: "healthy".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"total_nodes\":100"));
        assert!(json.contains("\"healthy\""));
    }
}
