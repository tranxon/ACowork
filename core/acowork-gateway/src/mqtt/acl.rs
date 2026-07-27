//! ACL (Access Control List) configuration for the MQTT broker.
//!
//! In the single-user phase (ADR-033 Phase 1), the broker is bound to
//! localhost only and ACL is permissive — all localhost clients are
//! trusted. This module provides the structure for future multi-user
//! ACL enforcement.
//!
//! See `docs/zh/protocols/mqtt.md` §10 for the ACL design.


/// ACL permission for a single topic filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclPermission {
    /// Client may publish to this topic filter.
    Publish,
    /// Client may subscribe to this topic filter.
    Subscribe,
    /// Client may both publish and subscribe.
    PublishSubscribe,
}

/// A single ACL rule: maps a client_id pattern to a set of topic permissions.
#[derive(Debug, Clone)]
pub struct AclRule {
    /// Client ID pattern (supports `*` wildcard, e.g. `"agent:*"`, `"user:*:desktop:*"`).
    pub client_id_pattern: String,
    /// Topic filter → permission.
    pub topics: Vec<(String, AclPermission)>,
}

/// The full ACL configuration.
///
/// In Phase 1, this is loaded from `configs/rumqttd.toml` (or defaults
/// to a permissive single-user policy). In the multi-user phase, the
/// Gateway dynamically generates per-user ACL rules.
#[derive(Debug, Clone, Default)]
pub struct AclConfig {
    /// Ordered list of ACL rules. The first matching rule wins.
    pub rules: Vec<AclRule>,
}

impl AclConfig {
    /// Default permissive ACL for the single-user phase.
    ///
    /// All localhost clients are trusted — no topic restrictions.
    /// This is the Phase 1 behavior per mqtt.md §10.2.
    pub fn permissive() -> Self {
        AclConfig { rules: Vec::new() }
    }

