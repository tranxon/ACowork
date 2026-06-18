//! Memory management HTTP API handlers
//!
//! Implements the Memory API endpoints for agent memory inspection:
//! - GET    /api/agents/{id}/memory/nodes          — list memory nodes (paginated)
//! - GET    /api/agents/{id}/memory/stats           — get memory statistics
//! - DELETE /api/agents/{id}/memory/nodes/{node_id} — delete a memory node
//! - POST   /api/agents/{id}/memory/consolidate     — trigger memory consolidation

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::grpc::SharedGrpcSessionMgr;
use crate::http::routes::{ApiError, AppState};

/// Build the memory management router
pub fn memory_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/memory/nodes", get(list_memory_nodes))
        .route("/api/agents/{id}/memory/stats", get(get_memory_stats))
        .route(
            "/api/agents/{id}/memory/nodes/{node_id}",
            delete(delete_memory_node),
        )
        .route(
            "/api/agents/{id}/memory/consolidate",
            post(trigger_consolidate),
        )
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

/// Send a memory request to the Runtime via gRPC and wait for the response.
///
/// Encapsulates the "lock → push → unlock → timeout → cleanup" pattern
/// that was duplicated across all four memory HTTP handlers.
///
/// Returns `Some(ClientMessage)` on success, `None` if the agent is not
/// connected, the response times out, or the sender is dropped.
pub(crate) async fn grpc_memory_roundtrip(
    grpc_mgr: &SharedGrpcSessionMgr,
    agent_id: &str,
    query: acowork_core::proto::server_message::Payload,
) -> Option<acowork_core::proto::ClientMessage> {
    let (request_id, rx) = {
        let mut mgr = grpc_mgr.lock().await;
        match mgr.send_memory_request(agent_id, query) {
            Some(h) => h,
            None => return None,
        }
    }; // Lock released here — do NOT hold across the timeout await

    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(msg)) => Some(msg),
        Ok(Err(_)) => {
            tracing::warn!("Runtime dropped memory response sender");
            None
        }
        Err(_) => {
            tracing::warn!(agent_id = %agent_id, request_id, "Memory request timed out");
            grpc_mgr.lock().await.cleanup_pending(request_id);
            None
        }
    }
}

