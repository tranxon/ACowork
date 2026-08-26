//! MQTT message dispatch (ADR-033).
//!
//! Handles incoming MQTT messages on the Gateway's broker connection.
//! Plain-text payloads (`http_port`, agent `status`) carry simple semantic
//! data (port number, online/offline) rather than `DataEnvelope` protobuf,
//! so they are matched by topic pattern and parsed inline.
//!
//! Status messages from the Runtime (`acowork/agents/{id}/status`) are
//! additionally re-published as protobuf `DataEnvelope` payloads on a
//! separate topic. This bridges the legacy plain-text retained topic
//! (used by older Runtimes and the Gateway's internal AgentRegistry) to
//! the modern protobuf contract consumed by the Desktop's
//! `data_envelope::Payload::AgentStatus` handler.
//!
//! See `docs/zh/protocols/mqtt.md` §8 (Topic patterns).

use std::sync::Arc;

use acowork_core::mqtt_proto::{
    data_envelope, AgentStatus as AgentStatusProto, DataEnvelope, NodeEnrollResult,
};
use prost::Message as _;

use crate::handlers::server::SharedState;
use crate::http::proxy::SharedRuntimeHttpRegistry;
use crate::mqtt::agent_registry::SharedAgentRegistry;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};
use crate::mqtt::enrollment::{
    EnrollmentTokenStore, NodeTokenStore, SharedEnrollmentTokenStore, SharedNodeTokenStore,
    TokenValidation,
};
use crate::mqtt::node_control::NodeControlClient;
use crate::mqtt::node_registry::SharedNodeRegistry;

/// Unified MQTT message handler — called from the Gateway's MQTT callback.
///
/// `mqtt_client` is used to re-publish plain-text status messages as
/// protobuf `DataEnvelope` (see module docs). Pass `None` in tests that
/// don't have a real broker. `node_control` correlates NodeEvent
/// results against in-flight node commands (ADR-055 §6.2).
///
/// ADR-055 Phase 5a: `enrollment_tokens` / `node_tokens` are the
/// credential stores backing the enrollment handshake; `auth_enabled`
/// gates enrollment-token validation (when false, enrollments are
/// accepted without a token, preserving the pre-5a default path).
#[allow(clippy::too_many_arguments)]
pub fn handle_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
    node_registry: &SharedNodeRegistry,
    mqtt_client: Option<&Arc<GatewayMqttClient>>,
    state: &SharedState,
    node_control: Option<&NodeControlClient>,
    enrollment_tokens: Option<&SharedEnrollmentTokenStore>,
    node_tokens: Option<&SharedNodeTokenStore>,
    auth_enabled: bool,
) {
    handle_plaintext_message(
        topic,
        payload,
        runtime_http_registry,
        agent_registry,
        node_registry,
        mqtt_client,
        state,
        node_control,
        enrollment_tokens,
        node_tokens,
        auth_enabled,
    );
}

/// Topic pattern matcher.
///
/// Supports MQTT wildcard matching:
/// - `+` matches a single level
/// - `#` matches all remaining levels (must be the last character)
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let filter_parts: Vec<&str> = filter.split('/').collect();
    let topic_parts: Vec<&str> = topic.split('/').collect();

    for (i, fp) in filter_parts.iter().enumerate() {
        if *fp == "#" {
            return true; // matches everything remaining
        }
        if i >= topic_parts.len() {
            return false;
        }
        if *fp != "+" && *fp != topic_parts[i] {
            return false;
        }
    }

    filter_parts.len() == topic_parts.len()
}

