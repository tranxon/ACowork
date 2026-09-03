//! Local Node Agent supervisor (ADR-055 §6.11, Phase 2a).
//!
//! D1: single topology protocol — the Gateway's own machine is just
//! another node (`node_id = "local"`). The Gateway spawns it as a
//! sibling binary at startup (after the MQTT broker is ready) and
//! supervises it:
//!
//! 1. **Orphan cleanup** — a local node orphaned by a previous
//!    Gateway run (Gateway crashed / was killed) is SIGTERM'd before
//!    spawning a fresh one. Detection mirrors
//!    `cleanup_orphaned_runtimes`: process-list scan for
//!    `acowork-node` whose cmdline carries our spawn markers
//!    (`--name local --gateway-mqtt-port {our port}`). The reserved
//!    name `local` is exclusively Gateway-spawned (§6.12), so this
//!    never touches user-managed nodes on the same machine.
//! 2. **Reuse window** — the Gateway client is already subscribed to
//!    `acowork/nodes/+/status`; wait a short window for a retained
//!    `online` — an externally-managed local node (e.g. systemd) is
//!    reused instead of spawning a duplicate.
//! 3. **Spawn + reaper** — the supervisor task owns the `Child`,
//!    reaps it on exit, and respawns after a retry delay (re-checking
//!    the registry first: another instance may have taken over).
//! 4. **Graceful shutdown** — on Gateway Ctrl-C the child is
//!    SIGTERM'd via its process group (the supervisor's `wait()`
//!    observes the exit and does NOT respawn). If the Gateway dies
//!    ungracefully, the node's own MQTT reconnect keeps it alive
//!    until the next Gateway run performs step 1.
//!
//! Phase 2a: the local node manages no Runtimes yet — this module
//! only establishes the residency and the supervision lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::RwLock;

use crate::mqtt::node_control::NodeControlClient;
use crate::mqtt::node_registry::SharedNodeRegistry;

/// Reserved local node id (§6.11 / §6.12).
const LOCAL_NODE_ID: &str = acowork_core::node::LOCAL_NODE_ID;

/// How long to wait for a retained `online` from an already-running
/// local node before spawning our own. The Gateway client re-subscribes
/// to `acowork/nodes/+/status` immediately before this runs, so a
/// retained message arrives within milliseconds — 500 ms is plenty.
/// (The old 3 s window delayed every clean cold start by 3 s.)
const REUSE_WINDOW: Duration = Duration::from_millis(500);

/// Delay before respawning a crashed local node (§6.11 step 3, 60s).
const RESPAWN_DELAY: Duration = Duration::from_secs(60);

/// Grace period after SIGTERM before escalating to SIGKILL.

///
/// Only referenced in the `#[cfg(unix)]` branch of `kill_process_group`;
/// Windows uses `taskkill /F` (no grace). Gated to unix so non-unix
/// targets don't even compile the constant.

#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Shared supervisor state: current child PID + stop flag.
///
/// The supervisor TASK owns the `Child` handle (needed for `wait()`);
/// other paths only see the PID and the stop flag — no handle
/// contention.
pub struct LocalNodeSupervisor {
    pid: RwLock<Option<u32>>,
    stopping: AtomicBool,
}

impl LocalNodeSupervisor {
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            pid: RwLock::new(None),
            stopping: AtomicBool::new(false),
        })
    }

    /// Graceful shutdown: mark stopping + SIGTERM the child's process
    /// group. The supervisor task reaps the exit and stands down.
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let pid = *self.pid.read().await;
        if let Some(pid) = pid {
            tracing::info!(pid, "Shutting down local node agent");
            kill_process_group(pid).await;
        }
    }
}

/// Locate the `acowork-node` binary — sibling of the current
/// executable (same convention as `acowork-runtime`, L1-1).
fn node_binary() -> std::path::PathBuf {
    let bin_name = if cfg!(windows) {
        "acowork-node.exe"
    } else {
        "acowork-node"
    };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(bin_name)))
        .unwrap_or_else(|| std::path::PathBuf::from(bin_name))
}

