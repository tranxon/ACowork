//! HTTP route definitions
//!
//! All API routes are defined here. Handlers are split into sub-modules
//! per domain (agents, vault, config, chat, etc.).

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    routing::get,
    Router,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::gateway::state::GatewayState;
use crate::grpc::SharedGrpcSessionMgr;
use crate::http::approval::ApprovalPendingRequests;
use crate::http::auth::HttpAuth;
use crate::ipc::session::SessionManager;

/// Shared state for HTTP handlers
pub type SharedHttpState = Arc<RwLock<GatewayState>>;

/// Shared session manager type (same as IPC server)
pub type SharedSessionMgr = Arc<tokio::sync::Mutex<SessionManager>>;

/// Bridge event type — known event types for Agent → HTTP client forwarding
///
/// Provides compile-time safety instead of raw string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeEventType {
    /// Streaming text chunk
    Chunk,
    /// LLM tool invocation
    ToolCall,
    /// Tool execution result
    ToolResult,
    /// Tool approval needed (user interaction required)
    ToolApprovalNeeded,
    /// Final response (complete)
    Done,
    /// Error response
    Error,
    /// Agent response interrupted by user stop signal
    Interrupted,
    /// Memory store updated (node added/removed/consolidated)
    MemoryUpdated,
    /// Skill execution event
    SkillExecuted,
    /// Iteration limit reached — agent paused, awaiting user decision
    IterationLimitPaused,
    /// Context usage report (from Runtime, forwarded to Desktop App)
    ContextUsage,
    /// LLM reasoning phase started — frontend shows pulsing "..." indicator
    ReasoningStarted,
    /// Session lifecycle status changed (ADR-014)
    SessionStateChanged,
    /// LLM asks user a question with options (ask_user_question tool)
    AskQuestion,
    /// Unknown/unrecognized action — payload is forwarded as-is so the
    /// frontend can decide what to do. This avoids silently treating new
    /// Runtime event types as "done" (which would break streaming state).
    Unknown,
}

impl BridgeEventType {
    /// Map an IPC action string to a BridgeEventType.
    /// Returns None for unrecognized actions.
    pub fn from_action(action: &str) -> Option<Self> {
        match action {
            "agent_response" => Some(Self::Done),
            "agent_chunk" => Some(Self::Chunk),
            "agent_tool_call" => Some(Self::ToolCall),
            "agent_tool_result" => Some(Self::ToolResult),
            "agent_error" => Some(Self::Error),
            "agent_interrupted" => Some(Self::Interrupted),
            "tool_approval_needed" => Some(Self::ToolApprovalNeeded),
            "memory_updated" => Some(Self::MemoryUpdated),
            "skill_executed" => Some(Self::SkillExecuted),
            "iteration_limit_paused" => Some(Self::IterationLimitPaused),
            "context_usage" => Some(Self::ContextUsage),
            "agent_reasoning_started" => Some(Self::ReasoningStarted),
            "session_state_changed" => Some(Self::SessionStateChanged),
            "ask_question" => Some(Self::AskQuestion),
            _ => None,
        }
    }

    /// Fallback event type for unrecognized actions.
    ///
    /// Returns `Unknown` instead of silently degrading to `Done`, which would
    /// cause the frontend to incorrectly end a streaming session.
    pub fn default_for_unknown() -> Self {
        Self::Unknown
    }

