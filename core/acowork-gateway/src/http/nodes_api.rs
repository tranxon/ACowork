//! Node management API (ADR-055 §6.13.3 / Phase 3g).
//!
//! Exposes the Gateway's [`crate::mqtt::node_registry::NodeRegistry`]
//! (LWT-driven online state + retained `NodeInfo` metadata) over HTTP so
//! the Desktop "Node Management" page and the install node-picker can
//! render the node topology without talking MQTT directly. The registry
//! is the Gateway-side source of truth for "which nodes exist / are
//! online" (Phase 2a) — this endpoint is a read-only projection of it.
//!
//! Prior to this endpoint the only node view was the `acowork-gateway
//! nodes list` CLI, which drains retained topics straight from the broker
//! (no daemon state). The HTTP endpoint reads the *daemon's* in-memory
//! registry instead, so it reflects the live Gateway's view.

use axum::{extract::State, routing::get, Json, Router};

use serde::Serialize;

use crate::http::routes::AppState;

/// Response for `GET /api/nodes` — a single Node Agent's live view.
///
/// Fields that depend on the retained `NodeInfo` snapshot (hostname, os,
/// arch, version, counts, endpoint) are `None` until the node publishes
/// its first info message; a node discovered only via the status topic
/// still appears with its `node_id` + `online` state.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeResponse {
    /// Logical node id (`local` for the Gateway's own node).
    pub node_id: String,
    /// Whether the node is currently online (status topic / LWT).
    pub online: bool,
    /// UTC timestamp of the last online transition (RFC 3339, None while
    /// the node has never been observed online).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online_since: Option<String>,
    /// Machine fingerprint (UUID v4) from the info snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,
    /// Node hostname (info snapshot).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Node OS (`std::env::consts::OS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Node architecture (`std::env::consts::ARCH`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Version of the acowork-node binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    /// Node control-plane protocol version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    /// Capability tags (grows over the ADR-055 phases).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Maximum concurrent Runtime processes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agents: Option<u32>,
    /// Current running agent count (informational heartbeat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_count: Option<u32>,
    /// This node's reverse-proxy base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_endpoint: Option<String>,
}

/// `GET /api/nodes` — list all known nodes (online + offline).
///
/// Reads the daemon's in-memory [`crate::mqtt::SharedNodeRegistry`],
/// sorted by node_id (stable ordering for the Desktop table).
pub async fn list_nodes(State(state): State<AppState>) -> Json<Vec<NodeResponse>> {
    let nodes = match state.node_registry.as_ref() {
        Some(registry) => registry.read().await.list_nodes(),
        // No node registry (MQTT disabled) — empty topology, not an error.
        None => Vec::new(),
    };

    let resp = nodes
        .into_iter()
        .map(|n| {
            let info = n.info.as_ref();
            NodeResponse {
                node_id: n.node_id,
                online: n.online,
                online_since: n.online_since.map(|t| t.to_rfc3339()),
                machine_uid: info.map(|i| i.machine_uid.clone()),
                hostname: info.map(|i| i.hostname.clone()),
                os: info.map(|i| i.os.clone()),
                arch: info.map(|i| i.arch.clone()),
                node_version: info.map(|i| i.node_version.clone()),
                protocol_version: info.map(|i| i.protocol_version),
                capabilities: info.map(|i| i.capabilities.clone()).unwrap_or_default(),
                max_agents: info.map(|i| i.max_agents),
                agent_count: info.map(|i| i.agent_count),
                http_endpoint: info.map(|i| i.http_endpoint.clone()),
            }
        })
        .collect();

    Json(resp)
}

/// Route definitions for the node management API.
pub fn nodes_routes() -> Router<AppState> {
    Router::new().route("/api/nodes", get(list_nodes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::routes::AppState;
    use crate::mqtt::node_registry::new_shared_registry;
    use acowork_core::mqtt_proto::NodeInfo;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-nodes-api-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        state.node_registry = Some(new_shared_registry());
        state
    }

    fn info(node_id: &str) -> NodeInfo {
        NodeInfo {
            node_id: node_id.to_string(),
            machine_uid: "uid-1".to_string(),
            hostname: "gpu-box".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec!["process".to_string(), "package".to_string()],
            max_agents: 16,
            agent_count: 2,
            http_endpoint: "http://10.0.0.2:19900".to_string(),
        }
    }

    #[tokio::test]
    async fn empty_registry_returns_empty_list() {
        let state = test_app_state();
        let resp = list_nodes(State(state)).await;
        assert!(resp.0.is_empty());
    }

    #[tokio::test]
    async fn status_only_node_has_online_without_info() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/local/status", b"online");
        }
        let resp = list_nodes(State(state)).await;
        assert_eq!(resp.0.len(), 1);
        let node = &resp.0[0];
        assert_eq!(node.node_id, "local");
        assert!(node.online);
        assert!(node.online_since.is_some());
        assert!(node.hostname.is_none());
        assert!(node.agent_count.is_none());
    }

    #[tokio::test]
    async fn info_populates_metadata_fields() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            let envelope = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::NodeInfo(
                    info("gpu-1"),
                )),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            reg.update_info_from_mqtt("acowork/nodes/gpu-1/info", &bytes);
        }
        let resp = list_nodes(State(state)).await;
        assert_eq!(resp.0.len(), 1);
        let node = &resp.0[0];
        assert_eq!(node.hostname.as_deref(), Some("gpu-box"));
        assert_eq!(node.os.as_deref(), Some("linux"));
        assert_eq!(node.arch.as_deref(), Some("x86_64"));
        assert_eq!(node.node_version.as_deref(), Some("0.1.0"));
        assert_eq!(node.protocol_version, Some(1));
        assert_eq!(node.capabilities, vec!["process", "package"]);
        assert_eq!(node.max_agents, Some(16));
        assert_eq!(node.agent_count, Some(2));
        assert_eq!(node.http_endpoint.as_deref(), Some("http://10.0.0.2:19900"));
    }

    #[tokio::test]
    async fn nodes_are_sorted_by_id() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/zeta/status", b"online");
            reg.update_status_from_mqtt("acowork/nodes/alpha/status", b"online");
        }
        let resp = list_nodes(State(state)).await;
        let ids: Vec<&str> = resp.0.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }
}