/// SIGTERM a process group, escalating to SIGKILL after
/// [`KILL_GRACE`]. No-op when the group is already gone.
async fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-15", &format!("-{pid}")])
            .output();
        // Escalation: the supervisor task's wait() would time out
        // eventually; a bounded sleep + SIGKILL keeps teardown snappy.
        tokio::time::sleep(KILL_GRACE).await;
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .output();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Find processes whose command line contains `pattern`, returning
/// `(pid, full command line)` pairs.
///
/// The classic `pgrep -af` trick silently breaks on macOS: BSD pgrep
/// ignores `-a` and prints bare PIDs, so a `splitn(2)` parse yields an
/// empty cmdline and every orphan-cleanup check matches nothing.
/// `ps -axo pid=,command=` is portable across Linux/macOS and prints
/// the full argv; on Windows neither `ps` nor `pgrep` exists, so this
/// returns empty and callers skip cleanup (same as before).
pub(crate) fn find_procs_by_cmdline(pattern: &str) -> Vec<(u32, String)> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(), // no ps (Windows) — skip cleanup
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, char::is_whitespace);
            let pid: u32 = parts.next()?.trim().parse().ok()?;
            let cmdline = parts.next().unwrap_or("").trim();
            if cmdline.contains(pattern) {
                Some((pid, cmdline.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Kill orphaned local-node processes from a previous Gateway run.
///
/// Marker: cmdline contains `acowork-node`, `--name local`, and our
/// MQTT port. pgrep-less environments (Windows): skipped (returns 0)
/// — same limitation as `cleanup_orphaned_runtimes`; Windows-specific
/// verification is a Phase 2b gate item.
fn cleanup_orphaned_local_nodes(mqtt_port: u16) -> usize {
    let my_pid = std::process::id();
    let port_marker = format!("--gateway-mqtt-port {mqtt_port}");

    let pids: Vec<u32> = find_procs_by_cmdline("acowork-node")
        .into_iter()
        .filter_map(|(pid, cmdline)| {
            if pid == my_pid {
                return None;
            }
            if cmdline.contains("--name local") && cmdline.contains(&port_marker) {
                Some(pid)
            } else {
                None
            }
        })
        .collect();

    if pids.is_empty() {
        return 0;
    }
    tracing::info!(
        count = pids.len(),
        "Killing orphaned local node agent(s) from a previous Gateway run"
    );
    for pid in &pids {
        let _ = std::process::Command::new("kill")
            .args(["-15", &pid.to_string()])
            .output();
    }
    pids.len()
}

/// Ensure a local node agent is running; supervise it forever.
///
/// Called from `Gateway::run` AFTER the MQTT broker + Gateway MQTT
/// client (with its `acowork/nodes/+/status` subscription) are up, so
/// a retained `online` from an already-running node reaches the
/// registry during the reuse window.
///
/// `local_token` (ADR-055 Phase 5a) is the pre-issued local node
/// credential, forwarded to the child via `--token` when MQTT auth is
/// enabled (None keeps the pre-5a credential-less spawn).
pub async fn ensure_local_node(
    mqtt_host: &str,
    mqtt_port: u16,
    packages_dir: &str,
    node_registry: SharedNodeRegistry,
    local_token: Option<String>,
    proxy_port: Option<u16>,
    lsp_relay_port: Option<u16>,
) -> std::io::Result<Arc<LocalNodeSupervisor>> {
    // Step 1: kill orphans from a previous Gateway run (they point at
    // OUR port, and the reserved `local` name is Gateway-exclusive).
    let orphans = cleanup_orphaned_local_nodes(mqtt_port);
    if orphans > 0 {
        // Avoid a spawn race with the dying process. Retained state is
        // not a concern: the in-memory broker died with the old
        // Gateway, so a fresh start sees an empty retained set.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Step 2: reuse window — an `online` local node (externally
    // managed) wins over spawning our own.
    let deadline = tokio::time::Instant::now() + REUSE_WINDOW;
    let mut reused = false;
    while tokio::time::Instant::now() < deadline {
        if node_registry.read().await.is_online(LOCAL_NODE_ID) {
            tracing::info!("Local node agent already online — reusing it");
            reused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let supervisor = LocalNodeSupervisor::new_shared();
    if !reused {
        spawn_and_supervise(
            mqtt_host,
            mqtt_port,
            packages_dir,
            node_registry,
            supervisor.clone(),
            local_token.as_deref(),
            proxy_port,
            lsp_relay_port,
        )
        .await?;
    }
    Ok(supervisor)
}

/// Spawn the local node child and supervise it: reaper + respawn with
/// re-check, forever (until `shutdown()` sets the stop flag).
#[allow(clippy::too_many_arguments)]
async fn spawn_and_supervise(
    mqtt_host: &str,
    mqtt_port: u16,
    packages_dir: &str,
    node_registry: SharedNodeRegistry,
    supervisor: Arc<LocalNodeSupervisor>,
    local_token: Option<&str>,
    proxy_port: Option<u16>,
    lsp_relay_port: Option<u16>,
) -> std::io::Result<()> {
    let bin = node_binary();
    if !bin.exists() {
        tracing::warn!(
            binary = %bin.display(),
            "acowork-node binary not found — local node agent disabled \
             (build the workspace to enable the node topology)"
        );
        return Ok(());
    }

    let mut child = spawn_node_child(
        &bin,
        mqtt_host,
        mqtt_port,
        packages_dir,
        local_token,
        proxy_port,
        lsp_relay_port,
    )?;
    tracing::info!(pid = child.id(), binary = %bin.display(), "Local node agent spawned");

    let supervise_host = mqtt_host.to_string();
    let supervise_packages_dir = packages_dir.to_string();
    let supervise_token = local_token.map(str::to_string);
    tokio::spawn(async move {
        loop {
            let pid = child.id();
            *supervisor.pid.write().await = pid;
            let exit = child.wait().await;
            *supervisor.pid.write().await = None;

            if supervisor.stopping.load(Ordering::SeqCst) {
                tracing::info!(exit_status = ?exit, "Local node agent stopped (gateway shutdown)");
                return;
            }
            tracing::warn!(exit_status = ?exit, "Local node agent exited — will respawn");

            // Retry delay, then re-check: another instance may have
            // taken over while we were down.
            tokio::time::sleep(RESPAWN_DELAY).await;
            if supervisor.stopping.load(Ordering::SeqCst) {
                return;
            }
            if node_registry.read().await.is_online(LOCAL_NODE_ID) {
                tracing::info!("Local node agent online again (external) — not respawning");
                return;
            }

            match spawn_node_child(&bin, &supervise_host, mqtt_port, &supervise_packages_dir, supervise_token.as_deref(), proxy_port, lsp_relay_port) {
                Ok(new_child) => {
                    child = new_child;
                    tracing::info!(pid = child.id(), "Local node agent respawned");
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to respawn local node agent — will keep retrying"
                    );
                    loop {
                        tokio::time::sleep(RESPAWN_DELAY).await;
                        if supervisor.stopping.load(Ordering::SeqCst) {
                            return;
                        }
                        if node_registry.read().await.is_online(LOCAL_NODE_ID) {
                            tracing::info!(
                                "Local node agent online (external) — supervisor standing down"
                            );
                            return;
                        }
                        if let Ok(new_child) = spawn_node_child(&bin, &supervise_host, mqtt_port, &supervise_packages_dir, supervise_token.as_deref(), proxy_port, lsp_relay_port) {
                            child = new_child;
                            tracing::info!(pid = child.id(), "Local node agent respawned");
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

/// Spawn the `acowork-node start` child with the spawn markers used
/// by orphan cleanup (`--name local --gateway-mqtt-port {port}`).
/// `token` (ADR-055 Phase 5a) is the pre-issued local node credential,
/// forwarded as `--token` when MQTT auth is enabled.
fn spawn_node_child(
    bin: &std::path::Path,
    mqtt_host: &str,
    mqtt_port: u16,
    packages_dir: &str,
    token: Option<&str>,
    proxy_port: Option<u16>,
    lsp_relay_port: Option<u16>,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "start",
        "--gateway-host",
        mqtt_host,
        "--gateway-mqtt-port",
        &mqtt_port.to_string(),
        "--name",
        LOCAL_NODE_ID,
        "--packages-dir",
        packages_dir,
    ]);
    if let Some(token) = token {
        cmd.args(["--token", token]);
    }
    // ADR-055 multi-instance: a second Gateway on the same machine
    // (tests, previews) must not steal the primary node's reverse-proxy
    // (:19900) or LSP relay (:19878) ports — forward the configured
    // overrides when present.
    if let Some(port) = proxy_port {
        cmd.args(["--proxy-port", &port.to_string()]);
    }
    if let Some(port) = lsp_relay_port {
        cmd.args(["--lsp-relay-port", &port.to_string()]);
    }
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());

    #[cfg(unix)]
    {
        // Own process group: Ctrl-C on the Gateway terminal must not
        // SIGINT the node directly (the Gateway shuts it down
        // explicitly); a kill can target the whole group.
        cmd.process_group(0);
    }

    cmd.spawn()
}

/// `acowork-gateway nodes list` — query the running Gateway's broker
/// for retained node status/info and print a table.
///
/// Connects to the broker as the publisher client (which auto-subscribes
/// `acowork/nodes/+/status` + `acowork/nodes/+/info` via the ConnAck
/// persistent-subscription handler), collects retained messages for a
/// short window, then renders the registry snapshot. No daemon state or
/// HTTP endpoint is touched — the retained topics ARE the node view.
pub async fn list_nodes_via_mqtt(mqtt_host: &str, mqtt_port: u16) -> crate::error::Result<()> {
    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};

    let node_registry = crate::mqtt::node_registry::new_shared_registry();

    // The client subscribes node topics automatically on ConnAck (they
    // are in PERSISTENT_SUBSCRIPTIONS). The callback feeds the registry.
    let reg_for_cb = node_registry.clone();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        // Only node topics feed the node registry (the publisher client
        // also receives agent status/ready/http_endpoint messages).
        if !topic.starts_with("acowork/nodes/") {
            return;
        }
        let reg = reg_for_cb.clone();
        tokio::spawn(async move {
            let mut registry = reg.write().await;
            if topic.ends_with("/status") {
                registry.update_status_from_mqtt(&topic, &payload);
            } else if topic.ends_with("/info") {
                registry.update_info_from_mqtt(&topic, &payload);
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {}:{} — is the Gateway daemon running? ({})",
                mqtt_host, mqtt_port, e
            ))
        })?;

    // Let retained status/info messages drain in.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let nodes = node_registry.read().await.list_nodes();
    drop(client);

    if nodes.is_empty() {
        println!("No nodes discovered.");
        return Ok(());
    }

    println!(
        "{:<16} {:<8} {:<8} {:<10} {:<12} {:<10} {:<10}",
        "NODE ID", "ONLINE", "AGENTS", "OS", "ARCH", "VERSION", "HOSTNAME"
    );
    for node in nodes {
        let info = node.info.as_ref();
        println!(
            "{:<16} {:<8} {:<8} {:<10} {:<12} {:<10} {:<10}",
            node.node_id,
            if node.online { "yes" } else { "no" },
            info.map(|i| i.agent_count.to_string())
                .unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.os.clone()).unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.arch.clone()).unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.node_version.clone())
                .unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.hostname.clone())
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    Ok(())
}

/// Extract the `agent_id` from a node agent topic
/// (`acowork/nodes/{node_id}/agents/{agent_id}/{kind}`) when the topic
/// matches the given node and `kind`.
fn extract_node_agent_id(topic: &str, node_id: &str, kind: &str) -> Option<String> {
    let prefix = format!("acowork/nodes/{node_id}/agents/");
    let suffix = format!("/{kind}");
    let rest = topic.strip_prefix(&prefix)?;
    let agent_id = rest.strip_suffix(&suffix)?;
    if agent_id.is_empty() || agent_id.contains('/') {
        return None;
    }
    Some(agent_id.to_string())
}

/// `nodes drain <node_id>` — stop every agent running on a node
/// (ADR-055 §6.13.3, migration precursor).
///
/// Runs as a standalone CLI process: connects to the running Gateway's
/// broker, discovers the node's installed agents from the retained
/// `installed` topics, then issues `stop` commands through the
/// [`NodeControlClient`]. `stop` is idempotent, so already-stopped
/// agents simply report `ok` (§6.2).
pub async fn drain_node_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
) -> crate::error::Result<()> {
    use std::collections::HashSet;
    use std::sync::OnceLock;

    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
    use prost::Message;
    use tokio::sync::Mutex;

    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};
    use crate::mqtt::node_control::NodeControlClient;

    let installed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let control_slot: Arc<OnceLock<Arc<NodeControlClient>>> = Arc::new(OnceLock::new());

    let cb_installed = installed.clone();
    let cb_control = control_slot.clone();
    let cb_node_id = node_id.to_string();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        let node_id = cb_node_id.clone();
        let installed = cb_installed.clone();
        let control = cb_control.clone();
        let topic = topic.to_string();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            if let Some(agent_id) = extract_node_agent_id(&topic, &node_id, "installed") {
                if !payload.is_empty() {
                    installed.lock().await.insert(agent_id);
                }
            } else if let Some(_agent_id) = extract_node_agent_id(&topic, &node_id, "events") {
                // Correlate NodeEvent replies with in-flight commands.
                if let Some(control) = control.get()
                    && let Ok(envelope) = DataEnvelope::decode(payload.as_slice())
                    && let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload
                {
                    control.handle_event(event).await;
                }
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;
    let client = Arc::new(client);
    let _ = control_slot.set(Arc::new(NodeControlClient::new(client.clone())));

    // Let retained `installed` messages drain in.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let agents: Vec<String> = installed.lock().await.iter().cloned().collect();
    let control = control_slot
        .get()
        .cloned()
        .expect("control slot set above");

    if agents.is_empty() {
        println!("Node '{node_id}' has no installed agents to drain.");
        return Ok(());
    }

    for agent_id in &agents {
        match control.stop_agent(node_id, agent_id, "drain").await {
            Ok(event) => {
                if event.status == "ok" {
                    println!("stopped {agent_id}");
                } else {
                    println!("{agent_id}: {}", event.message);
                }
            }
            Err(e) => println!("{agent_id}: error — {e}"),
        }
    }
    Ok(())
}

/// `nodes remove <node_id>` — remove a node's records by clearing its
/// retained status/info (ADR-055 §6.13.3). The node must be offline;
/// the CLI cannot assert liveness here, but an offline node's retained
/// status is `offline` and the registry drops it on the empty payload.
pub async fn remove_node_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
) -> crate::error::Result<()> {
    use acowork_core::node::{node_info_topic, node_status_topic};

    use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

    let client = GatewayMqttClient::new_publisher(mqtt_host, mqtt_port)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;

    // Publishing an empty retained message clears the retained entry
    // (MQTT semantics), which drops the node from `nodes list`.
    client
        .publish_raw(&node_status_topic(node_id), Vec::new(), MqttQoS::AtLeastOnce, true)
        .await
        .map_err(|e| crate::error::GatewayError::Ipc(e.to_string()))?;
    client
        .publish_raw(&node_info_topic(node_id), Vec::new(), MqttQoS::AtLeastOnce, true)
        .await
        .map_err(|e| crate::error::GatewayError::Ipc(e.to_string()))?;

    println!("Removed node '{node_id}' (cleared retained status/info).");
    Ok(())
}

/// Connect to the running Gateway's broker as a standalone CLI control
/// client. Returns a [`NodeControlClient`] whose event replies are routed
/// back through the broker callback (same pattern as
/// [`drain_node_via_mqtt`], §6.13.3 CLI control plane).
async fn cli_control_client(
    mqtt_host: &str,
    mqtt_port: u16,
) -> crate::error::Result<Arc<NodeControlClient>> {
    use std::sync::OnceLock;

    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
    use prost::Message;

    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};
    use crate::mqtt::node_control::NodeControlClient;

    let control_slot: Arc<OnceLock<Arc<NodeControlClient>>> = Arc::new(OnceLock::new());
    let cb_control = control_slot.clone();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        let control = cb_control.clone();
        let topic = topic.to_string();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            if !topic.ends_with("/events") {
                return;
            }
            if let Some(control) = control.get()
                && let Ok(envelope) = DataEnvelope::decode(payload.as_slice())
                && let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload
            {
                control.handle_event(event).await;
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;
    let client = Arc::new(client);
    let control = Arc::new(NodeControlClient::new(client));
    let _ = control_slot.set(control.clone());
    Ok(control)
}

/// CLI package-dispatch context shared by the `install`/`upgrade`
/// commands (ADR-055 §6.13.3): the broker endpoint, the target node, and
/// the pieces needed to spool a package into the registry and build the
/// download URL.
pub struct CliPackageDispatch<'a> {
    pub mqtt_host: &'a str,
    pub mqtt_port: u16,
    pub node_id: &'a str,
    pub registry_dir: &'a std::path::Path,
    pub advertise_host: &'a str,
    pub http_host: &'a str,
    pub http_port: u16,
    pub dev_mode: bool,
}

/// `install --node <node>` — dispatch an install to a target node via the
/// CLI control plane (ADR-055 §6.13.3).
///
/// Spools the package into the Gateway registry, then issues the async
/// install command carrying the registry download URL. Completion is
/// observed by the daemon via the node's retained `installed` inventory,
/// not here (fire-and-forget, ADR-055 §3.2).
pub async fn install_agent_via_mqtt(
    dispatch: &CliPackageDispatch<'_>,
    package_path: &std::path::Path,
) -> crate::error::Result<()> {
    let manifest = crate::http::agents::extract_manifest_from_package(package_path)?;
    let agent_id = manifest.agent_id.clone();

    std::fs::create_dir_all(dispatch.registry_dir).map_err(crate::error::GatewayError::Io)?;
    let registry_path = dispatch.registry_dir.join(format!("{agent_id}.agent"));
    std::fs::copy(package_path, &registry_path).map_err(crate::error::GatewayError::Io)?;

    // The download URL must be reachable from the target node: the
    // loopback-bound local node dials the HTTP bind host, remote nodes
    // the advertise host (ADR-055 D3).
    let url_host = if dispatch.node_id == acowork_core::node::LOCAL_NODE_ID {
        if dispatch.http_host == "0.0.0.0" || dispatch.http_host == "::" {
            "127.0.0.1"
        } else {
            dispatch.http_host
        }
    } else {
        dispatch.advertise_host
    };
    let url = format!(
        "http://{url_host}:{}/api/packages/{agent_id}/download",
        dispatch.http_port
    );

    let control = cli_control_client(dispatch.mqtt_host, dispatch.mqtt_port).await?;
    // ADR-059 §6: the CLI dispatch has no operation store; a fresh
    // operation id still gives the NodeEvent reply a correlation id.
    let operation_id = acowork_core::operation::OperationId::new();
    control
        .install_agent_by_url(
            dispatch.node_id,
            &agent_id,
            &url,
            dispatch.dev_mode,
            operation_id.as_str(),
        )
        .await
        .map_err(|e| crate::error::GatewayError::Lifecycle(e.to_string()))?;

    println!("Install dispatched to node '{}': {agent_id}", dispatch.node_id);
    Ok(())
}

/// `upgrade --node <node>` — dispatch an upgrade to a target node via the
/// CLI control plane (ADR-055 §6.13.3).
pub async fn upgrade_agent_via_mqtt(
    dispatch: &CliPackageDispatch<'_>,
    agent_id: &str,
    package_path: &std::path::Path,
) -> crate::error::Result<()> {
    let manifest = crate::http::agents::extract_manifest_from_package(package_path)?;
    if manifest.agent_id != agent_id {
        return Err(crate::error::GatewayError::Package(format!(
            "Package agent_id '{}' does not match upgrade target '{}'",
            manifest.agent_id, agent_id
        )));
    }

    std::fs::create_dir_all(dispatch.registry_dir).map_err(crate::error::GatewayError::Io)?;
    let registry_path = dispatch.registry_dir.join(format!("{agent_id}.agent"));
    std::fs::copy(package_path, &registry_path).map_err(crate::error::GatewayError::Io)?;

    // Same host selection as install: the local node dials the bind
    // host, remote nodes the advertise host (ADR-055 D3).
    let url_host = if dispatch.node_id == acowork_core::node::LOCAL_NODE_ID {
        if dispatch.http_host == "0.0.0.0" || dispatch.http_host == "::" {
            "127.0.0.1"
        } else {
            dispatch.http_host
        }
    } else {
        dispatch.advertise_host
    };
    let url = format!(
        "http://{url_host}:{}/api/packages/{agent_id}/download",
        dispatch.http_port
    );

    let control = cli_control_client(dispatch.mqtt_host, dispatch.mqtt_port).await?;
    control
        .upgrade_agent_by_url(dispatch.node_id, agent_id, &url, dispatch.dev_mode)
        .await
        .map_err(|e| crate::error::GatewayError::Lifecycle(e.to_string()))?;

    println!("Upgrade dispatched to node '{}': {agent_id}", dispatch.node_id);
    Ok(())
}

/// `uninstall --node <node>` — uninstall an agent on a target node
/// (blocking command round-trip, ADR-055 §6.13.3).
pub async fn uninstall_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
    agent_id: &str,
) -> crate::error::Result<()> {
    let control = cli_control_client(mqtt_host, mqtt_port).await?;
    match control.uninstall_agent(node_id, agent_id).await {
        Ok(event) if event.status == "ok" => {
            println!("Uninstalled '{agent_id}' on node '{node_id}'");
            Ok(())
        }
        Ok(event) => Err(crate::error::GatewayError::Lifecycle(event.message)),
        Err(e) => Err(crate::error::GatewayError::Lifecycle(e.to_string())),
    }
}

/// `start --node <node>` — start an agent on a target node (blocking
/// command round-trip, ADR-055 §6.13.3). Non-dev (production) start.
pub async fn start_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
    agent_id: &str,
) -> crate::error::Result<()> {
    let control = cli_control_client(mqtt_host, mqtt_port).await?;
    match control.start_agent(node_id, agent_id, false).await {
        Ok(event) if event.status == "ok" => {
            println!("Started '{agent_id}' on node '{node_id}'");
            Ok(())
        }
        Ok(event) => Err(crate::error::GatewayError::Lifecycle(event.message)),
        Err(e) => Err(crate::error::GatewayError::Lifecycle(e.to_string())),
    }
}

/// `stop --node <node>` — stop an agent on a target node (blocking
/// command round-trip, ADR-055 §6.13.3).
pub async fn stop_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
    agent_id: &str,
) -> crate::error::Result<()> {
    let control = cli_control_client(mqtt_host, mqtt_port).await?;
    match control.stop_agent(node_id, agent_id, "cli").await {
        Ok(event) if event.status == "ok" => {
            println!("Stopped '{agent_id}' on node '{node_id}'");
            Ok(())
        }
        Ok(event) => Err(crate::error::GatewayError::Lifecycle(event.message)),
        Err(e) => Err(crate::error::GatewayError::Lifecycle(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::extract_node_agent_id;

    #[test]
    fn extract_installed_agent_id() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-1/agents/com.example/installed", "gpu-1", "installed"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_events_agent_id() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-1/agents/com.example/events", "gpu-1", "events"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_wrong_node_is_none() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-2/agents/com.example/installed", "gpu-1", "installed"),
            None
        );
    }

    #[test]
    fn extract_wrong_kind_is_none() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-1/agents/com.example/status", "gpu-1", "installed"),
            None
        );
    }

    #[test]
    fn extract_malformed_is_none() {
        assert_eq!(extract_node_agent_id("acowork/nodes/gpu-1", "gpu-1", "installed"), None);
        assert_eq!(extract_node_agent_id("", "gpu-1", "installed"), None);
    }
}
