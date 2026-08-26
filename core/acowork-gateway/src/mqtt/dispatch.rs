//! MQTT message dispatch (ADR-033).
//!
//! Handles incoming MQTT messages on the Gateway's broker connection.
//! Plain-text payloads (`http_port`, agent `status`) carry simple semantic
//! data (port number, online/offline) rather than `DataEnvelope` protobuf,
//! so they are matched by topic pattern and parsed inline.
//!
//! Status messages from the Runtime (`acowork/agents/{id}/status`) are
//! additionally re-published as protobuf `DataEnvelope` payloads on a
//! separate topic. This bridges the legacy plain-text retained topic
//! (used by older Runtimes and the Gateway's internal AgentRegistry) to
//! the modern protobuf contract consumed by the Desktop's
//! `data_envelope::Payload::AgentStatus` handler.
//!
//! See `docs/zh/protocols/mqtt.md` §8 (Topic patterns).

use std::sync::Arc;

use acowork_core::mqtt_proto::{data_envelope, AgentStatus as AgentStatusProto, DataEnvelope};
use prost::Message as _;

use crate::handlers::server::SharedState;
use crate::http::proxy::SharedRuntimeHttpRegistry;
use crate::mqtt::agent_registry::SharedAgentRegistry;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};
use crate::mqtt::node_control::NodeControlClient;
use crate::mqtt::node_registry::SharedNodeRegistry;

/// Unified MQTT message handler — called from the Gateway's MQTT callback.
///
/// `mqtt_client` is used to re-publish plain-text status messages as
/// protobuf `DataEnvelope` (see module docs). Pass `None` in tests that
/// don't have a real broker. `node_control` correlates NodeEvent
/// results against in-flight node commands (ADR-055 §6.2).
#[allow(clippy::too_many_arguments)]
pub fn handle_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
    node_registry: &SharedNodeRegistry,
    mqtt_client: Option<&Arc<GatewayMqttClient>>,
    state: &SharedState,
    node_control: Option<&NodeControlClient>,
) {
    handle_plaintext_message(
        topic,
        payload,
        runtime_http_registry,
        agent_registry,
        node_registry,
        mqtt_client,
        state,
        node_control,
    );
}

/// Topic pattern matcher.
///
/// Supports MQTT wildcard matching:
/// - `+` matches a single level
/// - `#` matches all remaining levels (must be the last character)
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let filter_parts: Vec<&str> = filter.split('/').collect();
    let topic_parts: Vec<&str> = topic.split('/').collect();

    for (i, fp) in filter_parts.iter().enumerate() {
        if *fp == "#" {
            return true; // matches everything remaining
        }
        if i >= topic_parts.len() {
            return false;
        }
        if *fp != "+" && *fp != topic_parts[i] {
            return false;
        }
    }

    filter_parts.len() == topic_parts.len()
}

