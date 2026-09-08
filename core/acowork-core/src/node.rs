//! Node Agent control-plane constants and helpers (ADR-055).
//!
//! Shared between `acowork-node` (the Node Agent) and `acowork-gateway`
//! (the NodeRegistry / command publisher) so both sides derive topic
//! strings, client ids, and version gates from a single source of
//! truth. See `docs/adr/zh/ADR-055-remote-runtime-node-topology.md`
//! §6.2 (topic family), §6.12 (node identity), §6.9 (version
//! negotiation).

/// Node control-plane protocol version spoken by this build.
///
/// Bump when the `acowork/nodes/#` contract changes incompatibly.
/// The Gateway compares against [`NODE_MIN_SUPPORTED_PROTOCOL_VERSION`]
/// before issuing install/start commands (ADR-055 §6.9).
pub const NODE_PROTOCOL_VERSION: u32 = 1;

/// Minimum node protocol version the Gateway accepts commands to.
pub const NODE_MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 1;

/// Default capacity limit for Runtime processes per node
/// (ADR-055 §6.18).
pub const NODE_DEFAULT_MAX_AGENTS: u32 = 16;

/// Default TCP port for the Node's built-in reverse proxy
/// (`/agents/{id}/*` → machine-local Runtime loopback, ADR-055 §6.4).
pub const NODE_PROXY_PORT: u16 = 19900;

/// Base port for per-agent loopback HTTP ports allocated by the Node
/// when spawning Runtimes (ADR-055 §6.4 — the Node is the port
/// allocator so its reverse proxy can route `/agents/{id}/*`).
pub const NODE_HTTP_PORT_BASE: u16 = 19901;

/// MQTT client_id prefix for Node Agent connections
/// (`node:{node_id}`, aligned with the `agent:{id}` /
/// `gateway:publisher` convention in the protocol docs §8.5).
pub const NODE_CLIENT_ID_PREFIX: &str = "node:";

/// Maximum length of a node_id slug (ADR-055 §6.12).
const NODE_ID_MAX_LEN: usize = 32;

/// Build the MQTT client_id for a Node Agent.
pub fn node_client_id(node_id: &str) -> String {
    format!("{NODE_CLIENT_ID_PREFIX}{node_id}")
}

/// Topic: `acowork/nodes/{node_id}/status` (plain text "online"/"offline",
/// QoS 1 Retained + LWT — mirrors the agent status topic shape).
pub fn node_status_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/status")
}

/// Topic: `acowork/nodes/{node_id}/ready` (DataEnvelope<NodeReady>,
/// QoS 1 Retained — ADR-059 §7.2 Node control-plane readiness).
///
/// The Node publishes this AFTER MQTT CONNECT succeeds and every
/// control subscription is confirmed; the Gateway re-marks
/// `node.{node_id}` ready in its SubsystemReadinessRegistry on
/// receipt. An empty retained payload clears the snapshot (Node
/// shutdown / disconnect).
pub fn node_ready_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/ready")
}

/// Topic: `acowork/nodes/{node_id}/info` (NodeInfo envelope, Retained).
pub fn node_info_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/info")
}

/// Topic: `acowork/nodes/{node_id}/events` (node-level NodeEvent results).
pub fn node_events_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/events")
}

/// Topic: `acowork/nodes/{node_id}/agents/{agent_id}/control/{cmd}`
/// (Gateway → Node lifecycle command).
pub fn node_agent_control_topic(node_id: &str, agent_id: &str, cmd: &str) -> String {
    format!("acowork/nodes/{node_id}/agents/{agent_id}/control/{cmd}")
}

/// Topic: `acowork/nodes/{node_id}/agents/{agent_id}/events`
/// (per-agent NodeEvent results).
pub fn node_agent_events_topic(node_id: &str, agent_id: &str) -> String {
    format!("acowork/nodes/{node_id}/agents/{agent_id}/events")
}

/// Topic: `acowork/nodes/{node_id}/agents/{agent_id}/installed`
/// (per-agent installed-package inventory, Retained — ADR-055 §6.5).
pub fn node_agent_installed_topic(node_id: &str, agent_id: &str) -> String {
    format!("acowork/nodes/{node_id}/agents/{agent_id}/installed")
}

/// Topic: `acowork/nodes/{node_id}/lsps` (AvailableLsps envelope,
/// QoS 1 Retained — ADR-055 §6.7 node-local sidecar endpoint
/// distribution; replaces the deprecated `acowork/global/lsps`).
///
/// The Node publishes this on every LSP relay ready/unavailable
/// transition; the Runtime (codebase tool) and the Gateway
/// (`GET /api/agents/{id}/lsp-endpoint`) read it.
pub fn node_lsps_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/lsps")
}

/// Topic: `acowork/nodes/{node_id}/sidecars/{kind}/status` (retained
/// sidecar health snapshot — ADR-055 §6.7; migrates the legacy
/// `acowork/sidecar/+/status` family to the node topology).
///
/// `kind` is the `SidecarKind::as_str()` value (e.g. `lsp_relay`).
pub fn node_sidecar_status_topic(node_id: &str, kind: &str) -> String {
    format!("acowork/nodes/{node_id}/sidecars/{kind}/status")
}

