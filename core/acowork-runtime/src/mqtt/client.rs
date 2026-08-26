//! Runtime MQTT client (ADR-033 Phase 2).
//!
//! Connects to the Gateway's embedded broker with:
//! - `client_id: "agent:{agent_id}"`
//! - Last Will: `acowork/agents/{id}/status = "offline"` (Retained, QoS 1)
//!
//! On connect, publishes:
//! - `acowork/agents/{id}/status = "online"` (Retained)
//! - `acowork/agents/{id}/meta` (Retained) — AgentMeta protobuf
//! - `acowork/agents/{id}/config` (Retained) — AgentConfig protobuf
//!
//! Subscribes to:
//! - `acowork/global/#` — global resource available state
//! - `acowork/agents/{id}/sessions/control/#` — control commands from Desktop
//!
//! See `docs/zh/protocols/mqtt.md` §5.1 (startup sequence) and §8.1 (Will Message).

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, QoS};
use tokio::sync::{oneshot, Mutex};

use acowork_core::defaults;
use acowork_core::mqtt_proto::{
    data_envelope, session_message,
    AgentConfig, AgentMeta, AskQuestionPayload, ChunkPayload, CompactingPayload,
    ContextUsagePayload, DataEnvelope, DonePayload, ErrorPayload,
    IterationLimitPausedPayload, LoopDetectedPausedPayload, McpTransport as ProtoMcpTransport,
    NewDataAvailablePayload, RecordCompletePayload,
    SessionMessage, StoppedPayload, StreamDeltaPayload,
    StreamLine, TodoUpdatedPayload, ToolApprovalNeededPayload,
};
use acowork_mqtt_session::{
    classify as classify_err, ErrorDescriptor, ReconnectPolicy, SessionState, SessionStateRx,
    SessionStateTx,
};

use crate::mqtt::available_cache::SharedAvailableCache;
use acowork_core::protocol::McpTransportDef;

/// Convert a wire-format [`ProtoMcpTransport`] (from `McpRef` over MQTT)
/// to the on-disk [`McpTransportDef`] used by `agent_mcp.json::catalog`.
///
/// Inverse of `gateway::mqtt::global_resources_publisher::map_mcp_transport`.
/// The `Unspecified` variant is mapped to `Stdio` for backward
/// compatibility with hand-written catalog entries that omitted the
/// field; production data always carries an explicit transport.
fn mcp_transport_to_def(t: i32) -> McpTransportDef {
    match ProtoMcpTransport::try_from(t) {
        Ok(ProtoMcpTransport::Stdio) => McpTransportDef::Stdio,
        Ok(ProtoMcpTransport::Http) => McpTransportDef::Http,
        Ok(ProtoMcpTransport::Sse) => McpTransportDef::Sse,
        // Unspecified → Stdio fallback (matches the pre-proto era where
        // the only transport existed). Avoids panicking on a malformed
        // payload; a downgrade-to-stdio MCP will simply fail to spawn
        // when the runtime tries to connect to it.
        Ok(ProtoMcpTransport::Unspecified) | Err(_) => McpTransportDef::Stdio,
    }
}

/// Watchdog timeout for `eventloop.poll()`.
///
/// If poll() doesn't produce any event within this duration, the TCP
/// connection is likely half-dead (e.g. after OS sleep/wake where the
/// kernel hasn't detected the broken connection). We break to the
/// soft-restart path to create a fresh `AsyncClient` + `EventLoop`.
///
/// 20 s = 4 × keepalive interval (5 s). Normal connections produce at
/// least one PINGRESP within every keepalive interval, so 20 s without
/// any event strongly indicates a stuck socket. Previously 90 s but
/// the long delay caused poor recovery after OS sleep/wake.
const POLL_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(20);

/// Error type for Runtime MQTT client operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeMqttClientError {
    #[error("MQTT connection error: {0}")]
    Connection(String),
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("MQTT subscribe error: {0}")]
    Subscribe(String),
}

/// QoS level wrapper (mirrors the Gateway's MqttQoS).
#[derive(Debug, Clone, Copy)]
pub enum MqttQoS {
    AtMostOnce,
    AtLeastOnce,
}

impl From<MqttQoS> for QoS {
    fn from(qos: MqttQoS) -> Self {
        match qos {
            MqttQoS::AtMostOnce => QoS::AtMostOnce,
            MqttQoS::AtLeastOnce => QoS::AtLeastOnce,
        }
    }
}

/// Provider list update pushed from MQTT poll loop to SessionManager.
///
/// Carries both the provider metadata list (for `global_provider_list`)
/// and the decrypted API keys (for `provider_key_vault`).
#[derive(Debug, Clone)]
pub struct ProviderUpdate {
    pub provider_list: Vec<acowork_core::protocol::ProviderListItem>,
    pub provider_list_version: u64,
    pub provider_key_vault: Vec<acowork_core::protocol::ProviderKeyEntry>,
    /// ADR-056: Global default compact model reference forwarded from
    /// `AvailableProviders.default_compact_model`. `None` = no global override.
    pub default_compact_model: Option<acowork_core::protocol::CompactModelRef>,
}

/// Search provider update pushed from MQTT poll loop to SessionManager.
///
/// Carries both the search provider metadata list and the decrypted API keys.
#[derive(Debug, Clone)]
pub struct SearchUpdate {
    pub search_list: Vec<acowork_core::protocol::SearchProviderListItem>,
    pub search_key_vault: Vec<acowork_core::protocol::SearchKeyEntry>,
}

// ── MQTT protobuf -> domain mapping helpers ─────────────────────────────
//
// Extracted from the inline poll-loop closures so they are independently
// unit-testable and the poll loop stays readable. These are the Runtime-side
// counterparts of the Gateway's `build_available_providers` /
// `build_available_searches` in `global_resources_publisher.rs`.

/// Map MQTT `ProviderRef` list to domain `ProviderListItem` list.
///
/// Preserves `protocol_type` from the protobuf `LlmProtocol` field
/// (previously hardcoded to `ProtocolType::OpenAI` - see C1 fix).
fn map_provider_refs_to_list_items(
    refs: &[acowork_core::mqtt_proto::ProviderRef],
) -> Vec<acowork_core::protocol::ProviderListItem> {
    refs.iter()
        .map(|pr| acowork_core::protocol::ProviderListItem {
            id: pr.id.clone(),
            base_url: pr.base_url.clone(),
            protocol_type: acowork_core::protocol::llm_protocol_to_protocol_type(pr.protocol_type),
            models: pr
                .models
                .iter()
                .map(|m| acowork_core::protocol::ProviderModelEntry {
                    id: m.id.clone(),
                    capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                        context_window: m
                            .capabilities
                            .as_ref()
                            .map(|c| c.context_window)
                            .unwrap_or(128_000),
                        max_output_tokens: m
                            .capabilities
                            .as_ref()
                            .map(|c| c.max_output_tokens)
                            .unwrap_or(16_384),
                        max_input_tokens: None,
                        supports_tool_calling: true,
                        supports_reasoning: m
                            .capabilities
                            .as_ref()
                            .and_then(|c| c.supports_reasoning),
                        supports_attachment: None,
                        supports_temperature: None,
                        cost: None,
                        modalities: Some(acowork_core::protocol::ModelModalities {
                            input: m
                                .capabilities
                                .as_ref()
                                .map(|c| c.input_modalities.clone())
                                .unwrap_or_default(),
                            output: m
                                .capabilities
                                .as_ref()
                                .map(|c| c.output_modalities.clone())
                                .unwrap_or_default(),
                        }),
                        name: None,
                        family: None,
                        knowledge_cutoff: None,
                        default_reasoning_effort: m
                            .capabilities
                            .as_ref()
                            .and_then(|c| c.default_reasoning_effort.clone()),
                        thinking_mode: None,
                    },
                    max_output_tokens_limit: m.max_output_tokens_limit,
                })
                .collect(),
            compact_model: if pr.compact_model.is_empty() {
                None
            } else {
                Some(pr.compact_model.clone())
            },
            custom: pr.custom,
        })
        .collect()
}

/// Extract non-empty API keys from MQTT `ProviderRef` list.
fn extract_provider_keys(
    refs: &[acowork_core::mqtt_proto::ProviderRef],
) -> Vec<acowork_core::protocol::ProviderKeyEntry> {
    refs.iter()
        .filter(|pr| !pr.api_key.is_empty())
        .map(|pr| acowork_core::protocol::ProviderKeyEntry {
            provider_id: pr.id.clone(),
            api_key: pr.api_key.clone(),
        })
        .collect()
}