/// Handle a plain-text MQTT message (non-DataEnvelope payload).
///
/// Topics with simple text payloads rather than protobuf envelopes:
/// - `acowork/agents/+/http_endpoint` → registers Runtime HTTP port for reverse proxy
/// - `acowork/agents/+/status` → updates AgentRegistry online/offline
///   status AND re-publishes as a protobuf `DataEnvelope` so the
///   Desktop's `data_envelope::Payload::AgentStatus` handler also
///   receives the transition.
/// - `acowork/nodes/+/status` → updates NodeRegistry online/offline
///   (ADR-055 §6.2; plain text + LWT, same shape as agent status).
/// - `acowork/nodes/+/info` → protobuf `DataEnvelope<NodeInfo>`
///   metadata snapshot into the NodeRegistry (the payload IS an
///   envelope; routed here because the node topic family shares the
///   same dispatch surface).
///
/// This replaces the inline callback previously in `gateway/mod.rs`.
#[allow(clippy::too_many_arguments)]
pub fn handle_plaintext_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
    node_registry: &SharedNodeRegistry,
    mqtt_client: Option<&Arc<GatewayMqttClient>>,
    state: &SharedState,
    node_control: Option<&NodeControlClient>,
) {
    if topic_matches("acowork/agents/+/http_endpoint", topic) {
        let agent_id = topic
            .strip_prefix("acowork/agents/")
            .and_then(|s| s.strip_suffix("/http_endpoint"))
            .unwrap_or("");
        if agent_id.is_empty() {
            tracing::warn!(topic, "http_endpoint topic matched but agent_id extraction failed");
            return;
        }
        // ADR-055 D3: payload is now the full endpoint URL (e.g.
        // "http://127.0.0.1:54321") rather than a bare port. Surface
        // malformed payloads as warnings — without these, a bad payload
        // would cause the Gateway to 503 with no diagnostic trail.
        let endpoint = match std::str::from_utf8(payload) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id,
                    error = %e,
                    "http_endpoint payload is not valid UTF-8 — ignoring"
                );
                return;
            }
        };
        if endpoint.is_empty() {
            tracing::warn!(
                topic,
                agent_id,
                "http_endpoint payload is empty — Gateway will 503 every reverse-proxy request for this agent"
            );
            return;
        }
        let reg = runtime_http_registry.clone();
        let aid = agent_id.to_string();
        let aid_for_log = aid.clone();
        let endpoint_for_log = endpoint.clone();
        tokio::spawn(async move {
            reg.write().await.register(&aid, &endpoint);
        });
        tracing::info!(agent_id = %aid_for_log, endpoint = %endpoint_for_log, "Registered Runtime HTTP endpoint via MQTT");
    } else if topic_matches("acowork/agents/+/status", topic) {
        let reg = agent_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        let state_for_status = state.clone();
        // Clone the (optional) GatewayMqttClient for the spawned
        // re-publish task. None means the Gateway is running without
        // an embedded broker (e.g. tests); in that case we skip
        // re-publishing but still update the AgentRegistry.
        let mqtt_client_for_republish = mqtt_client.cloned();
        tokio::spawn(async move {
            // 1) Update internal registry (sleeping_at stamping, etc.)
            reg.write().await.update_from_mqtt(&topic_owned, &payload_owned);

            // ADR-055 §6.2: mirror liveness into GatewayState — drop the
            // running entry when the Runtime reports offline (crash,
            // auto-sleep, manual stop). The node reaper owns the process;
            // the Gateway only tracks the running/ready surface.
            if let Some(agent_id) = extract_agent_id_from_status_topic(&topic_owned)
                && String::from_utf8_lossy(&payload_owned).trim() == "offline"
            {
                state_for_status.write().await.remove_running(&agent_id);
            }

            // 2) Re-publish as a protobuf DataEnvelope so subscribers
            //    that listen for `data_envelope::Payload::AgentStatus`
            //    (the Desktop's chat_mqtt) also see the transition.
            //    Without this, the Desktop would only see the
            //    plain-text retained payload (which the Desktop also
            //    handles — see `parse_plaintext_agent_status` — but
            //    the protobuf branch is needed for any future Desktop
            //    code that wants the structured AgentStatus type
            //    rather than a string).
            let Some(client) = mqtt_client_for_republish else {
                return;
            };
            let Some(agent_id) = extract_agent_id_from_status_topic(&topic_owned) else {
                tracing::warn!(topic = %topic_owned, "status topic matched but agent_id extraction failed");
                return;
            };
            let payload_str = String::from_utf8_lossy(&payload_owned);
            let (online, sleeping) = match payload_str.trim() {
                "sleeping" => (true, true),
                "online" => (true, false),
                "offline" => (false, false),
                other => {
                    tracing::warn!(
                        topic = %topic_owned,
                        payload = %other,
                        "republish: unknown agent status payload"
                    );
                    return;
                }
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                    agent_id,
                    online,
                    sleeping,
                })),
            };
            // Same topic — the broker will replace the retained
            // plain-text payload with this protobuf envelope for new
            // subscribers. Existing plain-text subscribers continue to
            // see the cached plain text (broker delivers different
            // payloads to the same topic to subscribers based on
            // subscription time).
            if let Err(e) = client
                .publish_envelope(&topic_owned, &envelope, MqttQoS::AtLeastOnce, true)
                .await
            {
                tracing::warn!(
                    topic = %topic_owned,
                    error = %e,
                    "failed to re-publish agent status as protobuf"
                );
            }
        });
    } else if topic_matches("acowork/agents/+/ready", topic) {
        // Plain-text retained payload ("true" / "false") published by the
        // Runtime after Phase A–C have all populated the HTTP server's
        // late-bind slots and the runtime is ready to serve any
        // `/agents/{id}/*` request. The Gateway pins
        // `running_agents[id].ready` to this value so `/api/agents`
        // reports it; the Desktop fast-path keeps its `running && ready`
        // gate closed until the Gateway mirrors `ready=true`.
        //
        // The plain-text shape is intentional: it parallels
        // `acowork/agents/+/status` so the dispatch surface is uniform
        // and the broker's retained message gives a fresh Gateway
        // startup the same answer without polling the Runtime.
        let agent_id = match topic
            .strip_prefix("acowork/agents/")
            .and_then(|s| s.strip_suffix("/ready"))
        {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                tracing::warn!(topic, "ready topic matched but agent_id extraction failed");
                return;
            }
        };
        let payload_str = match std::str::from_utf8(payload) {
            Ok(s) => s.trim(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id = %agent_id,
                    error = %e,
                    "ready payload is not valid UTF-8 — ignoring"
                );
                return;
            }
        };
        let ready = match payload_str {
            "true" => true,
            "false" => false,
            other => {
                tracing::warn!(
                    topic,
                    agent_id = %agent_id,
                    payload = %other,
                    "ready payload is not 'true'/'false' — ignoring"
                );
                return;
            }
        };
        let state_for_ready = state.clone();
        let agent_id_for_log = agent_id.clone();
        tokio::spawn(async move {
            let mut gw = state_for_ready.write().await;
            gw.set_agent_ready(&agent_id_for_log, ready);
            tracing::info!(
                agent_id = %agent_id_for_log,
                ready,
                "Runtime ready signal received via MQTT"
            );
        });
    } else if topic_matches("acowork/nodes/+/status", topic) {
        // ADR-055 §6.2: node online/offline (plain text + LWT,
        // same shape as the agent status topic). Feeds the
        // NodeRegistry — the local-node supervisor and (Phase 2b)
        // the lifecycle control plane read it.
        let reg = node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_status_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/info", topic) {
        // ADR-055 §6.2: node metadata snapshot (protobuf
        // DataEnvelope<NodeInfo>, retained). Feeds the NodeRegistry.
        let reg = node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_info_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/agents/+/events", topic) {
        // ADR-055 §6.2: per-agent NodeEvent result (protobuf envelope,
        // QoS 1). Correlate against in-flight node commands.
        let Some(control) = node_control.cloned() else {
            tracing::debug!(topic, "Node agent event received but no NodeControlClient wired — dropping");
            return;
        };
        let payload_owned = payload.to_vec();
        let topic_owned = topic.to_string();
        tokio::spawn(async move {
            match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(envelope) => {
                    if let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload {
                        control.handle_event(event).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(topic = %topic_owned, error = %e, "Bad NodeEvent envelope on agent events topic");
                }
            }
        });
    } else if topic_matches("acowork/nodes/+/agents/+/installed", topic) {
        // ADR-055 §6.5: per-agent installed-package inventory (retained).
        // Aggregates into GatewayState::installed_agents, replacing the
        // pre-hard-cut on-disk packages scan (L2-9). An empty retained
        // payload clears the entry (uninstall).
        let Some((node_id, agent_id)) = extract_installed_topic_ids(topic) else {
            tracing::warn!(topic, "installed topic matched but id extraction failed");
            return;
        };
        let state_for_installed = state.clone();
        let payload_owned = payload.to_vec();
        let node_id_owned = node_id.clone();
        tokio::spawn(async move {
            let mut gw = state_for_installed.write().await;
            if payload_owned.is_empty() {
                gw.remove_installed(&agent_id);
                tracing::info!(node_id = %node_id_owned, agent_id, "Removed installed agent (node retained cleared)");
                return;
            }
            match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(envelope) => {
                    if let Some(data_envelope::Payload::InstalledAgentInfo(info)) = envelope.payload
                    {
                        // ADR-055 §3.2: an install completed (or a retained
                        // re-publish on node reconnect). Record whether this
                        // is a NEW install so cron triggers register exactly
                        // once (the async-install completion hook).
                        let is_new = !gw.installed_agents.contains_key(&info.agent_id);
                        let Some(aid) = gw.upsert_installed_from_node(&node_id_owned, &info) else {
                            return;
                        };

                        // ADR-017: apply the Gateway-owned avatar cache to
                        // the freshly-aggregated agent so list_agents shows
                        // the correct avatar for stopped agents.
                        if let Some(data_dir) = gw.config.as_ref().map(|c| c.data_dir.clone()) {
                            let cache = crate::http::agent_config::load_avatar_cache(
                                std::path::Path::new(&data_dir),
                            );
                            if let Some(entry) = cache.get(&aid)
                                && let Some(agent) = gw.installed_agents.get_mut(&aid)
                            {
                                agent.manifest.avatar = entry.avatar.clone();
                                agent.manifest.builtin_avatar = entry.builtin_avatar.clone();
                            }
                        }

                        // ADR-055 §3.2 / S3.3: register the manifest-declared
                        // cron triggers on first install. The node no longer
                        // owns cron; the Gateway registers them once the
                        // install-completed inventory arrives.
                        if is_new
                            && let Ok(manifest) =
                                acowork_core::AgentManifest::from_toml(&info.manifest_toml)
                        {
                            crate::cron::register_agent_cron_triggers(&mut gw, &aid, &manifest);
                        }

                        tracing::info!(node_id = %node_id_owned, agent_id = %aid, "Aggregated installed agent from node");
                    }
                }
                Err(e) => {
                    tracing::warn!(node_id = %node_id_owned, agent_id, error = %e, "Bad InstalledAgentInfo envelope on installed topic");
                }
            }
        });
    }
}