/// Handle a plain-text MQTT message (non-DataEnvelope payload).
///
/// Topics with simple text payloads rather than protobuf envelopes:
/// - `acowork/agents/+/http_endpoint` → registers Runtime HTTP port for reverse proxy
/// - `acowork/agents/+/status` → updates AgentRegistry online/offline
///   status AND re-publishes as a protobuf `DataEnvelope` so the
///   Desktop's `data_envelope::Payload::AgentStatus` handler also
///   receives the transition.
/// - `acowork/nodes/+/status` → updates NodeRegistry online/offline
///   (ADR-055 §6.2; plain text + LWT, same shape as agent status).
/// - `acowork/nodes/+/info` → protobuf `DataEnvelope<NodeInfo>`
///   metadata snapshot into the NodeRegistry (the payload IS an
///   envelope; routed here because the node topic family shares the
///   same dispatch surface).
/// - `acowork/nodes/+/enroll` → ADR-055 Phase 5a enrollment handshake
///   (protobuf `DataEnvelope<NodeEnroll>`); validated, then answered
///   with an `enroll_result` reply on the per-node result topic.
///
/// This replaces the inline callback previously in `gateway/mod.rs`.
#[allow(clippy::too_many_arguments)]
pub fn handle_plaintext_message(
    topic: &str,
    payload: &[u8],
    runtime_http_registry: &SharedRuntimeHttpRegistry,
    agent_registry: &SharedAgentRegistry,
    node_registry: &SharedNodeRegistry,
    mqtt_client: Option<&Arc<GatewayMqttClient>>,
    state: &SharedState,
    node_control: Option<&NodeControlClient>,
    enrollment_tokens: Option<&SharedEnrollmentTokenStore>,
    node_tokens: Option<&SharedNodeTokenStore>,
    auth_enabled: bool,
) {
    if topic_matches("acowork/agents/+/http_endpoint", topic) {
        let agent_id = topic
            .strip_prefix("acowork/agents/")
            .and_then(|s| s.strip_suffix("/http_endpoint"))
            .unwrap_or("");
        if agent_id.is_empty() {
            tracing::warn!(topic, "http_endpoint topic matched but agent_id extraction failed");
            return;
        }
        // ADR-055 D3: payload is now the full endpoint URL (e.g.
        // "http://127.0.0.1:54321") rather than a bare port. Surface
        // malformed payloads as warnings — without these, a bad payload
        // would cause the Gateway to 503 with no diagnostic trail.
        let endpoint = match std::str::from_utf8(payload) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id,
                    error = %e,
                    "http_endpoint payload is not valid UTF-8 — ignoring"
                );
                return;
            }
        };
        if endpoint.is_empty() {
            tracing::warn!(
                topic,
                agent_id,
                "http_endpoint payload is empty — Gateway will 503 every reverse-proxy request for this agent"
            );
            return;
        }
        let reg = runtime_http_registry.clone();
        let aid = agent_id.to_string();
        let aid_for_log = aid.clone();
        let endpoint_for_log = endpoint.clone();
        tokio::spawn(async move {
            reg.write().await.register(&aid, &endpoint);
        });
        tracing::info!(agent_id = %aid_for_log, endpoint = %endpoint_for_log, "Registered Runtime HTTP endpoint via MQTT");
    } else if topic_matches("acowork/agents/+/status", topic) {
        let reg = agent_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        let state_for_status = state.clone();
        // Clone the (optional) GatewayMqttClient for the spawned
        // re-publish task. None means the Gateway is running without
        // an embedded broker (e.g. tests); in that case we skip
        // re-publishing but still update the AgentRegistry.
        let mqtt_client_for_republish = mqtt_client.cloned();
        tokio::spawn(async move {
            // 1) Update internal registry (sleeping_at stamping, etc.)
            reg.write().await.update_from_mqtt(&topic_owned, &payload_owned);

            // ADR-055 §6.2: mirror liveness into GatewayState — drop the
            // running entry when the Runtime reports offline (crash,
            // auto-sleep, manual stop). The node reaper owns the process;
            // the Gateway only tracks the running/ready surface.
            if let Some(agent_id) = extract_agent_id_from_status_topic(&topic_owned)
                && String::from_utf8_lossy(&payload_owned).trim() == "offline"
            {
                state_for_status.write().await.remove_running(&agent_id);
            }

            // 2) Re-publish as a protobuf DataEnvelope so subscribers
            //    that listen for `data_envelope::Payload::AgentStatus`
            //    (the Desktop's chat_mqtt) also see the transition.
            //    Without this, the Desktop would only see the
            //    plain-text retained payload (which the Desktop also
            //    handles — see `parse_plaintext_agent_status` — but
            //    the protobuf branch is needed for any future Desktop
            //    code that wants the structured AgentStatus type
            //    rather than a string).
            let Some(client) = mqtt_client_for_republish else {
                return;
            };
            let Some(agent_id) = extract_agent_id_from_status_topic(&topic_owned) else {
                tracing::warn!(topic = %topic_owned, "status topic matched but agent_id extraction failed");
                return;
            };
            let payload_str = String::from_utf8_lossy(&payload_owned);
            let (online, sleeping) = match payload_str.trim() {
                "sleeping" => (true, true),
                "online" => (true, false),
                "offline" => (false, false),
                other => {
                    tracing::warn!(
                        topic = %topic_owned,
                        payload = %other,
                        "republish: unknown agent status payload"
                    );
                    return;
                }
            };
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                    agent_id,
                    online,
                    sleeping,
                })),
            };
            // Same topic — the broker will replace the retained
            // plain-text payload with this protobuf envelope for new
            // subscribers. Existing plain-text subscribers continue to
            // see the cached plain text (broker delivers different
            // payloads to the same topic to subscribers based on
            // subscription time).
            if let Err(e) = client
                .publish_envelope(&topic_owned, &envelope, MqttQoS::AtLeastOnce, true)
                .await
            {
                tracing::warn!(
                    topic = %topic_owned,
                    error = %e,
                    "failed to re-publish agent status as protobuf"
                );
            }
        });
    } else if topic_matches("acowork/agents/+/ready", topic) {
        // Plain-text retained payload ("true" / "false") published by the
        // Runtime after Phase A–C have all populated the HTTP server's
        // late-bind slots and the runtime is ready to serve any
        // `/agents/{id}/*` request. The Gateway pins
        // `running_agents[id].ready` to this value so `/api/agents`
        // reports it; the Desktop fast-path keeps its `running && ready`
        // gate closed until the Gateway mirrors `ready=true`.
        //
        // The plain-text shape is intentional: it parallels
        // `acowork/agents/+/status` so the dispatch surface is uniform
        // and the broker's retained message gives a fresh Gateway
        // startup the same answer without polling the Runtime.
        let agent_id = match topic
            .strip_prefix("acowork/agents/")
            .and_then(|s| s.strip_suffix("/ready"))
        {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                tracing::warn!(topic, "ready topic matched but agent_id extraction failed");
                return;
            }
        };
        let payload_str = match std::str::from_utf8(payload) {
            Ok(s) => s.trim(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id = %agent_id,
                    error = %e,
                    "ready payload is not valid UTF-8 — ignoring"
                );
                return;
            }
        };
        let ready = match payload_str {
            "true" => true,
            "false" => false,
            other => {
                tracing::warn!(
                    topic,
                    agent_id = %agent_id,
                    payload = %other,
                    "ready payload is not 'true'/'false' — ignoring"
                );
                return;
            }
        };
        let state_for_ready = state.clone();
        let agent_id_for_log = agent_id.clone();
        tokio::spawn(async move {
            let mut gw = state_for_ready.write().await;
            gw.set_agent_ready(&agent_id_for_log, ready);
            tracing::info!(
                agent_id = %agent_id_for_log,
                ready,
                "Runtime ready signal received via MQTT"
            );
        });
    } else if topic_matches("acowork/nodes/+/status", topic) {
        // ADR-055 §6.2: node online/offline (plain text + LWT,
        // same shape as the agent status topic). Feeds the
        // NodeRegistry — the local-node supervisor and (Phase 2b)
        // the lifecycle control plane read it.
        let reg = node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_status_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/info", topic) {
        // ADR-055 §6.2: node metadata snapshot (protobuf
        // DataEnvelope<NodeInfo>, retained). Feeds the NodeRegistry.
        let reg = node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_info_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/lsps", topic) {
        // ADR-055 §6.7 (Phase 4): node-local LSP relay endpoint
        // (protobuf DataEnvelope<AvailableLsps>, retained — replaces
        // the deprecated `acowork/global/lsps`). Feeds the
        // NodeRegistry's `lsp_endpoint` slot, served via
        // `GET /api/agents/{id}/lsp-endpoint`.
        let reg = node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_lsps_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/enroll", topic) {
        // ADR-055 Phase 5a: node enrollment handshake. The Node
        // publishes a DataEnvelope<NodeEnroll> with its identity and
        // (when auth is enabled) a one-time enrollment token; the
        // Gateway validates, mints/reuses the long-lived node token,
        // records it, and replies on `enroll_result` (QoS 1).
        let Some(node_id) = extract_enroll_topic_node_id(topic) else {
            tracing::warn!(topic, "enroll topic matched but node_id extraction failed");
            return;
        };
        let reg = node_registry.clone();
        let mqtt_for_reply = mqtt_client.cloned();
        let enroll_store = enrollment_tokens.cloned();
        let node_store = node_tokens.cloned();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            process_enroll_message(
                payload_owned,
                node_id,
                reg,
                mqtt_for_reply,
                enroll_store,
                node_store,
                auth_enabled,
            )
            .await;
        });
    } else if topic_matches("acowork/nodes/+/agents/+/events", topic) {
        // ADR-055 §6.2: per-agent NodeEvent result (protobuf envelope,
        // QoS 1). Correlate against in-flight node commands.
        let Some(control) = node_control.cloned() else {
            tracing::debug!(topic, "Node agent event received but no NodeControlClient wired — dropping");
            return;
        };
        let payload_owned = payload.to_vec();
        let topic_owned = topic.to_string();
        tokio::spawn(async move {
            match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(envelope) => {
                    if let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload {
                        control.handle_event(event).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(topic = %topic_owned, error = %e, "Bad NodeEvent envelope on agent events topic");
                }
            }
        });
    } else if topic_matches("acowork/nodes/+/agents/+/installed", topic) {
        // ADR-055 §6.5: per-agent installed-package inventory (retained).
        // Aggregates into GatewayState::installed_agents, replacing the
        // pre-hard-cut on-disk packages scan (L2-9). An empty retained
        // payload clears the entry (uninstall).
        let Some((node_id, agent_id)) = extract_installed_topic_ids(topic) else {
            tracing::warn!(topic, "installed topic matched but id extraction failed");
            return;
        };
        let state_for_installed = state.clone();
        let payload_owned = payload.to_vec();
        let node_id_owned = node_id.clone();
        tokio::spawn(async move {
            let mut gw = state_for_installed.write().await;
            if payload_owned.is_empty() {
                gw.remove_installed(&agent_id);
                tracing::info!(node_id = %node_id_owned, agent_id, "Removed installed agent (node retained cleared)");
                return;
            }
            match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(envelope) => {
                    if let Some(data_envelope::Payload::InstalledAgentInfo(info)) = envelope.payload
                    {
                        // ADR-055 §3.2: an install completed (or a retained
                        // re-publish on node reconnect). Record whether this
                        // is a NEW install so cron triggers register exactly
                        // once (the async-install completion hook).
                        let is_new = !gw.installed_agents.contains_key(&info.agent_id);
                        let Some(aid) = gw.upsert_installed_from_node(&node_id_owned, &info) else {
                            return;
                        };

                        // ADR-017: apply the Gateway-owned avatar cache to
                        // the freshly-aggregated agent so list_agents shows
                        // the correct avatar for stopped agents.
                        if let Some(data_dir) = gw.config.as_ref().map(|c| c.data_dir.clone()) {
                            let cache = crate::http::agent_config::load_avatar_cache(
                                std::path::Path::new(&data_dir),
                            );
                            if let Some(entry) = cache.get(&aid)
                                && let Some(agent) = gw.installed_agents.get_mut(&aid)
                            {
                                agent.manifest.avatar = entry.avatar.clone();
                                agent.manifest.builtin_avatar = entry.builtin_avatar.clone();
                            }
                        }

                        // ADR-055 §3.2 / S3.3: register the manifest-declared
                        // cron triggers on first install. The node no longer
                        // owns cron; the Gateway registers them once the
                        // install-completed inventory arrives.
                        if is_new
                            && let Ok(manifest) =
                                acowork_core::AgentManifest::from_toml(&info.manifest_toml)
                        {
                            crate::cron::register_agent_cron_triggers(&mut gw, &aid, &manifest);
                        }

                        tracing::info!(node_id = %node_id_owned, agent_id = %aid, "Aggregated installed agent from node");
                    }
                }
                Err(e) => {
                    tracing::warn!(node_id = %node_id_owned, agent_id, error = %e, "Bad InstalledAgentInfo envelope on installed topic");
                }
            }
        });
    }
}

