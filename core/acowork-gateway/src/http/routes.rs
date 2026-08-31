//! HTTP route definitions
//!
//! All API routes are defined here. Handlers are split into sub-modules
//! per domain (agents, vault, config, chat, etc.).

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use axum::extract::Request;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use acowork_core::operation::{OperationId, OperationRecord, OperationState};
use acowork_core::StructuredErrorBody;

/// Global body-size cap applied at the root of the Gateway's
/// merged router. See [`api_router`] for why we override axum's 2 MiB
/// default — long story short: every extractor (`Json`, `Bytes`,
/// `String`, `Multipart`, `Form`) shares this cap, so anything
/// larger than 2 MiB was previously rejected by axum itself with an
/// opaque error before our handlers could produce a clean JSON
/// response.
pub(crate) const GLOBAL_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Shared state for HTTP handlers
pub type SharedHttpState = Arc<RwLock<GatewayState>>;

/// Application state available to all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    /// Shared gateway state
    pub gateway_state: SharedHttpState,
    /// HTTP authentication
    pub auth: Arc<HttpAuth>,
    /// Tracing reload handle for dynamic log level changes
    pub log_reload_handle: Option<crate::LogReloadHandle>,
    /// ADR-033: MQTT Gateway client for publishing control commands to Runtime.
    pub mqtt_gateway_client: Option<Arc<crate::mqtt::GatewayMqttClient>>,
    /// ADR-033: MQTT global resources publisher trigger.
    /// HTTP handlers call `.trigger()` after resource changes to republish.
    pub mqtt_publisher_trigger: Option<crate::mqtt::MqttPublisherTrigger>,
    /// ADR-033: Runtime HTTP port registry for reverse proxy to Runtime localhost HTTP.
    pub runtime_http_registry: Option<crate::http::proxy::SharedRuntimeHttpRegistry>,
    /// ADR-033: Agent registry tracking online/offline status from MQTT.
    pub agent_registry: Option<crate::mqtt::agent_registry::SharedAgentRegistry>,
    /// ADR-055: Node control-plane client (issues agent lifecycle
    /// commands to Node Agents and correlates the NodeEvent replies).
    pub node_control: Option<crate::mqtt::node_control::NodeControlClient>,
    /// ADR-055: Node registry (LWT-driven online state + retained info
    /// snapshots). Used by handlers that route to node-local HTTP
    /// services (e.g. `/api/fs/browse?target={node_id}`, L7-1).
    pub node_registry: Option<crate::mqtt::SharedNodeRegistry>,
    /// ADR-059 §7.2: subsystem readiness registry. Mutation handlers
    /// that depend on a Node's control plane (e.g. `/api/agents/install`)
    /// check `is_ready("node.{node_id}")` before dispatching.
    pub bootstrap_registry: Option<crate::bootstrap::SharedSubsystemReadinessRegistry>,
    /// ADR-059 §6.2: operation store — tracks every accepted mutation
    /// (install / provider write / identity write) from `Accepted` to
    /// a terminal state, keyed by `operation_id`.
    pub operation_store: Option<crate::operation_store::SharedOperationStore>,
}

impl AppState {
    /// Create a new AppState with default models cache
    pub fn new(
        gateway_state: SharedHttpState,
        auth: Arc<HttpAuth>,
    ) -> Self {
        Self {
            gateway_state,
            auth,
            log_reload_handle: None,
            mqtt_gateway_client: None,
            mqtt_publisher_trigger: None,
            runtime_http_registry: None,
            agent_registry: None,
            node_control: None,
            node_registry: None,
            bootstrap_registry: None,
            operation_store: None,
        }
    }
}

/// Log request Origin and response CORS header for every HTTP request.
/// This middleware runs AFTER the CORS layer, so `Access-Control-Allow-Origin`
/// reflects what the browser actually sees.
async fn log_request_origin(req: Request, next: Next) -> axum::response::Response {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let method = req.method().clone();
    let uri = req.uri().clone();
    let msg = format!("HTTP request: origin={} method={} uri={}", origin, method, uri);
    tracing::info!("{}", msg);
    let response = next.run(req).await;
    let acao = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let msg = format!("HTTP response: status={} access-control-allow-origin={}", response.status(), acao);
    tracing::info!("{}", msg);
    response
}