/// Map MQTT `SearchRef` list to domain `SearchProviderListItem` list.
pub fn map_search_refs_to_list_items(
    refs: &[acowork_core::mqtt_proto::SearchRef],
) -> Vec<acowork_core::protocol::SearchProviderListItem> {
    refs.iter()
        .map(|pr| acowork_core::protocol::SearchProviderListItem {
            id: pr.id.clone(),
            name: pr.name.clone(),
            description: pr.description.clone(),
            requires_api_key: pr.requires_api_key,
            base_url: pr.base_url.clone(),
        })
        .collect()
}

/// Extract non-empty API keys from MQTT `SearchRef` list.
pub fn extract_search_keys(
    refs: &[acowork_core::mqtt_proto::SearchRef],
) -> Vec<acowork_core::protocol::SearchKeyEntry> {
    refs.iter()
        .filter(|pr| !pr.api_key.is_empty())
        .map(|pr| acowork_core::protocol::SearchKeyEntry {
            provider_id: pr.id.clone(),
            api_key: pr.api_key.clone(),
        })
        .collect()
}

/// Configuration for `RuntimeMqttClient::connect`.
///
/// ADR-034 Phase 8: replaces 11 individual parameters.
pub struct MqttConnectConfig<'a> {
    pub host: &'a str,
    pub port: u16,
    pub agent_id: &'a str,
    pub agent_name: &'a str,
    pub agent_version: &'a str,
    pub avatar: Option<&'a str>,
    pub builtin_avatar: Option<&'a str>,
    pub config_json: &'a str,
    pub available_cache: SharedAvailableCache,
    pub control_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    /// ADR-042: Sink for user-profile updates. The MQTT event loop sends
    /// the latest `acowork_core::protocol::UserProfile` here whenever
    /// `acowork/global/user_profile` retained is received (initial snapshot
    /// or hot-push). The receiver (held by `agent_init.rs` → `gateway_loop`)
    /// forwards to `SessionManager::update_user_identity` so all active
    /// sessions pick up the new identity_context.
    ///
    /// Optional: when None, the MQTT event loop still updates
    /// `available_cache` but does not notify SessionManager (suitable for
    /// tests and Standalone mode where there is no SessionManager).
    #[cfg_attr(not(test), allow(dead_code))]
    pub identity_update_tx: Option<
        tokio::sync::mpsc::UnboundedSender<acowork_core::protocol::UserProfile>,
    >,
    /// Sink for provider list updates. The MQTT event loop sends
    /// `ProviderUpdate` here whenever `acowork/global/providers` retained
    /// is received. The receiver (held by `agent_init.rs` → `gateway_loop`)
    /// forwards to `SessionManager::update_global_provider_list` so all
    /// sessions pick up the new provider list and API keys.
    ///
    /// Optional: when None, the MQTT event loop still updates
    /// `available_cache` but does not notify SessionManager.
    #[cfg_attr(not(test), allow(dead_code))]
    pub provider_update_tx: Option<
        tokio::sync::mpsc::UnboundedSender<ProviderUpdate>,
    >,
    /// Sink for search update updates. The MQTT event loop sends
    /// `SearchUpdate` here whenever `acowork/global/searches` retained
    /// is received. The receiver (held by `agent_init.rs` → `gateway_loop`)
    /// forwards to `SessionManager::update_search_config` so all sessions
    /// pick up the new search provider list and API keys.
    ///
    /// Optional: when None, the MQTT event loop still updates
    /// `available_cache` but does not notify SessionManager.
    #[cfg_attr(not(test), allow(dead_code))]
    pub search_update_tx: Option<
        tokio::sync::mpsc::UnboundedSender<SearchUpdate>,
    >,
    /// Per-agent workspace directory (`work_dir`). Used by the MQTT poll
    /// task to persist `acowork/global/mcps` into `agent_mcp.json::catalog`
    /// so the Tools-panel `PUT /agents/{id}/mcp-servers` validation can
    /// match catalog names against disk (the legacy gRPC path did this in
    /// `handle_agent_hello`; that path was removed in ADR-040, leaving the
    /// catalog sync as MQTT-only — see startup/subsystems.rs §MCP for the
    /// rationale and the on-disk contract).
    pub work_dir: std::path::PathBuf,
}

/// Event payload for `publish_tool_approval_needed`.
///
/// ADR-034 Phase 8: replaces 8 individual arguments.
pub struct ToolApprovalNeededEvent<'a> {
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub tool_name: &'a str,
    pub action: &'a str,
    pub risk_level: &'a str,
    pub reason: &'a str,
    pub tool_call_id: &'a str,
    pub approval_timeout_secs: u64,
}

/// The Runtime's MQTT client.
///
/// Wraps `rumqttc::AsyncClient` with:
/// - Last Will for automatic offline detection
/// - Agent lifecycle publishing (status/meta/config)
/// - Global resource subscription → `AvailableResourceCache`
/// - Control command subscription → caller-provided channel
pub struct RuntimeMqttClient {
    /// Shared client handle, swappable during soft-restart.
    ///
    /// The poll task holds a clone of this `Arc<Mutex<AsyncClient>>` and
    /// swaps in a fresh `AsyncClient` when it recreates the `EventLoop`.
    /// All publish methods obtain a clone via `self.client().await`,
    /// ensuring they always use the current handle.
    shared_client: Arc<Mutex<AsyncClient>>,
    /// The agent_id this client represents.
    agent_id: String,
    /// Cached inputs needed to re-run `run_bootstrap` on every
    /// (re)connect. See `docs/adr/zh/ADR-039-mqtt-client-lifecycle.md`.
    bootstrap_data: Arc<BootstrapData>,
    /// Keep the event loop polling task alive.
    _eventloop_guard: Arc<EventLoopGuard>,
    /// ADR-039 Phase 2: session state broadcast channel.
    /// External consumers can subscribe to state transitions.
    state_tx: SessionStateTx,
}

// `Clone` lets the same underlying MQTT client be shared between
// `AgentBootContext::mqtt_client` (consumed by Phase C/D's chunk relay
// and gateway loop) and the HTTP server's late-bind slot (consumed by
// `PUT /agents/{id}/config`). All fields are cheap-to-clone handles —
// the AsyncClient is internally reference-counted by rumqttc and the
// event-loop guard holds a JoinHandle, not owned state.
impl Clone for RuntimeMqttClient {
    fn clone(&self) -> Self {
        Self {
            shared_client: Arc::clone(&self.shared_client),
            agent_id: self.agent_id.clone(),
            bootstrap_data: Arc::clone(&self.bootstrap_data),
            _eventloop_guard: Arc::clone(&self._eventloop_guard),
            state_tx: self.state_tx.clone(),
        }
    }
}

struct EventLoopGuard {
    _task: tokio::task::JoinHandle<()>,
}

/// Cached inputs needed to re-run bootstrap on every (re)connect.
///
/// Built once in `RuntimeMqttClient::connect` and shared (via
/// `Arc<BootstrapData>`) between the initial bootstrap call inside
/// `connect` and the event-loop task that re-runs bootstrap on every
/// subsequent `ConnAck`. See ADR-039.
struct BootstrapData {
    agent_id: String,
    agent_name: String,
    agent_version: String,
    avatar: String,
    builtin_avatar: String,
    config_json: String,
    status_topic: String,
    meta_topic: String,
    config_topic: String,
    /// Filter passed to `AsyncClient::subscribe` (ends with `#`).
    control_filter: String,
    /// Prefix used to route incoming `control/#` publishes to
    /// `control_tx` (ends with `/`, **no** `#`).
    control_filter_prefix: String,
}

impl RuntimeMqttClient {
    /// Obtain a clone of the current `AsyncClient`.
    ///
    /// The lock is held only for the duration of the clone (an `Arc`
    /// bump inside rumqttc), so this is safe to call from async code.
    async fn client(&self) -> AsyncClient {
        self.shared_client.lock().await.clone()
    }

