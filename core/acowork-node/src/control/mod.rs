//! MQTT node control plane (ADR-055 §6.2).
//!
//! Topic family (see `acowork_core::node` for the builders):
//!
//! ```text
//! acowork/nodes/{id}/status                          plain "online"/"offline"
//!                                                    Retained + LWT (QoS 1)
//! acowork/nodes/{id}/info                            NodeInfo envelope Retained
//! acowork/nodes/{id}/events                          node-level NodeEvent
//! acowork/nodes/{id}/control/{cmd}                   node-level commands (ping)
//! acowork/nodes/{id}/agents/{aid}/control/{cmd}      agent lifecycle commands
//! acowork/nodes/{id}/agents/{aid}/events             per-agent NodeEvent
//! ```
//!
//! Ownership model (ADR-033 §5): the Gateway OWNS and publishes
//! commands; the Node OWNS and publishes execution results. Every
//! command carries a `request_id` echoed in the reply; QoS 1
//! duplicates are filtered by [`dedup::RequestDedup`].

pub mod dedup;
pub mod mqtt;

use std::sync::Arc;

use rumqttc::{AsyncClient, QoS};
use tokio::sync::{Mutex, RwLock};

use prost::Message as _;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEnroll, NodeEvent,
    NodeInfo,
};
use acowork_core::node::{
    node_agent_events_topic, node_agent_installed_topic, node_enroll_topic, node_events_topic,
    node_info_topic, node_lsps_topic, node_sidecar_status_topic, node_status_topic,
    NODE_PROTOCOL_VERSION,
};

use crate::config::{system_hostname, NodeConfig};
use crate::error::NodeError;
use crate::identity::{EnrollmentState, NodeIdentity};
use crate::process::spawn::kill_agent_process;
use crate::process::ProcessManager;
use crate::state::{NodeState, SharedNodeState};

use dedup::RequestDedup;
use mqtt::{NodeMqttClient, NodeMqttMessageCallback, SharedNodeMqttCredentials};

/// How often the daemon re-publishes the retained `info` heartbeat
/// (§6.15 — keeps `agent_count` / liveness fresh and lets the
/// Gateway detect "online but stuck" via timestamp).
const INFO_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The node control plane daemon.
pub struct NodeControlPlane {
    config: NodeConfig,
    identity: NodeIdentity,
    state: SharedNodeState,
    client: NodeMqttClient,
}