/// Build the HTTP router with all routes
pub fn build_router(state: AppState) -> Router {
    // CORS — permissive for all deployments.
    //
    // `CorsLayer::permissive()` alone — deliberately WITHOUT
    // `allow_credentials(true)`: permissive() answers `*` for origin /
    // method / header, and the CORS spec forbids combining `*` with
    // `Access-Control-Allow-Credentials: true`. tower-http asserts at
    // layer-build time and panics on that combination (this exact bug
    // crashed the HTTP server once — see ensure_usable_cors_rules).
    //
    // Credentials are not needed: the Gateway never sends `Set-Cookie`
    // and the desktop frontend fetches with the default
    // `credentials: 'same-origin'`, so no cross-origin credentials are
    // ever transmitted. The Gateway binds to loopback by default
    // (`[http].host = 127.0.0.1`), so any origin that can reach it is
    // already on the user's machine — 0 risk locally. For remote
    // deployments (`[http].host = 0.0.0.0` or a LAN address), CSRF is
    // prevented by the bearer-token middleware, not by CORS.
    //
    // Dev mode (Vite on :5173, Tauri custom protocol on macOS/Windows) is
    // always cross-origin against the Gateway (:19876); a hardcoded
    // allowlist breaks the moment the WebView resolves `localhost` to a
    // different IP literal than the one hardcoded.
    let cors = tower_http::cors::CorsLayer::permissive();

    Router::new()
        .route("/health", get(health_check))
        .route("/api/status", get(system_status))
        // ADR-059 Phase 1.3: readiness projection (liveness lives at
        // `/health`). `GET /api/bootstrap` exposes the same aggregated
        // snapshot as the retained MQTT topic `acowork/global/bootstrap`.
        .merge(crate::http::bootstrap_api::bootstrap_routes())
        // ADR-059 Phase 5.4: vault lock/unlock — the relock path must
        // demote the vault subsystem (phase drops below READY) and the
        // unlock path must restore it, reusing the cold-start unlock
        // sequence (see vault_api.rs).
        .merge(crate::http::vault_api::vault_routes())
        .merge(crate::http::agents::agent_routes())
        .merge(crate::http::chat::chat_routes())
        .merge(crate::http::provider_api::provider_routes())
        .merge(crate::http::config_api::config_routes())
        .merge(crate::http::cron_api::cron_routes())
        .merge(crate::http::models_api::models_routes())
        .merge(crate::http::memory_api::memory_routes())
        .merge(crate::http::skills_api::skills_routes())
        .merge(crate::http::workspaces::workspace_routes())
        .merge(crate::http::publish_api::publish_routes())
        .merge(crate::http::mcp_catalog_api::mcp_catalog_routes())
        .merge(crate::http::nodes_api::nodes_routes())
        .merge(crate::http::users_api::users_routes())
        .merge(crate::http::embedding_api::embedding_routes())
        .merge(crate::embedding_providers::embedding_providers_routes())
        .merge(crate::http::fs_browse::fs_routes())
        .merge(crate::http::global_resources_api::global_resources_routes())
        .merge(crate::http::proxy::proxy_routes())
        .merge(crate::http::debug_mqtt::debug_mqtt_routes())
        .merge(crate::http::settings_api::settings_routes())
        .with_state(state)
        // Global body-size cap. See `GLOBAL_BODY_LIMIT` for why we
        // override axum's 2 MiB default at the root of the gateway
        // router. Per-route service-layer limits (e.g. Runtime's
        // `MAX_UPLOAD_BYTES` for attachments, gateway's
        // `MAX_FILE_SIZE` for static file reads) remain the source of
        // truth for user-facing caps — this layer only ensures those
        // limits produce a clean error body instead of an opaque
        // extractor parse failure.
        .layer(DefaultBodyLimit::max(GLOBAL_BODY_LIMIT))
        .layer(middleware::from_fn(log_request_origin))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(cors)
}

// ── Health check (liveness-only) ─────────────────────────────────────

/// `GET /health` — liveness probe (no auth required).
///
/// ADR-059 Phase 1.3: this endpoint is STRICTLY liveness — it answers
/// as soon as the HTTP server is up and says nothing about subsystem
/// readiness (no checks, no ready flags). Consumers that need to know
/// whether the Gateway is ready to accept dependent work must use
/// `GET /api/bootstrap` and wait for `phase = READY`.
#[derive(Debug, Serialize)]
pub struct LivenessResponse {
    pub status: String,
    pub version: String,
    /// HTTP port this server listens on — the process identity half of
    /// the liveness contract.
    pub port: u16,
}

