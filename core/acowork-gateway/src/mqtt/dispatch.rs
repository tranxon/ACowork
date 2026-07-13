//! MQTT message dispatch (ADR-033).
//!
//! Parses `DataEnvelope` payloads from incoming MQTT messages and routes
//! them to the appropriate business logic handler. Plain text messages
//! (http_port, status) are handled separately via `handle_plaintext_message`.
//!
//! See `docs/zh/protocols/mqtt.md` §8 (Topic patterns).

use acowork_core::mqtt_proto::DataEnvelope;

use crate::mqtt::router::{route_message_by_topic, topic_matches, RouteResult};

/// Unified MQTT message handler — called from the Gateway's MQTT callback.
///
/// Tries both dispatch paths:
/// 1. `dispatch_message` — protobuf `DataEnvelope` parsing + routing
/// 2. `handle_plaintext_message` — plain-text http_port / status registration
pub fn handle_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &crate::http::proxy::SharedRuntimeHttpRegistry,
    agent_registry: &crate::mqtt::agent_registry::SharedAgentRegistry,
) {
    dispatch_message(topic, payload);
    handle_plaintext_message(topic, payload, runtime_http_registry, agent_registry);
}

/// Dispatch an incoming MQTT message (protobuf DataEnvelope).
///
/// Steps:
/// 1. Route the message by topic (router.rs).
/// 2. If the route is unimplemented (Phase 2+ handler), log and return.
/// 3. If the route matches, attempt to parse the payload as a `DataEnvelope`.
/// 4. Dispatch the envelope's oneof payload to the appropriate handler.
pub fn dispatch_message(topic: &str, payload: &[u8]) {
    // Step 1: Route by topic
    match route_message_by_topic(topic) {
        RouteResult::NoMatch => {
            // Not a known route pattern. Still try to parse as DataEnvelope
            // in case it's a generic proto message (Step 2 below).
        }
        RouteResult::Handled => {
            return;
        }
        RouteResult::Unimplemented(reason) => {
            tracing::debug!(
                topic,
                reason,
                "MQTT message routed to unimplemented handler (Phase 2+)"
            );
            return;
        }
    }

    // Step 2: Parse the payload as a DataEnvelope
    let envelope = match prost::Message::decode(payload) {
        Ok(e) => e,
        Err(e) => {
            // The payload might be plain text (e.g. agent status "online"/"offline")
            // rather than a protobuf envelope. Log at trace level to avoid noise.
            tracing::trace!(
                topic,
                error = %e,
                "MQTT payload is not a valid DataEnvelope (may be plain text)"
            );
            return;
        }
    };

    // Step 3: Dispatch by payload type
    dispatch_envelope(topic, &envelope);
}

/// Dispatch a parsed `DataEnvelope` to the appropriate handler.
fn dispatch_envelope(topic: &str, envelope: &DataEnvelope) {
    let payload = match &envelope.payload {
        Some(p) => p,
        None => {
            tracing::warn!(topic, "MQTT DataEnvelope has no payload");
            return;
        }
    };

    match payload {
        // ── Global resources (we publish these, ignore echoes) ──
        acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::AvailableMcps(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::AvailableSearches(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::AvailableEmbeddingModels(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::AvailableLsps(_) => {
            // These are our own published topics — ignore echoes.
        }

        // ── Agent lifecycle ──
        acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(status) => {
            tracing::debug!(
                agent_id = %status.agent_id,
                online = status.online,
                "MQTT AgentStatus received"
            );
        }
        acowork_core::mqtt_proto::data_envelope::Payload::AgentMeta(meta) => {
            tracing::debug!(
                agent_id = %meta.agent_id,
                "MQTT AgentMeta received"
            );
        }
        acowork_core::mqtt_proto::data_envelope::Payload::AgentConfig(config) => {
            tracing::debug!(
                agent_id = %config.agent_id,
                "MQTT AgentConfig received"
            );
        }

        // ── Session lifecycle ──
        acowork_core::mqtt_proto::data_envelope::Payload::SessionCreated(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionDeleted(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionMeta(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionConfig(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionMessage(_) => {
            tracing::debug!(
                topic,
                "MQTT session event received (Desktop-bound, Gateway does not process)"
            );
        }

        // ── Control commands (Desktop → Runtime, Gateway does not process) ──
        acowork_core::mqtt_proto::data_envelope::Payload::ControlCommand(_cmd) => {
            tracing::debug!(
                topic,
                "MQTT ControlCommand received (Desktop→Runtime, Gateway does not process)"
            );
        }

        // ── Memory ──
        acowork_core::mqtt_proto::data_envelope::Payload::MemoryNodeUpdate(_) => {
            tracing::debug!(topic, "MQTT MemoryNodeUpdate received");
        }

        // ── Sidecar ──
        acowork_core::mqtt_proto::data_envelope::Payload::SidecarStatus(_) => {
            tracing::debug!(topic, "MQTT SidecarStatus received");
        }
    }
}

/// Handle a plain-text MQTT message (non-DataEnvelope payload).
///
/// Called by the Gateway's MQTT message callback for topics that carry
/// simple text payloads rather than protobuf envelopes:
/// - `acowork/agents/+/http_port` → registers Runtime HTTP port for reverse proxy
/// - `acowork/agents/+/status` → updates AgentRegistry online/offline status
///
/// This replaces the inline callback previously in `gateway/mod.rs`.
pub fn handle_plaintext_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &crate::http::proxy::SharedRuntimeHttpRegistry,
    agent_registry: &crate::mqtt::agent_registry::SharedAgentRegistry,
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
        tokio::spawn(async move {
            reg.write().await.update_from_mqtt(&topic_owned, &payload_owned);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dispatch_global_resource_echo_is_ignored() {
        // Build an AvailableProviders envelope
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                acowork_core::mqtt_proto::AvailableProviders {
                    version: 1,
                    providers: vec![],
                },
            )),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        // Should not panic or log warnings — echoes are silently ignored
        dispatch_message("acowork/global/providers", &payload);
    }

    #[test]
    fn test_dispatch_invalid_payload_does_not_panic() {
        // Should handle gracefully (plain text "online"/"offline")
        dispatch_message("acowork/agents/foo/status", b"not a protobuf payload");
    }
}
