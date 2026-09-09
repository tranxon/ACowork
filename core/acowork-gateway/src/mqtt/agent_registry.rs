//! Agent Registry (ADR-033 Phase 1 scaffolding).
//!
//! Tracks agent-instance online status based on MQTT
//! `acowork/agents/{instance_id}/status` Retained messages. ADR-073: the
//! topic variable is the INSTANCE identity (UUID), so the registry key
//! is an instance id — two instances of the same package never collide.
//! In Phase 2+, it replaces the gRPC `()` as the source of truth for
//! which agents are online.
//!
//! See `docs/zh/protocols/mqtt.md` §3.2 and §8.1 (Will Message).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
use chrono::{DateTime, Utc};
use prost::Message as _;
use tokio::sync::RwLock;

/// Lifecycle state of an agent, derived from MQTT retained status messages.
///
/// `online=true` covers the `online`, `sleeping` and `degraded`
/// payloads — all three mean the Runtime's MQTT session is alive
/// (the process is connected to the broker), which is what the
/// reconcile loop / Desktop `running` gate care about:
///
/// - `sleeping` additionally records a timestamp the Desktop can use
///   to render an "auto-slept at HH:MM" badge.
/// - `degraded` means a (re)connect happened but the bootstrap steps
///   failed — the session is alive but not yet serving. It maps to
///   `online=true` so the reconcile loop does not fight the dispatch
///   event path (which keeps degraded agents tracked), while
///   `ready=false` on the `running_agents` entry surfaces the
///   not-yet-serving state to the UI.
/// - A manual stop or a crash leaves `online=false` (the broker
///   fired the LWT `offline`).
#[derive(Debug, Clone)]
pub struct AgentOnlineState {
    /// Whether the agent is currently reachable (online / sleeping /
    /// degraded — all mean the MQTT session is alive).
    pub online: bool,
    /// Whether the agent auto-slept (vs manually stopped / crashed).
    /// Stamped from the moment the Runtime published the `sleeping`
    /// retained message. `None` means "not sleeping" (either online,
    /// offline, or never seen a sleeping message).
    pub sleeping: bool,
    /// Wall-clock instant when the registry last observed a status update.
    pub last_updated: Instant,
    /// Wall-clock timestamp (UTC) the agent self-reported as going to
    /// sleep. Captured from the local clock the moment the `sleeping`
    /// payload was received. Persisted alongside `online`/`sleeping` so
    /// the Desktop can show "Last active at HH:MM" without keeping its
    /// own clock.
    pub sleeping_at: Option<DateTime<Utc>>,
    /// ADR-073: the INSTANCE identity this entry tracks — the registry
    /// key. Read from the topic variable (`acowork/agents/{instance_id}/status`)
    /// for plaintext statuses, or from the envelope `instance_id` field
    /// when present. Never the package id.
    pub instance_id: String,
    /// Package identity (`AgentStatus.agent_id`), present only when the
    /// status arrived as a DataEnvelope carrying it. Empty for
    /// plaintext statuses / gateway loopback — display & diagnostics
    /// only, NEVER a lookup key (ADR-073).
    pub agent_id: String,
    /// ADR-073: current location (`AgentStatus.node_id`) from the
    /// envelope. Empty for plaintext statuses. Positional metadata only.
    pub node_id: String,
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
    /// Snapshot of every entry the registry currently holds
    /// (online, sleeping, offline — anything in the map). Used by the
    /// reconciliation loop in `mqtt::dispatch::reconcile_running_agents`
    /// to walk the authoritative broker view without locking the
    /// registry for the entire iteration. The returned vector is a
    /// pure copy — safe to iterate without holding the read lock.
    pub fn snapshot(&self) -> Vec<(String, AgentOnlineState)> {
        self.agents
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect()
    }

    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update an agent's status from an MQTT message.
    ///
    /// `topic` should match `acowork/agents/{instance_id}/status`
    /// (ADR-073: the path variable is the INSTANCE identity).
    /// `payload` is either plain text ("online" / "sleeping" /
    /// "offline" / "degraded") or a protobuf `DataEnvelope<AgentStatus>`
    /// (the dispatch loopback / a structured publisher).
    ///
    /// `sleeping` is the Runtime's auto-sleep signal — see
    /// `acowork-runtime::agent::idle_watcher`. We treat it as `online=true`
    /// (the process is reachable; only the user-facing session has been
    /// suspended) but also stamp `sleeping_at` so the Desktop can render
    /// the auto-slept badge.
    ///
    /// `degraded` (bootstrap failed on a (re)connect) is also
    /// `online=true`: the process is connected, so killing the entry
    /// would fight the dispatch path; the not-ready state is surfaced
    /// through `running_agents[id].ready=false`.
    pub fn update_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        // Parse the instance id from the topic: acowork/agents/{instance_id}/status
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4 || parts[0] != "acowork" || parts[1] != "agents" || parts[3] != "status" {
            tracing::warn!(topic, "Invalid agent status topic format");
            return;
        }

