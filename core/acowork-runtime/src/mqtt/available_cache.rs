//! Global resource available cache (ADR-033 Phase 2).
//!
//! In-memory cache of the latest `acowork/global/{kind}` Retained messages.
//! Updated by the Runtime MQTT client's event loop as it receives
//! `Available*` payloads. The Runtime queries this cache to know which
//! providers, MCPs, searches, embedding models, LSP relays, and the
//! active user profile are currently available (health-check verified
//! by Gateway; user profile is authoritative per ADR-042).
//!
//! See `docs/zh/protocols/mqtt.md` §3.1.1 and §4.

use std::sync::Arc;

use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{
    AvailableEmbeddingModels, AvailableLsps, AvailableMcps, AvailableProviders,
    AvailableSearches, AvailableUsers, DataEnvelope,
};

/// In-memory snapshot of all global resource available states.
///
/// Updated atomically when new Retained messages arrive on `acowork/global/#`.
#[derive(Debug, Clone, Default)]
pub struct AvailableResourceCache {
    pub providers: Option<AvailableProviders>,
    pub mcps: Option<AvailableMcps>,
    pub searches: Option<AvailableSearches>,
    pub embedding_models: Option<AvailableEmbeddingModels>,
    pub lsps: Option<AvailableLsps>,
    /// ADR-042: Active user profile (Gateway-authoritative). None until
    /// the first `acowork/global/user_profile` retained is received, or
    /// when no user has been created yet.
    pub user_profile: Option<AvailableUsers>,
}

