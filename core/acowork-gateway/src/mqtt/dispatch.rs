//! MQTT message dispatch (ADR-033 Phase 1 scaffolding).
//!
//! Parses `DataEnvelope` payloads from incoming MQTT messages and routes
//! them to the appropriate business logic handler. In Phase 1, this is
//! minimal — the Gateway mainly publishes, not subscribes to business
//! topics. Phase 2+ will implement full dispatch for control commands,
//! session messages, etc.
//!
//! See `docs/zh/protocols/mqtt.md` §8 (Topic patterns).

use rumqttc::Publish;

use acowork_core::mqtt_proto::DataEnvelope;

use crate::mqtt::router::{route_message, RouteResult};

/// Dispatch an incoming MQTT message.
///
/// Steps:
/// 1. Route the message by topic (router.rs).
/// 2. If the route is unimplemented (Phase 2+ handler), log and return.
/// 3. If the route matches, attempt to parse the payload as a `DataEnvelope`.
/// 4. Dispatch the envelope's oneof payload to the appropriate handler.
pub fn dispatch_message(publish: &Publish) {
    // Step 1: Route by topic
    match route_message(publish) {
        RouteResult::NoMatch => {
            // Not a known route pattern. Still try to parse as DataEnvelope
            // in case it's a generic proto message (Step 2 below).
        }
        RouteResult::Handled => {
            return;
        }
        RouteResult::Unimplemented(reason) => {
            tracing::debug!(
                topic = %publish.topic,
                reason,
                "MQTT message routed to unimplemented handler (Phase 2+)"
            );
            return;
        }
    }

    // Step 2: Parse the payload as a DataEnvelope
    let envelope = match prost::Message::decode(publish.payload.as_ref()) {
        Ok(e) => e,
        Err(e) => {
            // The payload might be plain text (e.g. agent status "online"/"offline")
            // rather than a protobuf envelope. Log at trace level to avoid noise.
            tracing::trace!(
                topic = %publish.topic,
                error = %e,
                "MQTT payload is not a valid DataEnvelope (may be plain text)"
            );
            return;
        }
    };

    // Step 3: Dispatch by payload type
    dispatch_envelope(&publish.topic, &envelope);
}

/// Dispatch a parsed `DataEnvelope` to the appropriate handler.
///
/// In Phase 1, most payload types are unimplemented (they arrive from
/// Runtime, which doesn't use MQTT yet). The Gateway only publishes
/// `Available*` payloads — it doesn't receive them.
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

        // ── Agent lifecycle (Phase 2: Runtime publishes) ──
        acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(status) => {
            tracing::debug!(
                agent_id = %status.agent_id,
                online = status.online,
                "MQTT AgentStatus received (Phase 2: update AgentRegistry)"
            );
        }
        acowork_core::mqtt_proto::data_envelope::Payload::AgentMeta(meta) => {
            tracing::debug!(
                agent_id = %meta.agent_id,
                "MQTT AgentMeta received (Phase 2: cache for HTTP API)"
            );
        }
        acowork_core::mqtt_proto::data_envelope::Payload::AgentConfig(config) => {
            tracing::debug!(
                agent_id = %config.agent_id,
                "MQTT AgentConfig received (Phase 2: cache for HTTP API)"
            );
        }

        // ── Session lifecycle (Phase 2: Runtime publishes, Desktop subscribes) ──
        acowork_core::mqtt_proto::data_envelope::Payload::SessionCreated(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionDeleted(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionMeta(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionConfig(_)
        | acowork_core::mqtt_proto::data_envelope::Payload::SessionMessage(_) => {
            tracing::debug!(
                topic,
                "MQTT session event received (Phase 2: forward to Desktop)"
            );
        }

        // ── Control commands (Phase 2: Desktop publishes, Runtime subscribes) ──
        acowork_core::mqtt_proto::data_envelope::Payload::ControlCommand(_cmd) => {
            tracing::debug!(
                topic,
                "MQTT ControlCommand received (Phase 2: forward to Runtime)"
            );
        }

        // ── Memory (Phase 2: Runtime publishes) ──
        acowork_core::mqtt_proto::data_envelope::Payload::MemoryNodeUpdate(_) => {
            tracing::debug!(topic, "MQTT MemoryNodeUpdate received (Phase 2)");
        }

        // ── Sidecar (Phase 2: sidecar processes publish) ──
        acowork_core::mqtt_proto::data_envelope::Payload::SidecarStatus(_) => {
            tracing::debug!(topic, "MQTT SidecarStatus received (Phase 2)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::Publish;

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

        let publish = Publish::new(
            "acowork/global/providers",
            rumqttc::QoS::AtLeastOnce,
            payload,
        );

        // Should not panic or log warnings — echoes are silently ignored
        dispatch_message(&publish);
    }

    #[test]
    fn test_dispatch_invalid_payload_does_not_panic() {
        let publish = Publish::new(
            "acowork/agents/foo/status",
            rumqttc::QoS::AtLeastOnce,
            b"not a protobuf payload",
        );

        // Should handle gracefully (plain text "online"/"offline")
        dispatch_message(&publish);
    }
}