        let topic_instance_id = parts[2].to_string();
        let payload_str = match std::str::from_utf8(payload) {
            Ok(s) => s,
            Err(_) => {
                // The Gateway re-publishes plain-text status as a protobuf
                // `DataEnvelope` on this SAME topic (`dispatch.rs`), and our
                // own `acowork/agents/+/status` subscription receives that
                // loopback. Decode it instead of treating a binary payload
                // as offline — that used to flip a freshly-online agent
                // (e.g. the auto-started System Agent) back to offline
                // within the same second, hiding `ready=true` from
                // `/api/agents` and timing the Desktop out (Phase 5a
                // startup report).
                match DataEnvelope::decode(payload) {
                    Ok(envelope) => {
                        let Some(data_envelope::Payload::AgentStatus(status)) = envelope.payload
                        else {
                            tracing::warn!(
                                topic,
                                instance_id = %parts[2],
                                "agent status loopback envelope carries no AgentStatus payload"
                            );
                            return;
                        };
                        // ADR-073: the envelope may carry the instance
                        // identity in `instance_id` (structured shape;
                        // the dispatch loopback sets agent_id to "" and
                        // puts the identity here). Prefer it; fall back
                        // to the topic variable only for legacy
                        // envelopes that predate the field.
                        let instance_id = if status.instance_id.is_empty() {
                            topic_instance_id.clone()
                        } else {
                            status.instance_id.clone()
                        };
                        let now = Instant::now();
                        let sleeping_at = if status.sleeping {
                            self.agents
                                .get(&instance_id)
                                .and_then(|s| s.sleeping_at)
                                .or(Some(Utc::now()))
                        } else {
                            None
                        };
                        self.agents.insert(
                            instance_id.clone(),
                            AgentOnlineState {
                                online: status.online,
                                sleeping: status.sleeping,
                                last_updated: now,
                                sleeping_at,
                                instance_id,
                                agent_id: status.agent_id,
                                node_id: status.node_id,
                            },
                        );
                        tracing::debug!(
                            instance_id = %parts[2],
                            online = status.online,
                            sleeping = status.sleeping,
                            "Agent registry updated from MQTT (protobuf loopback)"
                        );
                        return;
                    }
                    Err(decode_err) => {
                        tracing::warn!(
                            topic,
                            instance_id = %parts[2],
                            error = %decode_err,
                            "agent status payload is neither UTF-8 nor a DataEnvelope — ignoring"
                        );
                        return;
                    }
                }
            }
        };
        let state = payload_str.trim();
        // `degraded` counts as online: the Runtime's MQTT session is
        // alive (bootstrap failed, but the process is connected and
        // will retry on the next reconnect). Only `offline` — fired
        // by the broker's LWT after the connection actually dropped —
        // flips the bit back.
        let online = matches!(state, "online" | "sleeping" | "degraded");
        let now = Instant::now();
        let sleeping_at_now = Utc::now();

