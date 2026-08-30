//! ADR-059: Bootstrap snapshot MQTT publisher.
//!
//! Owns the retained `acowork/global/bootstrap` topic. Subscribes to the
//! [`BootstrapOrchestrator`]'s change signal (a `watch` channel) and
//! republishes the latest snapshot on every change, so consumers
//! (Desktop, Runtime, Node, remote tooling) always see the Gateway's
//! current aggregated phase when they (re)connect to the broker.
//!
//! OCP: the publisher emits ONLY the wire-level `BootstrapState` payload
//! defined in `mqtt_payload.proto`. It does NOT expose per-subsystem
//! detail (see ADR-059 §5.4.4). Adding or renaming an internal subsystem
//! never changes this publisher's output.

use std::sync::Arc;

use tokio::task::JoinHandle;

use acowork_core::mqtt_proto::{
    self, data_envelope, BootstrapState as BootstrapStateProto, DataEnvelope,
};

use crate::bootstrap::{BootstrapOrchestrator, BootstrapSnapshot};
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

/// MQTT topic for the bootstrap snapshot (ADR-059 §5.1).
///
/// Retained, QoS 1. The Gateway is the sole publisher on this topic.
pub const TOPIC_BOOTSTRAP: &str = "acowork/global/bootstrap";

/// Handle returned by [`BootstrapPublisher::start`]. Dropping it stops
/// the publisher loop.
///
/// The handle is intentionally `Clone`-incompatible: there is exactly
/// one retained publisher per Gateway and dropping the handle is the
/// only way to stop the loop. (Borrowing [`MqttPublisherHandle`] from
/// the existing [`crate::mqtt::MqttGlobalResourcesPublisher`] is fine
/// for trigger / ready coordination, but the bootstrap publisher owns
/// its own background task.)
pub struct BootstrapPublisherHandle {
    _task: JoinHandle<()>,
    /// Held so that the orchestrator (and therefore the change signal)
    /// stays alive for the lifetime of the publisher loop.
    _orchestrator: Arc<BootstrapOrchestrator>,
}

/// Configurable knobs for [`BootstrapPublisher::start`].
pub struct BootstrapPublisherOptions<'a> {
    /// MQTT client to publish with. Usually the same
    /// `GatewayMqttClient` used by [`crate::mqtt::MqttGlobalResourcesPublisher`].
    pub client: &'a GatewayMqttClient,
    /// Orchestrator providing the live snapshot.
    pub orchestrator: Arc<BootstrapOrchestrator>,
}

/// Bootstrap snapshot publisher.
///
/// Construct via [`BootstrapPublisher::start`], which spawns the
/// background loop. The loop:
///
/// 1. Reads the initial snapshot from the orchestrator and publishes it
///    immediately. This ensures the retained payload is present BEFORE
///    any consumer subscribes — important for cold-start scenarios
///    where the Desktop opens a fresh MQTT connection before the
///    Gateway has had a chance to publish.
///
/// 2. Parks on the orchestrator's change signal and republishes on
///    every change. No periodic polling — `watch` events drive
///    republishes, and the channel's level-triggered semantics guarantee
///    a transition that happens while the loop is publishing is never
///    lost (ADR-059 §7.2 re-delivery consistency).
///
/// 3. Logs every publish (DEBUG) and any error (WARN). A failure to
///    publish does NOT abort the loop: the next change will retry.
pub struct BootstrapPublisher;

