//! MQTT message dispatch (ADR-033).
//!
//! Handles incoming MQTT messages on the Gateway's broker connection.
//! Plain-text payloads (`http_port`, agent `status`) carry simple semantic
//! data (port number, online/offline) rather than `DataEnvelope` protobuf,
//! so they are matched by topic pattern and parsed inline.
//!
//! See `docs/zh/protocols/mqtt.md` §8 (Topic patterns).

use crate::http::proxy::SharedRuntimeHttpRegistry;
use crate::mqtt::agent_registry::SharedAgentRegistry;

/// Unified MQTT message handler — called from the Gateway's MQTT callback.
///
/// Currently only `handle_plaintext_message` is implemented; future phases
/// may add protobuf `DataEnvelope` dispatchers alongside it.
pub fn handle_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
) {
    handle_plaintext_message(topic, payload, runtime_http_registry, agent_registry);
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
/// - `acowork/agents/+/status` → updates AgentRegistry online/offline status
///
/// This replaces the inline callback previously in `gateway/mod.rs`.
pub fn handle_plaintext_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
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
}