        // Preserve `sleeping_at` across the online→sleeping transition;
        // clear it on any non-sleeping status so the badge resets.
        let previous_sleeping_at = self
            .agents
            .get(&topic_instance_id)
            .and_then(|s| s.sleeping_at);
        let sleeping_at = if state == "sleeping" {
            Some(previous_sleeping_at.unwrap_or(sleeping_at_now))
        } else {
            None
        };

        // Plaintext status carries no package/location metadata — the
        // registry keys on the topic's instance id (ADR-073).
        self.agents.insert(
            topic_instance_id.clone(),
            AgentOnlineState {
                online,
                sleeping: state == "sleeping",
                last_updated: now,
                sleeping_at,
                instance_id: topic_instance_id,
                agent_id: String::new(),
                node_id: String::new(),
            },
        );

        tracing::debug!(
            instance_id = %parts[2],
            state = %state,
            online,
            "Agent registry updated from MQTT"
        );
    }

    /// Check if an agent instance is online (keyed by instance identity).
    pub fn is_online(&self, instance_id: &str) -> bool {
        self.agents
            .get(instance_id)
            .map(|s| s.online)
            .unwrap_or(false)
    }

    /// Get the `sleeping_at` UTC timestamp for an agent instance, if it
    /// is currently in the `sleeping` state. Used by `/api/agents` to
    /// surface the auto-sleep timestamp on the Desktop side without
    /// round-tripping to the Runtime.
    pub fn sleeping_at(&self, instance_id: &str) -> Option<DateTime<Utc>> {
        self.agents
            .get(instance_id)
            .and_then(|s| if s.sleeping { s.sleeping_at } else { None })
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

    /// Remove an agent instance from the registry (e.g. on uninstall).
    #[allow(dead_code)]
    pub fn remove(&mut self, instance_id: &str) {
        self.agents.remove(instance_id);
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
    fn test_update_from_mqtt_protobuf_loopback_is_online() {
        // The Gateway re-publishes plain-text status as a protobuf
        // DataEnvelope on the same topic; our own subscription receives
        // the loopback. It must be decoded (online=true), NOT treated
        // as offline (the pre-fix behaviour that hid `ready=true`).
        use acowork_core::mqtt_proto::{data_envelope, AgentStatus as AgentStatusProto, DataEnvelope};
        use prost::Message as _;
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                agent_id: "com.example".to_string(),
                online: true,
                sleeping: false,
                // ADR-073: empty instance/node identity = legacy envelope
                // (the registry keys on the topic's instance id anyway).
                instance_id: String::new(),
                node_id: String::new(),
            })),
        };
        let bytes = envelope.encode_to_vec();
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/com.example/status", &bytes);
        assert!(registry.is_online("com.example"));
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn test_loopback_envelope_with_instance_id_keys_by_instance_not_agent_id() {
        // ADR-073 regression: `dispatch.rs` re-publishes plaintext status
        // as a DataEnvelope with agent_id EMPTY and the identity in
        // `instance_id`. The registry must key on the instance id — keying
        // on the old agent_id field would collapse every loopback into a
        // single "" entry and instances would read offline after a
        // retained replay (broker/Gateway restart).
        use acowork_core::mqtt_proto::{data_envelope, AgentStatus as AgentStatusProto, DataEnvelope};
        use prost::Message as _;
        let uuid = "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c";
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                agent_id: String::new(),
                online: true,
                sleeping: false,
                instance_id: uuid.to_string(),
                node_id: "node-a".to_string(),
            })),
        };
        let bytes = envelope.encode_to_vec();
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt(&format!("acowork/agents/{uuid}/status"), &bytes);

        assert!(registry.is_online(uuid), "instance must be online");
        assert!(
            !registry.is_online(""),
            "the loopback must NOT create an empty-key entry"
        );
        assert_eq!(registry.online_count(), 1);
        let state = registry.agents.get(uuid).expect("entry keyed by instance id");
        assert_eq!(state.instance_id, uuid);
        assert_eq!(state.agent_id, "", "plaintext loopback carries no package id");
        assert_eq!(state.node_id, "node-a");
    }

    #[test]
    fn test_two_instances_same_package_are_independent() {
        // ADR-073: the same package installed twice publishes on two
        // instance-scoped topics. The online registry must keep the
        // entries independent — offline for one must never flip the other.
        use acowork_core::mqtt_proto::{data_envelope, AgentStatus as AgentStatusProto, DataEnvelope};
        use prost::Message as _;
        let uuid_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let uuid_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let mut registry = AgentRegistry::new();
        for (uuid, node) in [(uuid_a, "node-a"), (uuid_b, "node-b")] {
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                    agent_id: "com.foo.bar".to_string(),
                    online: true,
                    sleeping: false,
                    instance_id: uuid.to_string(),
                    node_id: node.to_string(),
                })),
            };
            let bytes = envelope.encode_to_vec();
            registry.update_from_mqtt(&format!("acowork/agents/{uuid}/status"), &bytes);
        }
        assert!(registry.is_online(uuid_a) && registry.is_online(uuid_b));
        assert_eq!(registry.online_count(), 2);
        assert_eq!(
            registry.agents.get(uuid_a).map(|s| s.agent_id.as_str()),
            Some("com.foo.bar"),
            "package id preserved as metadata on both entries"
        );

        // B goes offline (crash / LWT) — A must stay online.
        let offline = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                agent_id: "com.foo.bar".to_string(),
                online: false,
                sleeping: false,
                instance_id: uuid_b.to_string(),
                node_id: "node-b".to_string(),
            })),
        };
        let bytes = offline.encode_to_vec();
        registry.update_from_mqtt(&format!("acowork/agents/{uuid_b}/status"), &bytes);
        assert!(!registry.is_online(uuid_b), "B must be offline");
        assert!(registry.is_online(uuid_a), "A must be unaffected");
        assert_eq!(registry.online_count(), 1);
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

    #[test]
    fn test_sleeping_payload_is_online_and_stamps_timestamp() {
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/com.example/status", b"online");
        let before_sleep = Utc::now();
        registry.update_from_mqtt("acowork/agents/com.example/status", b"sleeping");
        let state = registry
            .agents
            .get("com.example")
            .expect("agent must be tracked");
        assert!(state.online, "sleeping must keep online=true");
        assert!(state.sleeping, "sleeping must set sleeping=true");
        let ts = state
            .sleeping_at
            .expect("sleeping_at must be stamped");
        assert!(ts >= before_sleep, "sleeping_at must be >= test start");
        assert!(
            ts <= Utc::now() + chrono::Duration::seconds(1),
            "sleeping_at must be near now"
        );
    }

    #[test]
    fn test_online_after_sleep_clears_sleeping_at() {
        // After the agent wakes up (online), sleeping_at must clear so the
        // Desktop doesn't keep showing the old badge.
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/com.example/status", b"sleeping");
        registry.update_from_mqtt("acowork/agents/com.example/status", b"online");
        let state = registry
            .agents
            .get("com.example")
            .expect("agent must be tracked");
        assert!(state.online);
        assert!(!state.sleeping);
        assert!(
            state.sleeping_at.is_none(),
            "sleeping_at must clear after waking"
        );
    }

    #[test]
    fn test_offline_after_sleep_preserves_sleeping_at_until_resurrected() {
        // LWT replaces sleeping with offline after the broker observes the
        // disconnect. We clear sleeping_at on offline too — once the
        // process is gone, the "slept at" badge no longer makes sense.
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt("acowork/agents/com.example/status", b"sleeping");
        registry.update_from_mqtt("acowork/agents/com.example/status", b"offline");
        let state = registry.agents.get("com.example").unwrap();
        assert!(!state.online);
        assert!(state.sleeping_at.is_none());
    }
}