/// `GET /health` — liveness check (no auth required)
pub async fn health_check(State(state): State<AppState>) -> Json<LivenessResponse> {
    let gw = state.gateway_state.read().await;
    let port = gw
        .config
        .as_ref()
        .map(|c| c.http.port)
        .unwrap_or_else(crate::config::default_http_port);
    Json(LivenessResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        port,
    })
}

// ── System status ─────────────────────────────────────────────────────

/// System status response
#[derive(Serialize)]
pub struct SystemStatusResponse {
    pub version: String,
    pub agents_installed: usize,
    pub agents_running: usize,
    pub uptime_secs: u64,
    /// ADR-055 D3 §6.3: MQTT broker port for Desktop discovery.
    /// Lets the Desktop derive the broker port dynamically instead of
    /// assuming the default 19875 (L3-6 residual gap — ADR-058 W4 fixed
    /// the host derivation; this closes the port half).
    pub mqtt_port: u16,
    /// ADR-055 Phase 5a: MQTT username for the Desktop's broker
    /// connection, present only when `mqtt.auth_enabled` is on.
    /// Informational at this tier — CONNECT identity is keyed by
    /// client_id (`user:{name}:desktop:{id}`), not username.
    pub mqtt_username: Option<String>,
    /// ADR-055 Phase 5a: MQTT password for the Desktop's broker
    /// connection — the HttpAuth bearer token, the same value the
    /// broker CONNECT check accepts for `user:*:desktop:*` clients.
    /// Present only when `mqtt.auth_enabled` is on.
    pub mqtt_password: Option<String>,
}

/// `GET /api/status` — system status
pub async fn system_status(State(state): State<AppState>) -> Json<SystemStatusResponse> {
    let gw = state.gateway_state.read().await;

    // ADR-033: Prefer AgentRegistry (MQTT LWT-based) for agent online count.
    // It reflects the broker's protocol-level view via Will Message, which is
    // more accurate than the Gateway's process-level running_agents count.
    let agents_running = if let Some(ref reg) = state.agent_registry {
        reg.read().await.online_count()
    } else {
        gw.running_agents.len()
    };

    // ADR-055 Phase 5a: MQTT credentials for the Desktop
    // (`connect_mqtt` consumes these when present). Exposed only when
    // MQTT auth is on; the password is the HttpAuth bearer token — the
    // same value the broker CONNECT handler accepts for `user:*:desktop:*`.
    let mqtt_auth_enabled = gw
        .mqtt_broker_auth
        .as_ref()
        .map(|a| a.auth_enabled)
        .unwrap_or(false);
    let (mqtt_username, mqtt_password) = if mqtt_auth_enabled {
        (
            Some("desktop".to_string()),
            state.auth.token().map(str::to_string),
        )
    } else {
        (None, None)
    };

    Json(SystemStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        agents_installed: gw.installed_agents.len(),
        agents_running,
        uptime_secs: 0, // TODO: track actual uptime
        mqtt_port: gw
            .config
            .as_ref()
            .map(|c| c.mqtt.port)
            .unwrap_or_else(crate::config::default_mqtt_port),
        mqtt_username,
        mqtt_password,
    })
}

// ── Error response helpers ────────────────────────────────────────────

/// ADR-059 §7.3/§7.4 — unified mutation ack returned by every
/// operation-bearing write API (`POST /api/providers`, `POST
/// /api/users`, `POST /api/agents/install`, `POST /api/mcp-catalog`).
///
/// The ack only carries protocol-level fields (OCP §5.4.4): the
/// operation id, its current state, the post-commit resource version,
/// and the structured terminal error when the operation failed. No
/// capability names / subsystem generations / process ids.
#[derive(Debug, Clone, Serialize)]
pub struct OperationAck {
    pub operation_id: OperationId,
    pub state: OperationState,
    pub resource_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_error: Option<StructuredErrorBody>,
}

impl OperationAck {
    /// Build an ack from a tracked record (state + resource version
    /// + terminal error mirrored from the record).
    pub fn from_record(record: &OperationRecord) -> Self {
        Self {
            operation_id: record.operation_id.clone(),
            state: record.state,
            resource_version: record.resource_version,
            terminal_error: record.terminal_error.clone(),
        }
    }
}

