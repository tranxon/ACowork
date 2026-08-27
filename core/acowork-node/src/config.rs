//! Node Agent configuration (ADR-055 §6.12 directory layout).

use std::path::{Path, PathBuf};

use crate::error::{NodeError, Result};

/// Node Agent runtime configuration.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Node data directory — `$HOME/.acowork/acowork-node/` by default
    /// (overridable via `--home` / `ACOWORK_NODE_HOME`).
    pub home: PathBuf,
    /// Agent package install directory. `None` defaults to
    /// `{home}/packages` (§6.12). Overridable via `--packages-dir` /
    /// `ACOWORK_NODE_PACKAGES_DIR` — the Gateway sets this when
    /// spawning the local node so its package inventory stays on the
    /// Gateway's own `packages_dir` (ADR-055 Phase 2b.3 hard-cut
    /// compatibility).
    pub packages_dir: Option<PathBuf>,
    /// Gateway MQTT broker host to connect to.
    pub gateway_host: String,
    /// Gateway MQTT broker port to connect to.
    pub gateway_mqtt_port: u16,
    /// Explicit node name (`--name`). When `None`, a new identity is
    /// derived from the hostname.
    pub name: Option<String>,
    /// Enrollment token (Phase 5a — carried through the config so the
    /// CLI surface is already in place; the broker does not validate
    /// it yet).
    pub token: Option<String>,
    /// Maximum concurrent Runtime processes (§6.18).
    pub max_agents: u32,
    /// ADR-055 §6.3: the address other machines use to reach this
    /// node's reverse proxy. Defaults to `127.0.0.1` (single-machine
    /// topology); set to a non-loopback IP for remote deployments.
    pub advertise_host: String,
    /// Bind address for the node reverse proxy (§6.4). Defaults to
    /// `0.0.0.0` so remote nodes are reachable without reconfiguring
    /// the bind; the advertise_host remains the reachable address.
    pub proxy_bind: String,
    /// TCP port for the node reverse proxy (`/agents/{id}/*`).
    pub proxy_port: u16,
    /// TCP port for the node-local LSP relay sidecar (ADR-055 §6.7,
    /// Phase 4). The relay listens on `127.0.0.1:{port}` and its
    /// endpoint is advertised via the retained per-node `lsps` topic.
    pub lsp_relay_port: u16,
    /// Runtime log file max size in MB before auto-split (forwarded to
    /// spawned Runtimes, mirrors the Gateway default).
    pub log_file_size_mb: u64,
    /// Maximum Runtime log files to keep (0 = unlimited).
    pub log_file_count: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            home: default_node_home(),
            packages_dir: None,
            gateway_host: acowork_core::defaults::GATEWAY_MQTT_HOST.to_string(),
            gateway_mqtt_port: acowork_core::defaults::GATEWAY_MQTT_PORT,
            name: None,
            token: None,
            max_agents: acowork_core::node::NODE_DEFAULT_MAX_AGENTS,
            advertise_host: "127.0.0.1".to_string(),
            proxy_bind: "0.0.0.0".to_string(),
            proxy_port: acowork_core::node::NODE_PROXY_PORT,
            lsp_relay_port: crate::sidecar::lsp_relay::LSP_RELAY_DEFAULT_PORT,
            log_file_size_mb: 10,
            log_file_count: 20,
        }
    }
}

impl NodeConfig {
    /// Gateway address as `host:port` (the MQTT broker endpoint).
    pub fn gateway_addr(&self) -> String {
        format!("{}:{}", self.gateway_host, self.gateway_mqtt_port)
    }

    /// ADR-055 §6.3: the base URL of this node's reverse proxy as it
    /// should be advertised to Runtimes (`http://{advertise_host}:{port}`).
    /// The Runtime appends `/agents/{id}` when publishing its retained
    /// `http_endpoint`.
    pub fn proxy_advertise_endpoint(&self) -> String {
        format!("http://{}:{}", self.advertise_host, self.proxy_port)
    }

    /// Resolve the agent package install directory: the explicit
    /// `--packages-dir` override, or `{home}/packages` (§6.12).
    pub fn packages_dir(&self) -> PathBuf {
        self.packages_dir
            .clone()
            .unwrap_or_else(|| self.home.join("packages"))
    }

    /// Ensure the home directory tree exists (§6.12):
    /// `identity.json`, `logs/`, `packages/`, `runtime-logs/`.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in ["logs", "runtime-logs"] {
            let dir = self.home.join(dir);
            std::fs::create_dir_all(&dir).map_err(|e| {
                NodeError::Config(format!(
                    "Failed to create node directory '{}': {}",
                    dir.display(),
                    e
                ))
            })?;
        }
        let packages = self.packages_dir();
        std::fs::create_dir_all(&packages).map_err(|e| {
            NodeError::Config(format!(
                "Failed to create node packages directory '{}': {}",
                packages.display(),
                e
            ))
        })?;
        Ok(())
    }
}

/// Default node home directory, resolution order:
///   `ACOWORK_NODE_HOME` env > `$HOME/.acowork/acowork-node/`
///   (Windows: `%USERPROFILE%\.acowork\acowork-node`) > `./.acowork-node`.
///
/// The env override lets multi-instance runs and the ADR-055 node
/// topology verification isolate node state; the Gateway-spawned local
/// node inherits it from the parent environment automatically.
pub fn default_node_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("ACOWORK_NODE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    // Windows has no `HOME` env var (only `USERPROFILE`); without this
    // branch the node silently fell back to `./.acowork-node` in the cwd,
    // scattering node state across whatever directory started the process.
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

/// Resolve the node home from an optional `--home` override.
pub fn resolve_home(explicit: Option<&Path>) -> PathBuf {
    explicit.map(Path::to_path_buf).unwrap_or_else(default_node_home)
}

/// Best-effort system hostname without a dedicated crate: libc
/// `gethostname` on Unix, `COMPUTERNAME` on Windows, "localhost"
/// fallback.
pub fn system_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `gethostname` writes at most `buf.len()` bytes into
        // the provided buffer and NUL-terminates on success.
        let rc = unsafe {
            libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len())
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_addr_formats_host_port() {
        let cfg = NodeConfig {
            gateway_host: "192.168.1.10".to_string(),
            gateway_mqtt_port: 19875,
            ..NodeConfig::default()
        };
        assert_eq!(cfg.gateway_addr(), "192.168.1.10:19875");
    }

    #[test]
    fn ensure_dirs_creates_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = NodeConfig {
            home: tmp.path().join("node-home"),
            ..NodeConfig::default()
        };
        cfg.ensure_dirs().unwrap();
        assert!(cfg.home.join("logs").is_dir());
        assert!(cfg.home.join("packages").is_dir());
        assert!(cfg.home.join("runtime-logs").is_dir());
    }

    #[test]
    fn system_hostname_is_non_empty() {
        assert!(!system_hostname().is_empty());
    }
}
