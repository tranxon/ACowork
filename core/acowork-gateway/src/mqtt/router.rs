//! Topic Router (ADR-033 Phase 1 scaffolding).
//!
//! Matches incoming MQTT messages to handlers based on topic patterns.
//! In Phase 1, this is minimal — the Gateway mainly publishes, not
//! subscribes to business topics. In Phase 2+, when Runtime connects
//! via MQTT, the router dispatches `agents/{id}/sessions/control/#`
//! messages to the appropriate handlers.
//!
//! See `docs/zh/protocols/mqtt.md` §3 (Topic tree) and §8.4 (wildcards).

use rumqttc::Publish;

/// Result of routing a message.
#[derive(Debug)]
pub enum RouteResult {
    /// The message was handled by a registered handler.
    Handled,
    /// The message did not match any route.
    NoMatch,
    /// The message matched a route but the handler was not registered
    /// (Phase 2+ handlers not yet implemented).
    Unimplemented(String),
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

/// Route an incoming MQTT message to the appropriate handler.
///
/// In Phase 1, this only logs the message. Phase 2+ will dispatch
/// to business logic handlers (control commands, agent status, etc.).
pub fn route_message(publish: &Publish) -> RouteResult {
    let topic = &publish.topic;

    // ── Global resources (we publish these, but might also receive echoes) ──
    if topic.starts_with("acowork/global/") {
        return RouteResult::NoMatch; // we're the publisher, ignore echoes
    }

    // ── Agent status (Phase 2: Runtime publishes, we track in AgentRegistry) ──
    if topic_matches("acowork/agents/+/status", topic) {
        return RouteResult::Unimplemented(format!("agent status: {}", topic));
    }

    // ── Agent meta/config (Phase 2: Runtime publishes, we cache for HTTP API) ──
    if topic_matches("acowork/agents/+/meta", topic)
        || topic_matches("acowork/agents/+/config", topic)
    {
        return RouteResult::Unimplemented(format!("agent meta/config: {}", topic));
    }

    // ── Session control (Phase 2: Desktop publishes, we forward to Runtime) ──
    if topic_matches("acowork/agents/+/sessions/control/#", topic) {
        return RouteResult::Unimplemented(format!("session control: {}", topic));
    }

    // ── Session lifecycle events (Phase 2: Runtime publishes) ──
    if topic_matches("acowork/agents/+/sessions/created", topic)
        || topic_matches("acowork/agents/+/sessions/deleted", topic)
    {
        return RouteResult::Unimplemented(format!("session lifecycle: {}", topic));
    }

    // ── Session messages (Phase 2: Runtime publishes, Desktop subscribes) ──
    if topic_matches("acowork/agents/+/sessions/+/messages/#", topic) {
        return RouteResult::Unimplemented(format!("session message: {}", topic));
    }

    // ── Memory (Phase 2: Runtime publishes) ──
    if topic_matches("acowork/agents/+/memory/#", topic) {
        return RouteResult::Unimplemented(format!("memory: {}", topic));
    }

    // ── Sidecar (Phase 2: sidecar processes publish) ──
    if topic_matches("acowork/sidecar/+/status", topic) {
        return RouteResult::Unimplemented(format!("sidecar: {}", topic));
    }

    tracing::trace!(topic, "MQTT message did not match any route");
    RouteResult::NoMatch
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
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/sessions/s1/meta"));
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
