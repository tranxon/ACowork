//! Agent Registry (ADR-033 Phase 1 scaffolding).
//!
//! Tracks agent online status based on MQTT `agents/{id}/status` Retained
//! messages. In Phase 1, this is a minimal in-memory map. In Phase 2+,
//! it replaces the gRPC `()` as the source of truth for
//! which agents are online.
//!
//! See `docs/zh/protocols/mqtt.md` §3.2 and §8.1 (Will Message).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

/// Online status of an agent, derived from MQTT retained status messages.
#[derive(Debug, Clone)]
pub struct AgentOnlineState {
    /// Whether the agent is currently online.
    pub online: bool,
    /// When this status was last updated (local wall clock).
    pub last_updated: Instant,
    /// The agent_id extracted from the topic.
    pub agent_id: String,
}

/// In-memory registry of agent online status.
///
/// Updated by subscribing to `acowork/agents/+/status` and parsing
/// the payload ("online" / "offline"). The Gateway uses this to
/// answer `GET /api/agents?status=active` without polling each Runtime.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentOnlineState>,
}

impl AgentRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update an agent's status from an MQTT message.
    ///
    /// `topic` should match `acowork/agents/{agent_id}/status`.
    /// `payload` should be "online" or "offline" (UTF-8 text).
    pub fn update_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        // Parse agent_id from topic: acowork/agents/{agent_id}/status
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4 || parts[0] != "acowork" || parts[1] != "agents" || parts[3] != "status" {
            tracing::warn!(topic, "Invalid agent status topic format");
            return;
        }

        let agent_id = parts[2].to_string();
        let payload_str = std::str::from_utf8(payload).unwrap_or("");
        let online = payload_str.trim() == "online";

        self.agents.insert(
            agent_id.clone(),
            AgentOnlineState {
                online,
                last_updated: Instant::now(),
                agent_id,
            },
        );

        tracing::debug!(agent_id = %parts[2], online, "Agent registry updated from MQTT");
    }

    /// Check if an agent is online.
    pub fn is_online(&self, agent_id: &str) -> bool {
        self.agents
            .get(agent_id)
            .map(|s| s.online)
            .unwrap_or(false)
    }

    /// Get all online agent IDs.
    pub fn online_agents(&self) -> Vec<String> {
        self.agents
            .iter()
            .filter(|(_, s)| s.online)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get the total number of tracked agents (online + offline).
    #[allow(dead_code)]
    pub fn total_tracked(&self) -> usize {
        self.agents.len()
    }

    /// Get the number of online agents.
    pub fn online_count(&self) -> usize {
        self.agents.values().filter(|s| s.online).count()
    }

    /// Remove an agent from the registry (e.g. on uninstall).
    #[allow(dead_code)]
    pub fn remove(&mut self, agent_id: &str) {
        self.agents.remove(agent_id);
    }
}

/// Thread-safe shared AgentRegistry.
pub type SharedAgentRegistry = Arc<RwLock<AgentRegistry>>;

/// Create a new shared AgentRegistry.
pub fn new_shared_registry() -> SharedAgentRegistry {
    Arc::new(RwLock::new(AgentRegistry::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_from_mqtt_online() {
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/com.example/status", b"online");
        assert!(registry.is_online("com.example"));
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn test_update_from_mqtt_offline() {
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/com.example/status", b"online");
        assert!(registry.is_online("com.example"));

        registry.update_from_mqtt("acowork/agents/com.example/status", b"offline");
        assert!(!registry.is_online("com.example"));
        assert_eq!(registry.online_count(), 0);
    }

    #[test]
    fn test_update_from_mqtt_invalid_topic() {
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("invalid/topic", b"online");
        assert_eq!(registry.total_tracked(), 0);
    }

    #[test]
    fn test_online_agents() {
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/a/status", b"online");
        registry.update_from_mqtt("acowork/agents/b/status", b"online");
        registry.update_from_mqtt("acowork/agents/c/status", b"offline");

        let online = registry.online_agents();
        assert_eq!(online.len(), 2);
        assert!(online.contains(&"a".to_string()));
        assert!(online.contains(&"b".to_string()));
    }
}