impl AvailableResourceCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the cache from an incoming MQTT message.
    ///
    /// `topic` should be an `acowork/global/{kind}` topic.
    /// `payload` should be a protobuf-encoded `DataEnvelope`.
    pub fn update_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let envelope: DataEnvelope = match prost::Message::decode(payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    topic,
                    error = %e,
                    "Failed to decode DataEnvelope from global resource topic"
                );
                return;
            }
        };

        let payload = match envelope.payload {
            Some(p) => p,
            None => return,
        };

        match payload {
            acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(p) => {
                tracing::debug!(
                    version = p.version,
                    count = p.providers.len(),
                    "Cached AvailableProviders"
                );
                self.providers = Some(p);
            }
            acowork_core::mqtt_proto::data_envelope::Payload::AvailableMcps(p) => {
                tracing::debug!(
                    version = p.version,
                    count = p.servers.len(),
                    "Cached AvailableMcps"
                );
                self.mcps = Some(p);
            }
            acowork_core::mqtt_proto::data_envelope::Payload::AvailableSearches(p) => {
                tracing::debug!(
                    version = p.version,
                    count = p.providers.len(),
                    "Cached AvailableSearches"
                );
                self.searches = Some(p);
            }
            acowork_core::mqtt_proto::data_envelope::Payload::AvailableEmbeddingModels(p) => {
                tracing::debug!(
                    version = p.version,
                    count = p.models.len(),
                    active_model = %p.active_model_id,
                    "Cached AvailableEmbeddingModels"
                );
                self.embedding_models = Some(p);
            }
            acowork_core::mqtt_proto::data_envelope::Payload::AvailableLsps(p) => {
                tracing::debug!(
                    ready = p.ready,
                    endpoint = %p.endpoint,
                    "Cached AvailableLsps"
                );
                self.lsps = Some(p);
            }
            acowork_core::mqtt_proto::data_envelope::Payload::AvailableUsers(p) => {
                let active = p
                    .active_user
                    .as_ref()
                    .map(|u| u.user_id.as_str())
                    .unwrap_or("");
                tracing::debug!(
                    version = p.version,
                    active_user_id = %active,
                    "Cached AvailableUsers"
                );
                self.user_profile = Some(p);
            }
            _ => {
                // Not a global resource payload — ignore
            }
        }
    }

    /// Check if a provider is in the available list AND callable.
    ///
    /// ADR-056: a provider is callable when it either carries a non-empty
    /// API key (cloud) or points at a local base_url (self-hosted Ollama
    /// etc. — empty key by design). This mirrors
    /// `AgentCore::is_default_compact_provider_available`, which is what
    /// `resolve_distill_model` actually consults at distillation time.
    ///
    /// Note: the proto `ProviderRef` carries the decrypted key (empty for
    /// local providers) and `base_url`, so no Vault round-trip is needed.
    #[allow(dead_code)]
    pub fn is_provider_available(&self, provider_id: &str) -> bool {
        let Some(available) = self.providers.as_ref() else {
            return false;
        };
        available.providers.iter().any(|pr| {
            pr.id == provider_id
                && (!pr.api_key.is_empty() || crate::providers::is_local_base_url(&pr.base_url))
        })
    }

    /// Check if an MCP server is in the available list.
    #[allow(dead_code)]
    pub fn is_mcp_available(&self, mcp_id: &str) -> bool {
        self.mcps
            .as_ref()
            .map(|p| p.servers.iter().any(|s| s.id == mcp_id))
            .unwrap_or(false)
    }

    /// Get the embedding endpoint if an embedding model is loaded.
    pub fn embed_endpoint(&self) -> Option<String> {
        self.embedding_models
            .as_ref()
            .filter(|e| !e.endpoint.is_empty() && !e.active_model_id.is_empty())
            .map(|e| e.endpoint.clone())
    }

    /// Get the LSP relay endpoint if it's ready.
    pub fn lsp_endpoint(&self) -> Option<String> {
        self.lsps
            .as_ref()
            .filter(|l| l.ready && !l.endpoint.is_empty())
            .map(|l| l.endpoint.clone())
    }

    /// ADR-042: Get the active user profile as the full
    /// `acowork_core::protocol::UserProfile` struct (used by
    /// `format_user_profile_context` to build identity_context).
    ///
    /// Returns `None` when:
    /// - The cache hasn't received the first `acowork/global/user_profile`
    ///   retained yet (Runtime just started), OR
    /// - The Gateway hasn't created any user (empty active_user).
    ///
    /// The `custom` HashMap is reconstructed from the JSON-serialised
    /// `custom_json` field; an empty string decodes to an empty map.
    pub fn active_user_profile(&self) -> Option<acowork_core::protocol::UserProfile> {
        let p = self.user_profile.as_ref()?;
        let active = p.active_user.as_ref()?;
        if active.user_id.is_empty() {
            return None;
        }
        let custom: std::collections::HashMap<String, String> =
            serde_json::from_str(&active.custom_json).unwrap_or_else(|e| {
                tracing::warn!(
                    user_id = %active.user_id,
                    error = %e,
                    "Failed to parse UserProfileRef.custom_json; using empty map"
                );
                Default::default()
            });
        Some(acowork_core::protocol::UserProfile {
            user_id: active.user_id.clone(),
            display_name: active.display_name.clone(),
            language: active.language.clone(),
            timezone: active.timezone.clone(),
            city: active.city.clone(),
            country: active.country.clone(),
            occupation: active.occupation.clone(),
            avatar: None,        // not carried in UserProfileRef
            builtin_avatar: None, // not carried in UserProfileRef
            communication_style: active.communication_style.clone(),
            custom,
            created_at: String::new(), // not carried in UserProfileRef
            updated_at: String::new(), // not carried in UserProfileRef
            is_active: true,           // subscribed topic only carries active
        })
    }
}

/// Thread-safe shared AvailableResourceCache.
pub type SharedAvailableCache = Arc<RwLock<AvailableResourceCache>>;