/// ADR-059 §7.3 — validate the mutation's `expected_version` against
/// the Gateway's CURRENT bootstrap snapshot version.
///
/// The client reads `GET /api/bootstrap` before writing and echoes the
/// snapshot's `version` back; a mismatch means the client's view is
/// stale (typically a Gateway restart — the new instance's version
/// counter restarts at 1, so any old-instance `expected_version` is
/// rejected instead of being accepted against a fresh process).
///
/// Returns `Ok(())` on match, or a structured `409
/// resource_version_conflict` carrying both version numbers.
pub async fn check_expected_version(
    state: &AppState,
    expected_version: Option<u64>,
) -> Result<(), ApiError> {
    let Some(expected) = expected_version else {
        // No precondition — the mutation proceeds optimistically.
        return Ok(());
    };
    let current = state
        .gateway_state
        .read()
        .await
        .bootstrap.orchestrator
        .as_ref()
        .map(|o| o.snapshot().version)
        .unwrap_or(0);
    if current != expected {
        return Err(ApiError::conflict_structured(
            StructuredErrorBody::resource_version_conflict(current, expected),
        ));
    }
    Ok(())
}

/// Standard API error response
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
    /// ADR-059 §6.3: structured protocol error body for mutation
    /// APIs. Absent for plain HTTP-layer errors (existing clients are
    /// unaffected); present for `dependency_not_ready` /
    /// `resource_version_conflict` etc. so clients can retry without
    /// parsing human-readable text.
    ///
    /// Boxed so the `Err` variant of every handler signature stays small:
    /// `StructuredErrorBody` carries ~10 `Option<...>` fields and is
    /// ~232 bytes inline. Boxing shrinks `ApiError` from ~248 B to ~40 B,
    /// which keeps `cargo clippy -D warnings` (`result_large_err`) quiet
    /// on every `Result<Json<X>, ApiError>` handler signature. Wire
    /// format is identical — `Box<T>: Serialize` derefs to `T` for
    /// `serde_json`, and `#[serde(skip_serializing_if = "Option::is_none")]`
    /// still hides absent values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<Box<StructuredErrorBody>>,
}

/// Handler errors return [`ApiError`] directly (no `(StatusCode,
/// Json<ApiError>)` tuple wrapper). [`ApiError::code`] carries the
/// intended HTTP status as a u16, which this impl decodes back into
/// an `http::StatusCode` for axum's response pipeline.
///
/// Keeping the `Err` variant small (≈ 80 bytes: String + u16 +
/// Option<StructuredErrorBody>) instead of a ~256-byte tuple
/// `(StatusCode, Json<ApiError>)` satisfies `cargo clippy -D warnings`
/// (`result_large_err`) without forcing every handler to thread a
/// `Box<...>` through `?`.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // `code` is the canonical u16 representation of the intended
        // status. Defensively fall back to 500 if a future helper ever
        // forgets to populate it, so the client still gets a valid
        // HTTP response rather than axum panicking on a status-code
        // conversion.
        let status = StatusCode::from_u16(self.code)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