    /// Connect to the MQTT broker and perform the Phase 2 startup sequence.
    ///
    /// ADR-034 Phase 8: takes a single `MqttConnectConfig` struct.
    pub async fn connect(
        cfg: MqttConnectConfig<'_>,
    ) -> Result<Self, RuntimeMqttClientError> {
        let client_id = format!("agent:{}", cfg.agent_id);

        // ADR-039: cache every input needed to re-run bootstrap on every
        // (re)connect (status/meta/config publish + persistent subscriptions).
        let bootstrap_data = Arc::new(BootstrapData {
            agent_id: cfg.agent_id.to_string(),
            agent_name: cfg.agent_name.to_string(),
            agent_version: cfg.agent_version.to_string(),
            avatar: cfg.avatar.unwrap_or("").to_string(),
            builtin_avatar: cfg.builtin_avatar.unwrap_or("").to_string(),
            config_json: cfg.config_json.to_string(),
            status_topic: format!("acowork/agents/{}/status", cfg.agent_id),
            meta_topic: format!("acowork/agents/{}/meta", cfg.agent_id),
            config_topic: format!("acowork/agents/{}/config", cfg.agent_id),
            control_filter: format!(
                "acowork/agents/{}/sessions/control/#",
                cfg.agent_id
            ),
            control_filter_prefix: format!(
                "acowork/agents/{}/sessions/control/",
                cfg.agent_id
            ),
        });

        // Configure MQTT options with Last Will.
        let mut options = MqttOptions::new(client_id.clone(), cfg.host, cfg.port);
        // Match the broker's `connection_timeout_ms` (5 s, see
        // `core/acowork-gateway/src/mqtt/broker.rs`). The previous 30 s
        // value caused the broker to disconnect the Runtime after every
        // OS sleep/wake (broker timed out at 5 s while the client still
        // thought itself connected until the next PINGREQ 30 s later).
        options.set_keep_alive(Duration::from_secs(5));
        options.set_clean_session(true);

        // ADR-039: align outgoing packet size with the broker's
        // `max_payload_size` (`GATEWAY_MQTT_MAX_PACKET_SIZE`). Without
        // this, large stream_delta packets (e.g. long thought content)
        // hit rumqttc's default 10 KB outgoing limit and trigger
        // `OutgoingPacketTooLarge`, which the broker translates into a
        // `connection closed by peer`.
        let pkt_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE;
        options.set_max_packet_size(pkt_size, pkt_size);

        // Last Will: if Runtime crashes/disconnects, broker publishes
        // "offline" retained.
        let will = LastWill::new(&bootstrap_data.status_topic, "offline", QoS::AtLeastOnce, true);
        options.set_last_will(will);

        let (client, mut eventloop) = AsyncClient::new(options.clone(), 100);
        let client_for_poll = client.clone();

        // The `AsyncClient` is shared between the struct (for publishes)
        // and the poll task (for soft-restart).  Wrapping it in
        // `Arc<Mutex<AsyncClient>>` allows the poll task to swap in a
        // fresh client after a soft-restart while publish callers
        // always observe the current handle.
        let shared_client: Arc<Mutex<AsyncClient>> = Arc::new(Mutex::new(client));

        // Spawn the event-loop poller. It owns `eventloop` for the
        // lifetime of the client. In addition to per-publish routing
        // it now observes `ConnAck` and re-runs `run_bootstrap` after
        // every (re)connect so that:
        //   - the Last Will cancellation (`status=online`) is re-asserted
        //   - retained meta/config are republished
        //   - the persistent `control/#` subscription is rebuilt
        // With `clean_session = true` the broker does NOT persist
        // subscriptions, so omitting this step makes the agent look
        // online but unresponsive - the exact symptom captured by ADR-039.
        let poll_agent_id = cfg.agent_id.to_string();
        let poll_cache = cfg.available_cache.clone();
        let poll_control_tx = cfg.control_tx.clone();
        let poll_identity_tx = cfg.identity_update_tx.clone();
        let poll_provider_tx = cfg.provider_update_tx.clone();
        let poll_search_tx = cfg.search_update_tx.clone();
        let poll_bootstrap = bootstrap_data.clone();
        let task_shared_client = Arc::clone(&shared_client);
        let task_options = options; // moved into poll task for soft-restart
        let _poll_client = client_for_poll; // needed for the old API
        // ADR-040 follow-up: the gRPC `handle_agent_hello` path used to
        // call `agent_config::save_agent_mcp_config_catalog` on every
        // (re)connect to refresh the per-agent catalog from the
        // Gateway-side source of truth. That path was deleted when the
        // gRPC hello-config transport went away (ADR-033 + ADR-040). The
        // MQTT path replaces the transport, but the **persistence** step
        // was left out — see `startup/subsystems.rs:89` for the residue.
        //
        // We close that gap here: when `acowork/global/mcps` retained is
        // received, after updating the in-memory `available_cache`, we
        // also persist the catalog (sans `auth_token`) into
        // `agent_mcp.json::catalog` so that:
        //
        //   1. `PUT /agents/{id}/mcp-servers` validation in
        //      `usecases::agent_tools_impl::put_mcp_servers` can resolve
        //      catalog names against `merged()` — without this, every
        //      PUT fails with "unknown MCP server names (not in
        //      catalog+local)" (regression introduced by ADR-040).
        //   2. The Tools-panel display in the Desktop can render the
        //      catalog by reading `GET /agents/{id}/mcp-catalog` from
        //      the Runtime (rather than from the Gateway, which is a
        //      different source of truth).
        //
        // `auth_token` is NOT persisted — it stays in-memory only,
        // mirroring the existing `provider_key_vault` pattern (see
        // `agent/agent_core.rs::provider_key_vault`). Wiring it into an
        // in-memory `mcp_key_vault` is a follow-up; today the Runtime
        // only reads the token via the existing catalog path (the agent
        // connect step does not require it because the Desktop's
        // Tools-panel PUT does not connect).
        let poll_work_dir = cfg.work_dir.clone();
        // ADR-039 §5.2.1: use a oneshot channel to synchronise
        // connect() with the first ConnAck instead of the old
        // wait_for_connection() anti-pattern (subscribe to a dummy
        // topic as a readiness probe). The sender is consumed on
        // the first ConnAck only; subsequent reconnects just
        // re-run bootstrap and log.
        let (first_conn_tx, first_conn_rx) =
            oneshot::channel::<Result<(), RuntimeMqttClientError>>();
        let mut first_conn_tx = Some(first_conn_tx);

        // ADR-039 Phase 2: session state broadcast + reconnect policy.
        let (state_tx, _) = SessionStateTx::new(SessionState::Connecting);
        let poll_state_tx = state_tx.clone();
        let reconnect_policy = ReconnectPolicy::default();

        let poll_task = tokio::spawn(async move {
            let mut soft_restart_count: u32 = 0;

            // Outer loop: each iteration is a fresh client + eventloop.
            // On fatal errors or watchdog timeout we break the inner
            // loop and recreate the AsyncClient + EventLoop from
            // scratch - a soft-restart that recovers from half-dead
            // sockets and corrupted state machines (e.g. after OS
            // sleep/wake).
            loop {
                let mut consecutive_failures: u32 = 0;

                // Inner loop: poll the current eventloop.
                loop {
                    tokio::select! {
                        event_result = eventloop.poll() => {
                            match event_result {
                                Ok(Event::Incoming(rumqttc::Incoming::Publish(publish))) => {
                                    let topic = &publish.topic;

                                    if topic.starts_with("acowork/global/") {
                                        let mut cache_write = poll_cache.write().await;
                                        cache_write.update_from_mqtt(topic, &publish.payload);

                                        if topic == "acowork/global/user_profile" {
                                            let profile = cache_write.active_user_profile();
                                            tracing::debug!(
                                                has_profile = profile.is_some(),
                                                "acowork/global/user_profile retained received"
                                            );
                                            drop(cache_write);
                                            if let (Some(tx), Some(profile)) = (poll_identity_tx.as_ref(), profile) {
                                                let _ = tx.send(profile);
                                            }
                                        } else if topic == "acowork/global/mcps" {
                                            // ADR-040 follow-up: persist catalog to disk
                                            // so PUT /agents/{id}/mcp-servers validation
                                            // can resolve names against merged(). See
                                            // the long-form comment near `poll_work_dir`
                                            // for the full rationale.
                                            let servers = cache_write
                                                .mcps
                                                .as_ref()
                                                .map(|m| m.servers.clone())
                                                .unwrap_or_default();
                                            drop(cache_write);
                                            let defs: Vec<
                                                acowork_core::protocol::McpServerConfigDef,
                                            > = servers
                                                .into_iter()
                                                .map(|s| acowork_core::protocol::McpServerConfigDef {
                                                    name: s.name.clone(),
                                                    transport: mcp_transport_to_def(s.transport),
                                                    url: if s.url.is_empty() { None } else { Some(s.url.clone()) },
                                                    command: s.command.clone(),
                                                    args: s.args.clone(),
                                                    env: s.env.clone(),
                                                    headers: s.headers.clone(),
                                                    tool_timeout_secs: if s.tool_timeout_secs == 0 {
                                                        None
                                                    } else {
                                                        Some(s.tool_timeout_secs)
                                                    },
                                                })
                                                .collect();
                                            if let Err(e) =
                                                crate::agent_config::save_agent_mcp_config_catalog(
                                                    &poll_work_dir,
                                                    &defs,
                                                )
                                            {
                                                tracing::warn!(
                                                    agent_id = %poll_agent_id,
                                                    error = %e,
                                                    "Failed to persist acowork/global/mcps catalog to agent_mcp.json (PUT /mcp-servers will reject catalog names until retry)"
                                                );
                                            } else {
                                                tracing::info!(
                                                    agent_id = %poll_agent_id,
                                                    catalog_count = defs.len(),
                                                    "Synced MCP catalog from acowork/global/mcps into agent_mcp.json::catalog"
                                                );
                                            }
                                        } else if topic == "acowork/global/providers" {
                                            // Persist provider list to agent_provider.json
                                            // so the Runtime can load it on restart.
                                            // API keys are NOT persisted - they stay in
                                            // available_cache (in-memory only).
                                            let (provider_list, version, key_vault, default_compact_model) =
                                                match cache_write.providers.as_ref() {
                                                    Some(p) => {
                                                        let list =
                                                            map_provider_refs_to_list_items(
                                                                &p.providers,
                                                            );
                                                        let keys =
                                                            extract_provider_keys(&p.providers);
                                                        // ADR-056: forward the global default
                                                        // compact model reference.
                                                        let dcm = p.default_compact_model.as_ref()
                                                            .map(|r| acowork_core::protocol::CompactModelRef {
                                                                provider_id: r.provider_id.clone(),
                                                                model_id: r.model_id.clone(),
                                                            });
                                                        (list, p.version, keys, dcm)
                                                    }
                                                    None => (vec![], 0, vec![], None),
                                                };
                                            drop(cache_write);

                                            // Persist to agent_provider.json (no API keys)
                                            if let Err(e) = crate::agent_config::save_agent_provider_config_from_available(
                                                &poll_work_dir,
                                                &provider_list,
                                                version,
                                                default_compact_model.as_ref(),
                                            ) {
                                                tracing::warn!(
                                                    agent_id = %poll_agent_id,
                                                    error = %e,
                                                    "Failed to persist acowork/global/providers to agent_provider.json"
                                                );
                                            } else {
                                                tracing::info!(
                                                    agent_id = %poll_agent_id,
                                                    provider_count = provider_list.len(),
                                                    has_default_compact = default_compact_model.is_some(),
                                                    "Synced provider list from acowork/global/providers into agent_provider.json"
                                                );
                                            }

                                            // Forward to SessionManager via channel
                                            if let Some(ref tx) = poll_provider_tx {
                                                let update = ProviderUpdate {
                                                    provider_list,
                                                    provider_list_version: version,
                                                    provider_key_vault: key_vault,
                                                    default_compact_model,
                                                };
                                                if let Err(e) = tx.send(update) {
                                                    tracing::warn!(
                                                        agent_id = %poll_agent_id,
                                                        error = %e,
                                                        "Failed to send provider update to SessionManager"
                                                    );
                                                }
                                            }
                                        } else if topic == "acowork/global/searches" {
                                            // Persist search provider catalog to agent_search.json
                                            let (search_list, key_vault) =
                                                match cache_write.searches.as_ref() {
                                                    Some(s) => {
                                                        let list =
                                                            map_search_refs_to_list_items(
                                                                &s.providers,
                                                            );
                                                        let keys =
                                                            extract_search_keys(&s.providers);
                                                        (list, keys)
                                                    }
                                                    None => (vec![], vec![]),
                                                };
                                            drop(cache_write);

                                            // Persist catalog to agent_search.json
                                            if let Err(e) = crate::agent_config::save_agent_search_config_catalog(
                                                &poll_work_dir,
                                                &search_list,
                                            ) {
                                                tracing::warn!(
                                                    agent_id = %poll_agent_id,
                                                    error = %e,
                                                    "Failed to persist acowork/global/searches catalog to agent_search.json"
                                                );
                                            } else {
                                                tracing::info!(
                                                    agent_id = %poll_agent_id,
                                                    search_count = search_list.len(),
                                                    "Synced search catalog from acowork/global/searches into agent_search.json"
                                                );
                                            }

                                            // Forward to SessionManager via channel
                                            if let Some(ref tx) = poll_search_tx {
                                                let update = SearchUpdate {
                                                    search_list,
                                                    search_key_vault: key_vault,
                                                };
                                                if let Err(e) = tx.send(update) {
                                                    tracing::warn!(
                                                        agent_id = %poll_agent_id,
                                                        error = %e,
                                                        "Failed to send search update to SessionManager"
                                                    );
                                                }
                                            }
                                        }
                                    }

                                    if topic.starts_with(&poll_bootstrap.control_filter_prefix) {
                                        let _ = poll_control_tx.send((topic.clone(), publish.payload.to_vec()));
                                    }
                                }
                                Ok(Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                                    tracing::info!(
                                        agent_id = %poll_agent_id,
                                        "Runtime MQTT broker confirmed (re)connection - re-running bootstrap"
                                    );
                                    let poll_client = task_shared_client.lock().await.clone();
                                    let result =
                                        Self::run_bootstrap(&poll_client, &poll_bootstrap).await;
                                    if let Err(ref e) = result {
                                        let _ = poll_client
                                            .publish(
                                                &poll_bootstrap.status_topic,
                                                QoS::AtLeastOnce,
                                                true,
                                                "degraded",
                                            )
                                            .await;
                                        tracing::error!(
                                            agent_id = %poll_agent_id,
                                            error = %e,
                                            "Runtime MQTT bootstrap after (re)connect failed - agent is degraded"
                                        );
                                        poll_state_tx.set(SessionState::Disconnected {
                                            reason: format!("bootstrap failed: {e}"),
                                        });
                                    } else {
                                        consecutive_failures = 0;
                                        poll_state_tx.set(SessionState::Connected);
                                    }
                                    if let Some(tx) = first_conn_tx.take() {
                                        let _ = tx.send(result);
                                    }
                                }
                                Ok(_) => continue,
                                Err(e) => {
                                    let class = classify_err(&ErrorDescriptor::from(&e));
                                    tracing::warn!(
                                        agent_id = %poll_agent_id,
                                        error = %e,
                                        err_class = class.label(),
                                        consecutive_failures,
                                        "Runtime MQTT event loop error"
                                    );

                                    if class.is_fatal() {
                                        // E2/E3/E4/E6: the EventLoop's internal
                                        // state may be corrupt. Break to the
                                        // soft-restart path instead of terminating
                                        // the poll task. A fresh EventLoop +
                                        // AsyncClient recovers from state-machine
                                        // corruption caused by network disruptions
                                        // (e.g. OS sleep/wake).
                                        poll_state_tx.set(SessionState::Disconnected {
                                            reason: format!("{}: {}", class.label(), e),
                                        });
                                        break;
                                    }

                                    poll_state_tx.set(SessionState::Reconnecting);
                                    consecutive_failures += 1;
                                    if let Some(backoff) =
                                        reconnect_policy.backoff(class, consecutive_failures - 1)
                                    {
                                        tracing::info!(
                                            agent_id = %poll_agent_id,
                                            attempt = backoff.attempt,
                                            sleep_ms = backoff.duration.as_millis(),
                                            "Backing off before reconnect attempt"
                                        );
                                        tokio::time::sleep(backoff.duration).await;
                                    }
                                }
                            }
                        }

                        // Watchdog: if poll() hasn't produced any event in
                        // POLL_WATCHDOG_TIMEOUT, the TCP socket is likely
                        // half-dead (e.g. after OS sleep/wake). Break to
                        // the soft-restart path to create a fresh connection.
                        _ = tokio::time::sleep(POLL_WATCHDOG_TIMEOUT) => {
                            tracing::warn!(
                                agent_id = %poll_agent_id,
                                timeout_s = POLL_WATCHDOG_TIMEOUT.as_secs(),
                                "Runtime MQTT poll() watchdog timeout - forcing soft-restart (possible half-dead socket)"
                            );
                            poll_state_tx.set(SessionState::Reconnecting);
                            break;
                        }
                    }
                }

                // Soft-restart: recreate client + EventLoop
                poll_state_tx.set(SessionState::Connecting);
                let (new_client, new_eventloop) =
                    AsyncClient::new(task_options.clone(), 100);
                *task_shared_client.lock().await = new_client;
                eventloop = new_eventloop;
                soft_restart_count += 1;
                tracing::info!(
                    agent_id = %poll_agent_id,
                    soft_restart_count,
                    "Runtime MQTT client soft-restarted with fresh EventLoop"
                );
            }
        });

        // ADR-039 §5.2.1: wait for the first ConnAck bootstrap to
        // complete via a oneshot channel instead of the old
        // wait_for_connection() anti-pattern. The event loop's
        // ConnAck handler runs run_bootstrap() and signals us here -
        // no double bootstrap, no dummy subscribe probe.
        let bootstrap_result = first_conn_rx.await.map_err(|_| {
            RuntimeMqttClientError::Connection(
                "bootstrap signal channel closed (event loop dropped)".into(),
            )
        })?;
        bootstrap_result?;

        tracing::info!(
            host = %cfg.host,
            port = %cfg.port,
            client_id = %client_id,
            agent_id = %cfg.agent_id,
            "Runtime MQTT client connected and bootstrapped \
             (status/meta/config + global/control)"
        );

        let mqtt_client = Self {
            shared_client,
            agent_id: cfg.agent_id.to_string(),
            bootstrap_data: bootstrap_data.clone(),
            _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }),
            state_tx,
        };

        Ok(mqtt_client)
    }

    /// Publish agent status (`online`, retained), agent meta, agent
    /// config (both retained), and subscribe to the global resource
    /// tree plus the per-agent control tree. Idempotent; safe to
    /// invoke on every (re)connect to restore both retained state
    /// and persistent subscriptions.
    ///
    /// Implements the "Bootstrap five-step contract" of ADR-039:
    /// 1. PUBLISH `status = online` (Retained) - overrides the Last
    ///    Will payload (`offline`) set during `connect()`.
    /// 2. PUBLISH `meta` (Retained) - agent capability descriptor.
    /// 3. PUBLISH `config` (Retained) - agent runtime configuration.
    /// 4. SUBSCRIBE `acowork/global/#` - global resources.
    /// 5. SUBSCRIBE `acowork/agents/{id}/sessions/control/#` - Desktop
    ///    control commands. Without this step, a `clean_session =
    ///    true` broker silently drops the subscription on the next
    ///    (re)connect - the symptom that prompted ADR-039.
    async fn run_bootstrap(
        client: &AsyncClient,
        data: &BootstrapData,
    ) -> Result<(), RuntimeMqttClientError> {
        // Step 1: PUBLISH status = "online" (Retained).
        client
            .publish(&data.status_topic, QoS::AtLeastOnce, true, "online")
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("status: {}", e)))?;

        // Step 2: PUBLISH meta (Retained).
        let meta = AgentMeta {
            agent_id: data.agent_id.clone(),
            name: data.agent_name.clone(),
            version: data.agent_version.clone(),
            avatar: data.avatar.clone(),
            builtin_avatar: data.builtin_avatar.clone(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentMeta(meta)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        client
            .publish(&data.meta_topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("meta: {}", e)))?;

        // Step 3: PUBLISH config (Retained).
        let config = AgentConfig {
            agent_id: data.agent_id.clone(),
            config_json: data.config_json.clone(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentConfig(config)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        client
            .publish(&data.config_topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("config: {}", e)))?;

        // Step 4: SUBSCRIBE acowork/global/#.
        client
            .subscribe("acowork/global/#", QoS::AtLeastOnce)
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("global: {}", e)))?;

        // Step 5: SUBSCRIBE agents/{id}/sessions/control/#.
        client
            .subscribe(&data.control_filter, QoS::AtLeastOnce)
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("control: {}", e)))?;

        Ok(())
    }

    /// ADR-039 Phase 2: returns the current MQTT session state.
    ///
    /// External consumers (health checks, DevMode) can poll this to
    /// see if the agent is connected, reconnecting, or permanently
    /// disconnected.
    pub fn session_state(&self) -> SessionState {
        self.state_tx.current()
    }

    /// ADR-039 Phase 2: subscribe to session state changes.
    pub fn session_state_rx(&self) -> SessionStateRx {
        // Create a receiver from the existing watch sender.
        // SessionStateTx wraps watch::Sender, and we can get a new
        // receiver via subscribe().
        self.state_tx.subscribe()
    }

    /// Publish a `DataEnvelope` payload to a topic.
    pub async fn publish_envelope(
        &self,
        topic: &str,
        envelope: &DataEnvelope,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = prost::Message::encode_to_vec(envelope);
        self.client().await
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Publish agent status (online/offline) as a plain text Retained message.
    pub async fn publish_status(&self, online: bool) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/status", self.agent_id);
        let payload = if online { "online" } else { "offline" };
        self.client().await
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("status: {}", e)))?;
        Ok(())
    }

    /// Publish the agent's HTTP-ready signal as a plain text Retained message.
    ///
    /// Tells the Gateway that the Runtime has finished Phase A through Phase C
    /// (HTTP server bound, session metadata slot populated, subsystems spawned)
    /// and is ready to serve `/agents/{id}/*` requests. The Gateway pins
    /// `running_agents[id].ready` to this value and only flips it back to
    /// `false` on `status="offline"` or a crash — `subsystems.rs:47` is the
    /// historical note that originally motivated this signal under gRPC
    /// (ADR-033 §3 carried it over to MQTT but the publish was dropped).
    ///
    /// Like [`publish_status`], the payload is Retained so a Gateway that
    /// restarts after the Runtime is already up will see the latest ready
    /// state on its first subscribe.
    pub async fn publish_ready(&self, ready: bool) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/ready", self.agent_id);
        let payload = if ready { "true" } else { "false" };
        self.client().await
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("ready: {}", e)))?;
        Ok(())
    }

    /// Gracefully disconnect from the MQTT broker.
    ///
    /// Sends a DISCONNECT packet and tears down the client. The caller
    /// does **not** need to wait for the broker to acknowledge — the
    /// reconnect path (`pub async fn connect`) handles reconnect on the
    /// next `connect()` call.
    ///
    /// Used by the idle-watcher's auto-sleep path: after publishing
    /// `"sleeping"` to the agent status retained topic, the watcher
    /// invokes `disconnect()` and then exits the process.
    ///
    /// NOTE on the Last Will: per MQTT spec a **clean** DISCONNECT must
    /// NOT trigger the Will — rumqttd honours this (its `Packet::Disconnect`
    /// handler deletes the stored last will), so no "offline" is published
    /// and the retained status stays `"sleeping"` until the Runtime wakes
    /// and re-publishes "online". Subscribers must therefore treat
    /// `sleeping → online` (not just `offline → online`) as a wake
    /// transition — see `apps/acowork-desktop/src/lib/workspaceFsEvents.ts`
    /// `isWakeTransition`.
    pub async fn disconnect(&self) -> Result<(), RuntimeMqttClientError> {
        let client = self.client().await;
        client
            .disconnect()
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("disconnect: {}", e)))?;
        Ok(())
    }

    /// Publish a raw payload to a topic (non-protobuf).
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: &[u8],
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client().await
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Subscribe to a topic filter.
    pub async fn subscribe(
        &self,
        filter: &str,
        qos: MqttQoS,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client().await
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("'{}': {}", filter, e)))?;
        Ok(())
    }

    /// Publish a session event (chunk, done, error, etc.) to the messages topic.
    #[allow(dead_code)]
    pub async fn publish_session_event(
        &self,
        session_id: &str,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.agent_id, session_id, event_type
        );
        // Session messages are QoS 0 (fire-and-forget for streaming events)
        self.publish_envelope(&topic, envelope, MqttQoS::AtMostOnce, false)
            .await
    }

    /// Publish a session lifecycle event (created/deleted).
    #[allow(dead_code)]
    pub async fn publish_session_lifecycle(
        &self,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/sessions/{}", self.agent_id, event_type);
        self.publish_envelope(&topic, envelope, MqttQoS::AtLeastOnce, false)
            .await
    }

    /// Get the agent_id this client represents.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Get a clone of the inner AsyncClient.
    pub async fn inner(&self) -> AsyncClient {
        self.client().await
    }
}

