//! NodeState — the GatewayState replacement (ADR-055 §6.20).
//!
//! In Phase 2a this holds only the runtime snapshot used by
//! `acowork-node status` and the info heartbeat. In Phase 2b the
//! Runtime process table (migrated from gateway `lifecycle/`) and the
//! local install table (migrated from gateway `package_manager/`)
//! land here — `SharedState` / `GatewayError` references are
//! re-based onto this type + [`crate::error::NodeError`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::Result;
use crate::identity::NodeIdentity;

/// A Runtime process slot in the node's process table.
///
/// Populated by `process::spawn` (migrated from gateway
/// `lifecycle/manager.rs`). Holds only what process management needs;
/// `connected` / `ready` are the Gateway's MQTT-driven concern
/// (AgentHello handshake), not the node's.
#[derive(Debug, Clone)]
pub struct AgentSlot {
    pub agent_id: String,
    /// PID on THIS machine (reported to the Gateway for diagnostics
    /// only — the Gateway never signals it directly).
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    /// Workspace directory path on this machine (diagnostics/display).
    pub workspace: String,
    /// Whether the agent was started in developer mode.
    pub dev_mode: bool,
    /// Debug Protocol port hint (set when dev_mode is true).
    pub debug_port: Option<u16>,
    /// Loopback HTTP port the Runtime listens on (allocated by the Node
    /// at spawn time, ADR-055 §6.4). The node reverse proxy routes
    /// `/agents/{id}/*` here — the node keeps the `{id} → port` mapping
    /// private to itself.
    pub http_port: u16,
}

/// An installed agent package on this node (ADR-055 §6.5: the Node is
/// the authority for the local copy of agent packages).
///
/// Migrated from the Gateway's `AgentInfo` (`gateway/state.rs`) with
/// the node_id field removed — the node's own install table is always
/// local, so there is no node discriminator here.
#[derive(Debug, Clone)]
pub struct InstalledAgent {
    pub agent_id: String,
    pub version: String,
    pub name: String,
    pub install_path: String,
    pub manifest: acowork_core::AgentManifest,
}

/// A running-agent summary persisted in [`NodeRuntimeSnapshot`] so the
/// node-local `agents list` / `agents kill` CLI can report running
/// state and PIDs without talking to the daemon (ADR-055 §6.13.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub pid: u32,
    pub http_port: u16,
    pub started_at: DateTime<Utc>,
    pub dev_mode: bool,
}

/// Point-in-time snapshot persisted to `{home}/state.json` so
/// `acowork-node status` works without talking to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeRuntimeSnapshot {
    /// Whether the daemon currently has an MQTT connection to the
    /// Gateway broker.
    pub connected: bool,
    pub last_connected_at: Option<DateTime<Utc>>,
    /// Gateway address the daemon last connected to.
    pub gateway_addr: Option<String>,
    /// Running agent count (always 0 until Phase 2b).
    pub agent_count: usize,
    /// Running-agent summaries (PID + loopback HTTP port) for the
    /// node-local CLI (`agents list` / `agents kill`).
    #[serde(default)]
    pub agents: Vec<AgentSnapshot>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Node Agent in-memory state.
#[derive(Debug)]
pub struct NodeState {
    /// Capacity limit (§6.18) — enforced by `process` in Phase 2b.
    pub max_agents: u32,
    /// Runtime process table (running agents, `agent_id` → slot).
    pub agents: HashMap<String, AgentSlot>,
    /// Local install table (installed agents, `agent_id` → info).
    pub installed_agents: HashMap<String, InstalledAgent>,
    /// Node-local LSP relay process state (ADR-055 §6.7, Phase 4).
    /// Managed by `sidecar::lsp_relay_supervisor` — the Node now hosts
    /// the relay instead of the Gateway. `None` while stopped.
    pub lsp_relay_process: Option<crate::sidecar::lsp_relay::LspRelayProcessState>,
    snapshot: NodeRuntimeSnapshot,
}

impl NodeState {
    pub fn new(max_agents: u32) -> Self {
        Self {
            max_agents,
            agents: HashMap::new(),
            installed_agents: HashMap::new(),
            lsp_relay_process: None,
            snapshot: NodeRuntimeSnapshot::default(),
        }
    }

    /// Whether an agent is installed on this node.
    pub fn is_installed(&self, agent_id: &str) -> bool {
        self.installed_agents.contains_key(agent_id)
    }

