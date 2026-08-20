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

use crate::http::proxy::SharedRuntimeHttpRegistry;
use crate::mqtt::agent_registry::SharedAgentRegistry;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

/// Unified MQTT message handler — called from the Gateway's MQTT callback.
///
/// `mqtt_client` is used to re-publish plain-text status messages as
/// protobuf `DataEnvelope` (see module docs). Pass `None` in tests that
/// don't have a real broker.
pub fn handle_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
    mqtt_client: Option<&Arc<GatewayMqttClient>>,
) {
    handle_plaintext_message(topic, payload, runtime_http_registry, agent_registry, mqtt_client);
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
/// - `acowork/agents/+/http_port` → registers Runtime HTTP port for reverse proxy
/// - `acowork/agents/+/status` → updates AgentRegistry online/offline
///   status AND re-publishes as a protobuf `DataEnvelope` so the
///   Desktop's `data_envelope::Payload::AgentStatus` handler also
///   receives the transition.
///
/// This replaces the inline callback previously in `gateway/mod.rs`.
pub fn handle_plaintext_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
    mqtt_client: Option<&Arc<GatewayMqttClient>>,
) {
    if topic_matches("acowork/agents/+/http_port", topic) {
        let agent_id = topic
            .strip_prefix("acowork/agents/")
            .and_then(|s| s.strip_suffix("/http_port"))
            .unwrap_or("");
        if agent_id.is_empty() {
            tracing::warn!(topic, "http_port topic matched but agent_id extraction failed");
            return;
        }
        // Surface malformed payloads as warnings — the runtime publishes the
        // port as a decimal string, so any other shape means a bug or a
        // misbehaving client. Without these warnings, a malformed payload
        // would cause the Gateway to return 503 with no diagnostic trail.
        let port = match std::str::from_utf8(payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id,
                    error = %e,
                    "http_port payload is not valid UTF-8 — ignoring"
                );
                return;
            }
        };
        let port = match port.trim().parse::<u16>() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id,
                    payload = %port,
                    error = %e,
                    "http_port payload is not a valid u16 — Gateway will 503 every reverse-proxy request for this agent"
                );
                return;
            }
        };
        let reg = runtime_http_registry.clone();
        let aid = agent_id.to_string();
        let aid_for_log = aid.clone();
        tokio::spawn(async move {
            reg.write().await.register(&aid, port);
        });
        tracing::info!(agent_id = %aid_for_log, port, "Registered Runtime HTTP port via MQTT");
    } else if topic_matches("acowork/agents/+/status", topic) {
        let reg = agent_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        // Clone the (optional) GatewayMqttClient for the spawned
        // re-publish task. None means the Gateway is running without
        // an embedded broker (e.g. tests); in that case we skip
        // re-publishing but still update the AgentRegistry.
        let mqtt_client_for_republish = mqtt_client.cloned();
        tokio::spawn(async move {
            // 1) Update internal registry (sleeping_at stamping, etc.)
            reg.write().await.update_from_mqtt(&topic_owned, &payload_owned);

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
    }
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