impl Drop for RuntimeMqttClient {
    fn drop(&mut self) {
        // Best-effort: publish "offline" before the connection drops.
        // The Last Will ensures this happens even on crash, but a clean
        // disconnect publishes immediately rather than waiting for keep-alive timeout.
        let shared_client = Arc::clone(&self.shared_client);
        let status_topic = format!("acowork/agents/{}/status", self.agent_id);
        tokio::spawn(async move {
            let client = shared_client.lock().await.clone();
            let _ = client
                .publish(status_topic, QoS::AtLeastOnce, true, "offline")
                .await;
        });
    }
}

/// Shared, thread-safe RuntimeMqttClient.
pub type SharedRuntimeMqttClient = Arc<Mutex<RuntimeMqttClient>>;

// ── MQTT Chunk Publisher (ADR-033 Phase 4: dual-channel session events) ──

/// Lightweight, clonable publisher for MQTT session events.
///
/// Created from a `RuntimeMqttClient` and passed to `SessionCore` so that
/// chunk/tool_call/done events can be published via MQTT alongside the
/// existing gRPC channel. All payloads use `DataEnvelope` protobuf encoding
/// per `docs/zh/protocols/mqtt.md` §4.
///
/// NOTE: Wired into the session loop via MQTT chunk relay task
/// in `subsystems.rs`. See P0-2 in `docs/review/zh/28-adr-033-mqtt-refactor-code-review.md`.
#[derive(Clone)]
pub struct MqttChunkPublisher {
    agent_id: String,
    shared_client: Arc<Mutex<AsyncClient>>,
}