impl ApiError {
    pub fn not_found(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 404,
            structured: None,
        }
    }

    pub fn bad_request(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 400,
            structured: None,
        }
    }

    /// ADR-056: unprocessable entity — the request is well-formed but the
    /// referenced entity does not exist (e.g. `default_compact_model`
    /// pointing at an unknown provider_id / model_id). Mirrors the 422
    /// contract in ADR-056 §4.1.
    pub fn unprocessable_entity(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 422,
            structured: None,
        }
    }

    pub fn internal(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 500,
            structured: None,
        }
    }

    pub fn unauthorized(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 401,
            structured: None,
        }
    }

    pub fn service_unavailable(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 503,
            structured: None,
        }
    }

    /// ADR-059 §2.3: conflict — the request depends on a resource that
    /// is not ready yet (e.g. installing onto a Node whose control
    /// plane has not announced `NodeReady`). The client should retry
    /// once `GET /api/bootstrap` reports `phase = READY`.
    pub fn conflict(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 409,
            structured: None,
        }
    }

    /// ADR-059 §6.3: structured conflict — HTTP 409 whose body carries
    /// a protocol-level [`StructuredErrorBody`] (e.g.
    /// `resource_version_conflict` with both version numbers).
    pub fn conflict_structured(body: StructuredErrorBody) -> Self {
        Self {
            error: format!("{:?}", body.code),
            code: 409,
            structured: Some(Box::new(body)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-http-routes-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
        )
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = test_app_state();
        let resp = health_check(State(state)).await;
        // Liveness-only contract (ADR-059 Phase 1.3): always "ok", no
        // checks map, no readiness claims.
        assert_eq!(resp.status, "ok");
        assert!(!resp.version.is_empty());
        assert!(resp.port > 0);
    }

    #[tokio::test]
    async fn test_system_status() {
        let state = test_app_state();
        let resp = system_status(State(state)).await;
        assert_eq!(resp.agents_installed, 0);
        assert_eq!(resp.agents_running, 0);
        // MQTT auth off by default → no credentials are exposed.
        assert_eq!(resp.mqtt_username, None);
        assert_eq!(resp.mqtt_password, None);
    }

    #[tokio::test]
    async fn test_system_status_exposes_mqtt_credentials_when_auth_enabled() {
        use std::sync::Mutex;

        // MQTT auth enabled + HttpAuth enabled (the broker CONNECT
        // check for `user:*:desktop:*` compares against the HttpAuth
        // bearer token, so the Desktop gets it as its MQTT password).
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-http-status-auth-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        {
            let mut gw = gw_state.write().await;
            gw.mqtt_broker_auth = Some(crate::mqtt::broker::BrokerAuth {
                auth_enabled: true,
                enrollment_tokens: Arc::new(Mutex::new(Default::default())),
                node_tokens: Arc::new(Mutex::new(Default::default())),
                publisher_token: "publisher-token".to_string(),
                http_token: None,
            });
        }
        let state = AppState::new(gw_state, Arc::new(HttpAuth::new(true)));
        let resp = system_status(State(state)).await;
        assert_eq!(resp.mqtt_username.as_deref(), Some("desktop"));
        let password = resp.mqtt_password.as_deref().expect("password exposed when auth on");
        assert_eq!(password.len(), 64, "256-bit hex token");

        // MQTT auth enabled but HttpAuth off → no password to hand out.
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        {
            let mut gw = gw_state.write().await;
            gw.mqtt_broker_auth = Some(crate::mqtt::broker::BrokerAuth {
                auth_enabled: true,
                enrollment_tokens: Arc::new(Mutex::new(Default::default())),
                node_tokens: Arc::new(Mutex::new(Default::default())),
                publisher_token: "publisher-token".to_string(),
                http_token: None,
            });
        }
        let state = AppState::new(gw_state, Arc::new(HttpAuth::new(false)));
        let resp = system_status(State(state)).await;
        assert_eq!(resp.mqtt_username.as_deref(), Some("desktop"));
        assert_eq!(resp.mqtt_password, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_router() {
        let state = test_app_state();
        let _router = build_router(state);
    }

    /// ADR-059 §6: the mutation ack carries exactly the protocol
    /// fields (`operation_id` / `state` / `resource_version` /
    /// `terminal_error`) — never internal capability names, subsystem
    /// generations or process ids (OCP boundary, §5.4.4).
    #[test]
    fn operation_ack_carries_only_protocol_fields() {
        let mut record = OperationRecord::new(3);
        record.state = OperationState::Committed;
        record.resource_version = Some(8);

        let json = serde_json::to_value(OperationAck::from_record(&record)).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 3, "terminal_error omitted when None: {obj:?}");
        assert_eq!(obj["state"], "committed");
        assert_eq!(obj["resource_version"], 8);
        assert_eq!(obj["operation_id"], record.operation_id.as_str());

        // A failed ack carries the structured terminal error instead.
        let mut failed = OperationRecord::new(3);
        failed.state = OperationState::Failed;
        failed.terminal_error = Some(StructuredErrorBody::dependency_not_ready(
            Some("BOOTING".to_string()),
            None,
            500,
        ));
        let json = serde_json::to_value(OperationAck::from_record(&failed)).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["state"], "failed");
        assert_eq!(obj["terminal_error"]["code"], "dependency_not_ready");
        assert_eq!(obj["terminal_error"]["current_phase"], "BOOTING");
        // OCP: no internal fields leak into the ack's terminal_error.
        assert!(obj["terminal_error"].get("operation_id").is_none());
        assert!(obj["terminal_error"].get("current_version").is_none());
    }

    #[test]
    fn test_build_router_with_operation_store() {
        let mut state = test_app_state();
        state.operation_store = Some(crate::operation_store::OperationStore::new_shared());
        let _router = build_router(state);
    }
}