/// Extract `(node_id, agent_id)` from
/// `acowork/nodes/{node_id}/agents/{agent_id}/installed`.
fn extract_installed_topic_ids(topic: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 6
        && parts[0] == "acowork"
        && parts[1] == "nodes"
        && parts[3] == "agents"
        && parts[5] == "installed"
    {
        let node_id = parts[2];
        let agent_id = parts[4];
        if !node_id.is_empty() && !agent_id.is_empty() {
            return Some((node_id.to_string(), agent_id.to_string()));
        }
    }
    None
}

/// Extract `node_id` from `acowork/nodes/{node_id}/enroll`.
fn extract_enroll_topic_node_id(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4 && parts[0] == "acowork" && parts[1] == "nodes" && parts[3] == "enroll" {
        let node_id = parts[2];
        if !node_id.is_empty() {
            return Some(node_id.to_string());
        }
    }
    None
}

/// Outcome of the enrollment decision (ADR-055 Phase 5a).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnrollDecision {
    /// Accepted; `reuse` is true when the node re-enrolls with the
    /// same machine_uid (its existing node token is reused).
    /// `consume_enrollment_token` is true when the presented token is
    /// a one-time enrollment token that must be consumed afterwards
    /// (false for a pre-issued node token — local-node path).
    Accept {
        reuse: bool,
        consume_enrollment_token: bool,
    },
    /// Rejected, with the reason surfaced in the `enroll_result`.
    Reject { reason: String },
}

