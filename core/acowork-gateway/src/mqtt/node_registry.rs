//! Node Registry (ADR-055 Phase 2a).
//!
//! Tracks Node Agents based on MQTT `acowork/nodes/{id}/status`
//! (plain text, Retained + LWT) and `acowork/nodes/{id}/info`
//! (protobuf `DataEnvelope<NodeInfo>`, Retained) messages. Mirrors
//! the `AgentRegistry` pattern (`agent_registry.rs`): LWT-driven
//! online/offline state + retained metadata snapshot.
//!
//! The registry is the Gateway-side source of truth for "which nodes
//! exist / are online" — used by the local-node supervisor (§6.11)
//! and, from Phase 2b on, by the lifecycle/package control plane
//! (`installed_agents.node_id` routing).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use prost::Message as _;
use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{data_envelope, DataEnvelope, NodeInfo};

/// Gateway-side view of one Node Agent.
#[derive(Debug, Clone)]
pub struct NodeInfoState {
    pub node_id: String,
    /// Whether the node is currently online (status topic; LWT flips
    /// this to false when the node dies ungracefully).
    pub online: bool,
    /// Last `NodeInfo` retained snapshot (None until the first info
    /// message arrives).
    pub info: Option<NodeInfo>,
    /// Node-local LSP relay endpoint (ADR-055 §6.7, Phase 4): the
    /// retained `acowork/nodes/{node_id}/lsps` AvailableLsps payload.
    /// `Some(endpoint)` while the node's relay is ready, `None` while
    /// unavailable or before the first message.
    pub lsp_endpoint: Option<String>,
    /// Machine fingerprint from the info snapshot — Gateway-side
    /// "same name, different machine" conflict detection (§6.12).
    pub machine_uid: Option<String>,
    /// Long-lived per-node credential issued at enroll time (ADR-055
    /// §6.12 / Phase 5a). Persisted in NodeTokenStore; this slot is
    /// the in-memory mirror so HTTP handlers (X-ACowork-Node-Token
    /// checks) and /api/nodes can read it without the store.
    pub node_token: Option<String>,
    /// When the registry last observed any message from this node.
    pub last_updated: Instant,
    /// UTC timestamp of the last observed online transition (None
    /// while the node has never been online).
    pub online_since: Option<DateTime<Utc>>,
}

/// In-memory registry of Node Agents.
#[derive(Debug, Default)]
pub struct NodeRegistry {
    nodes: HashMap<String, NodeInfoState>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update a node's status from a plain-text MQTT message.
    ///
    /// `topic` must match `acowork/nodes/{node_id}/status`; payload is
    /// "online" / "offline" (UTF-8 text, mirrors the agent status
    /// topic contract).
    pub fn update_status_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4
            || parts[0] != "acowork"
            || parts[1] != "nodes"
            || parts[3] != "status"
        {
            tracing::warn!(topic, "Invalid node status topic format");
            return;
        }
        let node_id = parts[2];
        if node_id.is_empty() {
            return;
        }