    /// Build the single-user-phase ACL rules from mqtt.md §10.2.
    ///
    /// These rules are documentation-only in Phase 1 (the broker does not
    /// enforce them yet). They will be activated in the multi-user phase.
    #[allow(dead_code)]
    pub fn single_user_rules() -> Vec<AclRule> {
        vec![
            // Desktop clients: subscribe to all agent/global topics,
            // publish control commands.
            AclRule {
                client_id_pattern: "user:*:desktop:*".to_string(),
                topics: vec![
                    ("acowork/agents/+/status".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/meta".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/config".to_string(), AclPermission::Subscribe),
                    ("acowork/global/#".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/created".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/deleted".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/+/config".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/+/state".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/+/messages/#".to_string(), AclPermission::Subscribe),
                    ("acowork/sidecar/+/status".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/control/#".to_string(), AclPermission::Publish),
                ],
            },
            // Runtime clients: publish agent data, subscribe to global + control.
            AclRule {
                client_id_pattern: "agent:*".to_string(),
                topics: vec![
                    ("acowork/agents/+/status".to_string(), AclPermission::PublishSubscribe),
                    ("acowork/agents/+/meta".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/config".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/sessions/created".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/sessions/deleted".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/sessions/+/config".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/sessions/+/state".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/sessions/+/messages/#".to_string(), AclPermission::Publish),
                    ("acowork/agents/+/memory/#".to_string(), AclPermission::Publish),
                    ("acowork/global/#".to_string(), AclPermission::Subscribe),
                    ("acowork/agents/+/sessions/control/#".to_string(), AclPermission::Subscribe),
                ],
            },
            // Gateway publisher: only publish global available state.
            AclRule {
                client_id_pattern: "gateway:publisher".to_string(),
                topics: vec![
                    ("acowork/global/#".to_string(), AclPermission::Publish),
                ],
            },
        ]
    }

    /// Load ACL configuration from a TOML file.
    ///
    /// In Phase 1, the file is documentation-only — the actual enforcement
    /// is permissive (localhost trust). This function parses the file for
    /// future use but does not enforce the rules yet.
    pub fn load_from_toml(path: &std::path::Path) -> Result<Self, AclConfigError> {
        if !path.exists() {
            tracing::debug!(
                path = %path.display(),
                "ACL config file not found, using permissive defaults"
            );
            return Ok(Self::permissive());
        }

        let content = std::fs::read_to_string(path).map_err(|e| {
            AclConfigError::Io(format!("Failed to read ACL config '{}': {}", path.display(), e))
        })?;

        // Parse the rumqttd TOML to extract the server config.
        // In Phase 1, we only validate the structure — enforcement is permissive.
        let parsed: toml::Value = toml::from_str(&content).map_err(|e| {
            AclConfigError::Parse(format!("Failed to parse ACL config TOML: {}", e))
        })?;

        // Verify the v4 server exists
        if let Some(v4) = parsed.get("v4").and_then(|v| v.as_table()) {
            for (name, server) in v4 {
                tracing::debug!(
                    server = name,
                    listen = server.get("listen").and_then(|v| v.as_str()),
                    "Loaded MQTT server config from TOML"
                );
            }
        }

        // Phase 1: return permissive config (enforcement deferred to multi-user phase)
        tracing::info!(
            "ACL config loaded from {} (Phase 1: permissive mode, enforcement deferred)",
            path.display()
        );
        Ok(Self::permissive())
    }

    /// Check if a client_id matches a pattern (with `*` wildcard).
    ///
    /// `"agent:*"` matches `"agent:com.example.foo"` but not `"gateway:publisher"`.
    /// `"user:*:desktop:*"` matches `"user:abc:desktop:123"`.
    #[allow(dead_code)]
    pub fn matches_pattern(pattern: &str, client_id: &str) -> bool {
        // Split both into segments and match segment-by-segment.
        // `*` matches any single segment (not cross-segment).
        let pattern_parts: Vec<&str> = pattern.split(':').collect();
        let id_parts: Vec<&str> = client_id.split(':').collect();

        if pattern_parts.len() != id_parts.len() {
            return false;
        }

        pattern_parts
            .iter()
            .zip(id_parts.iter())
            .all(|(p, id)| *p == "*" || p == id)
    }

    /// Check if a client is allowed to publish to a topic.
    #[allow(dead_code)]
    pub fn can_publish(&self, client_id: &str, _topic: &str) -> bool {
        // Phase 1: permissive — all localhost clients can publish anywhere.
        // Multi-user phase: enforce rules here.
        let _ = client_id;
        true
    }

    /// Check if a client is allowed to subscribe to a topic filter.
    #[allow(dead_code)]
    pub fn can_subscribe(&self, client_id: &str, _filter: &str) -> bool {
        // Phase 1: permissive — all localhost clients can subscribe to anything.
        let _ = client_id;
        true
    }
}

/// Error type for ACL config loading.
#[derive(Debug, thiserror::Error)]
pub enum AclConfigError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("Parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permissive_allows_everything() {
        let acl = AclConfig::permissive();
        assert!(acl.can_publish("agent:foo", "acowork/agents/foo/status"));
        assert!(acl.can_subscribe("user:1:desktop:2", "acowork/global/#"));
    }

    #[test]
    fn test_pattern_matching() {
        assert!(AclConfig::matches_pattern("agent:*", "agent:com.example"));
        assert!(AclConfig::matches_pattern("user:*:desktop:*", "user:abc:desktop:123"));
        assert!(!AclConfig::matches_pattern("agent:*", "gateway:publisher"));
        assert!(!AclConfig::matches_pattern("user:*:desktop:*", "user:abc:desktop:123:extra"));
        assert!(AclConfig::matches_pattern("gateway:publisher", "gateway:publisher"));
    }

    #[test]
    fn test_single_user_rules_cover_all_client_types() {
        let rules = AclConfig::single_user_rules();
        assert_eq!(rules.len(), 3, "should have desktop, runtime, and gateway publisher rules");

        // Desktop rule
        let desktop = &rules[0];
        assert!(desktop.client_id_pattern.starts_with("user:*:desktop"));
        assert!(desktop.topics.iter().any(|(t, _)| t == "acowork/agents/+/status"));

        // Runtime rule
        let runtime = &rules[1];
        assert_eq!(runtime.client_id_pattern, "agent:*");
        assert!(runtime.topics.iter().any(|(t, _)| t == "acowork/global/#"));

        // Gateway publisher rule
        let publisher = &rules[2];
        assert_eq!(publisher.client_id_pattern, "gateway:publisher");
        assert!(
            publisher
                .topics
                .iter()
                .any(|(t, _)| t == "acowork/global/#"),
            "gateway publisher should publish global resources"
        );
    }

    #[test]
    fn test_load_from_toml_missing_file_returns_permissive() {
        let acl = AclConfig::load_from_toml(std::path::Path::new("/nonexistent/rumqttd.toml"))
            .expect("missing file should return permissive");
        assert!(acl.rules.is_empty(), "permissive config has no rules");
    }
}
