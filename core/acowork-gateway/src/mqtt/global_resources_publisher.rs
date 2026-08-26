//! Global Resources Publisher (ADR-033 Phase 1).
//!
//! The Gateway's MQTT publisher for `acowork/global/{kind}` Retained topics.
//! Reads from `GatewayState.resource_cache` + sidecar process state, builds
//! `Available*` protobuf payloads, and publishes them as Retained messages
//! on the MQTT broker.
//!
//! In Phase 1, this runs alongside the existing gRPC `()`.
//! When a resource changes (provider added, MCP installed, embedding model
//! loaded, etc.), both pushers fire: gRPC pushes to connected Runtimes,
//! MQTT publishes the Retained snapshot for future subscribers.
//!
//! Retained messages ensure new Runtime subscribers receive the latest
//! global resource state immediately on subscription, without waiting for
//! the next periodic publish cycle.
//!
//! See `docs/zh/protocols/mqtt.md` §3.1.1 and §5.4.

use std::sync::Arc;

use tokio::sync::Notify;

use acowork_core::mqtt_proto::{
    self, AvailableEmbeddingModels, AvailableLsps, AvailableMcps, AvailableProviders,
    AvailableSearches, AvailableUsers, DataEnvelope, EmbeddingModelRef, McpRef,
    ProviderModelRef, ProviderRef, SearchRef, UserProfileRef,
};
use acowork_core::protocol::{McpTransportDef, ProtocolType};

use crate::gateway::state::GatewayState;
use crate::http::routes::SharedHttpState;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

/// MQTT topic constants for global resources (§3.1.1).
mod topics {
    pub const PROVIDERS: &str = "acowork/global/providers";
    pub const MCPS: &str = "acowork/global/mcps";
    pub const SEARCHES: &str = "acowork/global/searches";
    pub const EMBEDDING_MODELS: &str = "acowork/global/embedding_models";
    pub const LSPS: &str = "acowork/global/lsps";
    /// ADR-042: active user profile snapshot. Runtime uses this to populate
    /// the identity_context for the compact model's language hint.
    pub const USER_PROFILE: &str = "acowork/global/user_profile";
}

/// The Gateway's MQTT global resources publisher.
///
/// Holds a `GatewayMqttClient` and a `Notify` trigger. When a resource
/// changes, call `trigger_republish()` to wake the background loop,
/// which reads the latest state and publishes all `acowork/global/*`
/// Retained topics.
pub struct MqttGlobalResourcesPublisher {
    /// The MQTT client used to publish.
    client: GatewayMqttClient,
    /// Shared Gateway state (read-only access for building payloads).
    gateway_state: SharedHttpState,
    /// Notification trigger for republish.
    notify: Arc<Notify>,
}

/// Handle returned by `start()`. Dropping it stops the publisher loop.
pub struct MqttPublisherHandle {
    _task: tokio::task::JoinHandle<()>,
    notify: Arc<Notify>,
}

/// Clonable trigger for the MQTT publisher.
///
/// Stored in `AppState` so HTTP handlers can signal the publisher
/// to republish after a resource change, without holding a full
/// `MqttPublisherHandle` (which is not clonable).
#[derive(Clone)]
pub struct MqttPublisherTrigger {
    notify: Arc<Notify>,
}

impl MqttPublisherTrigger {
    /// Trigger an immediate republish of all global resource topics.
    pub fn trigger(&self) {
        self.notify.notify_one();
    }
}

impl MqttPublisherHandle {
    /// Trigger an immediate republish of all global resource topics.
    pub fn trigger_republish(&self) {
        self.notify.notify_one();
    }

    /// Get a clonable trigger that HTTP handlers can store in AppState.
    pub fn create_trigger(&self) -> MqttPublisherTrigger {
        MqttPublisherTrigger {
            notify: self.notify.clone(),
        }
    }
}