/// Pure read-only enrollment decision.
///
/// 1. When `auth_enabled`, the presented token must be either a valid
///    (known, unexpired, unconsumed) one-time enrollment token — the
///    standard path — OR the node's own long-lived token (pre-issued
///    local-node credential; mirrors the broker CONNECT check).
/// 2. `node_id` uniqueness: unclaimed → accept fresh; claimed by the
///    same machine_uid → accept with token reuse (idempotent
///    re-enrollment, e.g. after a Gateway restart); claimed by a
///    different machine_uid → reject (the node_id is a broker-level
///    identity and cannot be hijacked). A pre-issued record with an
///    empty machine_uid counts as unclaimed and is reused.
///
/// All side effects (token consumption, minting, registry write, reply)
/// are performed by the caller after this returns.
fn decide_enroll(
    node_id: &str,
    machine_uid: &str,
    enrollment_token: Option<&str>,
    auth_enabled: bool,
    enrollment_store: Option<&EnrollmentTokenStore>,
    node_store: Option<&NodeTokenStore>,
) -> EnrollDecision {
    let mut consume_enrollment_token = false;
    if auth_enabled {
        let Some(token) = enrollment_token else {
            return EnrollDecision::Reject {
                reason: "enrollment token required (mqtt.auth_enabled)".to_string(),
            };
        };
        let valid_enrollment = enrollment_store.map(|s| s.validate_token(token))
            == Some(TokenValidation::Valid);
        let valid_node_token = node_store
            .map(|s| s.node_token_matches(node_id, token))
            .unwrap_or(false);
        if valid_enrollment {
            consume_enrollment_token = true;
        } else if !valid_node_token {
            let reason = match enrollment_store.map(|s| s.validate_token(token)) {
                Some(TokenValidation::Expired) => "enrollment token expired",
                Some(TokenValidation::Consumed) => "enrollment token already used",
                Some(TokenValidation::Unknown) | None => "unknown enrollment token",
                Some(TokenValidation::Valid) => unreachable!(),
            };
            return EnrollDecision::Reject {
                reason: reason.to_string(),
            };
        }
    }
    match node_store.and_then(|s| s.machine_uid_of(node_id)) {
        None => EnrollDecision::Accept {
            reuse: false,
            consume_enrollment_token,
        },
        Some(existing) if existing.is_empty() || existing == machine_uid => EnrollDecision::Accept {
            reuse: true,
            consume_enrollment_token,
        },
        Some(_) => EnrollDecision::Reject {
            reason: format!("node_id '{node_id}' is already claimed by another machine"),
        },
    }
}

/// Parse a `DataEnvelope<NodeEnroll>` payload and enforce
/// topic/payload consistency: the payload's node_id must equal the
/// topic's node_id (otherwise a node could register a peer's identity).
fn parse_enroll_payload(
    payload: &[u8],
    topic_node_id: &str,
) -> Result<(String, Option<String>), String> {
    let envelope = DataEnvelope::decode(payload).map_err(|e| format!("bad DataEnvelope: {e}"))?;
    let enroll = match envelope.payload {
        Some(data_envelope::Payload::NodeEnroll(enroll)) => enroll,
        _ => return Err("payload is not a NodeEnroll envelope".to_string()),
    };
    if enroll.node_id != topic_node_id {
        return Err(format!(
            "payload node_id '{}' does not match topic node_id '{}'",
            enroll.node_id, topic_node_id
        ));
    }
    let token = if enroll.enrollment_token.is_empty() {
        None
    } else {
        Some(enroll.enrollment_token)
    };
    Ok((enroll.machine_uid, token))
}