/// Build the NodeInfo snapshot published on the retained info topic.
pub fn build_node_info(identity: &NodeIdentity, config: &NodeConfig, agent_count: u32) -> NodeInfo {
    NodeInfo {
        node_id: identity.node_id.clone(),
        machine_uid: identity.machine_uid.clone(),
        hostname: system_hostname(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        node_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: NODE_PROTOCOL_VERSION,
        // Grows over the ADR-055 phases. Phase 2a: control plane only.
        capabilities: vec!["control_plane".to_string()],
        max_agents: config.max_agents,
        agent_count,
        // ADR-055 §6.3 / L7-1: the reverse-proxy base URL the Gateway
        // uses to reach node-local HTTP services (fs_browse).
        http_endpoint: config.proxy_advertise_endpoint(),
    }
}

/// Decode an incoming envelope and dispatch a command to its handler.
///
/// Idempotent semantics per ADR-055 §6.2:
/// - `ping` → always succeeds (stateless);
/// - `start`/`stop` → already-running / already-exited return success;
/// - `install`/`uninstall`/`skills_import` → local package operations;
/// - `avatar_update` → Phase 2c/3, answers `not_implemented`.
async fn handle_command(
    state: &SharedNodeState,
    config: &NodeConfig,
    command: &NodeControlCommand,
    // ADR-055 Phase 5a: the node's long-lived token, attached to
    // package downloads as `X-ACowork-Node-Token`. `None` when not
    // enrolled yet (downloads then work only against brokers with
    // auth disabled).
    node_token: Option<&str>,
) -> NodeEvent {
    use node_control_command::Command;

    let reply = |status: &str, message: String| NodeEvent {
        node_id: command.node_id.clone(),
        request_id: command.request_id.clone(),
        status: status.to_string(),
        message,
        result_json: None,
    };
    // Structured-result reply (publish prepare/build carry a JSON
    // payload in `result_json` for the Gateway to re-parse).
    let reply_json = |status: &str, message: String, result_json: String| NodeEvent {
        node_id: command.node_id.clone(),
        request_id: command.request_id.clone(),
        status: status.to_string(),
        message,
        result_json: Some(result_json),
    };

    match command.command.as_ref() {
        Some(Command::Ping(_)) => reply("ok", "pong".to_string()),
        Some(Command::Start(cmd)) => {
            let mut mgr = ProcessManager::new(
                config.log_file_size_mb,
                config.log_file_count,
                Some(config.gateway_mqtt_port),
                config.gateway_host.clone(),
                command.node_id.clone(),
                Some(config.proxy_advertise_endpoint()),
                node_token.map(str::to_string),
            );
            match mgr.start_agent(&cmd.agent_id, state, cmd.dev_mode, true).await {
                Ok(()) => {
                    state.read().await.save_snapshot(&config.home);
                    reply("ok", format!("started '{}'", cmd.agent_id))
                }
                Err(NodeError::AgentAlreadyRunning(id)) => {
                    // Idempotent: start on a running agent succeeds.
                    reply("ok", format!("'{}' already running", id))
                }
                Err(e) => reply("error", format!("start '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::Stop(cmd)) => {
            let mut mgr = ProcessManager::new(
                config.log_file_size_mb,
                config.log_file_count,
                Some(config.gateway_mqtt_port),
                config.gateway_host.clone(),
                command.node_id.clone(),
                Some(config.proxy_advertise_endpoint()),
                node_token.map(str::to_string),
            );
            match mgr.stop_agent(&cmd.agent_id, state).await {
                Ok(()) => {
                    state.read().await.save_snapshot(&config.home);
                    reply("ok", format!("stopped '{}'", cmd.agent_id))
                }
                Err(NodeError::AgentNotRunning(id)) => {
                    // Idempotent: stop on an exited agent succeeds.
                    reply("ok", format!("'{}' already stopped", id))
                }
                Err(e) => reply("error", format!("stop '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::Uninstall(cmd)) => {
            let install_dir = config.packages_dir();
            let mut node = state.write().await;
            match crate::package::uninstall::uninstall_package(&cmd.agent_id, &install_dir, &mut node)
            {
                Ok(()) => {
                    // Clear the retained inventory entry (ADR-055 §6.5) so
                    // the Gateway drops the agent from installed_agents.
                    let installed_topic =
                        node_agent_installed_topic(&command.node_id, &cmd.agent_id);
                    let _ = dispatcher::clear_installed_info(installed_topic).await;
                    reply("ok", format!("uninstalled '{}'", cmd.agent_id))
                }
                Err(e) => reply("error", format!("uninstall '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::SkillsImport(cmd)) => {
            let skills_dir = {
                let node = state.read().await;
                match node.installed_agents.get(&cmd.agent_id) {
                    Some(info) => crate::package::skills::agent_skills_dir(&info.install_path),
                    None => {
                        return reply(
                            "error",
                            format!("skills_import '{}': agent not installed", cmd.agent_id),
                        );
                    }
                }
            };
            match crate::package::skills::install_skill_package(
                std::path::Path::new(&cmd.zip_path),
                &skills_dir,
            ) {
                Ok(name) => reply("ok", format!("skill '{}' imported", name)),
                Err(e) => {
                    reply("error", format!("skills_import '{}' failed: {}", cmd.agent_id, e))
                }
            }
        }
        Some(Command::Install(cmd)) => {
            // ADR-055 §3.2: install from a Gateway-hosted download URL
            // (asynchronous install) or a node-local spooled path (Phase
            // 2b single-machine). Resolve the source, then converge on
            // the shared `install_package` path.
            let install_dir = config.packages_dir();
            let mut spooled: Option<std::path::PathBuf> = None;
            let source_path: std::path::PathBuf = if cmd.local_path.is_empty() {
                if cmd.package_url.is_empty() {
                    return reply(
                        "error",
                        format!(
                            "install '{}': neither package_url nor local_path provided",
                            cmd.agent_id
                        ),
                    );
                }
                let tmp = std::env::temp_dir().join(format!(
                    "acowork-node-install-{}-{}.agent",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                ));
                match download_package(&cmd.package_url, &tmp, node_token).await {
                    Ok(()) => {
                        spooled = Some(tmp.clone());
                        tmp
                    }
                    Err(e) => {
                        return reply(
                            "error",
                            format!("install '{}' download failed: {}", cmd.agent_id, e),
                        );
                    }
                }
            } else {
                std::path::PathBuf::from(&cmd.local_path)
            };

            let mut node = state.write().await;
            // Signature strictness follows the Gateway's dev_mode
            // (ADR-055 §6.20): `true` allows unsigned packages.
            let result = crate::package::install::install_package(
                &source_path,
                &install_dir,
                &mut node,
                cmd.dev_mode,
            );

            if let Some(tmp) = spooled {
                let _ = std::fs::remove_file(&tmp);
            }

            match result {
                Ok(info) => {
                    // Publish retained inventory (ADR-055 §6.5) — the
                    // Gateway aggregates this into installed_agents.
                    if let Some(entry) = crate::package::build_installed_info(&info) {
                        let installed_topic =
                            node_agent_installed_topic(&command.node_id, &info.agent_id);
                        let _ = dispatcher::publish_installed_info(installed_topic, entry).await;
                    }
                    reply("ok", format!("installed '{}' v{}", info.agent_id, info.version))
                }
                Err(e) => reply("error", format!("install '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::AvatarUpdate(cmd)) => reply(
            "not_implemented",
            format!("avatar_update '{}' not implemented until ADR-055 Phase 2c", cmd.agent_id),
        ),
        Some(Command::Clone(cmd)) => {
            // Node-local clone (ADR-055 §6.6 L2-5): source and new agent
            // live on the same node, so the package directory is copied
            // directly (cross-machine memory export/import is a separate
            // §6.18 concern).
            let mode = match cmd.mode.as_str() {
                "full" => crate::package::clone::CloneMode::Full,
                _ => crate::package::clone::CloneMode::Skeleton,
            };
            let install_dir = config.packages_dir();
            let mut node = state.write().await;
            match crate::package::clone::clone_agent(
                &cmd.agent_id,
                &cmd.new_agent_id,
                mode,
                &install_dir,
                &mut node,
            ) {
                Ok(info) => {
                    // Publish retained inventory so the Gateway aggregates
                    // the new agent into installed_agents (and registers
                    // cron via the install-completed is_new hook).
                    if let Some(entry) = crate::package::build_installed_info(&info) {
                        let installed_topic =
                            node_agent_installed_topic(&command.node_id, &info.agent_id);
                        let _ = dispatcher::publish_installed_info(installed_topic, entry).await;
                    }
                    let json = serde_json::json!({ "install_path": info.install_path }).to_string();
                    reply_json(
                        "ok",
                        format!("cloned '{}' -> '{}'", cmd.agent_id, cmd.new_agent_id),
                        json,
                    )
                }
                Err(e) => reply("error", format!("clone '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::Upgrade(cmd)) => {
            // ADR-055 §3.2/§6.6: upgrade from a Gateway-hosted download
            // URL or a node-local spooled path, then converge on
            // `upgrade_package`.
            let install_dir = config.packages_dir();
            let mut spooled: Option<std::path::PathBuf> = None;
            let source_path: std::path::PathBuf = if cmd.local_path.is_empty() {
                if cmd.package_url.is_empty() {
                    return reply(
                        "error",
                        format!(
                            "upgrade '{}': neither package_url nor local_path provided",
                            cmd.agent_id
                        ),
                    );
                }
                let tmp = std::env::temp_dir().join(format!(
                    "acowork-node-upgrade-{}-{}.agent",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                ));
                match download_package(&cmd.package_url, &tmp, node_token).await {
                    Ok(()) => {
                        spooled = Some(tmp.clone());
                        tmp
                    }
                    Err(e) => {
                        return reply(
                            "error",
                            format!("upgrade '{}' download failed: {}", cmd.agent_id, e),
                        );
                    }
                }
            } else {
                std::path::PathBuf::from(&cmd.local_path)
            };

            let mut node = state.write().await;
            let result = crate::package::upgrade::upgrade_package(
                &cmd.agent_id,
                &source_path,
                &install_dir,
                &mut node,
                cmd.dev_mode,
            );

            if let Some(tmp) = spooled {
                let _ = std::fs::remove_file(&tmp);
            }

            match result {
                Ok(()) => {
                    // Republish retained inventory with the new version so
                    // the Gateway refreshes its installed_agents entry.
                    if let Some(info) = node.installed_agents.get(&cmd.agent_id).cloned()
                        && let Some(entry) = crate::package::build_installed_info(&info)
                    {
                        let installed_topic =
                            node_agent_installed_topic(&command.node_id, &info.agent_id);
                        let _ = dispatcher::publish_installed_info(installed_topic, entry).await;
                    }
                    reply("ok", format!("upgraded '{}'", cmd.agent_id))
                }
                Err(e) => reply("error", format!("upgrade '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::PublishPrepare(cmd)) => {
            let mut node = state.write().await;
            match crate::package::publish::prepare_publish(&cmd.agent_id, cmd.clean, &mut node) {
                Ok(result) => {
                    let json = serde_json::to_string(&result).unwrap_or_else(|e| {
                        format!(r#"{{"error":"{}"}}"#, e)
                    });
                    reply_json("ok", format!("publish prepare '{}' complete", cmd.agent_id), json)
                }
                Err(e) => {
                    reply("error", format!("publish prepare '{}' failed: {}", cmd.agent_id, e))
                }
            }
        }
        Some(Command::PublishBuild(cmd)) => {
            let output_dir = if cmd.output_dir.is_empty() {
                config.packages_dir()
            } else {
                std::path::PathBuf::from(&cmd.output_dir)
            };
            let key_dir = if cmd.key_dir.is_empty() {
                None
            } else {
                Some(std::path::Path::new(&cmd.key_dir))
            };
            let node = state.read().await;
            match crate::package::publish::build_package(
                &cmd.agent_id,
                &output_dir,
                cmd.sign,
                key_dir,
                &node,
            ) {
                Ok(result) => {
                    let json = serde_json::to_string(&result).unwrap_or_default();
                    reply_json("ok", format!("built '{}'", result.output_path), json)
                }
                Err(e) => reply("error", format!("publish build '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        None => reply("error", "empty command payload".to_string()),
    }
}

/// Download a package from the Gateway registry URL to a local temp
/// path (ADR-055 §3.2). The caller owns cleanup of `dest`.
///
/// ADR-055 Phase 5a: when the node holds a node_token it is attached
/// as `X-ACowork-Node-Token` — the Gateway's package channel is
/// gated on that header when `mqtt.auth_enabled` is on.
async fn download_package(
    url: &str,
    dest: &std::path::Path,
    node_token: Option<&str>,
) -> Result<(), NodeError> {
    let mut request = reqwest::Client::new().get(url);
    if let Some(token) = node_token {
        request = request.header("X-ACowork-Node-Token", token);
    }
    let bytes = request
        .send()
        .await
        .map_err(|e| NodeError::Mqtt(format!("download GET failed: {e}")))?
        .error_for_status()
        .map_err(|e| NodeError::Package(format!("download HTTP error: {e}")))?
        .bytes()
        .await
        .map_err(|e| NodeError::Package(format!("download read failed: {e}")))?;
    std::fs::write(dest, &bytes)
        .map_err(|e| NodeError::Package(format!("download write failed: {e}")))?;
    Ok(())
}

/// ADR-055 Phase 5a: build the NodeEnroll request envelope, or `None`
/// when no enrollment is needed — the node already holds a
/// Gateway-issued node_token (enrollment is one-shot; the Gateway
/// reuses the token on reconnect) or no enrollment token was provided.
fn build_enroll_payload(identity: &NodeIdentity, config: &NodeConfig) -> Option<DataEnvelope> {
    if identity.node_token.is_some() {
        return None; // Already enrolled.
    }
    let Some(token) = config.token.as_deref().filter(|t| !t.is_empty()) else {
        return None; // No credential to present.
    };
    let info = build_node_info(identity, config, 0);
    let enroll = NodeEnroll {
        node_id: identity.node_id.clone(),
        machine_uid: identity.machine_uid.clone(),
        os: info.os,
        arch: info.arch,
        node_version: info.node_version,
        protocol_version: info.protocol_version,
        capabilities: info.capabilities,
        enrollment_token: token.to_string(),
    };
    Some(DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeEnroll(enroll)),
    })
}

/// Publish the enrollment request on `acowork/nodes/{id}/enroll`
/// (QoS 1, non-retained). Returns true when a request was actually
/// published. Re-run on every (re)connect by the bootstrap; it is a
/// no-op once the node holds a node_token.
async fn publish_enroll(
    client: &AsyncClient,
    identity: &NodeIdentity,
    config: &NodeConfig,
) -> bool {
    let Some(envelope) = build_enroll_payload(identity, config) else {
        return false;
    };
    let topic = node_enroll_topic(&identity.node_id);
    match client
        .publish(
            topic.clone(),
            QoS::AtLeastOnce,
            false,
            prost::Message::encode_to_vec(&envelope),
        )
        .await
    {
        Ok(()) => {
            tracing::info!(topic = %topic, "Node enrollment request published");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, topic = %topic, "Failed to publish enrollment request");
            false
        }
    }
}

/// Handle an `acowork/nodes/{id}/enroll_result` reply: persist the
/// Gateway-issued node_token into identity.json. Returns true when
/// the topic was an enroll_result (consumed).
///
/// Idempotent (ADR-055 §6.12): the token is only written when the
/// identity has none yet — a re-enroll after a Gateway restart reuses
/// the same token, and a node that already holds a token must never
/// have it overwritten or cleared by a stale/error reply.
async fn handle_enroll_result(
    topic: &str,
    payload: &[u8],
    identity: &Arc<RwLock<NodeIdentity>>,
    home: &std::path::Path,
) -> bool {
    let Some(node_id) = topic
        .strip_prefix("acowork/nodes/")
        .and_then(|rest| rest.strip_suffix("/enroll_result"))
    else {
        return false;
    };
    let Ok(envelope) = DataEnvelope::decode(payload) else {
        tracing::warn!(topic, "Undecodable enroll_result payload");
        return true;
    };
    let Some(data_envelope::Payload::NodeEnrollResult(result)) = envelope.payload else {
        return true;
    };
    if result.node_id != node_id {
        tracing::warn!(
            topic,
            result_node = %result.node_id,
            "enroll_result node_id does not match topic"
        );
        return true;
    }
    if result.status != "ok" || result.node_token.is_empty() {
        tracing::warn!(
            topic,
            status = %result.status,
            message = %result.message,
            "Enrollment rejected by Gateway — no node token issued"
        );
        return true;
    }
    let mut id = identity.write().await;
    if id.node_token.is_some() {
        return true; // Already persisted — never overwrite.
    }
    id.set_node_token(Some(result.node_token.clone()));
    match id.save(home) {
        Ok(()) => tracing::info!(
            node_id = %id.node_id,
            "Node token persisted to identity.json"
        ),
        Err(e) => tracing::warn!(error = %e, "Failed to persist node_token to identity.json"),
    }
    true
}

/// Extract (node_id, Some(agent_id)) from an agent-level control topic
/// or (node_id, None) from a node-level control topic. Returns `None`
/// for topics outside the control family.
fn parse_control_topic(topic: &str, own_node_id: &str) -> Option<(String, Option<String>)> {
    let agent_prefix = format!("acowork/nodes/{own_node_id}/agents/");
    let node_prefix = format!("acowork/nodes/{own_node_id}/control/");
    if let Some(rest) = topic.strip_prefix(&agent_prefix) {
        // rest = {agent_id}/control/{cmd}
        let mut parts = rest.splitn(3, '/');
        let agent_id = parts.next().unwrap_or("");
        let control = parts.next().unwrap_or("");
        if !agent_id.is_empty() && control == "control" {
            return Some((own_node_id.to_string(), Some(agent_id.to_string())));
        }
        return None;
    }
    if topic.strip_prefix(&node_prefix).is_some() {
        return Some((own_node_id.to_string(), None));
    }
    None
}

impl NodeControlPlane {
    /// Run the daemon: connect, bootstrap on every (re)connect,
    /// answer control commands, republish the info heartbeat, and
    /// shut down cleanly on Ctrl-C.
    pub async fn run(config: NodeConfig) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let gateway_addr = config.gateway_addr();
        let identity =
            NodeIdentity::load_or_create(&config.home, config.name.as_deref(), Some(&gateway_addr))?;
        let node_id = identity.node_id.clone();
        tracing::info!(
            node_id = %node_id,
            gateway = %gateway_addr,
            "Node Agent starting"
        );

        let node_state = {
            let mut s = crate::state::NodeState::new(config.max_agents);
            // Rebuild the local install table from the packages dir so a
            // restart re-discovers installed agents (ADR-055 §6.5).
            crate::package::restore_installed_agents(&mut s, &config.packages_dir());
            s
        };
        let state: SharedNodeState = Arc::new(RwLock::new(node_state));
        let dedup: Arc<Mutex<RequestDedup>> = Arc::new(Mutex::new(RequestDedup::default()));

        // ADR-055 §6.19: re-adopt orphan Runtimes left running after a
        // Node restart (a Node crash does NOT kill Runtimes, §6.10).
        // Runs AFTER the install table is restored (the adoption gate)
        // and BEFORE the reverse proxy starts, so `/agents/{id}/*`
        // routes immediately; it is also before the control-topic
        // subscription (which happens on ConnAck), so no start/stop
        // command races the reconciliation window (§6.19 point 5).
        let readopted = {
            let mgr = ProcessManager::new(
                config.log_file_size_mb,
                config.log_file_count,
                Some(config.gateway_mqtt_port),
                config.gateway_host.clone(),
                node_id.clone(),
                Some(config.proxy_advertise_endpoint()),
                identity.node_token.clone(),
            );
            mgr.readopt_orphans(&state).await
        };
        let readopted: Arc<Mutex<Option<Vec<String>>>> =
            Arc::new(Mutex::new(Some(readopted)));

        // Bootstrap (re-run on every ConnAck — clean_session means the
        // broker dropped our subscriptions on disconnect): publish
        // retained status=online + info, re-subscribe control filters,
        // mark identity enrolled, refresh the snapshot file.
        let bs_node_id = node_id.clone();
        let bs_config = config.clone();
        let bs_state = state.clone();
        let bs_identity = Arc::new(RwLock::new(identity.clone()));
        let bs_home = config.home.clone();
        let bs_gateway_addr = gateway_addr.clone();
        let bs_readopted = readopted.clone();
        // Cloned BEFORE the bootstrap closure moves it in — the
        // message callback (created after connect) also needs it.
        let dispatch_identity = bs_identity.clone();
        // The node HTTP server (proxy auth, Phase 5a) also reads the
        // live identity — cloned before message_callback moves it.
        let http_identity = bs_identity.clone();

        // ADR-055 Phase 5a: live CONNECT credential — starts as the
        // node_token (reconnect) or the enrollment token (first boot)
        // and is swapped to the node_token when the enroll_result
        // reply arrives (a consumed enrollment token would be rejected
        // on reconnect).
        let mqtt_credentials: SharedNodeMqttCredentials = Arc::new(Mutex::new(
            identity
                .node_token
                .clone()
                .or_else(|| config.token.clone()),
        ));
        let bs_credentials = mqtt_credentials.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let node_id = bs_node_id.clone();
            let config = bs_config.clone();
            let state = bs_state.clone();
            let identity = bs_identity.clone();
            let home = bs_home.clone();
            let gateway_addr = bs_gateway_addr.clone();
            let readopted = bs_readopted.clone();
            tokio::spawn(async move {
                let status_topic = node_status_topic(&node_id);
                let info_topic = node_info_topic(&node_id);

                if let Err(e) = client
                    .publish(
                        status_topic.clone(),
                        QoS::AtLeastOnce,
                        true,
                        "online".as_bytes(),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to publish node status=online");
                }

                let agent_count = state.read().await.agents.len() as u32;
                let info = build_node_info(&identity.read().await.clone(), &config, agent_count);
                let envelope = DataEnvelope {
                    version: 1,
                    payload: Some(data_envelope::Payload::NodeInfo(info)),
                };
                if let Err(e) = client
                    .publish(
                        info_topic,
                        QoS::AtLeastOnce,
                        true,
                        prost::Message::encode_to_vec(&envelope),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to publish node info");
                }

                // ADR-055 §6.5: publish the retained installed-package
                // inventory so the Gateway can rebuild its installed_agents
                // view without scanning the packages dir (L2-9). Re-run on
                // every (re)connect because clean_session drops the broker's
                // retained set on a fresh broker.
                let installed: Vec<_> = state
                    .read()
                    .await
                    .installed_agents
                    .values()
                    .cloned()
                    .collect();
                for entry in installed {
                    let Some(info) = crate::package::build_installed_info(&entry) else {
                        continue;
                    };
                    let installed_topic = node_agent_installed_topic(&node_id, &info.agent_id);
                    let envelope = DataEnvelope {
                        version: 1,
                        payload: Some(data_envelope::Payload::InstalledAgentInfo(info)),
                    };
                    if let Err(e) = client
                        .publish(
                            installed_topic,
                            QoS::AtLeastOnce,
                            true,
                            prost::Message::encode_to_vec(&envelope),
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to publish installed-agent info");
                    }
                }

                // ADR-055 §6.7: re-assert the retained per-node LSP
                // relay endpoint on every (re)connect — clean_session
                // drops the broker's retained set on a fresh broker,
                // and the sidecar supervisor's transition publishes may
                // have happened before this ConnAck. Ready state comes
                // from the shared process state (None → unavailable).
                let lsp_ready = state
                    .read()
                    .await
                    .lsp_relay_process
                    .as_ref()
                    .map(|p| p.ready)
                    .unwrap_or(false);
                if let Err(e) = crate::sidecar::publish_lsps_state(
                    &client,
                    &node_id,
                    &config.advertise_host,
                    config.lsp_relay_port,
                    lsp_ready,
                )
                .await
                {
                    tracing::warn!(error = %e, "Failed to publish node LSP relay state");
                }

                // ADR-055 Phase 5a: request a long-lived node token
                // on first enrollment. One-shot — no-op once
                // identity.node_token is set (the Gateway reuses the
                // same token on reconnect).
                publish_enroll(&client, &identity.read().await.clone(), &config).await;

                // Control subscriptions: node-level + agent-level.
                for filter in [
                    format!("acowork/nodes/{node_id}/control/#"),
                    format!("acowork/nodes/{node_id}/agents/+/control/#"),
                    // ADR-055 Phase 5a: enrollment reply (own node).
                    format!("acowork/nodes/{node_id}/enroll_result"),
                ] {
                    if let Err(e) = client.subscribe(filter, QoS::AtLeastOnce).await {
                        tracing::warn!(error = %e, "Failed to subscribe node control filter");
                    }
                }

                // Mark enrolled (idempotent) + persist.
                {
                    let mut id = identity.write().await;
                    id.mark_enrolled(&gateway_addr);
                    let _ = id.save(&home);
                }
                {
                    let mut s = state.write().await;
                    s.set_connected(true, Some(gateway_addr));
                    s.save_snapshot(&home);
                }
                // ADR-055 §6.19: emit the `node_readopted` diagnostic
                // event once, if this boot re-adopted any orphans. The
                // Runtime's own MQTT reconnect + retained `http_endpoint`
                // replay already converges the Gateway (§6.19 point 6);
                // this event is for observability only.
                if let Ok(mut guard) = readopted.try_lock()
                    && let Some(ids) = guard.take()
                    && !ids.is_empty()
                {
                    let event = NodeEvent {
                        node_id: node_id.clone(),
                        request_id: "node_readopted".to_string(),
                        status: "ok".to_string(),
                        message: format!(
                            "re-adopted {} orphan Runtime(s): {}",
                            ids.len(),
                            ids.join(", ")
                        ),
                        result_json: None,
                    };
                    let envelope = DataEnvelope {
                        version: 1,
                        payload: Some(data_envelope::Payload::NodeEvent(event)),
                    };
                    let _ = client
                        .publish(
                            node_events_topic(&node_id),
                            QoS::AtLeastOnce,
                            false,
                            prost::Message::encode_to_vec(&envelope),
                        )
                        .await;
                }
                tracing::info!(node_id = %node_id, "Node bootstrap complete");
            });
        });

        // Incoming control command dispatch.
        let dispatch_node_id = node_id.clone();
        let dispatch_dedup = dedup.clone();
        let dispatch_state = state.clone();
        let dispatch_config = config.clone();
        let dispatch_home = config.home.clone();
        let dispatch_credentials = bs_credentials.clone();
        let message_callback: NodeMqttMessageCallback = Arc::new(move |topic, payload| {
            let node_id = dispatch_node_id.clone();
            let dedup = dispatch_dedup.clone();
            let state = dispatch_state.clone();
            let config = dispatch_config.clone();
            let identity = dispatch_identity.clone();
            let home = dispatch_home.clone();
            let credentials = dispatch_credentials.clone();
            tokio::spawn(async move {
                // ADR-055 Phase 5a: enrollment reply — persist the
                // Gateway-issued node_token into identity.json and
                // switch the live CONNECT credential so a reconnect
                // never re-presents the (now consumed) enrollment
                // token.
                if handle_enroll_result(&topic, &payload, &identity, &home).await {
                    if let Some(token) = identity.read().await.node_token.clone() {
                        *credentials.lock().await = Some(token);
                    }
                    return;
                }
                if let Err(e) = handle_incoming(
                    &node_id,
                    &topic,
                    &payload,
                    &dedup,
                    &state,
                    &config,
                    &identity,
                )
                .await
                {
                    tracing::warn!(topic = %topic, error = %e, "Failed to process node control message");
                }
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &node_id,
            mqtt_credentials,
            bootstrap,
            Some(message_callback),
        )
        .await?;

        // Route command replies through this connection.
        dispatcher::install(client.shared_handle());

        // ADR-055 §6.4: start the node reverse proxy (`:19900`) so the
        // Gateway reaches every local Runtime through one port. It is a
        // best-effort service — if the port is taken (e.g. another node
        // instance) the control plane still runs, only HTTP reverse
        // proxying is disabled.
        let proxy_state = state.clone();
        let proxy_bind = format!("{}:{}", config.proxy_bind, config.proxy_port);
        let proxy_node_id = node_id.clone();
        // Signals that the node HTTP server (hosting `/health`) is
        // bound — the LSP relay sidecar start waits for this so the
        // relay's parent-health watchdog has a live target from birth.
        let (health_tx, health_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            // ADR-055 §6.4 + L7-1: the node HTTP server hosts both the
            // `/agents/{id}/*` reverse proxy and the `/fs/browse` remote
            // filesystem browser on the same `:19900` listener.
            // ADR-055 Phase 5a §6.8: the node HTTP router carries the
            // live identity so the proxy can validate inbound
            // `X-ACowork-Node-Token` against the issued node_token.
            let node_http_state = crate::state::NodeHttpState {
                node: proxy_state.clone(),
                identity: http_identity,
            };
            let app = crate::proxy::router(node_http_state.clone())
                .merge(crate::fs_browse::router(node_http_state));
            let listener = match tokio::net::TcpListener::bind(&proxy_bind).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        addr = %proxy_bind,
                        error = %e,
                        "Node reverse proxy failed to bind — agent HTTP reverse-proxy disabled"
                    );
                    return;
                }
            };
            tracing::info!(
                addr = %proxy_bind,
                node_id = %proxy_node_id,
                "Node reverse proxy listening"
            );
            let _ = health_tx.send(());
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "Node reverse proxy terminated");
            }
        });

        // ADR-055 §6.7: start the node-local LSP relay sidecar once the
        // node HTTP server is up (its `/health` is the relay's
        // self-exit watchdog target). Best-effort — a failed bind
        // leaves the node running without codebase tooling.
        if health_rx.await.is_ok() {
            let sup_cfg = crate::sidecar::lsp_relay_supervisor::LspRelaySupervisorConfig {
                data_dir: config.home.clone(),
                port: config.lsp_relay_port,
                health_url: format!("http://127.0.0.1:{}/health", config.proxy_port),
                node_id: node_id.clone(),
                advertise_host: config.advertise_host.clone(),
            };
            crate::sidecar::lsp_relay_supervisor::start_lsp_relay_supervisor(sup_cfg, state.clone());
        }

        let plane = Self {
            config,
            identity,
            state,
            client,
        };

        plane.run_loop().await
    }

    /// Main loop: info heartbeat + graceful shutdown.
    async fn run_loop(mut self) -> Result<(), NodeError> {
        let mut heartbeat = tokio::time::interval(INFO_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let agent_count = self.state.read().await.agents.len() as u32;
                    let info = build_node_info(&self.identity, &self.config, agent_count);
                    let envelope = DataEnvelope {
                        version: 1,
                        payload: Some(data_envelope::Payload::NodeInfo(info)),
                    };
                    let topic = node_info_topic(&self.identity.node_id);
                    if let Err(e) = self
                        .client
                        .publish_envelope(&topic, &envelope, QoS::AtLeastOnce, true)
                        .await
                    {
                        tracing::warn!(error = %e, "Info heartbeat publish failed");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutdown signal received — going offline");
                    self.shutdown().await;
                    return Ok(());
                }
            }
        }
    }

    /// Graceful shutdown: terminate all hosted Runtime processes
    /// (ADR-055 §6.10 — the ADR-018 cleanup migrated into the Node),
    /// publish retained status=offline (the LWT covers the crash path),
    /// refresh the snapshot, persist identity.
    async fn shutdown(&mut self) {
        // Terminate every tracked Runtime before we go offline. SIGTERM
        // lets each Runtime perform its own graceful exit (idle-sleep
        // bookkeeping, LWT); the broker emits their retained "offline"
        // on disconnect.
        let pids: Vec<u32> = {
            self.state
                .read()
                .await
                .agents
                .values()
                .map(|s| s.pid)
                .collect()
        };
        for pid in pids {
            if let Err(e) = kill_agent_process(pid).await {
                tracing::warn!(
                    pid,
                    error = %e,
                    "Failed to terminate Runtime during node shutdown"
                );
            }
        }
        // Clear the process table so the persisted snapshot does not
        // record running agents that we just terminated.
        self.state.write().await.agents.clear();

        // ADR-055 §6.7: terminate the node-local LSP relay sidecar and
        // clear its retained state (the relay would otherwise linger
        // until its parent-health watchdog notices the node /health is
        // gone).
        let lsp_pid = self
            .state
            .read()
            .await
            .lsp_relay_process
            .as_ref()
            .map(|p| p.pid);
        if let Some(pid) = lsp_pid
            && pid != 0
        {
            let _ = crate::sidecar::lsp_relay::kill_lsp_relay(pid).await;
        }
        {
            let mut s = self.state.write().await;
            s.lsp_relay_process = None;
        }
        let _ = dispatcher::clear_lsps_state(&self.identity.node_id).await;

        let status_topic = node_status_topic(&self.identity.node_id);
        if let Err(e) = self
            .client
            .publish_text(&status_topic, "offline", QoS::AtLeastOnce, true)
            .await
        {
            tracing::warn!(error = %e, "Failed to publish status=offline on shutdown");
        }
        {
            let mut s = self.state.write().await;
            s.set_connected(false, None);
            s.save_snapshot(&self.config.home);
        }
        if self.identity.enrollment == EnrollmentState::Enrolled {
            let _ = self.identity.save(&self.config.home);
        }
    }

    /// Enroll-only mode (`acowork-node enroll`): publish the identity
    /// (retained info) once, then exit. The status topic is left at
    /// "offline" because no daemon is running — daemon liveness is
    /// owned by the LWT / explicit status publishes, not enrollment.
    ///
    /// ADR-055 Phase 5a: with `--token` this also performs the
    /// enrollment handshake — the Gateway replies on
    /// `enroll_result` with a long-lived node_token, which is
    /// persisted into identity.json before the command exits.
    pub async fn enroll(config: NodeConfig) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let gateway_addr = config.gateway_addr();
        let identity =
            NodeIdentity::load_or_create(&config.home, config.name.as_deref(), Some(&gateway_addr))?;
        let node_id = identity.node_id.clone();

        let state: SharedNodeState = Arc::new(RwLock::new(crate::state::NodeState::new(
            config.max_agents,
        )));
        let dedup: Arc<Mutex<RequestDedup>> = Arc::new(Mutex::new(RequestDedup::default()));

        // Minimal bootstrap for the one-shot enrollment: publish info
        // retained + status online, wait briefly for the QoS 1 flow,
        // then publish status offline retained and return.
        let bs_identity = Arc::new(RwLock::new(identity.clone()));
        let bs_config = config.clone();
        let bs_state = state.clone();
        let bs_home = config.home.clone();
        let bs_gateway_addr = gateway_addr.clone();
        // Cloned BEFORE the bootstrap closure moves it in — the
        // message callback (created after connect) also needs it.
        let dispatch_identity = bs_identity.clone();
        let mqtt_credentials: SharedNodeMqttCredentials = Arc::new(Mutex::new(
            identity
                .node_token
                .clone()
                .or_else(|| config.token.clone()),
        ));
        let bs_credentials = mqtt_credentials.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let identity = bs_identity.clone();
            let config = bs_config.clone();
            let state = bs_state.clone();
            let home = bs_home.clone();
            let gateway_addr = bs_gateway_addr.clone();
            tokio::spawn(async move {
                let id = identity.read().await.clone();
                let status_topic = node_status_topic(&id.node_id);
                let _ = client
                    .publish(&status_topic, QoS::AtLeastOnce, true, "online".as_bytes())
                    .await;
                let info = build_node_info(&id, &config, 0);
                let envelope = DataEnvelope {
                    version: 1,
                    payload: Some(data_envelope::Payload::NodeInfo(info)),
                };
                let _ = client
                    .publish(
                        node_info_topic(&id.node_id),
                        QoS::AtLeastOnce,
                        true,
                        prost::Message::encode_to_vec(&envelope),
                    )
                    .await;
                // ADR-055 Phase 5a: request the long-lived node token.
                publish_enroll(&client, &id, &config).await;
                {
                    let mut id = identity.write().await;
                    id.mark_enrolled(&gateway_addr);
                    let _ = id.save(&home);
                }
                let mut s = state.write().await;
                s.set_connected(true, Some(gateway_addr));
                s.save_snapshot(&home);
            });
        });

        let dispatch_node_id = node_id.clone();
        let dispatch_dedup = dedup.clone();
        let dispatch_state = state.clone();
        let dispatch_config = config.clone();
        let dispatch_home = config.home.clone();
        let dispatch_credentials = bs_credentials.clone();
        // Kept for the final persist below — message_callback moves
        // dispatch_identity into its closure.
        let final_identity = dispatch_identity.clone();
        let message_callback: NodeMqttMessageCallback = Arc::new(move |topic, payload| {
            let node_id = dispatch_node_id.clone();
            let dedup = dispatch_dedup.clone();
            let state = dispatch_state.clone();
            let config = dispatch_config.clone();
            let identity = dispatch_identity.clone();
            let home = dispatch_home.clone();
            let credentials = dispatch_credentials.clone();
            tokio::spawn(async move {
                // ADR-055 Phase 5a: persist the node_token and switch
                // the live CONNECT credential before the one-shot
                // exits.
                if handle_enroll_result(&topic, &payload, &identity, &home).await {
                    if let Some(token) = identity.read().await.node_token.clone() {
                        *credentials.lock().await = Some(token);
                    }
                    return;
                }
                let _ = handle_incoming(
                    &node_id,
                    &topic,
                    &payload,
                    &dedup,
                    &state,
                    &config,
                    &identity,
                )
                .await;
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &node_id,
            mqtt_credentials,
            bootstrap,
            Some(message_callback),
        )
        .await?;
        dispatcher::install(client.shared_handle());

        // Give the QoS 1 publishes time to flush through the event
        // loop before publishing offline + exiting.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let status_topic = node_status_topic(&node_id);
        let _ = client
            .publish_text(&status_topic, "offline", QoS::AtLeastOnce, true)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut identity = final_identity.read().await.clone();
        identity.mark_enrolled(&gateway_addr);
        identity.save(&config.home)?;
        tracing::info!(
            node_id = %identity.node_id,
            machine_uid = %identity.machine_uid,
            node_token = identity.node_token.as_deref().map(|_| "<set>").unwrap_or("<none>"),
            gateway = %gateway_addr,
            "Node enrolled (identity persisted)"
        );
        Ok(())
    }

    /// `acowork-node rename <new>` — migrate the node's logical name
    /// (ADR-055 §6.12). Migrates the retained info/status/installed
    /// topics to the new node_id, clears the old retained set, and
    /// persists the new name to identity.json. Crash-safe: the old
    /// name stays valid until the retained migration completes
    /// (identity.json is written last).
    ///
    /// Precondition: the node daemon is stopped (rename reuses the old
    /// node_id client id, which collides with a running daemon).
    pub async fn rename(config: NodeConfig, new_name: &str) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let mut identity = NodeIdentity::load(&config.home)?.ok_or_else(|| {
            NodeError::Identity("No identity.json — run `acowork-node start` first".to_string())
        })?;
        let old_id = identity.node_id.clone();
        validate_rename_target(&old_id, new_name)?;

        // Rebuild the local install table so the retained `installed`
        // inventory is republished under the new node_id (§6.5).
        let mut state = NodeState::new(config.max_agents);
        crate::package::restore_installed_agents(&mut state, &config.packages_dir());
        let installed: Vec<_> = state.installed_agents.values().cloned().collect();

        // NodeInfo under the NEW name (machine_uid / hostname unchanged).
        let mut new_identity = identity.clone();
        new_identity.node_id = new_name.to_string();
        let info = build_node_info(&new_identity, &config, installed.len() as u32);

        let old_id_for_cb = old_id.clone();
        let new_id_for_cb = new_name.to_string();
        let installed_for_cb = installed.clone();
        let info_for_cb = info.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let old_id = old_id_for_cb.clone();
            let new_id = new_id_for_cb.clone();
            let installed = installed_for_cb.clone();
            let info = info_for_cb.clone();
            tokio::spawn(async move {
                migrate_retained(&client, &old_id, &new_id, &info, &installed).await;
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &old_id,
            // ADR-055 Phase 5a: present the long-lived node token so
            // rename works against an auth-enabled broker.
            Arc::new(Mutex::new(identity.node_token.clone())),
            bootstrap,
            None,
        )
        .await?;

        // Let the bootstrap publish flush before disconnecting.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(client);

        // Persist the new name LAST — the old name remains valid if any
        // step above failed (crash-safe, §6.12).
        identity.node_id = new_name.to_string();
        identity.save(&config.home)?;
        tracing::info!(old_node_id = %old_id, node_id = %new_name, "Node renamed");
        Ok(())
    }

    /// `acowork-node leave [--force]` — decommission the node
    /// (ADR-055 §6.13.2). Gracefully drains local Runtimes (unless
    /// `--force`), clears the retained status/info/installed topics
    /// (which drops the node from the Gateway's view), and leaves
    /// identity.json in place so the node can be re-started later.
    pub async fn leave(config: NodeConfig, force: bool) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let identity = NodeIdentity::load(&config.home)?.ok_or_else(|| {
            NodeError::Identity("No identity.json — run `acowork-node start` first".to_string())
        })?;
        let node_id = identity.node_id.clone();

        // Drain local Runtimes (unless --force): SIGTERM each running
        // agent and wait for it to exit so the broker sees their LWT.
        if !force {
            drain_running_agents(&config.home).await;
        }

        // Rebuild the install table to enumerate the retained `installed`
        // topics that must be cleared.
        let mut state = NodeState::new(config.max_agents);
        crate::package::restore_installed_agents(&mut state, &config.packages_dir());
        let installed: Vec<_> = state.installed_agents.values().cloned().collect();

        let node_id_for_cb = node_id.clone();
        let installed_for_cb = installed.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let node_id = node_id_for_cb.clone();
            let installed = installed_for_cb.clone();
            tokio::spawn(async move {
                clear_retained(&client, &node_id, &installed).await;
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &node_id,
            // ADR-055 Phase 5a: present the long-lived node token so
            // leave works against an auth-enabled broker.
            Arc::new(Mutex::new(identity.node_token.clone())),
            bootstrap,
            None,
        )
        .await?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(client);

        tracing::info!(node_id = %node_id, "Node left (retained cleared)");
        Ok(())
    }
}

/// Validate a rename target: a valid slug, not the reserved `local`
/// name, and different from the current name (ADR-055 §6.12).
fn validate_rename_target(old_id: &str, new_name: &str) -> Result<(), NodeError> {
    if !acowork_core::node::node_id_is_valid(new_name) {
        return Err(NodeError::Identity(format!(
            "Invalid node name '{new_name}': must be 2-32 chars of [a-z0-9-], \
             no leading/trailing hyphen"
        )));
    }
    if new_name == acowork_core::node::LOCAL_NODE_ID {
        return Err(NodeError::Identity(format!(
            "'{}' is reserved for the Gateway's own local node — choose another name",
            acowork_core::node::LOCAL_NODE_ID
        )));
    }
    if new_name == old_id {
        return Err(NodeError::Identity(format!(
            "Node is already named '{old_id}'"
        )));
    }
    Ok(())
}

/// Migrate the retained topics from `old_id` to `new_id` (ADR-055
/// §6.12 step ②/③): publish info + installed inventory under the new
/// name, then clear the old retained set with zero-byte publishes.
async fn migrate_retained(
    client: &AsyncClient,
    old_id: &str,
    new_id: &str,
    info: &NodeInfo,
    installed: &[crate::state::InstalledAgent],
) {
    // 1. Publish retained info under the NEW node_id.
    let info_envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeInfo(info.clone())),
    };
    let _ = client
        .publish(
            node_info_topic(new_id),
            QoS::AtLeastOnce,
            true,
            prost::Message::encode_to_vec(&info_envelope),
        )
        .await;

    // 2. Republish the installed inventory under the NEW node_id.
    for entry in installed {
        if let Some(installed_info) = crate::package::build_installed_info(entry) {
            let agent_id = installed_info.agent_id.clone();
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::InstalledAgentInfo(installed_info)),
            };
            let _ = client
                .publish(
                    node_agent_installed_topic(new_id, &agent_id),
                    QoS::AtLeastOnce,
                    true,
                    prost::Message::encode_to_vec(&envelope),
                )
                .await;
        }
    }

    // 3. Clear the OLD retained set (zero-byte retained = delete).
    clear_retained(client, old_id, installed).await;
}

/// Clear a node's retained status/info/installed topics with zero-byte
/// publishes (MQTT delete semantics). Used by both `rename` (old name)
/// and `leave` (current name).
async fn clear_retained(
    client: &AsyncClient,
    node_id: &str,
    installed: &[crate::state::InstalledAgent],
) {
    let _ = client
        .publish(node_status_topic(node_id), QoS::AtLeastOnce, true, Vec::new())
        .await;
    let _ = client
        .publish(node_info_topic(node_id), QoS::AtLeastOnce, true, Vec::new())
        .await;
    for entry in installed {
        let _ = client
            .publish(
                node_agent_installed_topic(node_id, &entry.agent_id),
                QoS::AtLeastOnce,
                true,
                Vec::new(),
            )
            .await;
    }
    // ADR-055 §6.7: also clear the node-local LSP relay retained set.
    let _ = client
        .publish(node_lsps_topic(node_id), QoS::AtLeastOnce, true, Vec::new())
        .await;
    let _ = client
        .publish(
            node_sidecar_status_topic(node_id, "lsp_relay"),
            QoS::AtLeastOnce,
            true,
            Vec::new(),
        )
        .await;
}

/// SIGTERM every running Runtime tracked in the persisted snapshot and
/// wait for each to exit (ADR-055 §6.13.2 `leave` graceful drain).
async fn drain_running_agents(home: &std::path::Path) {
    let Some(snapshot) = NodeState::load_snapshot(home) else {
        return;
    };
    for agent in snapshot.agents {
        tracing::info!(agent_id = %agent.agent_id, pid = agent.pid, "Draining Runtime for leave");
        if let Err(e) = kill_agent_process(agent.pid).await {
            tracing::warn!(pid = agent.pid, error = %e, "Failed to terminate Runtime during leave");
            continue;
        }
        // Wait up to ~10s for the process to exit so the broker observes
        // the Runtime LWT before we clear the node retained set.
        for _ in 0..40 {
            if !crate::process::spawn::is_process_alive(agent.pid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

/// Process one incoming control-plane message.
///
/// QoS 1 duplicate discipline: if the request_id was already handled,
/// re-send the cached reply without re-executing.
async fn handle_incoming(
    own_node_id: &str,
    topic: &str,
    payload: &[u8],
    dedup: &Arc<Mutex<RequestDedup>>,
    state: &SharedNodeState,
    config: &NodeConfig,
    identity: &Arc<RwLock<NodeIdentity>>,
) -> Result<(), NodeError> {
    let Some((topic_node_id, agent_id)) = parse_control_topic(topic, own_node_id) else {
        // Not a control topic (e.g. our own retained publishes echoed
        // back if subscribed broadly) — ignore silently.
        return Ok(());
    };

    let command: NodeControlCommand =
        DataEnvelope::decode(payload)
            .map_err(|e| NodeError::Protocol(format!("Bad NodeControlCommand envelope: {e}")))?
            .payload
            .and_then(|p| match p {
                data_envelope::Payload::NodeControlCommand(cmd) => Some(cmd),
                _ => None,
            })
            .ok_or_else(|| {
                NodeError::Protocol(format!(
                    "Envelope on control topic '{topic}' is not a NodeControlCommand"
                ))
            })?;

    if command.node_id != topic_node_id {
        return Err(NodeError::Protocol(format!(
            "command.node_id '{}' != topic node_id '{}'",
            command.node_id, topic_node_id
        )));
    }

    let request_id = command.request_id.clone();
    if request_id.is_empty() {
        return Err(NodeError::Protocol(
            "NodeControlCommand without request_id".to_string(),
        ));
    }

    // Determine the reply topic BEFORE the dedup check so duplicates
    // are re-sent to the same place.
    let reply_topic = match &agent_id {
        Some(aid) => node_agent_events_topic(own_node_id, aid),
        None => node_events_topic(own_node_id),
    };

    {
        let mut d = dedup.lock().await;
        if d.contains(&request_id)
            && let Some(cached) = d.cached_reply(&request_id).cloned()
        {
            tracing::debug!(
                request_id = %request_id,
                "Duplicate control command — re-sending cached reply"
            );
            publish_event(reply_topic, cached).await?;
            return Ok(());
        }
    }

    let node_token = identity.read().await.node_token.clone();
    let reply = handle_command(state, config, &command, node_token.as_deref()).await;
    let reply_clone = reply.clone();
    {
        let mut d = dedup.lock().await;
        d.insert(&request_id, reply_clone);
    }
    publish_event(reply_topic, reply).await
}

/// Publish a NodeEvent envelope (QoS 1, non-retained).
///
/// Events are results, not state — retained semantics would make an
/// old result shadow new ones after a Gateway restart.
async fn publish_event(topic: String, event: NodeEvent) -> Result<(), NodeError> {
    // The message callback (sync closure) has no return channel to the
    // owning `NodeMqttClient`, so reply publishes are routed through
    // the process-wide dispatcher: the daemon installs its shared
    // client handle at startup; `publish_event` fails fast (warn log)
    // if it is missing.
    dispatcher::publish(topic, event).await
}

/// Process-wide event dispatcher for command replies.
///
/// Holds the daemon's SHARED client handle (not a snapshot of the
/// client) so replies keep flowing through the current connection
/// after a soft-restart swaps the inner `AsyncClient`.
pub(crate) mod dispatcher {
    use std::sync::{Arc, OnceLock};
    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope, InstalledAgentInfo, NodeEvent};
    use rumqttc::{AsyncClient, QoS};
    use tokio::sync::Mutex;

    static CLIENT: OnceLock<Arc<Mutex<AsyncClient>>> = OnceLock::new();

    pub fn install(shared: Arc<Mutex<AsyncClient>>) {
        let _ = CLIENT.set(shared);
    }

    pub async fn publish(topic: String, event: NodeEvent) -> Result<(), crate::error::NodeError> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEvent(event)),
        };
        publish_envelope(topic, envelope, false).await
    }

    /// Publish a retained installed-agent inventory entry (ADR-055 §6.5).
    pub async fn publish_installed_info(
        topic: String,
        info: InstalledAgentInfo,
    ) -> Result<(), crate::error::NodeError> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::InstalledAgentInfo(info)),
        };
        publish_envelope(topic, envelope, true).await
    }

    /// Clear a retained installed-agent entry (uninstall) — an empty
    /// retained payload removes the broker's retained message.
    pub async fn clear_installed_info(topic: String) -> Result<(), crate::error::NodeError> {
        publish_raw(topic, Vec::new(), true).await
    }

    /// Publish the retained node-local LSP relay state (ADR-055 §6.7):
    /// the per-node `lsps` envelope + sidecar status topic. Delegates
    /// to [`crate::sidecar::publish_lsps_state`] with the process-wide
    /// shared handle (used by the sidecar supervisor's transitions).
    pub async fn publish_lsps_state(
        node_id: String,
        advertise_host: String,
        port: u16,
        ready: bool,
    ) -> Result<(), crate::error::NodeError> {
        let Some(shared) = CLIENT.get() else {
            tracing::warn!("Node event dispatcher not installed — dropping lsps publish");
            return Ok(());
        };
        let client = shared.lock().await.clone();
        crate::sidecar::publish_lsps_state(&client, &node_id, &advertise_host, port, ready).await
    }

    /// Clear the retained node-local LSP relay state (shutdown / leave)
    /// — empty retained payloads remove the broker's retained messages.
    pub async fn clear_lsps_state(node_id: &str) -> Result<(), crate::error::NodeError> {
        use acowork_core::node::{node_lsps_topic, node_sidecar_status_topic};
        publish_raw(node_lsps_topic(node_id), Vec::new(), true).await?;
        publish_raw(node_sidecar_status_topic(node_id, "lsp_relay"), Vec::new(), true).await
    }

    async fn publish_envelope(
        topic: String,
        envelope: DataEnvelope,
        retained: bool,
    ) -> Result<(), crate::error::NodeError> {
        let payload = prost::Message::encode_to_vec(&envelope);
        publish_raw(topic, payload, retained).await
    }

    async fn publish_raw(
        topic: String,
        payload: Vec<u8>,
        retained: bool,
    ) -> Result<(), crate::error::NodeError> {
        match CLIENT.get() {
            Some(shared) => {
                let client = shared.lock().await.clone();
                client
                    .publish(topic.clone(), QoS::AtLeastOnce, retained, payload)
                    .await
                    .map_err(|e| {
                        crate::error::NodeError::Mqtt(format!("Node publish '{topic}': {e}"))
                    })
            }
            None => {
                tracing::warn!(
                    topic = %topic,
                    "Node event dispatcher not installed — dropping publish"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::node::{node_agent_control_topic, node_enroll_result_topic};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn ping_command() -> NodeControlCommand {
        NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-1".to_string(),
            command: Some(node_control_command::Command::Ping(Default::default())),
        }
    }

    fn test_state() -> SharedNodeState {
        Arc::new(RwLock::new(crate::state::NodeState::new(16)))
    }

    fn test_config() -> NodeConfig {
        let tmp = tempfile::tempdir().unwrap();
        NodeConfig {
            home: tmp.path().to_path_buf(),
            ..NodeConfig::default()
        }
    }

    #[tokio::test]
    async fn ping_answers_ok() {
        let state = test_state();
        let config = test_config();
        let reply = handle_command(&state, &config, &ping_command(), None).await;
        assert_eq!(reply.status, "ok");
        assert_eq!(reply.message, "pong");
        assert_eq!(reply.request_id, "req-1");
    }

    #[tokio::test]
    async fn start_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-2".to_string(),
            command: Some(node_control_command::Command::Start(
                acowork_core::mqtt_proto::NodeStart {
                    agent_id: "com.example".to_string(),
                    dev_mode: false,
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        // The agent is not installed on this empty node → error (not
        // `not_implemented`, which was the Phase 2a placeholder).
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn install_without_source_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-3".to_string(),
            command: Some(node_control_command::Command::Install(
                acowork_core::mqtt_proto::NodeInstall {
                    agent_id: "com.example".to_string(),
                    package_url: String::new(),
                    local_path: String::new(),
                    dev_mode: false,
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("package_url"));
    }

    #[tokio::test]
    async fn empty_command_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-4".to_string(),
            command: None,
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn clone_source_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-clone".to_string(),
            command: Some(node_control_command::Command::Clone(
                acowork_core::mqtt_proto::NodeClone {
                    agent_id: "com.example".to_string(),
                    new_agent_id: "com.example.clone".to_string(),
                    mode: "skeleton".to_string(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn upgrade_without_source_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-upgrade".to_string(),
            command: Some(node_control_command::Command::Upgrade(
                acowork_core::mqtt_proto::NodeUpgrade {
                    agent_id: "com.example".to_string(),
                    package_url: String::new(),
                    local_path: String::new(),
                    dev_mode: false,
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("package_url"));
    }

    #[tokio::test]
    async fn publish_prepare_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-prepare".to_string(),
            command: Some(node_control_command::Command::PublishPrepare(
                acowork_core::mqtt_proto::NodePublishPrepare {
                    agent_id: "com.example".to_string(),
                    clean: false,
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn publish_build_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-build".to_string(),
            command: Some(node_control_command::Command::PublishBuild(
                acowork_core::mqtt_proto::NodePublishBuild {
                    agent_id: "com.example".to_string(),
                    output_dir: String::new(),
                    sign: false,
                    key_dir: String::new(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[test]
    fn control_topic_parsing() {
        assert_eq!(
            parse_control_topic("acowork/nodes/local/control/ping", "local"),
            Some(("local".to_string(), None))
        );
        assert_eq!(
            parse_control_topic(
                "acowork/nodes/local/agents/com.example/control/start",
                "local"
            ),
            Some(("local".to_string(), Some("com.example".to_string())))
        );
        // Other nodes' topics must not parse for us.
        assert_eq!(parse_control_topic("acowork/nodes/other/control/ping", "local"), None);
        // Non-control topics are ignored.
        assert_eq!(parse_control_topic("acowork/nodes/local/status", "local"), None);
        assert_eq!(parse_control_topic("acowork/nodes/local/info", "local"), None);
    }

    #[test]
    fn control_topic_construction_matches_parsing() {
        let topic = node_agent_control_topic("local", "com.example", "start");
        assert_eq!(
            parse_control_topic(&topic, "local"),
            Some(("local".to_string(), Some("com.example".to_string())))
        );
    }

    #[test]
    fn rename_target_valid_slug_is_accepted() {
        assert!(validate_rename_target("gpu-server", "gpu-2").is_ok());
    }

    #[test]
    fn rename_target_reserved_local_is_rejected() {
        let err = validate_rename_target("gpu-server", "local").unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn rename_target_invalid_slug_is_rejected() {
        assert!(validate_rename_target("gpu-server", "Bad_Name").is_err());
        assert!(validate_rename_target("gpu-server", "-lead").is_err());
        assert!(validate_rename_target("gpu-server", "a").is_err());
    }

    #[test]
    fn rename_target_same_name_is_rejected() {
        let err = validate_rename_target("gpu-server", "gpu-server").unwrap_err();
        assert!(err.to_string().contains("already named"));
    }

    // ── ADR-055 Phase 5a enrollment ───────────────────────────────

    fn test_identity() -> NodeIdentity {
        NodeIdentity {
            node_id: "gpu-1".to_string(),
            machine_uid: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
            node_token: None,
            gateway_addr: None,
            enrollment: EnrollmentState::Created,
            created_at: chrono::Utc::now(),
            enrolled_at: None,
        }
    }

    fn enroll_result_envelope(node_token: &str, status: &str) -> Vec<u8> {
        let result = acowork_core::mqtt_proto::NodeEnrollResult {
            node_id: "gpu-1".to_string(),
            machine_uid: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
            node_token: node_token.to_string(),
            status: status.to_string(),
            message: "test reply".to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEnrollResult(result)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn build_enroll_payload_with_token() {
        let config = NodeConfig {
            token: Some("tok-1234".to_string()),
            ..test_config()
        };
        let envelope = build_enroll_payload(&test_identity(), &config)
            .expect("enroll payload built when token present and no node_token");
        let data_envelope::Payload::NodeEnroll(enroll) = envelope.payload.unwrap() else {
            panic!("expected NodeEnroll payload");
        };
        assert_eq!(enroll.node_id, "gpu-1");
        assert_eq!(enroll.machine_uid, "0f0e0d0c-0b0a-4009-8007-060504030201");
        assert_eq!(enroll.enrollment_token, "tok-1234");
        assert_eq!(enroll.protocol_version, NODE_PROTOCOL_VERSION);
        assert!(!enroll.capabilities.is_empty());
        assert!(!enroll.os.is_empty() && !enroll.arch.is_empty());
    }

    #[test]
    fn build_enroll_payload_none_without_token_or_when_enrolled() {
        // No enrollment token → no payload.
        assert!(build_enroll_payload(&test_identity(), &test_config()).is_none());
        // Already holds a node_token → no payload (one-shot enrollment;
        // the Gateway reuses the token on reconnect).
        let mut identity = test_identity();
        identity.node_token = Some("existing-token".to_string());
        let config = NodeConfig {
            token: Some("tok-1234".to_string()),
            ..test_config()
        };
        assert!(build_enroll_payload(&identity, &config).is_none());
    }

    #[tokio::test]
    async fn enroll_result_persists_node_token_once() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let identity = Arc::new(RwLock::new(test_identity()));
        let topic = node_enroll_result_topic("gpu-1");
        let payload = enroll_result_envelope("tok-node-0001", "ok");

        assert!(handle_enroll_result(&topic, &payload, &identity, home).await);
        assert_eq!(
            identity.read().await.node_token.as_deref(),
            Some("tok-node-0001")
        );
        // Persisted to disk.
        let loaded = NodeIdentity::load(home).unwrap().unwrap();
        assert_eq!(loaded.node_token.as_deref(), Some("tok-node-0001"));

        // Idempotent: a second (stale) reply never overwrites.
        let stale = enroll_result_envelope("tok-node-0002", "ok");
        assert!(handle_enroll_result(&topic, &stale, &identity, home).await);
        assert_eq!(
            identity.read().await.node_token.as_deref(),
            Some("tok-node-0001"),
            "existing token must not be overwritten"
        );
    }

    #[tokio::test]
    async fn enroll_result_error_reply_does_not_write_token() {
        let tmp = tempfile::tempdir().unwrap();
        let identity = Arc::new(RwLock::new(test_identity()));
        let payload = enroll_result_envelope("", "error");
        assert!(handle_enroll_result(
            &node_enroll_result_topic("gpu-1"),
            &payload,
            &identity,
            tmp.path(),
        )
        .await);
        assert_eq!(identity.read().await.node_token, None);
    }

    #[tokio::test]
    async fn enroll_result_other_topic_is_ignored() {
        let identity = Arc::new(RwLock::new(test_identity()));
        assert!(!handle_enroll_result(
            "acowork/nodes/gpu-1/info",
            &[],
            &identity,
            std::path::Path::new("/tmp"),
        )
        .await);
    }
}