    /// Get the serialized string value (matches frontend WebSocket protocol)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chunk => "chunk",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ToolApprovalNeeded => "tool_approval_needed",
            Self::Done => "done",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
            Self::MemoryUpdated => "memory_updated",
            Self::SkillExecuted => "skill_executed",
            Self::IterationLimitPaused => "iteration_limit_paused",
            Self::ContextUsage => "context_usage",
            Self::ReasoningStarted => "reasoning_started",
            Self::SessionStateChanged => "session_state_changed",
            Self::AskQuestion => "ask_question",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for BridgeEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Bridge event for forwarding Agent responses to HTTP clients
#[derive(Debug, Clone, serde::Serialize)]
pub struct BridgeEvent {
    /// Agent ID that produced the response
    pub agent_id: String,
    /// Message ID for correlation
    pub message_id: String,
    /// Event type
    pub event_type: BridgeEventType,
    /// Event payload (JSON)
    pub payload: serde_json::Value,
}

/// Pending session request map (S1.14)
///
/// When the Gateway HTTP API forwards a session query to the Runtime
/// via IPC (IntentReceived push), it stores a oneshot sender here
/// keyed by request_id. When the Runtime sends the result back via
/// IntentSend with action "session_response", the IPC dispatch handler
/// finds the pending sender and fulfills it, which unblocks the
/// HTTP handler awaiting the oneshot receiver.
pub type SessionPendingRequests =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>>>;

/// Application state available to all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    /// Shared gateway state
    pub gateway_state: SharedHttpState,
    /// HTTP authentication
    pub auth: Arc<HttpAuth>,
    /// Shared session manager for pushing messages to agents
    /// Set by Gateway::run() when the IPC server is initialized
    pub session_mgr: Option<SharedSessionMgr>,
    /// Bridge channel for forwarding Agent responses to HTTP clients
    /// The IPC server publishes events; HTTP WebSocket subscribes
    pub bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    /// Cache for models.dev API responses
    pub(crate) models_cache: crate::http::models_api::ModelsCache,
    /// Pending session requests for IPC response correlation (S1.14)
    pub session_pending: SessionPendingRequests,
    /// Pending approval requests for tool approval flow (C1+C2)
    pub approval_pending: ApprovalPendingRequests,
    /// Tracing reload handle for dynamic log level changes
    pub log_reload_handle: Option<crate::LogReloadHandle>,
    /// gRPC session manager for Gateway→Runtime request-response
    pub grpc_session_mgr: Option<SharedGrpcSessionMgr>,
    /// Whether CORS is enabled (allows any origin for remote Desktop connections)
    pub cors_enabled: bool,
}

impl AppState {
    /// Create a new AppState with default models cache
    pub fn new(
        gateway_state: SharedHttpState,
        auth: Arc<HttpAuth>,
        session_mgr: Option<SharedSessionMgr>,
        bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
        session_pending: Option<SessionPendingRequests>,
    ) -> Self {
        Self {
            gateway_state,
            auth,
            session_mgr,
            bridge_tx,
            models_cache: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            session_pending: session_pending.unwrap_or_else(|| {
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
            }),
            approval_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            log_reload_handle: None,
            grpc_session_mgr: None,
            cors_enabled: false,
        }
    }

    /// Create a new AppState sharing an existing models cache (e.g. from GatewayState)
    pub(crate) fn with_models_cache(
        gateway_state: SharedHttpState,
        auth: Arc<HttpAuth>,
        session_mgr: Option<SharedSessionMgr>,
        bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
        models_cache: crate::http::models_api::ModelsCache,
        session_pending: Option<SessionPendingRequests>,
        log_reload_handle: Option<crate::LogReloadHandle>,
    ) -> Self {
        Self {
            gateway_state,
            auth,
            session_mgr,
            bridge_tx,
            models_cache,
            session_pending: session_pending.unwrap_or_else(|| {
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
            }),
            approval_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            log_reload_handle,
            grpc_session_mgr: None,
            cors_enabled: false,
        }
    }
}

/// Build the HTTP router with all routes
pub fn build_router(state: AppState) -> Router {
    // When CORS is enabled (remote Desktop ↔ Gateway scenarios),
    // allow any origin. Otherwise, restrict to localhost.
    let cors = if state.cors_enabled {
        tower_http::cors::CorsLayer::permissive()
            .allow_credentials(true)
    } else {
        tower_http::cors::CorsLayer::new()
            .allow_origin([
                "http://localhost:3000".parse().unwrap(),
                "http://localhost:5173".parse().unwrap(),
                "http://127.0.0.1:3000".parse().unwrap(),
            ])
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
            ])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
            ])
    };

    Router::new()
        .route("/health", get(health_check))
        .route("/api/status", get(system_status))
        .merge(crate::http::agents::agent_routes())
        .merge(crate::http::chat::chat_routes())
        .merge(crate::http::vault_api::vault_routes())
        .merge(crate::http::config_api::config_routes())
        .merge(crate::http::cron_api::cron_routes())
        .merge(crate::http::models_api::models_routes())
        .merge(crate::http::memory_api::memory_routes())
        .merge(crate::http::skills_api::skills_routes())
        .merge(crate::http::workspaces::workspace_routes())
        .merge(crate::http::publish_api::publish_routes())
        .merge(crate::http::approval::approval_routes())
        .merge(crate::http::question::question_routes())
        .merge(crate::http::documents::documents_routes())
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(cors)
}

