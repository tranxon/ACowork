//! Global resource available cache (ADR-033 Phase 2).
//!
//! In-memory cache of the latest `acowork/global/{kind}` Retained messages.
//! Updated by the Runtime MQTT client's event loop as it receives
//! `Available*` payloads. The Runtime queries this cache to know which
//! providers, MCPs, searches, embedding models, and LSP relays are
//! currently available (health-check verified by Gateway).
//!
//! See `docs/zh/protocols/mqtt.md` §3.1.1 and §4.

use std::sync::Arc;

use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{
    AvailableEmbeddingModels, AvailableLsps, AvailableMcps, AvailableProviders,
    AvailableSearches, DataEnvelope,
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
            _ => {
                // Not a global resource payload — ignore
            }
        }
    }

    /// Check if a provider is in the available list.
    #[allow(dead_code)]
    pub fn is_provider_available(&self, provider_id: &str) -> bool {
        self.providers
            .as_ref()
            .map(|p| p.providers.iter().any(|pr| pr.id == provider_id))
            .unwrap_or(false)
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
                    providers: vec![acowork_core::mqtt_proto::ProviderRef {
                        id: "openai".to_string(),
                        base_url: "https://api.openai.com/v1".to_string(),
                        protocol_type: acowork_core::mqtt_proto::LlmProtocol::Openai as i32,
                        models: vec![],
                        compact_model: String::new(),
                        custom: false,
                        api_key: String::new(),
                    }],
                },
            )),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        cache.update_from_mqtt("acowork/global/providers", &payload);

        assert!(cache.is_provider_available("openai"));
        assert!(!cache.is_provider_available("anthropic"));
        assert_eq!(cache.providers.as_ref().unwrap().version, 5);
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
}