/// Extract `(node_id, agent_id)` from
/// `acowork/nodes/{node_id}/agents/{agent_id}/installed`.
fn extract_installed_topic_ids(topic: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 6
        && parts[0] == "acowork"
        && parts[1] == "nodes"
        && parts[3] == "agents"
        && parts[5] == "installed"
    {
        let node_id = parts[2];
        let agent_id = parts[4];
        if !node_id.is_empty() && !agent_id.is_empty() {
            return Some((node_id.to_string(), agent_id.to_string()));
        }
    }
    None
}

/// Extract `agent_id` from `acowork/agents/{agent_id}/status`.
///
/// Unlike `extract_agent_id_from_topic` (which is permissive), this
/// version requires the exact 4-segment shape of the status topic.
fn extract_agent_id_from_status_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4 && parts[0] == "acowork" && parts[1] == "agents" && parts[3] == "status" {
        let agent_id = parts[2];
        if !agent_id.is_empty() {
            return Some(agent_id.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::GatewayState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_state() -> SharedState {
        Arc::new(RwLock::new(GatewayState::new(
            "/tmp/acowork-dispatch-test-vault",
        )))
    }

    #[tokio::test]
    async fn test_node_status_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        handle_plaintext_message(
            "acowork/nodes/local/status",
            b"online",
            &http_reg,
            &agent_reg,
            &node_reg,
            None,
            &state,
            None,
        );
        // handle_plaintext_message spawns a tokio task; wait for it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(node_reg.read().await.is_online("local"));
    }

    #[tokio::test]
    async fn test_node_info_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let info = acowork_core::mqtt_proto::NodeInfo {
            node_id: "gpu-1".to_string(),
            machine_uid: "uid-1".to_string(),
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
        let payload = prost::Message::encode_to_vec(&envelope);

        handle_plaintext_message(
            "acowork/nodes/gpu-1/info",
            &payload,
            &http_reg,
            &agent_reg,
            &node_reg,
            None,
            &state,
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = node_reg.read().await;
        assert_eq!(
            reg.get("gpu-1").and_then(|n| n.machine_uid.clone()),
            Some("uid-1".to_string())
        );
    }

    #[test]
    fn test_topic_matches_exact() {
        assert!(topic_matches("acowork/global/providers", "acowork/global/providers"));
        assert!(!topic_matches("acowork/global/providers", "acowork/global/mcps"));
    }

    #[test]
    fn test_topic_matches_single_wildcard() {
        assert!(topic_matches("acowork/agents/+/status", "acowork/agents/com.example/status"));
        assert!(topic_matches("acowork/agents/+/status", "acowork/agents/foo/status"));
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/meta"));
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/sessions/s1/config"));
    }

    #[test]
    fn test_topic_matches_multi_wildcard() {
        assert!(topic_matches("acowork/agents/+/sessions/+/messages/#", "acowork/agents/foo/sessions/s1/messages/chunk"));
        assert!(topic_matches("acowork/global/#", "acowork/global/providers"));
        assert!(topic_matches("acowork/global/#", "acowork/global/anything/deep"));
        assert!(!topic_matches("acowork/global/#", "acowork/agents/foo/status"));
    }

    #[test]
    fn test_topic_matches_edge_cases() {
        // # must be last
        assert!(topic_matches("#", "anything/at/all"));
        // + matches exactly one level
        assert!(!topic_matches("a/+", "a/b/c"));
        assert!(topic_matches("a/+/c", "a/b/c"));
    }

    #[test]
    fn extract_agent_id_from_status_topic_ok() {
        assert_eq!(
            extract_agent_id_from_status_topic("acowork/agents/com.example/status"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_agent_id_from_status_topic_rejects_extra_segments() {
        // The permissive `extract_agent_id_from_topic` would accept
        // this; the status variant must NOT (re-publish logic relies
        // on the exact 4-segment shape).
        assert_eq!(
            extract_agent_id_from_status_topic(
                "acowork/agents/com.example/sessions/s-1/messages/chunk",
            ),
            None,
        );
    }

    #[test]
    fn extract_agent_id_from_status_topic_rejects_wrong_topic() {
        assert_eq!(extract_agent_id_from_status_topic("acowork/global/providers"), None);
        assert_eq!(extract_agent_id_from_status_topic("acowork/agents//status"), None);
    }
}