// ── Health check ──────────────────────────────────────────────────────

/// Overall health status
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// All checks passed
    Ok,
    /// Some checks failed (system still functional)
    Degraded,
}

/// Individual check result
#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Health check response with dependency checks
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub checks: std::collections::HashMap<String, CheckResult>,
}

/// Minimum disk space for healthy operation (100 MB)
const MIN_DISK_SPACE_BYTES: u64 = 100 * 1024 * 1024;

/// `GET /health` — health check (no auth required)
///
/// Checks critical dependencies and returns an aggregated status:
/// - `"ok"` — all checks passed
/// - `"degraded"` — some checks failed (IPC unavailable, disk low)
pub async fn health_check(
    State(state): State<AppState>,
) -> Json<HealthResponse> {
    let mut checks = std::collections::HashMap::new();
    let mut has_degraded = false;

    // 1. IPC Session Manager check
    match &state.session_mgr {
        Some(_) => {
            checks.insert("ipc".to_string(), CheckResult {
                status: "ok".to_string(),
                detail: None,
            });
        }
        None => {
            has_degraded = true;
            checks.insert("ipc".to_string(), CheckResult {
                status: "degraded".to_string(),
                detail: Some("Session manager not initialized".to_string()),
            });
        }
    }

    // 2. CronStore database check
    {
        let gw = state.gateway_state.read().await;
        match &gw.cron_store {
            Some(store) => {
                match store.health_check() {
                    Ok(()) => {
                        checks.insert("cron_store".to_string(), CheckResult {
                            status: "ok".to_string(),
                            detail: None,
                        });
                    }
                    Err(e) => {
                        has_degraded = true; // Cron is non-critical
                        checks.insert("cron_store".to_string(), CheckResult {
                            status: "unhealthy".to_string(),
                            detail: Some(format!("Database error: {}", e)),
                        });
                    }
                }
            }
            None => {
                has_degraded = true;
                checks.insert("cron_store".to_string(), CheckResult {
                    status: "degraded".to_string(),
                    detail: Some("CronStore not initialized".to_string()),
                });
            }
        }
    }

    // 4. Disk space check on data directory
    {
        let gw = state.gateway_state.read().await;
        // Use the data directory for disk space check
        let data_dir = gw.config.as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"));
        match fs2::available_space(&data_dir) {
            Ok(available) => {
                if available < MIN_DISK_SPACE_BYTES {
                    has_degraded = true;
                    checks.insert("disk".to_string(), CheckResult {
                        status: "degraded".to_string(),
                        detail: Some(format!(
                            "Low disk space: {} MB available",
                            available / (1024 * 1024)
                        )),
                    });
                } else {
                    checks.insert("disk".to_string(), CheckResult {
                        status: "ok".to_string(),
                        detail: Some(format!(
                            "{} MB available",
                            available / (1024 * 1024)
                        )),
                    });
                }
            }
            Err(e) => {
                has_degraded = true;
                checks.insert("disk".to_string(), CheckResult {
                    status: "degraded".to_string(),
                    detail: Some(format!("Cannot check disk space: {}", e)),
                });
            }
        }
    }

    let overall = if has_degraded {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    };

    Json(HealthResponse {
        status: match overall {
            HealthStatus::Ok => "ok".to_string(),
            HealthStatus::Degraded => "degraded".to_string(),
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        checks,
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
}

/// `GET /api/status` — system status
pub async fn system_status(
    State(state): State<AppState>,
) -> Json<SystemStatusResponse> {
    let gw = state.gateway_state.read().await;
    Json(SystemStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        agents_installed: gw.installed_agents.len(),
        agents_running: gw.running_agents.len(),
        uptime_secs: 0, // TODO: track actual uptime
    })
}

// ── Error response helpers ────────────────────────────────────────────

/// Standard API error response
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
}

impl ApiError {
    pub fn not_found(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::NOT_FOUND, Json(Self {
            error: msg.to_string(),
            code: 404,
        }))
    }

    pub fn bad_request(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::BAD_REQUEST, Json(Self {
            error: msg.to_string(),
            code: 400,
        }))
    }

    pub fn internal(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(Self {
            error: msg.to_string(),
            code: 500,
        }))
    }

    pub fn unauthorized(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::UNAUTHORIZED, Json(Self {
            error: msg.to_string(),
            code: 401,
        }))
    }

    pub fn service_unavailable(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::SERVICE_UNAVAILABLE, Json(Self {
            error: msg.to_string(),
            code: 503,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rollball-test-http-routes-{}-{}", std::process::id(), unique));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = test_app_state();
        let resp = health_check(State(state)).await;
        assert_eq!(resp.status, "degraded"); // degraded because no session_mgr/stores
        assert!(!resp.version.is_empty());
        assert!(!resp.checks.is_empty());
    }

    #[tokio::test]
    async fn test_system_status() {
        let state = test_app_state();
        let resp = system_status(State(state)).await;
        assert_eq!(resp.agents_installed, 0);
        assert_eq!(resp.agents_running, 0);
    }

    #[test]
    fn test_build_router() {
        let state = test_app_state();
        let _router = build_router(state);
    }

    // ── BridgeEventType tests ────────────────────────────────────────────────

    #[test]
    fn test_bridge_event_type_from_action() {
        assert_eq!(BridgeEventType::from_action("agent_response"), Some(BridgeEventType::Done));
        assert_eq!(BridgeEventType::from_action("agent_chunk"), Some(BridgeEventType::Chunk));
        assert_eq!(BridgeEventType::from_action("agent_tool_call"), Some(BridgeEventType::ToolCall));
        assert_eq!(BridgeEventType::from_action("agent_tool_result"), Some(BridgeEventType::ToolResult));
        assert_eq!(BridgeEventType::from_action("agent_error"), Some(BridgeEventType::Error));
        assert_eq!(BridgeEventType::from_action("agent_interrupted"), Some(BridgeEventType::Interrupted));
        assert_eq!(BridgeEventType::from_action("tool_approval_needed"), Some(BridgeEventType::ToolApprovalNeeded));
        assert_eq!(BridgeEventType::from_action("memory_updated"), Some(BridgeEventType::MemoryUpdated));
        assert_eq!(BridgeEventType::from_action("skill_executed"), Some(BridgeEventType::SkillExecuted));
        assert_eq!(BridgeEventType::from_action("unknown_action"), None);
    }

    #[test]
    fn test_bridge_event_type_as_str() {
        assert_eq!(BridgeEventType::Chunk.as_str(), "chunk");
        assert_eq!(BridgeEventType::Done.as_str(), "done");
        assert_eq!(BridgeEventType::Error.as_str(), "error");
        assert_eq!(BridgeEventType::Interrupted.as_str(), "interrupted");
        assert_eq!(BridgeEventType::ToolCall.as_str(), "tool_call");
        assert_eq!(BridgeEventType::ToolResult.as_str(), "tool_result");
        assert_eq!(BridgeEventType::ToolApprovalNeeded.as_str(), "tool_approval_needed");
        assert_eq!(BridgeEventType::MemoryUpdated.as_str(), "memory_updated");
        assert_eq!(BridgeEventType::SkillExecuted.as_str(), "skill_executed");
        assert_eq!(BridgeEventType::Unknown.as_str(), "unknown");
    }

    #[test]
    fn test_default_for_unknown_is_not_done() {
        // Unknown actions must NOT be silently treated as "done" —
        // that would break streaming state in the frontend.
        assert_ne!(BridgeEventType::default_for_unknown(), BridgeEventType::Done);
        assert_eq!(BridgeEventType::default_for_unknown(), BridgeEventType::Unknown);
    }

    #[test]
    fn test_bridge_event_type_serialization() {
        let event = BridgeEvent {
            agent_id: "com.test".to_string(),
            message_id: "msg-1".to_string(),
            event_type: BridgeEventType::Chunk,
            payload: serde_json::json!({"delta": "hi"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        // serde rename_all = snake_case
        assert!(json.contains("\"chunk\""));
    }
}