impl BootstrapPublisher {
    /// Start the bootstrap publisher loop.
    ///
    /// The returned handle owns the background task; dropping it stops
    /// the loop. The orchestrator is held alive for the same lifetime
    /// so the change-notify subscription never dangles.
    pub fn start(opts: BootstrapPublisherOptions<'_>) -> BootstrapPublisherHandle {
        let mut changes = opts.orchestrator.subscribe_changes();
        let orchestrator = opts.orchestrator.clone();
        let client = opts.client.clone();
        // Seed the retained payload BEFORE returning so the caller can
        // rely on `acowork/global/bootstrap` being available immediately.
        let initial_snapshot = orchestrator.snapshot();
        let initial_topic = TOPIC_BOOTSTRAP.to_string();
        let initial_client = client.clone();
        // Use a oneshot-friendly publish: the broker may not be ready
        // at construction time, so we spawn a background loop that
        // first publishes the initial snapshot and then waits for
        // change notifications.
        let orchestrator_for_loop = orchestrator.clone();
        let task = tokio::spawn(async move {
            tracing::info!(
                topic = initial_topic,
                instance_id = %initial_snapshot.instance_id,
                phase = ?initial_snapshot.phase,
                version = initial_snapshot.version,
                "Bootstrap publisher: starting loop"
            );

            // Initial publish.
            publish_snapshot(&initial_client, &initial_snapshot).await;

            // Change-driven republish loop. No periodic polling — the
            // orchestrator signals us on every transition. Unlike a
            // `Notify`, a watch receiver observes the LATEST version
            // even when several transitions happened while the loop
            // was publishing, so the retained topic always converges.
            loop {
                // Park on the orchestrator's change signal.
                if changes.changed().await.is_err() {
                    // The orchestrator was dropped — nothing left to
                    // publish.
                    break;
                }
                let snapshot = orchestrator_for_loop.snapshot();
                publish_snapshot(&initial_client, &snapshot).await;
            }
        });
        BootstrapPublisherHandle {
            _task: task,
            _orchestrator: orchestrator,
        }
    }
}

/// Publish a single snapshot. Errors are logged but never propagated —
/// the publisher loop keeps running and the next change will retry.
async fn publish_snapshot(client: &GatewayMqttClient, snapshot: &BootstrapSnapshot) {
    let envelope = envelope_from_snapshot(snapshot);
    match client
        .publish_envelope(TOPIC_BOOTSTRAP, &envelope, MqttQoS::AtLeastOnce, true)
        .await
    {
        Ok(()) => {
            tracing::debug!(
                topic = TOPIC_BOOTSTRAP,
                instance_id = %snapshot.instance_id,
                phase = ?snapshot.phase,
                version = snapshot.version,
                "Bootstrap snapshot published (Retained)"
            );
        }
        Err(e) => {
            tracing::warn!(
                topic = TOPIC_BOOTSTRAP,
                instance_id = %snapshot.instance_id,
                phase = ?snapshot.phase,
                version = snapshot.version,
                error = %e,
                "Bootstrap snapshot publish failed; will retry on next change"
            );
        }
    }
}

/// Convert an internal [`BootstrapSnapshot`] into the wire-level
/// `DataEnvelope` carrying `BootstrapState`.
fn envelope_from_snapshot(snapshot: &BootstrapSnapshot) -> DataEnvelope {
    let proto = BootstrapStateProto {
        protocol_version: snapshot.protocol_version,
        instance_id: snapshot.instance_id.clone(),
        version: snapshot.version,
        phase: snapshot.phase.as_proto_i32(),
        phase_detail: snapshot.phase_detail.clone(),
        issued_at_ms: snapshot.issued_at_ms,
    };
    DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::BootstrapState(proto)),
    }
}

// Suppress dead_code lint for the unused import in tests — prost types
// are referenced by name inside test payloads below.
#[allow(dead_code)]
fn _ensure_proto_import_kept() -> mqtt_proto::BootstrapState {
    mqtt_proto::BootstrapState::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_carries_bootstrap_state_payload() {
        let snap = BootstrapSnapshot::booting("instance-A".to_string());
        let env = envelope_from_snapshot(&snap);
        match env.payload {
            Some(data_envelope::Payload::BootstrapState(bs)) => {
                assert_eq!(bs.instance_id, "instance-A");
                assert_eq!(bs.protocol_version, 1);
                assert_eq!(bs.phase, 1); // Booting
            }
            other => panic!("expected BootstrapState payload, got {:?}", other),
        }
    }

    #[test]
    fn envelope_propagates_phase_transitions() {
        use crate::bootstrap::orchestrator::{BootstrapPhase, BootstrapSnapshot};

        let snap = BootstrapSnapshot {
            protocol_version: 1,
            instance_id: "instance-B".to_string(),
            version: 7,
            phase: BootstrapPhase::Ready,
            phase_detail: "1/1 required ready".to_string(),
            issued_at_ms: 12345,
        };
        let env = envelope_from_snapshot(&snap);
        match env.payload {
            Some(data_envelope::Payload::BootstrapState(bs)) => {
                assert_eq!(bs.phase, 2); // Ready
                assert_eq!(bs.version, 7);
                assert_eq!(bs.phase_detail, "1/1 required ready");
                assert_eq!(bs.issued_at_ms, 12345);
            }
            other => panic!("expected BootstrapState payload, got {:?}", other),
        }
    }
}