impl MqttChunkPublisher {
    /// Create from a RuntimeMqttClient.
    pub fn from_runtime_client(client: &RuntimeMqttClient) -> Self {
        Self {
            agent_id: client.agent_id().to_string(),
            shared_client: Arc::clone(&client.shared_client),
        }
    }

    /// Obtain a clone of the current AsyncClient.
    async fn client(&self) -> AsyncClient {
        self.shared_client.lock().await.clone()
    }

    /// Return the agent_id this publisher is bound to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Clear a retained `messages/{event_type}` event for a session.
    ///
    /// Publishes a zero-byte payload with `retain = true` to the
    /// `acowork/agents/{id}/sessions/{sid}/messages/{event_type}` topic.
    /// Per MQTT spec this deletes the previously stored retained message,
    /// preventing a reconnecting Desktop from receiving stale blocking
    /// events (tool approval / ask question) from a previous turn.
    pub async fn clear_retained_event(&self, session_id: &str, event_type: &str) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.agent_id, session_id, event_type
        );
        if let Err(e) = self
            .client()
            .await
            .publish(topic, QoS::AtLeastOnce, true, &[])
            .await
        {
            tracing::warn!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                event_type = %event_type,
                error = %e,
                "Failed to clear retained MQTT event"
            );
        }
    }

    /// Publish a session lifecycle envelope (created/deleted) to the
    /// `acowork/agents/{id}/sessions/{event_type}` topic with QoS 1.
    pub async fn publish_lifecycle(
        &self,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/sessions/{}", self.agent_id, event_type);
        let bytes = prost::Message::encode_to_vec(envelope);
        self.client().await
            .publish(topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("lifecycle: {}", e)))
    }

    /// ADR-038: Publish a `SessionOpened` ack to
    /// `acowork/agents/{id}/sessions/{sid}/opened` (QoS 1, non-retained).
    pub async fn publish_session_opened(
        &self,
        session_id: &str,
        status: &str,
        model: Option<String>,
        provider: Option<String>,
        last_active_at: Option<String>,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = acowork_core::mqtt_proto::SessionOpened {
            session_id: session_id.to_string(),
            status: status.to_string(),
            model: model.unwrap_or_default(),
            provider: provider.unwrap_or_default(),
            last_active_at: last_active_at.unwrap_or_default(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::SessionOpened(payload)),
        };
        let topic = format!(
            "acowork/agents/{}/sessions/{}/opened",
            self.agent_id, session_id
        );
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.client().await
            .publish(topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("session_opened: {}", e)))
    }

    /// ADR-038: Publish a `SessionNotOpened` error to
    /// `acowork/agents/{id}/sessions/{sid}/not_opened` (QoS 0).
    pub async fn publish_session_not_opened(
        &self,
        session_id: &str,
        attempted_command: &str,
        reason: &str,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = acowork_core::mqtt_proto::SessionNotOpened {
            session_id: session_id.to_string(),
            attempted_command: attempted_command.to_string(),
            reason: reason.to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::SessionNotOpened(payload),
            ),
        };
        let topic = format!(
            "acowork/agents/{}/sessions/{}/not_opened",
            self.agent_id, session_id
        );
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.client().await
            .publish(topic, QoS::AtMostOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("session_not_opened: {}", e)))
    }

    /// Publish per-session persisted metadata to
    /// `acowork/agents/{id}/sessions/{sid}/meta` with **Retained=true,
    /// QoS=1**.
    ///
    /// Invariant: payload is the *latest complete value*, never a diff.
    /// The broker retains the last value per session, so a Desktop that
    /// (re)connects mid-conversation immediately receives the current
    /// title/model/provider/etc. without an HTTP fetch.
    ///
    /// Distinction from `publish_session_state_changed`:
    ///   - `session_meta`    → persisted per-session config (title, model,
    ///     provider, reasoning_effort, temperature, workspace_id,
    ///     message_count, cumulative tokens)
    ///   - `session_state_changed` → runtime state (status, current
    ///     context_usage, embedding_provider)
    ///
    /// Retained semantics: the broker overwrites the previous retained
    /// message, so hot-field flooding (tokens / message_count changing on
    /// every LLM round-trip) cannot leak stale snapshots — the Desktop
    /// always sees the latest one on (re)connect.
    /// Publish a SessionConfig snapshot to the retained `sessions/{sid}/config`
    /// topic (ADR-043).
    ///
    /// Config fields (title, model, provider, workspace_id, reasoning_effort,
    /// temperature) are low-frequency user actions - published immediately
    /// with no throttle.
    pub async fn publish_session_config(
        &self,
        session_id: &str,
        config: &acowork_core::mqtt_proto::SessionConfig,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/config",
            self.agent_id, session_id
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::SessionConfig(
                config.clone(),
            )),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        if let Err(e) = self
            .client()
            .await
            .publish(topic, QoS::AtLeastOnce, /* retain */ true, bytes)
            .await
        {
            tracing::warn!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                error = %e,
                "Failed to publish session_config"
            );
        }
    }

    /// Publish a SessionState snapshot to the retained `sessions/{sid}/state`
    /// topic (ADR-043).
    ///
    /// Runtime telemetry (status, message_count, tokens, ratio,
    /// context_usage) is high-frequency - the state relay coalesces behind
    /// a 3 s cooldown before calling this method.
    pub async fn publish_session_state(
        &self,
        session_id: &str,
        state: &acowork_core::mqtt_proto::SessionState,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/state",
            self.agent_id, session_id
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::SessionState(
                state.clone(),
            )),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        if let Err(e) = self
            .client()
            .await
            .publish(topic, QoS::AtLeastOnce, /* retain */ true, bytes)
            .await
        {
            tracing::warn!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                error = %e,
                "Failed to publish session_state"
            );
        }
    }

    /// Publish a session event envelope to the broker at QoS 0 (default for
    /// `messages/*` streaming events per ADR-035 D1 / `mqtt.md` §8.3).
    async fn publish(&self, session_id: &str, event_type: &str, payload: &[u8]) {
        self.publish_with_qos(session_id, event_type, payload, QoS::AtMostOnce, false).await;
    }

    /// Publish a session event envelope at the given QoS.
    ///
    /// ADR-035 O2/D1: `record_complete` is published at QoS 1 (AtLeastOnce)
    /// because it is the authoritative terminal event — losing it leaves
    /// the message stuck in the streaming state with no fallback.
    /// `tool_approval_needed` and `session_state_changed` are also QoS 1
    /// (AtLeastOnce) because losing them breaks the approval flow and
    /// leaves the frontend with a stale session state.
    /// All other `messages/*` events stay at QoS 0.
    ///
    /// `session_state_changed` is published with `retain = true` so that
    /// a Desktop client reconnecting to the broker immediately receives
    /// the latest session status (Idle / Streaming) without needing an
    /// HTTP round-trip. This is the primary recovery path for missed
    /// state transitions during an MQTT outage.
    async fn publish_with_qos(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.agent_id, session_id, event_type
        );
        if let Err(e) = self
            .client()
            .await
            .publish(topic, qos, retain, payload)
            .await
        {
            tracing::warn!(error = %e, session_id, event_type, "Failed to publish MQTT session event");
        }
    }

    /// Publish a chunk event via MQTT (QoS 0, protobuf DataEnvelope).
    #[allow(dead_code)]
    pub(crate) async fn publish_chunk(
        &self,
        session_id: &str,
        message_id: &str,
        delta: &str,
    ) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let d = delta.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Chunk(ChunkPayload {
                message_id: mid,
                delta: d,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "chunk", &bytes).await;
    }

    /// Publish a done event via MQTT (QoS 0, protobuf DataEnvelope).
    pub(crate) async fn publish_done(&self, session_id: &str, message_id: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Done(DonePayload {
                message_id: mid,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "done", &bytes).await;
    }

    // ADR-035 C1: publish_tool_call / publish_tool_result removed — tool_call
    // and tool_result records are now delivered via the unified
    // `record_complete` event (see publish_record_complete below), which
    // carries role / message_id / content and applies D9.2 truncation for
    // tool_result at the publish layer.

    /// Publish an error event via MQTT (QoS 1, protobuf DataEnvelope).
    pub(crate) async fn publish_error(&self, session_id: &str, message_id: &str, error_msg: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let err = error_msg.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Error(ErrorPayload {
                message_id: mid,
                error: err,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "error", &bytes).await;
    }

    /// Publish a stopped event via MQTT (QoS 1, protobuf DataEnvelope).
    pub(crate) async fn publish_stopped(&self, session_id: &str, message_id: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Stopped(StoppedPayload {
                message_id: mid,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "stopped", &bytes).await;
    }

    /// ADR-045: Publish a tool progress heartbeat (QoS 0).
    pub(crate) async fn publish_tool_progress(
        &self,
        session_id: &str,
        tool_call_id: &str,
        elapsed_ms: u64,
        timeout_ms: u64,
    ) {
        let sid = session_id.to_string();
        let tid = tool_call_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::ToolProgress(
                acowork_core::mqtt_proto::ToolProgressPayload {
                    session_id: sid.clone(),
                    tool_call_id: tid,
                    elapsed_ms,
                    timeout_ms,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "tool_progress", &bytes).await;
    }

    /// Publish a context_usage event via MQTT (QoS 0).
    ///
    /// Carries the full [`acowork_core::protocol::ContextUsageInfo`] serialised
    /// as JSON in `context_usage`. The Desktop Rust subscriber expands it
    /// into the same shape it emits for `SessionStateChanged`, so the frontend
    /// can render the StatusBar from either source without special-casing.
    pub(crate) async fn publish_context_usage(
        &self,
        session_id: &str,
        ctx_info: &acowork_core::protocol::ContextUsageInfo,
    ) {
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        // Backwards-compat: keep the legacy individual token fields populated
        // for any in-flight Desktop subscriber that hasn't switched to
        // `context_usage` yet.
        let input_tokens = ctx_info.input_tokens;
        let output_tokens = ctx_info.output_tokens;
        let total_input_tokens = ctx_info.total_input_tokens.unwrap_or(0);
        let total_output_tokens = ctx_info.total_output_tokens.unwrap_or(0);
        let cu_json = serde_json::to_string(ctx_info).unwrap_or_default();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::ContextUsage(ContextUsagePayload {
                session_id: sid.clone(),
                input_tokens,
                output_tokens,
                total_input_tokens,
                total_output_tokens,
                context_usage: cu_json,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "context_usage", &bytes).await;
    }

    /// Publish a compacting_started/compacting_ended event via MQTT (QoS 1).
    pub(crate) async fn publish_compacting(&self, session_id: &str, started: bool) {
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let event_type = if started { "compacting_started" } else { "compacting_ended" };
        let payload = CompactingPayload {
            session_id: sid.clone(),
        };
        let event = if started {
            session_message::Event::CompactingStarted(payload)
        } else {
            session_message::Event::CompactingEnded(payload)
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(event),
            })),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, event_type, &bytes).await;
    }

    /// Publish an ask_question event via MQTT (QoS 1, retained).
    ///
    /// Retained so that a Desktop reconnecting during a pending question
    /// immediately receives the question content without needing an HTTP
    /// round-trip. The retained message is cleared (zero-byte publish)
    /// when the user answers – see `ClearRetainedEvent`.
    pub(crate) async fn publish_ask_question(&self, session_id: &str, message_id: &str, question_json: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let qj = question_json.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::AskQuestion(AskQuestionPayload {
                message_id: mid,
                question_json: qj,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish_with_qos(&sid, "ask_question", &bytes, QoS::AtLeastOnce, true).await;
    }

    /// Publish a todo_updated event via MQTT (QoS 0).
    pub(crate) async fn publish_todo_updated(&self, session_id: &str, todos_json: &str) {
        let sid = session_id.to_string();
        let tj = todos_json.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::TodoUpdated(TodoUpdatedPayload {
                todos_json: tj,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "todo_updated", &bytes).await;
    }

    /// Publish an iteration_limit_paused event via MQTT (QoS 1).
    pub(crate) async fn publish_iteration_limit_paused(
        &self,
        session_id: &str,
        iteration: u32,
        max_iterations: u32,
        message: String,
    ) {
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::IterationLimitPaused(
                IterationLimitPausedPayload {
                    session_id: sid.clone(),
                    iteration,
                    max_iterations,
                    message,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "iteration_limit_paused", &bytes).await;
    }

    /// Publish a loop_detected_paused event via MQTT (QoS 1).
    pub(crate) async fn publish_loop_detected_paused(
        &self,
        session_id: &str,
        message: &str,
    ) {
        let sid = session_id.to_string();
        let msg = message.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::LoopDetectedPaused(
                LoopDetectedPausedPayload {
                    session_id: sid.clone(),
                    message: msg,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "loop_detected_paused", &bytes).await;
    }

    /// Publish a tool_approval_needed event via MQTT (QoS 1).
    ///
    /// ADR-034 Phase 8: takes a single `ToolApprovalNeededEvent` struct.
    pub(crate) async fn publish_tool_approval_needed(
        &self,
        ev: ToolApprovalNeededEvent<'_>,
    ) {
        let sid = ev.session_id.to_string();
        let rid = ev.request_id.to_string();
        let tn = ev.tool_name.to_string();
        let act = ev.action.to_string();
        let rl = ev.risk_level.to_string();
        let rsn = ev.reason.to_string();
        let tcid = ev.tool_call_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::ToolApprovalNeeded(
                ToolApprovalNeededPayload {
                    session_id: sid.clone(),
                    request_id: rid,
                    tool_name: tn,
                    action: act,
                    risk_level: rl,
                    reason: rsn,
                    tool_call_id: tcid,
                    approval_timeout_secs: ev.approval_timeout_secs,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish_with_qos(&sid, "tool_approval_needed", &bytes, QoS::AtLeastOnce, true).await;
    }

    /// Publish a new_data_available event via MQTT (QoS 0).
    pub(crate) async fn publish_new_data_available(
        &self,
        session_id: &str,
        interval_ms: u32,
        title: Option<&str>,
    ) {
        let sid = session_id.to_string();
        let t = title.map(|s| s.to_string()).unwrap_or_default();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::NewDataAvailable(
                NewDataAvailablePayload {
                    session_id: sid.clone(),
                    interval_ms,
                    title: t,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "new_data_available", &bytes).await;
    }

    /// Publish a `stream_delta` event via MQTT (QoS 1, ADR-035 + reorder fix).
    ///
    /// Carries the new COMPLETE streaming lines since the last push. Each
    /// `StreamLine.content` is a whole line — never a partial line or token.
    ///
    /// `seq` is the per-session monotonic counter assigned at the emit site
    /// (`SessionCore::next_seq`); the chunk_relay is single-threaded so the
    /// value matches the order in which the relay enqueued this frame. The
    /// Desktop uses it to place this frame at the correct position in
    /// `messages[]` even if the broker delivers frames in a different order.
    /// See `mqtt_payload.proto::StreamDeltaPayload.seq` for the receiving
    /// contract.
    pub(crate) async fn publish_stream_delta(
        &self,
        session_id: &str,
        lines: &[StreamLine],
        seq: u64,
    ) {
        if lines.is_empty() {
            return;
        }
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let lines = lines.to_vec();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::StreamDelta(StreamDeltaPayload {
                session_id: sid.clone(),
                lines,
                seq: Some(seq),
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        // QoS 1 — `stream_delta` was QoS 0 in ADR-035 initial cut, but
        // a fire-and-forget channel cannot guarantee order against
        // QoS 1 `record_complete` events arriving in parallel. With QoS
        // 1 on both endpoints the broker preserves relative order
        // end-to-end; combined with the `seq` payload the Desktop is
        // also robust to any rare reorder.
        self.publish_with_qos(&sid, "stream_delta", &bytes, QoS::AtLeastOnce, false).await;
    }

    /// Publish a `record_complete` event via MQTT (QoS 1, ADR-035 C1/O2).
    ///
    /// Carries the COMPLETE finalized record. The frontend freezes the
    /// active stream buffer into `messages[]` on receipt and clears
    /// `activeStream`. Published at QoS 1 (AtLeastOnce) because this is
    /// the authoritative terminal event — losing it leaves the message
    /// stuck in the streaming state (ADR-035 O2).
    ///
    /// `tool_name` / `tool_call_id` / `is_error` are populated for
    /// `tool_call` and `tool_result` roles; otherwise empty / false. They
    /// mirror the JSONL metadata so the frontend can pair tool_call with
    /// tool_result and render correct labels without an extra HTTP fetch.
    ///
    /// ADR-035 D9.2: `tool_result` content is truncated to the first 5
    /// lines before publishing. The full content stays in JSONL for LLM
    /// context. No exception — the frontend never receives full tool_result.
    //
    // The 8 arguments (self + 7 fields) are intentional: the struct
    // form (`RecordCompletePayload`) is the protobuf DTO and we want
    // this publisher wrapper to look the same. A dedicated builder
    // would just shuffle the same fields around without reducing the
    // surface area, so we keep it inline and silence the lint.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn publish_record_complete(
        &self,
        session_id: &str,
        role: &str,
        message_id: &str,
        content: &str,
        tool_name: &str,
        tool_call_id: &str,
        is_error: bool,
        seq: u64,
    ) {
        // ADR-035 D9.2: truncate tool_result to first 5 lines for display.
        // Full content stays in JSONL for LLM context. No exception.
        let final_content = if role == "tool_result" {
            truncate_tool_result_lines(content)
        } else {
            content.to_string()
        };
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let role = role.to_string();
        let mid = message_id.to_string();
        let tool_name = tool_name.to_string();
        let tool_call_id = tool_call_id.to_string();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::RecordComplete(RecordCompletePayload {
                session_id: sid.clone(),
                role,
                message_id: mid,
                content: final_content,
                tool_name,
                tool_call_id,
                is_error,
                seq: Some(seq),
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        // ADR-035 O2: QoS 1 — record_complete is the authoritative
        // terminal event; losing it leaves the message stuck. The
        // per-session `seq` makes the frame position self-healing on
        // the Desktop (see `insertBySeq`).
        self.publish_with_qos(&sid, "record_complete", &bytes, QoS::AtLeastOnce, false).await;
    }
}

/// ADR-035 D9.2: truncate tool_result to first 5 lines for frontend display.
/// Full content stays in JSONL. No exception.
fn truncate_tool_result_lines(result_json: &str) -> String {
    let lines: Vec<&str> = result_json.lines().collect();
    if lines.len() <= 5 {
        return result_json.to_string();
    }
    let mut truncated = lines.into_iter().take(5).collect::<Vec<_>>().join("\n");
    truncated.push_str("\n...(truncated)");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_runtime_mqtt_client_connects_and_publishes() {
        // Start a broker (using the Gateway's broker module). Threaded
        // mode: `start_broker` blocks forever on rumqttd's
        // `Broker::start()` (it joins the server threads), so the
        // Gateway exposes only `start_broker`.
        let port = 18980;
        let broker = acowork_gateway::mqtt::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let cache = crate::mqtt::available_cache::new_shared_cache();
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

        // Throwaway work_dir — the test only verifies bootstrap + publishes,
        // not the MCP catalog poll task. A temp path keeps the poll task's
        // filesystem writes (if any) from polluting the project tree.
        let work_dir = std::env::temp_dir().join("acowork-test-mqtt-client-18980");

        let client = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port,
                agent_id: "com.test.agent",
                agent_name: "Test Agent",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
                provider_update_tx: None,
                search_update_tx: None,
                work_dir,
            },
        )
        .await
        .expect("Runtime MQTT client should connect");

        // Verify status was published as retained by subscribing and receiving it
        use rumqttc::{AsyncClient as SubClient, MqttOptions as SubOpts};
        let mut sub_opts = SubOpts::new("test:subscriber", "127.0.0.1", port);
        sub_opts.set_keep_alive(Duration::from_secs(5));
        let (sub_client, mut sub_eventloop) = SubClient::new(sub_opts, 10);
        sub_client
            .subscribe("acowork/agents/com.test.agent/#", QoS::AtLeastOnce)
            .await
            .unwrap();

        let mut received_topics = Vec::new();
        for _ in 0..100 {
            match sub_eventloop.poll().await {
                Ok(Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    received_topics.push(p.topic);
                    if received_topics.len() >= 3 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        assert!(
            received_topics
                .contains(&"acowork/agents/com.test.agent/status".to_string()),
            "should receive status: {:?}",
            received_topics
        );
        assert!(
            received_topics
                .contains(&"acowork/agents/com.test.agent/meta".to_string()),
            "should receive meta: {:?}",
            received_topics
        );
        assert!(
            received_topics
                .contains(&"acowork/agents/com.test.agent/config".to_string()),
            "should receive config: {:?}",
            received_topics
        );

        drop(sub_client);
        drop(client);
        drop(broker);
    }

    /// ADR-039 Phase 2: verify that `run_bootstrap` is idempotent —
    /// calling it multiple times does not error, and retained messages
    /// are still present (overwritten, not duplicated).
    #[tokio::test]
    async fn test_bootstrap_idempotency() {
        let port = 18981;
        let broker = acowork_gateway::mqtt::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let cache = crate::mqtt::available_cache::new_shared_cache();
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

        // Throwaway work_dir (see test_runtime_mqtt_client_connects_and_publishes
        // for rationale — bootstrap idempotency test never inspects the poll task).
        let work_dir = std::env::temp_dir().join("acowork-test-mqtt-bootstrap-18981");

        let client = RuntimeMqttClient::connect(
            MqttConnectConfig {
                host: "127.0.0.1",
                port,
                agent_id: "com.test.bootstrap",
                agent_name: "Bootstrap Test",
                agent_version: "1.0.0",
                avatar: None,
                builtin_avatar: None,
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
                provider_update_tx: None,
                search_update_tx: None,
                work_dir,
            },
        )
        .await
        .expect("Runtime MQTT client should connect");

        // Get the bootstrap data from the client and call run_bootstrap
        // a second and third time to verify idempotency.
        let data = &client.bootstrap_data;

        // Second bootstrap (first was done by connect()).
        // `client()` is `async fn` returning `AsyncClient` by clone —
        // the borrow into a new local keeps the future owned across `.await`.
        let second_handle = client.client().await;
        RuntimeMqttClient::run_bootstrap(&second_handle, data)
            .await
            .expect("second bootstrap should succeed (idempotent)");

        // Third bootstrap (refetched; the handle was moved into run_bootstrap).
        let third_handle = client.client().await;
        RuntimeMqttClient::run_bootstrap(&third_handle, data)
            .await
            .expect("third bootstrap should succeed (idempotent)");

        // Verify retained messages are still present.
        use rumqttc::{AsyncClient as SubClient, MqttOptions as SubOpts};
        let mut sub_opts = SubOpts::new("test:bootstrap:sub", "127.0.0.1", port);
        sub_opts.set_keep_alive(Duration::from_secs(5));
        let (sub_client, mut sub_loop) = SubClient::new(sub_opts, 10);
        sub_client
            .subscribe("acowork/agents/com.test.bootstrap/#", QoS::AtLeastOnce)
            .await
            .unwrap();

        let mut received = Vec::new();
        for _ in 0..100 {
            match sub_loop.poll().await {
                Ok(Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    received.push(p.topic);
                    if received.len() >= 3 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }

        assert!(
            received.contains(&"acowork/agents/com.test.bootstrap/status".to_string()),
            "should receive status after multiple bootstraps"
        );
        assert!(
            received.contains(&"acowork/agents/com.test.bootstrap/meta".to_string()),
            "should receive meta after multiple bootstraps"
        );
        assert!(
            received.contains(&"acowork/agents/com.test.bootstrap/config".to_string()),
            "should receive config after multiple bootstraps"
        );

        drop(sub_client);
        drop(client);
        drop(broker);
    }
}