/// Topic: `acowork/nodes/{node_id}/enroll` (NodeEnroll envelope,
/// QoS 1 non-retained — node enrollment handshake, ADR-055 §6.12
/// Phase 5a security).
pub fn node_enroll_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/enroll")
}

/// Topic: `acowork/nodes/{node_id}/enroll_result` (NodeEnrollResult
/// envelope, QoS 1 non-retained — Gateway → Node enrollment reply).
pub fn node_enroll_result_topic(node_id: &str) -> String {
    format!("acowork/nodes/{node_id}/enroll_result")
}

/// Validate a node_id slug: `^[a-z0-9]([a-z0-9-]{0,30}[a-z0-9])?$`
/// (lowercase letters / digits / hyphens, 2–32 chars, no leading or
/// trailing hyphen).
pub fn node_id_is_valid(node_id: &str) -> bool {
    if node_id.len() < 2 || node_id.len() > NODE_ID_MAX_LEN {
        return false;
    }
    let bytes = node_id.as_bytes();
    let is_slug_char =
        |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    if !bytes.iter().all(|&b| is_slug_char(b)) {
        return false;
    }
    // No leading/trailing hyphen (len >= 2 guarantees both ends exist).
    bytes[0] != b'-' && bytes[bytes.len() - 1] != b'-'
}

/// Normalize a hostname into a valid node_id slug: lowercase, map every
/// character outside `[a-z0-9]` to `-`, collapse nothing, truncate to
/// 32 chars, trim leading/trailing hyphens. Falls back to `"node"`
/// when the result would be empty or a single char (e.g. hosts named
/// "W" or "___").
pub fn node_id_from_hostname(hostname: &str) -> String {
    let slug: String = hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(NODE_ID_MAX_LEN)
        .collect();
    let trimmed = slug.trim_matches('-');
    if node_id_is_valid(trimmed) {
        trimmed.to_string()
    } else {
        "node".to_string()
    }
}

/// Best-effort machine hostname: libc `gethostname` on Unix,
/// `COMPUTERNAME` on Windows, `"localhost"` fallback (same portable
/// logic the Node Agent used in `acowork-node/src/config.rs`).
pub fn system_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `gethostname` writes at most `buf.len()` bytes into
        // the provided buffer and NUL-terminates on success.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..end]) {
                return s.to_string();
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            return name;
        }
    }
    "localhost".to_string()
}

/// Node id of the Gateway's own-machine Node Agent: the machine
/// hostname normalized into a slug (ADR-055 §6.11).
///
/// Unified naming rule: every Node Agent — whether spawned by the
/// Gateway or started manually on a remote machine — defaults to its
/// machine's hostname slug; there is no reserved `local` name anymore.
/// The Gateway computes this from its own hostname, which equals the
/// name the Node derives at first start (`--name` absent), so registry
/// lookups / topics / orphan-cleanup all agree.
pub fn local_node_id() -> String {
    node_id_from_hostname(&system_hostname())
}