impl MqttGlobalResourcesPublisher {
    /// Create a new publisher with the given MQTT client and Gateway state.
    pub fn new(client: GatewayMqttClient, gateway_state: SharedHttpState) -> Self {
        Self {
            client,
            gateway_state,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Start the publisher background loop.
    ///
    /// The loop:
    /// 1. Publishes all `acowork/global/*` topics once at startup (initial Retained snapshot).
    /// 2. Waits for `trigger_republish()` calls from HTTP handlers after resource changes.
    /// 3. On each trigger, re-reads state and republishes all topics as Retained messages.
    ///
    /// No periodic polling — Retained messages ensure new subscribers get the latest
    /// snapshot immediately, and every resource-mutating HTTP handler calls
    /// `MqttPublisherTrigger::trigger()` to drive republish on change.
    pub fn start(self) -> MqttPublisherHandle {
        let notify = self.notify.clone();
        let notify_for_loop = self.notify.clone();
        let task = tokio::spawn(async move {
            tracing::info!("MQTT Global Resources Publisher loop started");

            // Initial publish: send the current snapshot immediately.
            self.publish_all().await;

            // Trigger-driven republish loop — no periodic polling needed.
            // Every resource-mutating HTTP handler (add/remove/update provider,
            // MCP catalog entry, embedding model, search key, global config)
            // calls `trigger()` which wakes this loop via `Notify`.
            loop {
                notify_for_loop.notified().await;
                tracing::debug!("MQTT publisher: triggered republish");
                self.publish_all().await;
            }
        });

        MqttPublisherHandle { _task: task, notify }
    }

    /// Publish all `acowork/global/*` Retained topics.
    ///
    /// Reads the current GatewayState snapshot and publishes:
    /// - `acowork/global/providers` — AvailableProviders
    /// - `acowork/global/mcps` — AvailableMcps
    /// - `acowork/global/searches` — AvailableSearches
    /// - `acowork/global/embedding_models` — AvailableEmbeddingModels
    /// - `acowork/global/lsps` — AvailableLsps
    /// - `acowork/global/user_profile` — AvailableUsers (ADR-042)
    async fn publish_all(&self) {
        let gw = self.gateway_state.read().await;

        // Build all payloads from the snapshot.
        let providers_payload = build_available_providers(&gw);
        let mcps_payload = build_available_mcps(&gw);
        let searches_payload = build_available_searches(&gw);
        let embedding_payload = build_available_embedding_models(&gw);
        let lsp_payload = build_available_lsps(&gw);
        let user_profile_payload = build_available_users(&gw);

        tracing::debug!(
            provider_count = providers_payload.providers.len(),
            api_key_lengths = ?providers_payload
                .providers
                .iter()
                .map(|p| (p.id.clone(), p.api_key.len()))
                .collect::<Vec<_>>(),
            "MQTT publisher: built AvailableProviders payload (debug)"
        );

        // Drop the read lock before publishing (don't hold it across network I/O).
        drop(gw);

        // Publish each topic. Errors are logged but don't abort the batch.
        self.publish_providers(providers_payload).await;
        self.publish_mcps(mcps_payload).await;
        self.publish_searches(searches_payload).await;
        self.publish_embedding_models(embedding_payload).await;
        self.publish_lsps(lsp_payload).await;
        self.publish_user_profiles(user_profile_payload).await;
    }

    async fn publish_providers(&self, payload: AvailableProviders) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableProviders(payload)),
        };
        self.publish_envelope_raw(topics::PROVIDERS, &envelope).await;
    }

    async fn publish_mcps(&self, payload: AvailableMcps) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableMcps(payload)),
        };
        self.publish_envelope_raw(topics::MCPS, &envelope).await;
    }

    async fn publish_searches(&self, payload: AvailableSearches) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableSearches(payload)),
        };
        self.publish_envelope_raw(topics::SEARCHES, &envelope).await;
    }

    async fn publish_embedding_models(&self, payload: AvailableEmbeddingModels) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableEmbeddingModels(payload)),
        };
        self.publish_envelope_raw(topics::EMBEDDING_MODELS, &envelope).await;
    }

    async fn publish_lsps(&self, payload: AvailableLsps) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableLsps(payload)),
        };
        self.publish_envelope_raw(topics::LSPS, &envelope).await;
    }

    /// ADR-042: publish the active user profile snapshot.
    ///
    /// Empty `active_user` (no user created yet) is still published as a
    /// retained message with `version` bumped — this signals to Runtime
    /// "no identity, fall back to detection-based heuristics".
    async fn publish_user_profiles(&self, payload: AvailableUsers) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableUsers(payload)),
        };
        self.publish_envelope_raw(topics::USER_PROFILE, &envelope).await;
    }

    /// Helper: publish a DataEnvelope with Retained=true.
    async fn publish_envelope_raw(&self, topic: &str, envelope: &DataEnvelope) {
        if let Err(e) = self
            .client
            .publish_envelope(topic, envelope, MqttQoS::AtLeastOnce, true)
            .await
        {
            tracing::warn!(topic, error = %e, "Failed to publish MQTT global resource topic");
        } else {
            tracing::debug!(topic, "Published MQTT global resource (Retained)");
        }
    }
}

