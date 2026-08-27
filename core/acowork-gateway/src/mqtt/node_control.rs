//! Gateway → Node control-plane client (ADR-055 §6.2 / Phase 2b).
//!
//! The Gateway is the authority for agent lifecycle: it publishes
//! `NodeControlCommand`s to
//! `acowork/nodes/{node_id}/agents/{agent_id}/control/{cmd}` and waits
//! for the correlated `NodeEvent` on
//! `acowork/nodes/{node_id}/agents/{agent_id}/events`. QoS 1 duplicates
//! are handled by the Node's request_id dedup; the Gateway correlates
//! the reply by `request_id` via a pending-request table.
//!
//! Incoming node events are routed here from
//! [`crate::mqtt::dispatch`] (the single Gateway MQTT callback).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEvent,
};
use acowork_core::node::{node_agent_control_topic, node_agent_events_topic};
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

/// Timeout for a node command round-trip. Covers network + Runtime
/// startup latency on the node; the Gateway surfaces a timeout error
/// so callers can retry idempotently.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Error type for node control-plane operations.
#[derive(Debug, thiserror::Error)]
pub enum NodeControlError {
    #[error("Node '{node_id}' is offline")]
    NodeOffline { node_id: String },
    #[error("Node command failed for '{agent_id}': {message}")]
    CommandFailed { agent_id: String, message: String },
    #[error("Node command timed out (request_id {request_id})")]
    Timeout { request_id: String },
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("Gateway MQTT client is not available (broker disabled)")]
    NoClient,
}

/// The Gateway's client for issuing agent lifecycle commands to nodes.
#[derive(Clone)]
pub struct NodeControlClient {
    client: Arc<GatewayMqttClient>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<NodeEvent>>>>,
}