        let payload_str = match std::str::from_utf8(payload) {
            Ok(s) => s.trim(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    node_id,
                    error = %e,
                    "node status payload is not valid UTF-8 — treating as offline"
                );
                "offline"
            }
        };
        let online = payload_str == "online";

        let now = Instant::now();
        let entry = self.nodes.entry(node_id.to_string()).or_insert_with(|| {
            tracing::info!(node_id, "Node discovered via status topic");
            NodeInfoState {
                node_id: node_id.to_string(),
                online: false,
                info: None,
                lsp_endpoint: None,
                machine_uid: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        if online && !entry.online {
            entry.online_since = Some(Utc::now());
        } else if !online {
            entry.online_since = None;
        }
        entry.online = online;
        entry.last_updated = now;

        tracing::debug!(node_id, online, "Node registry status updated");
    }

    /// Update a node's metadata from a protobuf `DataEnvelope<NodeInfo>`
    /// MQTT message on `acowork/nodes/{node_id}/info`.
    pub fn update_info_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4
            || parts[0] != "acowork"
            || parts[1] != "nodes"
            || parts[3] != "info"
        {
            tracing::warn!(topic, "Invalid node info topic format");
            return;
        }
        let node_id = parts[2].to_string();

        let envelope = match DataEnvelope::decode(payload) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!(topic, error = %e, "node info payload is not a valid DataEnvelope");
                return;
            }
        };
        let info = match envelope.payload {
            Some(data_envelope::Payload::NodeInfo(info)) => info,
            _ => {
                tracing::warn!(
                    topic,
                    "node info topic carried a non-NodeInfo envelope"
                );
                return;
            }
        };

        if info.node_id != node_id {
            tracing::warn!(
                topic,
                envelope_node_id = %info.node_id,
                "node info payload node_id does not match topic — ignoring"
            );
            return;
        }

        let now = Instant::now();
        let machine_uid = Some(info.machine_uid.clone());
        let entry = self.nodes.entry(node_id.clone()).or_insert_with(|| {
            tracing::info!(node_id = %node_id, "Node discovered via info topic");
            NodeInfoState {
                node_id: node_id.clone(),
                online: false,
                info: None,
                lsp_endpoint: None,
                machine_uid: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        entry.info = Some(info);
        entry.machine_uid = machine_uid;
        entry.last_updated = now;
    }

    /// Update a node's LSP relay endpoint from the retained
    /// `acowork/nodes/{node_id}/lsps` topic (ADR-055 §6.7, Phase 4):
    /// protobuf `DataEnvelope<AvailableLsps>`. Ready → `Some(endpoint)`;
    /// unavailable or an empty retained payload (node cleared it) → None.
    pub fn update_lsps_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4
            || parts[0] != "acowork"
            || parts[1] != "nodes"
            || parts[3] != "lsps"
        {
            tracing::warn!(topic, "Invalid node lsps topic format");
            return;
        }
        let node_id = parts[2].to_string();

        let now = Instant::now();
        let entry = self.nodes.entry(node_id.clone()).or_insert_with(|| {
            tracing::info!(node_id = %node_id, "Node discovered via lsps topic");
            NodeInfoState {
                node_id: node_id.clone(),
                online: false,
                info: None,
                lsp_endpoint: None,
                machine_uid: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });

        // Empty retained payload = the node cleared its lsps state
        // (shutdown / leave) — the relay is no longer available.
        if payload.is_empty() {
            entry.lsp_endpoint = None;
            entry.last_updated = now;
            tracing::debug!(node_id = %node_id, "Node lsps cleared (retained empty)");
            return;
        }

        let envelope = match DataEnvelope::decode(payload) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!(topic, error = %e, "node lsps payload is not a valid DataEnvelope");
                return;
            }
        };
        let lsps = match envelope.payload {
            Some(data_envelope::Payload::AvailableLsps(lsps)) => lsps,
            _ => {
                tracing::warn!(topic, "node lsps topic carried a non-AvailableLsps envelope");
                return;
            }
        };

        entry.lsp_endpoint = if lsps.ready && !lsps.endpoint.is_empty() {
            Some(lsps.endpoint)
        } else {
            None
        };
        entry.last_updated = now;
        tracing::debug!(
            node_id = %node_id,
            endpoint = ?entry.lsp_endpoint,
            "Node registry lsps updated"
        );
    }

    /// Whether a node is currently online.
    pub fn is_online(&self, node_id: &str) -> bool {
        self.nodes.get(node_id).map(|n| n.online).unwrap_or(false)
    }

    /// Get the info snapshot for a node.
    pub fn get(&self, node_id: &str) -> Option<&NodeInfoState> {
        self.nodes.get(node_id)
    }

    /// All known nodes (online + offline).
    pub fn list_nodes(&self) -> Vec<NodeInfoState> {
        let mut nodes: Vec<NodeInfoState> = self.nodes.values().cloned().collect();
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        nodes
    }

    /// Number of online nodes.
    pub fn online_count(&self) -> usize {
        self.nodes.values().filter(|n| n.online).count()
    }

    /// Remove a node record (e.g. `nodes remove` CLI, Phase 2c).
    #[allow(dead_code)]
    pub fn remove(&mut self, node_id: &str) {
        self.nodes.remove(node_id);
    }

    /// Record the long-lived token minted at enroll time (ADR-055
    /// Phase 5a). Creates the entry when the node was never seen via
    /// status/info topics (enroll may arrive first).
    pub fn set_node_token(&mut self, node_id: &str, token: String) {
        let now = Instant::now();
        let entry = self.nodes.entry(node_id.to_string()).or_insert_with(|| {
            tracing::info!(node_id, "Node registered via enrollment");
            NodeInfoState {
                node_id: node_id.to_string(),
                online: false,
                info: None,
                lsp_endpoint: None,
                machine_uid: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        entry.node_token = Some(token);
        entry.last_updated = now;
    }
}

/// Thread-safe shared NodeRegistry.
pub type SharedNodeRegistry = Arc<RwLock<NodeRegistry>>;

/// Create a new shared NodeRegistry.
pub fn new_shared_registry() -> SharedNodeRegistry {
    Arc::new(RwLock::new(NodeRegistry::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_envelope(node_id: &str, machine_uid: &str) -> Vec<u8> {
        let info = NodeInfo {
            node_id: node_id.to_string(),
            machine_uid: machine_uid.to_string(),
            hostname: "h".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            max_agents: 16,
            agent_count: 0,
            http_endpoint: "http://127.0.0.1:19900".to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(info)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn status_topic_drives_online_state() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/nodes/local/status", b"online");
        assert!(registry.is_online("local"));
        assert_eq!(registry.online_count(), 1);
        assert!(registry.get("local").unwrap().online_since.is_some());

        // LWT flips it offline.
        registry.update_status_from_mqtt("acowork/nodes/local/status", b"offline");
        assert!(!registry.is_online("local"));
        assert!(registry.get("local").unwrap().online_since.is_none());
    }

    #[test]
    fn info_topic_populates_metadata() {
        let mut registry = NodeRegistry::new();
        registry.update_info_from_mqtt(
            "acowork/nodes/gpu-1/info",
            &info_envelope("gpu-1", "uid-1234"),
        );
        let node = registry.get("gpu-1").unwrap();
        assert_eq!(node.machine_uid.as_deref(), Some("uid-1234"));
        assert_eq!(node.info.as_ref().unwrap().protocol_version, 1);
        // Info alone does not imply online.
        assert!(!registry.is_online("gpu-1"));
    }

    #[test]
    fn node_id_mismatch_between_topic_and_payload_is_rejected() {
        let mut registry = NodeRegistry::new();
        registry.update_info_from_mqtt(
            "acowork/nodes/other/info",
            &info_envelope("gpu-1", "uid-1234"),
        );
        // The info is untrustworthy (topic says "other", payload says
        // "gpu-1") — no entry is created at all; the status topic will
        // create "other" separately when the node publishes liveness.
        assert!(registry.get("gpu-1").is_none());
        assert!(registry.get("other").is_none());
    }

    #[test]
    fn invalid_topics_are_ignored() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/agents/x/status", b"online");
        registry.update_status_from_mqtt("acowork/nodes/a/b/status", b"online");
        registry.update_info_from_mqtt("acowork/nodes/local/status", b"garbage");
        assert!(registry.list_nodes().is_empty());
    }

    #[test]
    fn list_nodes_is_sorted_by_node_id() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/nodes/zeta/status", b"online");
        registry.update_status_from_mqtt("acowork/nodes/alpha/status", b"online");
        let ids: Vec<String> = registry.list_nodes().into_iter().map(|n| n.node_id).collect();
        assert_eq!(ids, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    fn lsps_envelope(endpoint: &str, ready: bool) -> Vec<u8> {
        let lsps = acowork_core::mqtt_proto::AvailableLsps {
            version: 1,
            endpoint: endpoint.to_string(),
            ready,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AvailableLsps(lsps)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn lsps_topic_sets_endpoint_when_ready() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt(
            "acowork/nodes/gpu-1/lsps",
            &lsps_envelope("http://192.168.1.10:19878", true),
        );
        assert_eq!(
            registry.get("gpu-1").unwrap().lsp_endpoint.as_deref(),
            Some("http://192.168.1.10:19878")
        );
    }

    #[test]
    fn lsps_topic_clears_endpoint_when_not_ready() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt(
            "acowork/nodes/gpu-1/lsps",
            &lsps_envelope("http://192.168.1.10:19878", true),
        );
        registry.update_lsps_from_mqtt("acowork/nodes/gpu-1/lsps", &lsps_envelope("", false));
        assert!(registry.get("gpu-1").unwrap().lsp_endpoint.is_none());
    }

    #[test]
    fn lsps_topic_empty_retained_payload_clears_endpoint() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt(
            "acowork/nodes/gpu-1/lsps",
            &lsps_envelope("http://192.168.1.10:19878", true),
        );
        registry.update_lsps_from_mqtt("acowork/nodes/gpu-1/lsps", &[]);
        assert!(registry.get("gpu-1").unwrap().lsp_endpoint.is_none());
    }

    #[test]
    fn invalid_lsps_topics_are_ignored() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt("acowork/global/lsps", &lsps_envelope("x", true));
        registry.update_lsps_from_mqtt("acowork/nodes/a/b/lsps", &lsps_envelope("x", true));
        assert!(registry.list_nodes().is_empty());
    }
}