// ── Payload builders ───────────────────────────────────────────────────

/// Build `AvailableProviders` from the GatewayState resource cache.
///
/// In Phase 1, "available" = all providers in the cache (the cache is
/// rebuilt by HTTP handlers when providers change). In Phase 2+, the
/// health-check loop will filter to only ready providers.
fn build_available_providers(gw: &GatewayState) -> AvailableProviders {
    let cache = &gw.resource_cache.provider_list;
    let providers: Vec<ProviderRef> = cache
        .providers
        .iter()
        .map(|p| {
            // Decrypt the provider's API key from the Gateway's Vault.
            // Empty string when no key is configured (e.g. local Ollama).
            // MQTT broker is localhost-only so it's safe to publish the
            // decrypted key in the retained payload — see mqtt.md §3.1.1.
            let api_key = gw
                .vault
                .get_provider(&p.id)
                .map(|entry| entry.api_key)
                .unwrap_or_default();
            ProviderRef {
                id: p.id.clone(),
                base_url: p.base_url.clone(),
                protocol_type: map_protocol_type(&p.protocol_type).into(),
                models: p
                    .models
                    .iter()
                    .map(|m| {
                        let (input_modalities, output_modalities) = m
                            .capabilities
                            .modalities
                            .as_ref()
                            .map(|moda| (moda.input.clone(), moda.output.clone()))
                            .unwrap_or_default();
                        ProviderModelRef {
                            id: m.id.clone(),
                            capabilities: Some(mqtt_proto::ModelCapabilities {
                                context_window: m.capabilities.context_window,
                                max_output_tokens: m.capabilities.max_output_tokens,
                                input_modalities,
                                output_modalities,
                                supports_reasoning: m.capabilities.supports_reasoning,
                                default_reasoning_effort: m.capabilities.default_reasoning_effort.clone(),
                            }),
                            max_output_tokens_limit: m.max_output_tokens_limit,
                        }
                    })
                    .collect(),
                compact_model: p.compact_model.clone().unwrap_or_default(),
                custom: p.custom,
                api_key,
            }
        })
        .collect();

    AvailableProviders {
        version: cache.version,
        providers,
        // ADR-056: forward the global default compact model so Runtime can
        // resolve the distillation fallback chain without an extra round-trip.
        // `None` (i.e. not set in `provider_list.json`) means "no global
        // override" — Runtime falls back to provider.compact_model and chat.
        default_compact_model: cache.default_compact_model.clone().map(|r| {
            mqtt_proto::CompactModelRef {
                provider_id: r.provider_id,
                model_id: r.model_id,
            }
        }),
    }
}

/// Build `AvailableMcps` from the GatewayState resource cache.
///
/// The `auth_token` is extracted from env vars and headers via
/// `extract_api_key_from_mcp_config` — same logic that builds
/// `mcp_key_vault` for gRPC AgentHello. Empty when no auth required.
fn build_available_mcps(gw: &GatewayState) -> AvailableMcps {
    let cache = &gw.resource_cache.mcp_list;
    // MCP catalog is the source of truth for env/headers, not the
    // resource_cache (which only stores server lists). Load it here.
    let data_dir = gw
        .config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));
    let catalog: Vec<acowork_core::protocol::McpServerConfigDef> = crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir)
        .ok()
        .unwrap_or_default();
    let servers: Vec<McpRef> = cache
        .servers
        .iter()
        .map(|s| {
            // Look up the catalog entry to extract env/headers for the token.
            let auth_token = catalog
                .iter()
                .find(|c| c.name == s.id)
                .and_then(crate::resource_cache::extract_api_key_from_mcp_config)
                .unwrap_or_default();
            McpRef {
                id: s.id.clone(),
                name: s.name.clone(),
                transport: map_mcp_transport(&s.transport).into(),
                url: s.url.clone().unwrap_or_default(),
                command: s.command.clone(),
                args: s.args.clone(),
                env: s.env.clone(),
                headers: s.headers.clone(),
                tool_timeout_secs: s.tool_timeout_secs.unwrap_or(0),
                auth_token,
            }
        })
        .collect();

    AvailableMcps {
        version: cache.version,
        servers,
    }
}