impl NodeControlClient {
    pub fn new(client: Arc<GatewayMqttClient>) -> Self {
        Self {
            client,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Publish a command and await the correlated NodeEvent reply.
    async fn send(&self, node_id: &str, agent_id: &str, command: NodeControlCommand) -> Result<NodeEvent, NodeControlError> {
        let request_id = command.request_id.clone();
        let cmd_name = command_name(&command);
        let topic = node_agent_control_topic(node_id, agent_id, &cmd_name);
        let reply_topic = node_agent_events_topic(node_id, agent_id);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeControlCommand(command)),
        };
        if let Err(e) = self
            .client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
        {
            self.pending.lock().await.remove(&request_id);
            return Err(NodeControlError::Publish(e.to_string()));
        }
        tracing::debug!(topic = %topic, reply_topic = %reply_topic, request_id = %request_id, "Published node control command");

        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(_)) => {
                // oneshot sender dropped — should not happen; treat as timeout.
                self.pending.lock().await.remove(&request_id);
                Err(NodeControlError::Timeout { request_id })
            }
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(NodeControlError::Timeout { request_id })
            }
        }
    }

    /// Route an incoming NodeEvent to a pending request (called from
    /// dispatch). Returns silently if no request is waiting on this id.
    pub async fn handle_event(&self, event: NodeEvent) {
        if let Some(tx) = self.pending.lock().await.remove(&event.request_id) {
            let _ = tx.send(event);
        }
    }

    /// Start an agent Runtime on a node.
    pub async fn start_agent(&self, node_id: &str, agent_id: &str, dev_mode: bool) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Start(
                    acowork_core::mqtt_proto::NodeStart {
                        agent_id: agent_id.to_string(),
                        dev_mode,
                    },
                )),
            },
        )
        .await
    }

    /// Stop an agent Runtime on a node.
    pub async fn stop_agent(&self, node_id: &str, agent_id: &str, reason: &str) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Stop(
                    acowork_core::mqtt_proto::NodeStop {
                        agent_id: agent_id.to_string(),
                        reason: reason.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Uninstall an agent package on a node.
    pub async fn uninstall_agent(&self, node_id: &str, agent_id: &str) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Uninstall(
                    acowork_core::mqtt_proto::NodeUninstall {
                        agent_id: agent_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Install an agent package on a node from a node-local spooled path.
    pub async fn install_agent(&self, node_id: &str, agent_id: &str, local_path: &str, dev_mode: bool) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Install(
                    acowork_core::mqtt_proto::NodeInstall {
                        agent_id: agent_id.to_string(),
                        package_url: String::new(),
                        local_path: local_path.to_string(),
                        dev_mode,
                    },
                )),
            },
        )
        .await
    }

    /// Install an agent package on a node from a Gateway-hosted download
    /// URL (ADR-055 §3.2). Fire-and-forget: the install is asynchronous —
    /// the node pulls the package from the URL, installs it locally, and
    /// the Gateway observes completion via the retained `installed`
    /// inventory entry (the command's NodeEvent reply still carries the
    /// `request_id` for diagnostics, but nothing blocks on it here).
    pub async fn install_agent_by_url(
        &self,
        node_id: &str,
        agent_id: &str,
        package_url: &str,
        dev_mode: bool,
    ) -> Result<(), NodeControlError> {
        let command = NodeControlCommand {
            node_id: node_id.to_string(),
            request_id: Uuid::new_v4().to_string(),
            command: Some(node_control_command::Command::Install(
                acowork_core::mqtt_proto::NodeInstall {
                    agent_id: agent_id.to_string(),
                    package_url: package_url.to_string(),
                    local_path: String::new(),
                    dev_mode,
                },
            )),
        };
        let topic = node_agent_control_topic(node_id, agent_id, "install");
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeControlCommand(command)),
        };
        self.client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
            .map_err(|e| NodeControlError::Publish(e.to_string()))
    }

    /// Clone an installed agent to a new agent ID on the source agent's
    /// node (ADR-055 §6.6 L2-5, node-local operation).
    pub async fn clone_agent(
        &self,
        node_id: &str,
        agent_id: &str,
        new_agent_id: &str,
        mode: &str,
    ) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Clone(
                    acowork_core::mqtt_proto::NodeClone {
                        agent_id: agent_id.to_string(),
                        new_agent_id: new_agent_id.to_string(),
                        mode: mode.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Upgrade an installed agent from a Gateway-hosted download URL
    /// (ADR-055 §3.2). Fire-and-forget — completion is observed via the
    /// retained `installed` inventory entry with the new version.
    pub async fn upgrade_agent_by_url(
        &self,
        node_id: &str,
        agent_id: &str,
        package_url: &str,
        dev_mode: bool,
    ) -> Result<(), NodeControlError> {
        let command = NodeControlCommand {
            node_id: node_id.to_string(),
            request_id: Uuid::new_v4().to_string(),
            command: Some(node_control_command::Command::Upgrade(
                acowork_core::mqtt_proto::NodeUpgrade {
                    agent_id: agent_id.to_string(),
                    package_url: package_url.to_string(),
                    local_path: String::new(),
                    dev_mode,
                },
            )),
        };
        let topic = node_agent_control_topic(node_id, agent_id, "upgrade");
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeControlCommand(command)),
        };
        self.client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
            .map_err(|e| NodeControlError::Publish(e.to_string()))
    }

    /// Run publish-preparation checks on a node. The structured result is
    /// carried in `NodeEvent.result_json` (JSON).
    pub async fn publish_prepare(
        &self,
        node_id: &str,
        agent_id: &str,
        clean: bool,
    ) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::PublishPrepare(
                    acowork_core::mqtt_proto::NodePublishPrepare {
                        agent_id: agent_id.to_string(),
                        clean,
                    },
                )),
            },
        )
        .await
    }

    /// Build (and optionally sign) a .agent package on a node. The
    /// structured result is carried in `NodeEvent.result_json` (JSON).
    pub async fn publish_build(
        &self,
        node_id: &str,
        agent_id: &str,
        output_dir: &str,
        sign: bool,
        key_dir: &str,
    ) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::PublishBuild(
                    acowork_core::mqtt_proto::NodePublishBuild {
                        agent_id: agent_id.to_string(),
                        output_dir: output_dir.to_string(),
                        sign,
                        key_dir: key_dir.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Import a skills ZIP on a node (path is node-local).
    pub async fn skills_import(&self, node_id: &str, agent_id: &str, zip_path: &str) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            agent_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::SkillsImport(
                    acowork_core::mqtt_proto::NodeSkillsImport {
                        agent_id: agent_id.to_string(),
                        zip_path: zip_path.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Interpret a NodeEvent reply: map to a Gateway-style error on
    /// non-ok status (not_implemented → clear error).
    pub fn check_reply(agent_id: &str, event: &NodeEvent) -> Result<(), NodeControlError> {
        match event.status.as_str() {
            "ok" => Ok(()),
            "error" | "not_implemented" => Err(NodeControlError::CommandFailed {
                agent_id: agent_id.to_string(),
                message: event.message.clone(),
            }),
            other => Err(NodeControlError::CommandFailed {
                agent_id: agent_id.to_string(),
                message: format!("unexpected status '{}': {}", other, event.message),
            }),
        }
    }
}

/// Derive the control topic command segment from a NodeControlCommand.
fn command_name(command: &NodeControlCommand) -> String {
    match command.command.as_ref() {
        Some(node_control_command::Command::Ping(_)) => "ping",
        Some(node_control_command::Command::Start(_)) => "start",
        Some(node_control_command::Command::Stop(_)) => "stop",
        Some(node_control_command::Command::Install(_)) => "install",
        Some(node_control_command::Command::Uninstall(_)) => "uninstall",
        Some(node_control_command::Command::SkillsImport(_)) => "skills_import",
        Some(node_control_command::Command::AvatarUpdate(_)) => "avatar_update",
        Some(node_control_command::Command::Clone(_)) => "clone",
        Some(node_control_command::Command::Upgrade(_)) => "upgrade",
        Some(node_control_command::Command::PublishPrepare(_)) => "publish_prepare",
        Some(node_control_command::Command::PublishBuild(_)) => "publish_build",
        None => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_name_derivation() {
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "r".to_string(),
            command: Some(node_control_command::Command::Start(Default::default())),
        };
        assert_eq!(command_name(&cmd), "start");
    }

    #[test]
    fn command_name_phase_3b_commands() {
        let cases = [
            (node_control_command::Command::Clone(Default::default()), "clone"),
            (node_control_command::Command::Upgrade(Default::default()), "upgrade"),
            (
                node_control_command::Command::PublishPrepare(Default::default()),
                "publish_prepare",
            ),
            (
                node_control_command::Command::PublishBuild(Default::default()),
                "publish_build",
            ),
        ];
        for (command, expected) in cases {
            let cmd = NodeControlCommand {
                node_id: "local".to_string(),
                request_id: "r".to_string(),
                command: Some(command),
            };
            assert_eq!(command_name(&cmd), expected);
        }
    }

    #[test]
    fn check_reply_ok() {
        let ev = NodeEvent {
            node_id: "local".to_string(),
            request_id: "r".to_string(),
            status: "ok".to_string(),
            message: "started".to_string(),
            result_json: None,
        };
        assert!(NodeControlClient::check_reply("a", &ev).is_ok());
    }

    #[test]
    fn check_reply_error() {
        let ev = NodeEvent {
            node_id: "local".to_string(),
            request_id: "r".to_string(),
            status: "error".to_string(),
            message: "agent not installed".to_string(),
            result_json: None,
        };
        assert!(NodeControlClient::check_reply("a", &ev).is_err());
    }
}