/// Process an enrollment request end-to-end: validate → mint/reuse
/// node token → record in NodeRegistry → reply `enroll_result`.
///
/// Runs on the tokio runtime; the std mutex guards (non-Send) are
/// scoped so they never live across an await.
#[allow(clippy::too_many_arguments)]
async fn process_enroll_message(
    payload: Vec<u8>,
    node_id: String,
    node_registry: SharedNodeRegistry,
    mqtt_client: Option<Arc<GatewayMqttClient>>,
    enrollment_tokens: Option<SharedEnrollmentTokenStore>,
    node_tokens: Option<SharedNodeTokenStore>,
    auth_enabled: bool,
) {
    let (machine_uid, enrollment_token) = match parse_enroll_payload(&payload, &node_id) {
        Ok(ok) => ok,
        Err(error) => {
            tracing::warn!(node_id = %node_id, error = %error, "dropping malformed enroll message");
            return;
        }
    };
    if machine_uid.is_empty() {
        tracing::warn!(node_id = %node_id, "enroll payload missing machine_uid — dropping");
        return;
    }

    // Read-only decision; both guards drop before any await below
    // (std::sync::MutexGuard is not Send, so it cannot cross a
    // tokio::spawn boundary or an await point).
    let decision = {
        let enroll_guard = enrollment_tokens
            .as_ref()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
        let node_guard = node_tokens
            .as_ref()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
        decide_enroll(
            &node_id,
            &machine_uid,
            enrollment_token.as_deref(),
            auth_enabled,
            enroll_guard.as_deref(),
            node_guard.as_deref(),
        )
    };

    match decision {
        EnrollDecision::Reject { reason } => {
            tracing::warn!(node_id = %node_id, reason = %reason, "Enrollment rejected");
            publish_enroll_result(
                &mqtt_client,
                NodeEnrollResult {
                    node_id,
                    machine_uid,
                    node_token: String::new(),
                    status: "rejected".to_string(),
                    message: reason,
                },
            )
            .await;
        }
        EnrollDecision::Accept {
            reuse,
            consume_enrollment_token,
        } => {
            // Consume the one-time enrollment token when the decision
            // says so (a pre-issued node token is long-lived and is
            // left untouched). Validation ran under the same mutex
            // scope above, so a failure here means concurrent use.
            if auth_enabled && consume_enrollment_token {
                let consumed = match (enrollment_token.as_deref(), enrollment_tokens.as_ref()) {
                    (Some(token), Some(store)) => store
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .consume_token(token, &node_id),
                    _ => false,
                };
                if !consumed {
                    tracing::warn!(node_id = %node_id, "enrollment token consumption failed");
                    publish_enroll_result(
                        &mqtt_client,
                        NodeEnrollResult {
                            node_id,
                            machine_uid,
                            node_token: String::new(),
                            status: "rejected".to_string(),
                            message: "enrollment token already used".to_string(),
                        },
                    )
                    .await;
                    return;
                }
            }

            let node_token = match node_tokens.as_ref() {
                Some(store) => store
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .upsert(&node_id, &machine_uid),
                // No store wired (unit tests): mint a one-off token so
                // the handshake still completes with a usable credential.
                None => crate::mqtt::enrollment::generate_token(),
            };

            // Registry write is a tokio RwLock — safe to await here;
            // the std mutex guards above are already dropped.
            node_registry
                .write()
                .await
                .set_node_token(&node_id, node_token.clone());

            tracing::info!(node_id = %node_id, reuse, "Node enrolled");
            publish_enroll_result(
                &mqtt_client,
                NodeEnrollResult {
                    node_id,
                    machine_uid,
                    node_token,
                    status: "ok".to_string(),
                    message: String::new(),
                },
            )
            .await;
        }
    }
}

/// Publish an `enroll_result` reply on `acowork/nodes/{id}/enroll_result`
/// (QoS 1, not retained — the reply is per-request). No-op without a
/// client (tests / broker-less dispatch).
async fn publish_enroll_result(
    mqtt_client: &Option<Arc<GatewayMqttClient>>,
    result: NodeEnrollResult,
) {
    let Some(client) = mqtt_client else { return };
    let node_id = result.node_id.clone();
    let topic = acowork_core::node::node_enroll_result_topic(&node_id);
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeEnrollResult(result)),
    };
    if let Err(e) = client
        .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
        .await
    {
        tracing::warn!(node_id = %node_id, error = %e, "failed to publish enroll result");
    }
}