/// Build `AvailableSearches` from the GatewayState resource cache.
///
/// `api_key` is decrypted from the Gateway's Vault at PUBLISH time,
/// mirroring the logic in `build_search_key_vault` for gRPC AgentHello.
/// Empty when the search provider has no key configured.
fn build_available_searches(gw: &GatewayState) -> AvailableSearches {
    let cache = &gw.resource_cache.search_list;
    let providers: Vec<SearchRef> = cache
        .providers
        .iter()
        .map(|s| {
            let api_key = gw
                .vault
                .get_search_key(&s.id)
                .map(|entry| entry.api_key)
                .unwrap_or_default();
            SearchRef {
                id: s.id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                requires_api_key: s.requires_api_key,
                base_url: s.base_url.clone(),
                api_key,
            }
        })
        .collect();

    AvailableSearches {
        version: cache.version,
        providers,
    }
}

/// Build `AvailableEmbeddingModels` from the GatewayState resource cache
/// + embed process state.
fn build_available_embedding_models(gw: &GatewayState) -> AvailableEmbeddingModels {
    let cache = &gw.resource_cache.embedding_models;
    let models: Vec<EmbeddingModelRef> = cache
        .models
        .iter()
        .map(|m| EmbeddingModelRef {
            id: m.id.clone(),
            name: m.name.clone(),
            description: m.description.clone().unwrap_or_default(),
            dimension: m.dimension as u32,
            max_tokens: m.max_tokens as u32,
            size_mb: m.size_mb,
            languages: m.languages.clone(),
            hf_repo: m.hf_repo.clone(),
            onnx_file: m.onnx_file.clone(),
            tokenizer_file: m.tokenizer_file.clone(),
            bundled: m.bundled,
            recommended: m.recommended,
        })
        .collect();

    // Active model info from embed process state.
    let (active_model_id, active_dimension, endpoint) = match &gw.embed_process {
        Some(eps) if eps.ready => (
            eps.active_model_id.clone().unwrap_or_default(),
            eps.active_dimension.unwrap_or(0) as u32,
            // ADR-055 D3: advertise host instead of hard-coded 127.0.0.1.
            format!("http://{}:{}/v1", gw.advertise_host, eps.port),
        ),
        _ => (String::new(), 0, String::new()),
    };

    AvailableEmbeddingModels {
        version: cache.version,
        models,
        active_model_id,
        active_dimension,
        endpoint,
    }
}

/// Build `AvailableLsps` from the GatewayState LSP relay process state.
fn build_available_lsps(gw: &GatewayState) -> AvailableLsps {
    match &gw.lsp_relay_process {
        Some(lsp) if lsp.ready => AvailableLsps {
            version: 1,
            // ADR-055 D3: advertise host instead of hard-coded 127.0.0.1.
            endpoint: format!("http://{}:{}", gw.advertise_host, lsp.port),
            ready: true,
        },
        _ => AvailableLsps {
            version: 1,
            endpoint: String::new(),
            ready: false,
        },
    }
}

/// ADR-042: Build `AvailableUsers` from the GatewayState user profile list.
///
/// Finds the user with `is_active == true` and serialises it into
/// `UserProfileRef`. UI-only fields (avatar / builtin_avatar /
/// created_at / updated_at / is_active) are omitted — Runtime never
/// renders user profile UI. `custom` HashMap is serialised to JSON.
fn build_available_users(gw: &GatewayState) -> AvailableUsers {
    let list = &gw.resource_cache.user_profile_list;
    let active = list.users.iter().find(|u| u.is_active).map(|u| {
        let custom_json = serde_json::to_string(&u.custom).unwrap_or_else(|e| {
            tracing::warn!(
                user_id = %u.user_id,
                error = %e,
                "Failed to serialise UserProfile.custom to JSON; sending empty"
            );
            "{}".to_string()
        });
        UserProfileRef {
            user_id: u.user_id.clone(),
            display_name: u.display_name.clone(),
            language: u.language.clone(),
            timezone: u.timezone.clone(),
            city: u.city.clone(),
            country: u.country.clone(),
            occupation: u.occupation.clone(),
            communication_style: u.communication_style.clone(),
            custom_json,
        }
    });

    AvailableUsers {
        version: list.version,
        active_user: active,
    }
}