    /// Whether an agent process is tracked as running on this node.
    pub fn is_running(&self, agent_id: &str) -> bool {
        self.agents.contains_key(agent_id)
    }

    /// Add an installed agent to the local install table.
    pub fn add_installed(&mut self, info: InstalledAgent) {
        self.installed_agents.insert(info.agent_id.clone(), info);
    }

    /// Remove an installed agent from the local install table.
    pub fn remove_installed(&mut self, agent_id: &str) -> Option<InstalledAgent> {
        self.installed_agents.remove(agent_id)
    }

    /// Add a running agent slot to the process table.
    pub fn add_agent(&mut self, slot: AgentSlot) {
        self.agents.insert(slot.agent_id.clone(), slot);
    }

    /// Remove a running agent slot from the process table.
    pub fn remove_agent(&mut self, agent_id: &str) -> Option<AgentSlot> {
        self.agents.remove(agent_id)
    }

    pub fn set_connected(&mut self, connected: bool, gateway_addr: Option<String>) {
        self.snapshot.connected = connected;
        if connected {
            self.snapshot.last_connected_at = Some(Utc::now());
            self.snapshot.gateway_addr = gateway_addr;
        }
        self.snapshot.updated_at = Some(Utc::now());
    }

    pub fn snapshot(&self) -> &NodeRuntimeSnapshot {
        &self.snapshot
    }

    /// Persist the snapshot to `{home}/state.json` (best-effort:
    /// failures are logged, never fatal — the status command simply
    /// sees stale data).
    pub fn save_snapshot(&self, home: &Path) {
        let path = home.join("state.json");
        // Materialize a snapshot that includes the running-agent table
        // so the node-local CLI can list/kill without the daemon.
        let mut snap = self.snapshot.clone();
        snap.agents = self
            .agents
            .values()
            .map(|s| AgentSnapshot {
                agent_id: s.agent_id.clone(),
                pid: s.pid,
                http_port: s.http_port,
                started_at: s.started_at,
                dev_mode: s.dev_mode,
            })
            .collect();
        snap.agent_count = snap.agents.len();
        match serde_json::to_string_pretty(&snap) {
            Ok(content) => {
                if let Err(e) = std::fs::write(&path, content) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to persist node runtime snapshot"
                    );
                }
            }
            Err(e) => tracing::warn!(error = %e, "Failed to serialize node runtime snapshot"),
        }
    }

    /// Load the persisted snapshot written by a daemon run.
    pub fn load_snapshot(home: &Path) -> Option<NodeRuntimeSnapshot> {
        let path = home.join("state.json");
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

/// Thread-safe shared NodeState.
pub type SharedNodeState = std::sync::Arc<RwLock<NodeState>>;

/// HTTP router state shared by the node's reverse-proxy and fs_browse
/// routers (merged onto one listener, ADR-055 §6.4).
///
/// Phase 5a: carries the live [`NodeIdentity`] so the proxy can
/// validate inbound `X-ACowork-Node-Token` headers against the
/// Gateway-issued `node_token` (§6.8).
#[derive(Clone)]
pub struct NodeHttpState {
    /// Runtime process table + local install table.
    pub node: SharedNodeState,
    /// Live identity — `node_token` is read on every proxied request.
    pub identity: Arc<RwLock<NodeIdentity>>,
}

/// Load a persisted snapshot, mapped to the crate Result for CLI use.
pub fn read_snapshot(home: &Path) -> Result<Option<NodeRuntimeSnapshot>> {
    Ok(NodeState::load_snapshot(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = NodeState::new(16);
        state.set_connected(true, Some("127.0.0.1:19875".to_string()));

        state.save_snapshot(tmp.path());
        let loaded = NodeState::load_snapshot(tmp.path()).unwrap();
        assert!(loaded.connected);
        assert_eq!(loaded.gateway_addr.as_deref(), Some("127.0.0.1:19875"));
        assert!(loaded.last_connected_at.is_some());
    }

    #[test]
    fn missing_snapshot_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(NodeState::load_snapshot(tmp.path()).is_none());
    }

    #[test]
    fn set_connected_false_keeps_last_connected_at() {
        let mut state = NodeState::new(16);
        state.set_connected(true, Some("a:1".into()));
        let first = state.snapshot().last_connected_at.unwrap();
        state.set_connected(false, None);
        assert!(!state.snapshot().connected);
        assert_eq!(state.snapshot().last_connected_at, Some(first));
    }
}
