//! Minimal compat stubs — all gRPC code deleted (ADR-033).
//! These types exist only to avoid breaking existing type signatures.
//! They are always None / no-op / default.
//!
//! # Deprecation
//!
//! ADR-033 replaced gRPC with MQTT. These stubs remain temporarily to
//! avoid a large refactor of call sites in `gateway/mod.rs` and `http/`.
//! They should be removed entirely once the call sites are cleaned up
//! (see P2-1 in docs/review/zh/28-adr-033-mqtt-refactor-code-review.md).

#![allow(deprecated)]

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

// ── Session Manager (always empty) ──

/// gRPC stub — always returns false.
///
/// ADR-033: gRPC removed, this is a no-op placeholder waiting for
/// the cron system to be migrated to MQTT ControlCommand.
pub struct GrpcSessionStub;
#[allow(deprecated)]
impl GrpcSessionStub {
    pub async fn push_message(&self, _: impl std::fmt::Debug) -> bool { false }
}

/// ADR-033: gRPC removed — GrpcSessionManager is a no-op placeholder.
/// Returns `None` for all lookups, making `cron` → `find_by_agent_id`
/// return `None` for every agent — cron triggers are silently dropped.
///
/// P2-1 upstream: migrate cron to MQTT `ControlCommand` push
/// and remove this entire module.
pub struct GrpcSessionManager;
#[allow(deprecated)]
impl GrpcSessionManager {
    pub fn new() -> Self { Self }
    pub fn find_by_agent_id(&self, _: &str) -> Option<(String, GrpcSessionStub)> { None }
    pub fn find_session_by_agent_id(&self, _: &str) -> Option<GrpcSessionStub> { None }
    pub fn unregister_all_agent_sessions(&mut self) {}
    pub fn cleanup_pending(&self, _: u64) {}
    pub fn cleanup_pending_by_agent_id(&self, _: &str) {}
    pub fn register(&mut self, _: String, _: GrpcSessionStub) -> String { "stub".into() }
    pub fn session_count(&self) -> usize { 0 }
    pub fn authenticated_count(&self) -> usize { 0 }
    pub fn find_by_conn_id(&self, _: &str) -> Option<(String, GrpcSessionStub)> { None }
    pub fn send_session_state_request(&self, _: &str, _: &str) -> Option<(u64, tokio::sync::oneshot::Receiver<acowork_core::proto::ClientMessage>)> { None }
    pub fn send_latest_session_request(&self, _: &str) -> Option<(u64, tokio::sync::oneshot::Receiver<acowork_core::proto::ClientMessage>)> { None }
    pub fn send_memory_request(&self, _: &str, _: acowork_core::proto::server_message::Payload) -> Option<(u64, tokio::sync::oneshot::Receiver<acowork_core::proto::ClientMessage>)> { None }
    pub async fn push_to_agent(&self, _: &str, _: impl std::fmt::Debug) -> bool { false }
}

/// ADR-033: gRPC removed — SharedGrpcSessionMgr is a no-op placeholder.
pub type SharedGrpcSessionMgr = Arc<Mutex<GrpcSessionManager>>;

// ── gRPC server (never starts) ──

/// ADR-033: gRPC removed — `start_grpc_server` is permanently pending.
/// Keep the signature so routes.rs can wire it up without ifdefs;
/// the function body is an infinite `pending!()`.
#[allow(dead_code)]
pub async fn start_grpc_server<S,C,B,P,D>(
    _: SocketAddr, _: S, _: SharedGrpcSessionMgr, _: C, _: B, _: P, _: D,
) -> Result<(), Box<dyn std::error::Error+Send+Sync>> {
    tracing::info!("gRPC disabled (ADR-033)");
    std::future::pending::<()>().await;
    Ok(())
}

// ── Resource Pusher (all no-ops) ──

use crate::gateway::state::GatewayState;
use crate::http::routes::SharedHttpState;
use std::path::PathBuf;

pub fn build_embed_sidecar_payload(s: &GatewayState) -> Option<(String,String)> {
    let e = s.embed_process.as_ref()?;
    let m = e.active_model_id.as_ref()?;
    Some((format!("http://127.0.0.1:{}/v1",e.port), serde_json::json!({"model_id":m,"dimension":e.active_dimension.unwrap_or(0)}).to_string()))
}

/// ADR-033: gRPC removed — GlobalResourcePusher is a no-op placeholder.
/// Kept so call sites compile; all push methods are empty.
#[derive(Clone)]
pub struct GlobalResourcePusher { _s: SharedHttpState, _d: PathBuf }
#[allow(deprecated)]
impl GlobalResourcePusher {
    pub fn new(_: Option<SharedGrpcSessionMgr>, s: SharedHttpState, d: PathBuf) -> Self { Self{_s:s,_d:d} }
    pub async fn push_llm_config(&self) {}
    pub async fn push_mcp_catalog(&self) {}
    pub async fn push_search_config(&self) {}
    pub async fn push_user_profile(&self) {}
    pub async fn push_sidecar_endpoint(&self, _: acowork_core::protocol::SidecarKind, _: String, _: String) {}
    pub async fn push_migration_start(&self, _: &str, _: &str, _: &str, _: &str, _: usize) -> bool { false }
    pub fn has_grpc_mgr(&self) -> bool { false }
}