// ── Enum mappers ───────────────────────────────────────────────────────

fn map_protocol_type(pt: &ProtocolType) -> mqtt_proto::LlmProtocol {
    match pt {
        ProtocolType::OpenAI => mqtt_proto::LlmProtocol::Openai,
        ProtocolType::Anthropic => mqtt_proto::LlmProtocol::Anthropic,
        ProtocolType::Google => mqtt_proto::LlmProtocol::Google,
        ProtocolType::Ollama => mqtt_proto::LlmProtocol::Ollama,
    }
}

fn map_mcp_transport(t: &McpTransportDef) -> mqtt_proto::McpTransport {
    match t {
        McpTransportDef::Stdio => mqtt_proto::McpTransport::Stdio,
        McpTransportDef::Http => mqtt_proto::McpTransport::Http,
        McpTransportDef::Sse => mqtt_proto::McpTransport::Sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_protocol_type() {
        assert_eq!(
            map_protocol_type(&ProtocolType::OpenAI),
            mqtt_proto::LlmProtocol::Openai
        );
        assert_eq!(
            map_protocol_type(&ProtocolType::Anthropic),
            mqtt_proto::LlmProtocol::Anthropic
        );
    }

    #[test]
    fn test_map_mcp_transport() {
        assert_eq!(
            map_mcp_transport(&McpTransportDef::Stdio),
            mqtt_proto::McpTransport::Stdio
        );
        assert_eq!(
            map_mcp_transport(&McpTransportDef::Http),
            mqtt_proto::McpTransport::Http
        );
    }

    #[test]
    fn test_build_available_providers_empty() {
        let gw = GatewayState::new("/tmp/test-vault");
        let payload = build_available_providers(&gw);
        assert_eq!(payload.version, 0);
        assert!(payload.providers.is_empty());
    }

    #[test]
    fn test_build_available_lsps_no_process() {
        let gw = GatewayState::new("/tmp/test-vault");
        let payload = build_available_lsps(&gw);
        assert!(!payload.ready);
        assert!(payload.endpoint.is_empty());
    }

    #[tokio::test]
    async fn test_publisher_publishes_retained_snapshot() {
        use crate::http::routes::SharedHttpState;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let port = 18978;
        // Threaded mode: `start_broker` blocks forever on rumqttd's
        // `Broker::start()` (it joins the server threads), so calling it
        // from the test thread would hang whenever the port is free.
        let broker = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");

        let gw_state: SharedHttpState = Arc::new(RwLock::new(GatewayState::new("/tmp/test-vault")));
        let publisher = MqttGlobalResourcesPublisher::new(client, gw_state);
        let handle = publisher.start();

        // Trigger a republish
        handle.trigger_republish();

        // Give the publisher a moment to publish
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Subscribe and verify we receive the retained messages.
        use rumqttc::{AsyncClient, MqttOptions, QoS};
        let mut opts = MqttOptions::new("test:subscriber", "127.0.0.1", port);
        opts.set_keep_alive(std::time::Duration::from_secs(5));
        let (sub_client, mut eventloop) = AsyncClient::new(opts, 10);
        sub_client
            .subscribe("acowork/global/#", QoS::AtLeastOnce)
            .await
            .unwrap();

        // Poll for retained messages
        let mut received_topics = Vec::new();
        for _ in 0..50 {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    received_topics.push(p.topic.clone());
                    if received_topics.len() >= 6 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }

        assert!(
            received_topics.contains(&"acowork/global/providers".to_string()),
            "should receive providers retained: {:?}",
            received_topics
        );
        assert!(
            received_topics.contains(&"acowork/global/mcps".to_string()),
            "should receive mcps retained: {:?}",
            received_topics
        );
        // ADR-042: verify the new user_profile retained topic is published.
        assert!(
            received_topics.contains(&"acowork/global/user_profile".to_string()),
            "should receive user_profile retained (ADR-042): {:?}",
            received_topics
        );

        drop(sub_client);
        drop(handle);
        drop(broker);
    }
}
