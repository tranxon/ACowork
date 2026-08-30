//! Sidecar payload utilities (ADR-033 — extracted from deleted grpc module).

use crate::gateway::state::GatewayState;

/// Build the (endpoint, spec_json) payload for a SidecarEndpointUpdate
/// targeting SidecarKind::Embed, from the current GatewayState.
pub fn build_embed_sidecar_payload(state: &GatewayState) -> Option<(String, String)> {
    let eps = state.embed_process.as_ref()?;
    let model_id = eps.active_model_id.as_ref()?;
    // ADR-055 D3: use the resolved advertise host instead of the
    // hard-coded 127.0.0.1 so remote Runtime processes can reach the
    // embed sidecar across the network.
    let endpoint = format!("http://{}:{}/v1", state.advertise_host, eps.port);
    let spec_json = serde_json::json!({
        "model_id": model_id,
        "dimension": eps.active_dimension.unwrap_or(0),
    })
    .to_string();
    Some((endpoint, spec_json))
}
