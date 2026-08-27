//! Node-local sidecar hosting (ADR-055 §6.7) — Phase 4.
//!
//! **Migration landing point**: the LSP relay moves from the Gateway to
//! each Node (`node-local` scope) because the LSP server must share the
//! filesystem with the agent workspace
//! (`root_uri = file://{workspace_root}`). The supervisor code is
//! migrated from the Gateway's `lifecycle/lsp_relay_supervisor.rs`
//! (the tested template — SSE heartbeat, crash recovery), with the
//! parent-health probe retargeted to the Node `/health`.
//!
//! **Sidecar status topics** (retained, QoS 1 — ADR-055 §6.7):
//!
//! ```text
//! acowork/nodes/{node_id}/lsps                       AvailableLsps envelope
//! acowork/nodes/{node_id}/sidecars/lsp_relay/status  JSON sidecar status
//! ```
//!
//! The `acowork/global/lsps` topic the Gateway used to publish is
//! deprecated — Runtimes subscribe to their own node's topic instead.

pub mod lsp_relay;
pub mod lsp_relay_supervisor;

use acowork_core::mqtt_proto::{data_envelope, AvailableLsps, DataEnvelope};
use acowork_core::node::{node_lsps_topic, node_sidecar_status_topic};
use rumqttc::{AsyncClient, QoS};

/// Build the retained `AvailableLsps` payload (ADR-055 §6.7).
///
/// Ready → `endpoint = http://{advertise_host}:{port}` (D3: the
/// advertise host, not a hard-coded loopback — remote Runtimes reach
/// the relay on THIS node); unavailable → empty endpoint + `ready:false`.
pub(crate) fn build_lsps_payload(advertise_host: &str, port: u16, ready: bool) -> AvailableLsps {
    if ready {
        AvailableLsps {
            version: 1,
            endpoint: format!("http://{advertise_host}:{port}"),
            ready: true,
        }
    } else {
        AvailableLsps {
            version: 1,
            endpoint: String::new(),
            ready: false,
        }
    }
}

/// Publish the retained node-local LSP state through the given client:
/// `acowork/nodes/{node_id}/lsps` (AvailableLsps envelope) plus the
/// `acowork/nodes/{node_id}/sidecars/lsp_relay/status` JSON topic
/// (ADR-055 §6.7).
///
/// Takes the client explicitly so the bootstrap closure can use its
/// ConnAck-time client before `control::dispatcher` is installed; the
/// supervisor path goes through
/// [`crate::control::dispatcher::publish_lsps_state`] which delegates
/// here with the process-wide shared handle.
pub(crate) async fn publish_lsps_state(
    client: &AsyncClient,
    node_id: &str,
    advertise_host: &str,
    port: u16,
    ready: bool,
) -> Result<(), crate::error::NodeError> {
    let payload = build_lsps_payload(advertise_host, port, ready);
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::AvailableLsps(payload)),
    };
    client
        .publish(
            node_lsps_topic(node_id),
            QoS::AtLeastOnce,
            true,
            prost::Message::encode_to_vec(&envelope),
        )
        .await
        .map_err(|e| crate::error::NodeError::Mqtt(format!("Node publish lsps: {e}")))?;

    let endpoint = if ready {
        format!("http://{advertise_host}:{port}")
    } else {
        String::new()
    };
    let status = serde_json::json!({
        "status": if ready { "ready" } else { "unavailable" },
        "port": port,
        "endpoint": endpoint,
    });
    client
        .publish(
            node_sidecar_status_topic(node_id, "lsp_relay"),
            QoS::AtLeastOnce,
            true,
            serde_json::to_vec(&status).unwrap_or_default(),
        )
        .await
        .map_err(|e| crate::error::NodeError::Mqtt(format!("Node publish sidecar status: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_payload_advertises_host_port() {
        let p = build_lsps_payload("192.168.1.10", 19878, true);
        assert!(p.ready);
        assert_eq!(p.endpoint, "http://192.168.1.10:19878");
        assert_eq!(p.version, 1);
    }

    #[test]
    fn unavailable_payload_has_empty_endpoint() {
        let p = build_lsps_payload("192.168.1.10", 19878, false);
        assert!(!p.ready);
        assert!(p.endpoint.is_empty());
    }
}