/// Extract `agent_id` from `acowork/agents/{agent_id}/status`.
///
/// Unlike `extract_agent_id_from_topic` (which is permissive), this
/// version requires the exact 4-segment shape of the status topic.
fn extract_agent_id_from_status_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4 && parts[0] == "acowork" && parts[1] == "agents" && parts[3] == "status" {
        let agent_id = parts[2];
        if !agent_id.is_empty() {
            return Some(agent_id.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::GatewayState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_state() -> SharedState {
        Arc::new(RwLock::new(GatewayState::new(
            "/tmp/acowork-dispatch-test-vault",
        )))
    }

    #[tokio::test]
    async fn test_node_status_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        handle_plaintext_message(
            "acowork/nodes/local/status",
            b"online",
            &http_reg,
            &agent_reg,
            &node_reg,
            None,
            &state,
            None,
            None,
            None,
            false,
        );
        // handle_plaintext_message spawns a tokio task; wait for it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(node_reg.read().await.is_online("local"));
    }

    #[tokio::test]
    async fn test_node_info_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let info = acowork_core::mqtt_proto::NodeInfo {
            node_id: "gpu-1".to_string(),
            machine_uid: "uid-1".to_string(),
            hostname: "h".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            max_agents: 16,
            agent_count: 0,
            http_endpoint: "http://127.0.0.1:19900".to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(info)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        handle_plaintext_message(
            "acowork/nodes/gpu-1/info",
            &payload,
            &http_reg,
            &agent_reg,
            &node_reg,
            None,
            &state,
            None,
            None,
            None,
            false,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = node_reg.read().await;
        assert_eq!(
            reg.get("gpu-1").and_then(|n| n.machine_uid.clone()),
            Some("uid-1".to_string())
        );
    }

    #[tokio::test]
    async fn test_node_lsps_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let lsps = acowork_core::mqtt_proto::AvailableLsps {
            version: 1,
            endpoint: "http://192.168.1.10:19878".to_string(),
            ready: true,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AvailableLsps(lsps)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        handle_plaintext_message(
            "acowork/nodes/gpu-1/lsps",
            &payload,
            &http_reg,
            &agent_reg,
            &node_reg,
            None,
            &state,
            None,
            None,
            None,
            false,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = node_reg.read().await;
        assert_eq!(
            reg.get("gpu-1").and_then(|n| n.lsp_endpoint.clone()),
            Some("http://192.168.1.10:19878".to_string())
        );
    }

    fn temp_test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("acowork-dispatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn enroll_payload(node_id: &str, machine_uid: &str, token: Option<&str>) -> Vec<u8> {
        let enroll = acowork_core::mqtt_proto::NodeEnroll {
            node_id: node_id.to_string(),
            machine_uid: machine_uid.to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            enrollment_token: token.unwrap_or("").to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEnroll(enroll)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn extract_enroll_topic_node_id_ok() {
        assert_eq!(
            extract_enroll_topic_node_id("acowork/nodes/gpu-1/enroll"),
            Some("gpu-1".to_string())
        );
        assert_eq!(extract_enroll_topic_node_id("acowork/nodes//enroll"), None);
        assert_eq!(extract_enroll_topic_node_id("acowork/nodes/gpu-1/info"), None);
        assert_eq!(extract_enroll_topic_node_id("acowork/nodes/gpu-1/enroll/x"), None);
    }

    #[test]
    fn parse_enroll_payload_enforces_topic_consistency() {
        let payload = enroll_payload("gpu-1", "uid-1", Some("tok-1"));
        let (machine_uid, token) =
            parse_enroll_payload(&payload, "gpu-1").expect("valid enroll parses");
        assert_eq!(machine_uid, "uid-1");
        assert_eq!(token.as_deref(), Some("tok-1"));

        // Topic/payload node_id mismatch must be rejected (a node
        // cannot enroll under a peer's node_id via the topic).
        assert!(parse_enroll_payload(&payload, "other-node").is_err());

        // Empty enrollment token is normalized to None.
        let no_token = enroll_payload("gpu-1", "uid-1", None);
        let (_, token) =
            parse_enroll_payload(&no_token, "gpu-1").expect("token-less enroll parses");
        assert!(token.is_none());

        // Non-NodeEnroll envelope payloads are rejected.
        let info = acowork_core::mqtt_proto::NodeInfo {
            node_id: "gpu-1".to_string(),
            machine_uid: "uid-1".to_string(),
            hostname: "h".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            max_agents: 16,
            agent_count: 0,
            http_endpoint: "http://127.0.0.1:19900".to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(info)),
        };
        assert!(
            parse_enroll_payload(&prost::Message::encode_to_vec(&envelope), "gpu-1").is_err()
        );
    }

    #[test]
    fn decide_enroll_accepts_fresh_node_without_token_when_auth_disabled() {
        let dir = temp_test_dir("decide-fresh");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let node_store = NodeTokenStore::load(&dir);
        let decision =
            decide_enroll("gpu-1", "uid-1", None, false, Some(&enrollment), Some(&node_store));
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: false,
                consume_enrollment_token: false,
            }
        );
    }

    #[test]
    fn decide_enroll_accepts_preissued_node_token_without_consuming() {
        // ADR-055 Phase 5a local-node path: the Gateway pre-issues a
        // long-lived node token (placeholder machine_uid) BEFORE the
        // node first connects; enroll must accept it as the credential
        // without consuming it as a one-time enrollment token.
        let dir = temp_test_dir("decide-preissued");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let mut node_store = NodeTokenStore::load(&dir);
        let preissued = node_store.upsert("local", "");

        let decision = decide_enroll(
            "local",
            "real-uid",
            Some(&preissued),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: true,
                consume_enrollment_token: false,
            }
        );
    }

    #[test]
    fn decide_enroll_requires_token_when_auth_enabled() {
        let dir = temp_test_dir("decide-token-required");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let node_store = NodeTokenStore::load(&dir);
        let decision =
            decide_enroll("gpu-1", "uid-1", None, true, Some(&enrollment), Some(&node_store));
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("required")
        ));
    }

    #[test]
    fn decide_enroll_valid_unknown_consumed_expired_tokens() {
        let dir = temp_test_dir("decide-token-states");
        let mut enrollment = EnrollmentTokenStore::load(&dir);
        let node_store = NodeTokenStore::load(&dir);

        // Valid token → accept.
        let valid = enrollment.create_token(std::time::Duration::from_secs(3600));
        let decision = decide_enroll(
            "gpu-1",
            "uid-1",
            Some(&valid),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: false,
                consume_enrollment_token: true,
            }
        );

        // Unknown token → reject.
        let decision = decide_enroll(
            "gpu-1",
            "uid-1",
            Some("not-a-real-token"),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("unknown")
        ));

        // Consumed token → reject.
        assert!(enrollment.consume_token(&valid, "gpu-1"));
        let decision = decide_enroll(
            "gpu-1",
            "uid-1",
            Some(&valid),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("already used")
        ));

        // Expired token (ttl = 0 → expires_at == now) → reject.
        let expired = enrollment.create_token(std::time::Duration::ZERO);
        let decision = decide_enroll(
            "gpu-1",
            "uid-1",
            Some(&expired),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("expired")
        ));
    }

    #[test]
    fn decide_enroll_reuses_token_for_same_machine_and_rejects_other() {
        let dir = temp_test_dir("decide-uid");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let mut node_store = NodeTokenStore::load(&dir);
        node_store.upsert("gpu-1", "uid-1");

        // Same machine_uid re-enrollment → reuse (idempotent).
        let decision = decide_enroll(
            "gpu-1",
            "uid-1",
            None,
            false,
            Some(&enrollment),
            Some(&node_store),
        );
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: true,
                consume_enrollment_token: false,
            }
        );

        // Different machine_uid claiming the same node_id → reject.
        let decision = decide_enroll(
            "gpu-1",
            "uid-2",
            None,
            false,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("claimed")
        ));
    }

    #[tokio::test]
    async fn enroll_handshake_end_to_end() {
        // Real broker + auth: enroll → node token minted → registry
        // record → enroll_result reply observed by the node client.
        let dir = temp_test_dir("e2e");
        let enrollment_store = crate::mqtt::new_shared_enrollment_store(&dir);
        let node_store = crate::mqtt::new_shared_node_token_store(&dir);
        let token = enrollment_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .create_token(std::time::Duration::from_secs(3600));

        let port = 18977; // distinct from the broker smoke-test port
        let host = "127.0.0.1";
        let auth = crate::mqtt::broker::BrokerAuth {
            auth_enabled: true,
            enrollment_tokens: enrollment_store.clone(),
            node_tokens: node_store.clone(),
            publisher_token: "publisher-tok".to_string(),
            http_token: None,
        };
        let _handle = crate::mqtt::start_broker_with_auth(host, port, Some(auth))
            .expect("broker should start");

        // Gateway-side client: subscribes the persistent topics and
        // routes messages through handle_plaintext_message. The client
        // is created AFTER the callback (same set-after-connect pattern
        // as gateway/mod.rs), so the callback borrows it via a slot.
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();
        let reg_for_cb = node_reg.clone();
        let state_for_cb = state.clone();
        let enroll_store_for_cb = enrollment_store.clone();
        let node_store_for_cb = node_store.clone();
        let dispatch_slot: std::sync::Arc<
            tokio::sync::Mutex<Option<Arc<crate::mqtt::GatewayMqttClient>>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let slot_for_cb = dispatch_slot.clone();
        let callback: crate::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
            let slot = slot_for_cb.clone();
            let reg = reg_for_cb.clone();
            let state = state_for_cb.clone();
            let es = enroll_store_for_cb.clone();
            let ns = node_store_for_cb.clone();
            tokio::spawn(async move {
                let client = slot.lock().await.clone();
                crate::mqtt::dispatch::handle_plaintext_message(
                    &topic,
                    &payload,
                    &crate::http::proxy::new_shared_registry(),
                    &crate::mqtt::agent_registry::new_shared_registry(),
                    &reg,
                    client.as_ref(),
                    &state,
                    None,
                    Some(&es),
                    Some(&ns),
                    true,
                );
            });
        });
        let gw_client = crate::mqtt::GatewayMqttClient::new_publisher_with_callback_and_credentials(
            host,
            port,
            callback,
            "gateway:publisher",
            "publisher-tok",
        )
        .await
        .expect("gateway client should connect");
        *dispatch_slot.lock().await = Some(Arc::new(gw_client.clone()));
        let _gw_client = gw_client;
        // Give the ConnAck subscription round a moment to settle.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Node-side client: node:{id} username + enrollment token as
        // the CONNECT password; subscribes the result topic.
        use rumqttc::{AsyncClient, Incoming, MqttOptions, QoS};
        let mut mqttoptions = MqttOptions::new("node:gpu-1", host, port);
        mqttoptions.set_keep_alive(std::time::Duration::from_secs(5));
        mqttoptions.set_credentials("node:gpu-1".to_string(), token.clone());
        let (node_client, mut node_eventloop) = AsyncClient::new(mqttoptions, 10);
        let result_topic = acowork_core::node::node_enroll_result_topic("gpu-1");
        node_client
            .subscribe(&result_topic, QoS::AtLeastOnce)
            .await
            .expect("subscribe result topic");

        // Poll the node event loop in the background (this also flushes
        // the subscribe + pending publishes).
        let enroll_topic = acowork_core::node::node_enroll_topic("gpu-1");
        let enroll_msg = enroll_payload("gpu-1", "uid-1", Some(&token));
        let node_poll = tokio::spawn(async move {
            let mut saw_result = false;
            for _ in 0..100 {
                match node_eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(Incoming::Publish(p))) => {
                        if p.topic == result_topic {
                            saw_result = DataEnvelope::decode(p.payload.as_ref())
                                .ok()
                                .and_then(|e| e.payload)
                                .is_some_and(|p| {
                                    matches!(p, data_envelope::Payload::NodeEnrollResult(_))
                                });
                            if saw_result {
                                break;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            saw_result
        });
        // Wait until the node connection is established before publishing.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        node_client
            .publish(&enroll_topic, QoS::AtLeastOnce, false, enroll_msg.as_slice())
            .await
            .expect("publish enroll");

        let saw_result = tokio::time::timeout(std::time::Duration::from_secs(10), node_poll)
            .await
            .expect("enroll reply must arrive")
            .expect("node poll task must not fail");
        assert!(saw_result, "node client should observe the enroll_result reply");

        // Side effects: node token minted + persisted, registry record,
        // one-time enrollment token consumed.
        let node_token = node_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_token("gpu-1")
            .expect("node token minted")
            .to_string();
        assert_eq!(node_token.len(), 64);
        assert_eq!(
            node_reg
                .read()
                .await
                .get("gpu-1")
                .and_then(|n| n.node_token.clone())
                .as_deref(),
            Some(node_token.as_str())
        );
        assert_eq!(
            enrollment_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .validate_token(&token),
            TokenValidation::Consumed
        );

        // Re-enrollment with the same machine_uid reuses the token and
        // still completes (idempotent), even without a fresh token. A
        // fresh short-lived client carries the re-enroll publish (the
        // first node client's eventloop task has already finished).
        let reused = enrollment_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .create_token(std::time::Duration::from_secs(3600));
        let mut re_mqttoptions = MqttOptions::new("node:gpu-1", host, port);
        re_mqttoptions.set_keep_alive(std::time::Duration::from_secs(5));
        // Reconnect with the minted node token — the realistic
        // re-enroll path after a Gateway restart.
        re_mqttoptions.set_credentials("node:gpu-1".to_string(), node_token.clone());
        let (re_client, mut re_eventloop) = AsyncClient::new(re_mqttoptions, 10);
        let second_payload = enroll_payload("gpu-1", "uid-1", Some(&reused));
        re_client
            .publish(&enroll_topic, QoS::AtLeastOnce, false, second_payload.as_slice())
            .await
            .expect("publish re-enroll");
        tokio::spawn(async move {
            for _ in 0..20 {
                if re_eventloop.poll().await.is_err() {
                    break;
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            node_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_token("gpu-1")
                .expect("token preserved on re-enroll"),
            node_token.as_str(),
            "same machine_uid re-enroll must reuse the node token"
        );
    }

    #[test]
    fn test_topic_matches_exact() {
        assert!(topic_matches("acowork/global/providers", "acowork/global/providers"));
        assert!(!topic_matches("acowork/global/providers", "acowork/global/mcps"));
    }

    #[test]
    fn test_topic_matches_single_wildcard() {
        assert!(topic_matches("acowork/agents/+/status", "acowork/agents/com.example/status"));
        assert!(topic_matches("acowork/agents/+/status", "acowork/agents/foo/status"));
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/meta"));
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/sessions/s1/config"));
    }

    #[test]
    fn test_topic_matches_multi_wildcard() {
        assert!(topic_matches("acowork/agents/+/sessions/+/messages/#", "acowork/agents/foo/sessions/s1/messages/chunk"));
        assert!(topic_matches("acowork/global/#", "acowork/global/providers"));
        assert!(topic_matches("acowork/global/#", "acowork/global/anything/deep"));
        assert!(!topic_matches("acowork/global/#", "acowork/agents/foo/status"));
    }

    #[test]
    fn test_topic_matches_edge_cases() {
        // # must be last
        assert!(topic_matches("#", "anything/at/all"));
        // + matches exactly one level
        assert!(!topic_matches("a/+", "a/b/c"));
        assert!(topic_matches("a/+/c", "a/b/c"));
    }

    #[test]
    fn extract_agent_id_from_status_topic_ok() {
        assert_eq!(
            extract_agent_id_from_status_topic("acowork/agents/com.example/status"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_agent_id_from_status_topic_rejects_extra_segments() {
        // The permissive `extract_agent_id_from_topic` would accept
        // this; the status variant must NOT (re-publish logic relies
        // on the exact 4-segment shape).
        assert_eq!(
            extract_agent_id_from_status_topic(
                "acowork/agents/com.example/sessions/s-1/messages/chunk",
            ),
            None,
        );
    }

    #[test]
    fn extract_agent_id_from_status_topic_rejects_wrong_topic() {
        assert_eq!(extract_agent_id_from_status_topic("acowork/global/providers"), None);
        assert_eq!(extract_agent_id_from_status_topic("acowork/agents//status"), None);
    }
}