/// Whether a node's reported protocol version can receive commands
/// from this build (ADR-055 §6.9 version negotiation gate).
pub fn is_node_protocol_compatible(reported: u32) -> bool {
    reported >= NODE_MIN_SUPPORTED_PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_follow_adr_055_layout() {
        assert_eq!(node_status_topic("gpu-1"), "acowork/nodes/gpu-1/status");
        assert_eq!(node_info_topic("gpu-1"), "acowork/nodes/gpu-1/info");
        assert_eq!(node_events_topic("gpu-1"), "acowork/nodes/gpu-1/events");
        assert_eq!(
            node_agent_control_topic("gpu-1", "com.example", "start"),
            "acowork/nodes/gpu-1/agents/com.example/control/start"
        );
        assert_eq!(
            node_agent_events_topic("gpu-1", "com.example"),
            "acowork/nodes/gpu-1/agents/com.example/events"
        );
        assert_eq!(
            node_agent_installed_topic("gpu-1", "com.example"),
            "acowork/nodes/gpu-1/agents/com.example/installed"
        );
        assert_eq!(
            node_lsps_topic("gpu-1"),
            "acowork/nodes/gpu-1/lsps"
        );
        assert_eq!(
            node_sidecar_status_topic("gpu-1", "lsp_relay"),
            "acowork/nodes/gpu-1/sidecars/lsp_relay/status"
        );
        assert_eq!(
            node_enroll_topic("gpu-1"),
            "acowork/nodes/gpu-1/enroll"
        );
        assert_eq!(
            node_enroll_result_topic("gpu-1"),
            "acowork/nodes/gpu-1/enroll_result"
        );
    }

    #[test]
    fn client_id_uses_colon_convention() {
        assert_eq!(node_client_id("local"), "node:local");
    }

    #[test]
    fn node_id_validation_accepts_valid_slugs() {
        assert!(node_id_is_valid("local"));
        assert!(node_id_is_valid("gpu-server"));
        assert!(node_id_is_valid("a1"));
        assert!(node_id_is_valid(&"a".repeat(32)));
    }

    #[test]
    fn node_id_validation_rejects_invalid_slugs() {
        assert!(!node_id_is_valid("a")); // too short
        assert!(!node_id_is_valid(&"a".repeat(33))); // too long
        assert!(!node_id_is_valid("-abc"));
        assert!(!node_id_is_valid("abc-"));
        assert!(!node_id_is_valid("Abc")); // uppercase
        assert!(!node_id_is_valid("ab_c")); // underscore
        assert!(!node_id_is_valid("ab.c")); // dot
        assert!(!node_id_is_valid("ab c")); // space
        assert!(!node_id_is_valid("")); // empty
    }

    #[test]
    fn hostname_normalization() {
        assert_eq!(node_id_from_hostname("NICHOLAS-PC"), "nicholas-pc");
        assert_eq!(node_id_from_hostname("My_Server.01"), "my-server-01");
        assert_eq!(node_id_from_hostname("--"), "node"); // all-invalid
        assert_eq!(node_id_from_hostname("W"), "node"); // single char
        // 40-char hostname truncates to 32 valid slug chars
        let long = "abcdefghij".repeat(4); // 40 chars
        let slug = node_id_from_hostname(&long);
        assert_eq!(slug.len(), 32);
        assert!(node_id_is_valid(&slug));
    }

    #[test]
    fn protocol_version_gate() {
        assert!(is_node_protocol_compatible(1));
        assert!(is_node_protocol_compatible(2));
        assert!(!is_node_protocol_compatible(0));
    }

    #[test]
    fn system_hostname_is_non_empty() {
        assert!(!system_hostname().is_empty());
    }

    #[test]
    fn local_node_id_is_valid_hostname_slug() {
        let id = local_node_id();
        assert!(node_id_is_valid(&id), "local_node_id() = '{id}' must be a valid slug");
        // The Gateway and the Node must derive the SAME id from the
        // same machine — the identity created at first start reuses
        // this exact function.
        assert_eq!(id, node_id_from_hostname(&system_hostname()));
    }
}

// ── Default Node home directory (shared with Gateway) ────────────────
//
// Single source of truth so `acowork-gateway` can derive a default
// `packages_dir` (= `<node_home>/packages`) without depending on
// `acowork-node` (ADR-055 §6.20 dependency red line). Previously
// lived in `acowork-node/src/config.rs`.
//
// Resolution order:
//   1. `ACOWORK_NODE_HOME` env (multi-instance isolation; inherited
//      by the Gateway-spawned local node via §6.13.2 unit)
//   2. Windows: `%USERPROFILE%\.acowork\acowork-node`
//      Unix:    `$HOME/.acowork/acowork-node`
//   3. `./.acowork-node` (cwd fallback when no HOME is set)

/// Default Node Agent home directory.
///
/// Platform conventions:
/// - Windows: `%USERPROFILE%\.acowork\acowork-node`
/// - Linux/macOS: `$HOME/.acowork/acowork-node`
///
/// Override at runtime via the `ACOWORK_NODE_HOME` env var (multi-instance
/// runs, ADR-055 node topology verification) or `acowork-node --home`.
pub fn default_node_home() -> std::path::PathBuf {
    use std::path::PathBuf;

    if let Some(dir) = std::env::var_os("ACOWORK_NODE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    // Windows has no `HOME` env var (only `USERPROFILE`); without this
    // branch the node silently falls back to `./.acowork-node` in the
    // cwd, scattering node state across whatever directory started
    // the process.
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE")
        && !profile.is_empty()
    {
        return PathBuf::from(profile)
            .join(".acowork")
            .join("acowork-node");
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".acowork")
            .join("acowork-node");
    }
    PathBuf::from(".").join(".acowork-node")
}

#[cfg(test)]
mod default_node_home_tests {
    use super::default_node_home;

    /// Serialize tests that mutate `ACOWORK_NODE_HOME` to prevent flaky
    /// failures from parallel env-var races (mirrors the pattern in
    /// `acowork-gateway/src/config.rs`).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn respects_acowork_node_home_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("ACOWORK_NODE_HOME");
        unsafe { std::env::set_var("ACOWORK_NODE_HOME", "/tmp/acowork-node-test") };
        assert_eq!(
            default_node_home(),
            std::path::PathBuf::from("/tmp/acowork-node-test")
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("ACOWORK_NODE_HOME", v) },
            None => unsafe { std::env::remove_var("ACOWORK_NODE_HOME") },
        }
    }

    #[test]
    fn empty_env_falls_through() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("ACOWORK_NODE_HOME");
        unsafe { std::env::set_var("ACOWORK_NODE_HOME", "") };
        // Empty is treated as unset — falls through to HOME/USERPROFILE.
        let path = default_node_home();
        assert!(!path.as_os_str().is_empty());
        match prev {
            Some(v) => unsafe { std::env::set_var("ACOWORK_NODE_HOME", v) },
            None => unsafe { std::env::remove_var("ACOWORK_NODE_HOME") },
        }
    }
}
