//! Resource pusher stub — all methods are no-ops (ADR-033 gRPC removed).
//!
//! ADR-034 Phase 6: Extracted from compat.rs so compat.rs can be deleted.
//! The pusher interface is preserved for call sites in provider_api,
//! mcp_catalog_api, embedding_api, etc. — all push methods are no-ops
//! because MQTT-based resource sync replaced gRPC push.

use crate::gateway::state::GatewayState;
use crate::http::routes::SharedHttpState;
use std::path::PathBuf;

/// Build the embed sidecar endpoint payload from the gateway's current
/// embed process state. Returns `(endpoint_url, spec_json)` or `None`
/// if no embed process is running.
pub fn build_embed_sidecar_payload(s: &GatewayState) -> Option<(String, String)> {
    let e = s.embed_process.as_ref()?;
    let m = e.active_model_id.as_ref()?;
    Some((
        format!("http://127.0.0.1:{}/v1", e.port),
        serde_json::json!({"model_id": m, "dimension": e.active_dimension.unwrap_or(0)}).to_string(),
    ))
}

/// ADR-033: Global resource pusher — all methods are no-ops.
/// Kept so call sites in provider_api, mcp_catalog_api, etc. compile
/// without modification. MQTT-based resource sync replaced gRPC push.
#[derive(Clone)]
pub struct ResourcePusher {
    _s: SharedHttpState,
    _d: PathBuf,
}

impl ResourcePusher {
    pub fn new(s: SharedHttpState, d: PathBuf) -> Self {
        Self { _s: s, _d: d }
    }

    pub async fn push_llm_config(&self) {}
    pub async fn push_mcp_catalog(&self) {}
    pub async fn push_search_config(&self) {}
    pub async fn push_user_profile(&self) {}
    pub async fn push_sidecar_endpoint(
        &self,
        _: acowork_core::protocol::SidecarKind,
        _: String,
        _: String,
    ) {
    }
    pub async fn push_migration_start(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: usize,
    ) -> bool {
        false
    }
}