/// Create a new shared AvailableResourceCache.
pub fn new_shared_cache() -> SharedAvailableCache {
    Arc::new(RwLock::new(AvailableResourceCache::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_from_mqtt_providers() {
        let mut cache = AvailableResourceCache::new();
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                AvailableProviders {
                    version: 5,
                    // ADR-056: No global default compact model in this fixture.
                    default_compact_model: None,
                    providers: vec![
                        acowork_core::mqtt_proto::ProviderRef {
                            id: "openai".to_string(),
                            base_url: "https://api.openai.com/v1".to_string(),
                            protocol_type: acowork_core::mqtt_proto::LlmProtocol::Openai as i32,
                            models: vec![],
                            compact_model: String::new(),
                            custom: false,
                            api_key: "sk-test".to_string(),
                        },
                        // Local provider — empty key by design, still callable.
                        acowork_core::mqtt_proto::ProviderRef {
                            id: "ollama-local".to_string(),
                            base_url: "http://localhost:11434/v1".to_string(),
                            protocol_type: acowork_core::mqtt_proto::LlmProtocol::Ollama as i32,
                            models: vec![],
                            compact_model: String::new(),
                            custom: false,
                            api_key: String::new(),
                        },
                    ],
                },
            )),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        cache.update_from_mqtt("acowork/global/providers", &payload);

        // Cloud provider with key → available; unknown provider → not.
        assert!(cache.is_provider_available("openai"));
        assert!(!cache.is_provider_available("anthropic"));
        assert_eq!(cache.providers.as_ref().unwrap().version, 5);
    }

    #[test]
    fn test_is_provider_available_local_provider_without_key() {
        // ADR-056 §9.1 ①: local provider (empty api_key + local base_url)
        // counts as available — distillation can target it without a key.
        let mut cache = AvailableResourceCache::new();
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                AvailableProviders {
                    version: 1,
                    default_compact_model: None,
                    providers: vec![acowork_core::mqtt_proto::ProviderRef {
                        id: "ollama-local".to_string(),
                        base_url: "http://localhost:11434/v1".to_string(),
                        protocol_type: acowork_core::mqtt_proto::LlmProtocol::Ollama as i32,
                        models: vec![],
                        compact_model: String::new(),
                        custom: false,
                        api_key: String::new(),
                    }],
                },
            )),
        };
        cache.update_from_mqtt("acowork/global/providers", &prost::Message::encode_to_vec(&envelope));
        assert!(cache.is_provider_available("ollama-local"));
    }

    #[test]
    fn test_is_provider_available_cloud_without_key_is_unavailable() {
        // ADR-056 §9.1 ②: cloud provider with empty api_key is NOT callable
        // (key revoked / never configured) → distillation falls back.
        let mut cache = AvailableResourceCache::new();
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                AvailableProviders {
                    version: 1,
                    default_compact_model: None,
                    providers: vec![acowork_core::mqtt_proto::ProviderRef {
                        id: "anthropic".to_string(),
                        base_url: "https://api.anthropic.com/v1".to_string(),
                        protocol_type: acowork_core::mqtt_proto::LlmProtocol::Anthropic as i32,
                        models: vec![],
                        compact_model: String::new(),
                        custom: false,
                        api_key: String::new(),
                    }],
                },
            )),
        };
        cache.update_from_mqtt("acowork/global/providers", &prost::Message::encode_to_vec(&envelope));
        assert!(!cache.is_provider_available("anthropic"));
    }

    #[test]
    fn test_is_provider_available_unknown_provider() {
        // ADR-056 §9.1 ③: provider absent from the available list → not available.
        let mut cache = AvailableResourceCache::new();
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                AvailableProviders {
                    version: 1,
                    default_compact_model: None,
                    providers: vec![],
                },
            )),
        };
        cache.update_from_mqtt("acowork/global/providers", &prost::Message::encode_to_vec(&envelope));
        assert!(!cache.is_provider_available("ghost-provider"));
    }

    #[test]
    fn test_update_from_mqtt_invalid_payload() {
        let mut cache = AvailableResourceCache::new();
        cache.update_from_mqtt("acowork/global/providers", b"not protobuf");
        assert!(cache.providers.is_none(), "invalid payload should not update cache");
    }

    #[test]
    fn test_embed_endpoint() {
        let mut cache = AvailableResourceCache::new();

        // No embedding models → no endpoint
        assert!(cache.embed_endpoint().is_none());

        // With active model
        cache.embedding_models = Some(AvailableEmbeddingModels {
            version: 1,
            models: vec![],
            active_model_id: "bge-small".to_string(),
            active_dimension: 512,
            endpoint: "http://127.0.0.1:18080/v1".to_string(),
        });
        assert_eq!(
            cache.embed_endpoint().as_deref(),
            Some("http://127.0.0.1:18080/v1")
        );
    }

    #[test]
    fn test_lsp_endpoint() {
        let mut cache = AvailableResourceCache::new();

        // No LSP → no endpoint
        assert!(cache.lsp_endpoint().is_none());

        // LSP not ready
        cache.lsps = Some(AvailableLsps {
            version: 1,
            endpoint: "http://127.0.0.1:19878".to_string(),
            ready: false,
        });
        assert!(cache.lsp_endpoint().is_none());

        // LSP ready
        cache.lsps = Some(AvailableLsps {
            version: 1,
            endpoint: "http://127.0.0.1:19878".to_string(),
            ready: true,
        });
        assert_eq!(
            cache.lsp_endpoint().as_deref(),
            Some("http://127.0.0.1:19878")
        );
    }

    // ── ADR-042: user_profile topic tests ─────────────────────────────

    #[test]
    fn test_update_from_mqtt_user_profile() {
        use acowork_core::mqtt_proto::UserProfileRef;
        let mut cache = AvailableResourceCache::new();

        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableUsers(
                AvailableUsers {
                    version: 7,
                    active_user: Some(UserProfileRef {
                        user_id: "u-1".to_string(),
                        display_name: "大鱼".to_string(),
                        language: "zh-CN".to_string(),
                        timezone: "Asia/Shanghai".to_string(),
                        city: Some("Beijing".to_string()),
                        country: Some("CN".to_string()),
                        occupation: Some("Software Engineer".to_string()),
                        communication_style: Some("concise".to_string()),
                        custom_json: r#"{"theme":"dark"}"#.to_string(),
                    }),
                },
            )),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        cache.update_from_mqtt("acowork/global/user_profile", &payload);

        let profile = cache.active_user_profile().expect("active user should be present");
        assert_eq!(profile.user_id, "u-1");
        assert_eq!(profile.display_name, "大鱼");
        assert_eq!(profile.language, "zh-CN");
        assert_eq!(profile.timezone, "Asia/Shanghai");
        assert_eq!(profile.city.as_deref(), Some("Beijing"));
        assert_eq!(profile.occupation.as_deref(), Some("Software Engineer"));
        assert_eq!(profile.communication_style.as_deref(), Some("concise"));
        assert_eq!(profile.custom.get("theme").map(String::as_str), Some("dark"));
        assert!(profile.is_active); // wire always carries active
    }

    #[test]
    fn test_active_user_profile_empty_when_no_user() {
        // Empty active_user (no user created yet) → None.
        let mut cache = AvailableResourceCache::new();
        cache.update_from_mqtt(
            "acowork/global/user_profile",
            &prost::Message::encode_to_vec(&DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableUsers(
                    AvailableUsers {
                        version: 0,
                        active_user: None,
                    },
                )),
            }),
        );
        assert!(cache.active_user_profile().is_none());
    }

    #[test]
    fn test_active_user_profile_invalid_custom_json() {
        // Bad JSON in custom_json should not crash — fall back to empty map.
        let mut cache = AvailableResourceCache::new();
        cache.update_from_mqtt(
            "acowork/global/user_profile",
            &prost::Message::encode_to_vec(&DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableUsers(
                    AvailableUsers {
                        version: 1,
                        active_user: Some(acowork_core::mqtt_proto::UserProfileRef {
                            user_id: "u-1".into(),
                            display_name: "Test".into(),
                            language: "en-US".into(),
                            timezone: "UTC".into(),
                            city: None,
                            country: None,
                            occupation: None,
                            communication_style: None,
                            custom_json: "not valid json".into(),
                        }),
                    },
                )),
            }),
        );
        let profile = cache.active_user_profile().expect("should still return Some");
        assert!(profile.custom.is_empty());
    }
}