/// `GET /api/agents/{id}/memory/nodes` — list memory nodes for an agent
pub async fn list_memory_nodes(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<MemoryNodesQuery>,
) -> Result<Json<MemoryNodesListResponse>, (StatusCode, Json<ApiError>)> {
    let page = query.effective_page();
    let size = query.effective_size();

    tracing::info!(
        agent_id = %agent_id,
        page,
        size,
        r#type = ?query.r#type,
        keyword = ?query.keyword,
        time_range = ?query.time_range,
        "Memory API: list_memory_nodes"
    );

    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // Query Runtime via gRPC — lock only for push, then wait without lock
    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let proto_query = acowork_core::proto::server_message::Payload::MemoryNodesQuery(
            acowork_core::proto::MemoryNodesQuery {
                page,
                size,
                r#type: query.r#type.unwrap_or_default(),
                keyword: query.keyword.unwrap_or_default(),
                time_range: query.time_range.unwrap_or_default(),
            },
        );

        if let Some(response) = grpc_memory_roundtrip(grpc_mgr, &agent_id, proto_query).await {
            if let Some(acowork_core::proto::client_message::Payload::MemoryNodesResult(result)) =
                response.payload
            {
                let node_count = result.nodes.len();
                tracing::info!(
                    agent_id = %agent_id,
                    total = result.total,
                    node_count,
                    "Memory API: list_memory_nodes response"
                );
                let nodes = result
                    .nodes
                    .into_iter()
                    .map(|n| MemoryNodeResponse {
                        node_id: n.node_id,
                        node_type: n.node_type,
                        content: n.content,
                        confidence: n.confidence,
                        decay_score: n.decay_score,
                        created_at: n.created_at,
                        last_accessed_at: n.last_accessed_at,
                        access_count: n.access_count,
                        status: n.status,
                    })
                    .collect();
                return Ok(Json(MemoryNodesListResponse {
                    total: result.total,
                    page: result.page,
                    size: result.size,
                    nodes,
                }));
            }
            tracing::warn!(
                agent_id = %agent_id,
                "Memory API: unexpected response payload type"
            );
        }
    } else {
        tracing::warn!(
            agent_id = %agent_id,
            "Memory API: no gRPC session manager available"
        );
    }

    // No gRPC connection or query failed — return empty
    tracing::info!(
        agent_id = %agent_id,
        "Memory API: list_memory_nodes returning empty (no connection)"
    );
    Ok(Json(MemoryNodesListResponse {
        total: 0,
        page,
        size,
        nodes: vec![],
    }))
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
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // Query Runtime via gRPC — lock only for push, then wait without lock
    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let proto_query = acowork_core::proto::server_message::Payload::MemoryStatsQuery(
            acowork_core::proto::MemoryStatsQuery {},
        );

        if let Some(response) = grpc_memory_roundtrip(grpc_mgr, &agent_id, proto_query).await
            && let Some(acowork_core::proto::client_message::Payload::MemoryStatsResult(result)) =
                response.payload
            {
                return Ok(Json(MemoryStatsResponse {
                    total_nodes: result.total_nodes,
                    storage_bytes: result.storage_bytes,
                    by_type: result.by_type,
                    by_status: result.by_status,
                    avg_decay_score: result.avg_decay_score,
                    index_health: result.index_health,
                }));
            }
    }

    // No gRPC connection or query failed — return empty stats
    Ok(Json(MemoryStatsResponse {
        total_nodes: 0,
        storage_bytes: 0,
        by_type: std::collections::HashMap::new(),
        by_status: std::collections::HashMap::new(),
        avg_decay_score: 0.0,
        index_health: "not_connected".to_string(),
    }))
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
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // Query Runtime via gRPC — lock only for push, then wait without lock
    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let proto_query = acowork_core::proto::server_message::Payload::MemoryDeleteQuery(
            acowork_core::proto::MemoryDeleteQuery { node_id },
        );

        if let Some(response) = grpc_memory_roundtrip(grpc_mgr, &agent_id, proto_query).await
            && let Some(acowork_core::proto::client_message::Payload::MemoryDeleteResult(result)) =
                response.payload
            {
                return Ok(Json(DeleteNodeResponse {
                    node_id: result.node_id,
                    deleted: result.deleted,
                    message: result.message,
                }));
            }
    }

    // No gRPC connection — return error
    Err(ApiError::service_unavailable(
        "Agent not connected via gRPC",
    ))
}

/// `POST /api/agents/{id}/memory/consolidate` — trigger memory consolidation
pub async fn trigger_consolidate(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<ConsolidateRequest>,
) -> Result<Json<ConsolidateResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // Query Runtime via gRPC — lock only for push, then wait without lock
    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let proto_query = acowork_core::proto::server_message::Payload::MemoryConsolidateQuery(
            acowork_core::proto::MemoryConsolidateQuery {
                force: body.force.unwrap_or(false),
                retention_days: body.retention_days.unwrap_or(0),
            },
        );

        if let Some(response) = grpc_memory_roundtrip(grpc_mgr, &agent_id, proto_query).await
            && let Some(acowork_core::proto::client_message::Payload::MemoryConsolidateResult(
                result,
            )) = response.payload
            {
                return Ok(Json(ConsolidateResponse {
                    started: result.started,
                    duration_ms: result.duration_ms,
                    episodes_consolidated: result.episodes_consolidated,
                    knowledge_nodes_generated: result.knowledge_nodes_generated,
                    message: result.message,
                }));
            }
    }

    // No gRPC connection — return error
    Err(ApiError::service_unavailable(
        "Agent not connected via gRPC",
    ))
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
