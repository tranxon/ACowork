//! Orphan Runtime process discovery for re-adoption (ADR-055 §6.19).
//!
//! After a Node restart the in-memory process table is empty, but the
//! Runtime processes it spawned may still be running (a Node crash does
//! NOT kill Runtimes — §6.10). Re-adoption scans the local process
//! list for `acowork-runtime --agent-id {id}` command lines, parses the
//! spawn metadata back out, and lets the manager rebuild its process
//! table so the reverse proxy can route `/agents/{id}/*` again.
//!
//! The metadata needed to reconstruct an [`AgentSlot`] is deliberately
//! on the command line (`--agent-id`, `--http-port`, `--dev-mode`) —
//! the same parameterization that made re-adoption possible in the
//! first place (ADR-055 §6.19 point 3, referencing L1-2).
//!
//! `--work-dir` is NOT parsed from the command line: it can contain
//! spaces (breaks naive whitespace tokenization) and is instead
//! re-derived from the install table (`{install_path}/workspace`), the
//! same expression `ProcessManager::start_agent` uses.

use std::collections::HashMap;

use crate::state::InstalledAgent;

/// A candidate Runtime process discovered by scanning the local process
/// list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCandidate {
    /// PID on this machine.
    pub pid: u32,
    /// Agent id (`--agent-id`).
    pub agent_id: String,
    /// Loopback HTTP port the Runtime listens on (`--http-port`). This
    /// is required for the reverse proxy's `{id} → port` mapping.
    pub http_port: u16,
    /// Whether the Runtime runs the Debug Protocol (`--dev-mode`).
    pub dev_mode: bool,
}

/// Parse spawn metadata out of a Runtime command line (everything AFTER
/// the binary path, as produced by `spawn_agent_process`).
///
/// Returns `None` when the tokens are not a recognizable Runtime
/// invocation — specifically when `--agent-id` or `--http-port` is
/// missing, since both are required to reconstruct a routable process
/// table slot.
pub fn parse_runtime_args(pid: u32, args: &[String]) -> Option<RuntimeCandidate> {
    let mut agent_id: Option<String> = None;
    let mut http_port: Option<u16> = None;
    let mut dev_mode = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent-id" => {
                agent_id = args.get(i + 1).cloned();
                i += 2;
            }
            "--http-port" => {
                http_port = args.get(i + 1).and_then(|v| v.parse::<u16>().ok());
                i += 2;
            }
            "--dev-mode" => {
                dev_mode = true;
                i += 1;
            }
            _ => i += 1,
        }
    }

    let agent_id = agent_id?;
    let http_port = http_port?;
    Some(RuntimeCandidate {
        pid,
        agent_id,
        http_port,
        dev_mode,
    })
}

/// Classify scanned candidates against the node's install table.
///
/// A candidate is adoptable only when its agent is still installed —
/// an `acowork-runtime` left running for an agent that has since been
/// uninstalled is residual and is NOT adopted (it is reported and left
/// for the operator / a `start` command to sort out; we do not
/// SIGKILL unadopted processes on a best-effort scan).
///
/// Returns `(adopt, skip)` — the skip set is for diagnostics only.
pub fn classify_candidates(
    candidates: Vec<RuntimeCandidate>,
    installed: &HashMap<String, InstalledAgent>,
) -> (Vec<RuntimeCandidate>, Vec<RuntimeCandidate>) {
    candidates
        .into_iter()
        .partition(|c| installed.contains_key(&c.agent_id))
}

/// Scan the local process list for `acowork-runtime` processes and
/// parse their spawn metadata (ADR-055 §6.19).
///
/// Unix (Linux/macOS): `ps -axo pid=,args=`. Windows: `Get-CimInstance
/// Win32_Process`. The scan is best-effort — failures degrade to an
/// empty result (logged) and never fail startup.
pub async fn scan_runtime_processes() -> Vec<RuntimeCandidate> {
    let bin_name = if cfg!(windows) {
        "acowork-runtime.exe"
    } else {
        "acowork-runtime"
    };
    #[cfg(windows)]
    {
        scan_windows(bin_name).await
    }
    #[cfg(not(windows))]
    {
        scan_unix(bin_name).await
    }
}

#[cfg(not(windows))]
async fn scan_unix(bin_name: &str) -> Vec<RuntimeCandidate> {
    let output = match tokio::process::Command::new("ps")
        .args(["-axo", "pid=,args="])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "Re-adopt: failed to run `ps` — skipping orphan scan");
            return Vec::new();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| parse_ps_line(line, bin_name))
        .collect()
}

/// Parse a single `ps -axo pid=,args=` output line into a candidate.
///
/// Pure function (unit-testable without spawning `ps`): the first
/// whitespace-delimited token is the right-aligned PID, the remainder
/// is the full command line. `argv[0]` must end with the runtime binary
/// name (also drops the `ps`/`grep` style self-matches).
#[cfg(not(windows))]
fn parse_ps_line(line: &str, bin_name: &str) -> Option<RuntimeCandidate> {
    let line = line.trim();
    let space_idx = line.find(' ')?;
    let pid: u32 = line[..space_idx].trim().parse().ok()?;
    let args_str = line[space_idx..].trim();
    let tokens: Vec<String> = args_str.split_whitespace().map(str::to_string).collect();
    if !tokens
        .first()
        .map(|t| t.ends_with(bin_name))
        .unwrap_or(false)
    {
        return None;
    }
    parse_runtime_args(pid, &tokens[1..])
}

#[cfg(windows)]
async fn scan_windows(bin_name: &str) -> Vec<RuntimeCandidate> {
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='{}'\" | Select-Object ProcessId,CommandLine | ConvertTo-Json",
        bin_name
    );
    let output = match tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "Re-adopt: failed to run powershell — skipping orphan scan");
            return Vec::new();
        }
    };

    #[derive(serde::Deserialize)]
    struct Proc {
        #[serde(rename = "ProcessId")]
        pid: u32,
        #[serde(rename = "CommandLine", default)]
        command_line: String,
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let procs: Vec<Proc> = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => {
            // ConvertTo-Json returns a single object (not an array) when
            // only one process matches.
            match serde_json::from_str::<Proc>(&stdout) {
                Ok(single) => vec![single],
                Err(_) => Vec::new(),
            }
        }
    };

    let mut found = Vec::new();
    for p in procs {
        if p.command_line.is_empty() {
            continue;
        }
        // Minimal Windows command-line tokenization — enough to locate
        // the flag/value pairs we need (`--agent-id` / `--http-port`
        // never contain spaces).
        let tokens: Vec<String> = p.command_line.split_whitespace().map(str::to_string).collect();
        if !tokens
            .first()
            .map(|t| t.ends_with(bin_name))
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(c) = parse_runtime_args(p.pid, &tokens[1..]) {
            found.push(c);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_full_command_line() {
        let c = parse_runtime_args(
            4242,
            &args(&[
                "--agent-id",
                "com.example.weather",
                "--package-path",
                "/home/u/.acowork/acowork-node/packages/com.example.weather",
                "--manifest-path",
                "/home/u/.acowork/acowork-node/packages/com.example.weather/manifest.toml",
                "--work-dir",
                "/home/u/.acowork/acowork-node/packages/com.example.weather/workspace",
                "--gateway-host",
                "192.168.1.10",
                "--node-id",
                "gpu-server",
                "--mqtt-port",
                "19875",
                "--http-port",
                "19905",
                "--http-advertise-endpoint",
                "http://192.168.1.10:19900",
                "--log-level",
                "info",
                "--log-file-size-mb",
                "10",
                "--log-file-count",
                "20",
            ]),
        )
        .unwrap();
        assert_eq!(c.pid, 4242);
        assert_eq!(c.agent_id, "com.example.weather");
        assert_eq!(c.http_port, 19905);
        assert!(!c.dev_mode);
    }

    #[test]
    fn parse_dev_mode_flag() {
        let c = parse_runtime_args(
            7,
            &args(&[
                "--agent-id",
                "com.example.dev",
                "--http-port",
                "19901",
                "--dev-mode",
                "--debug-port",
                "19878",
            ]),
        )
        .unwrap();
        assert!(c.dev_mode);
        assert_eq!(c.agent_id, "com.example.dev");
    }

    #[test]
    fn parse_missing_http_port_is_none() {
        // Without --http-port the reverse proxy cannot route the
        // orphan, so it is not adoptable.
        assert!(parse_runtime_args(1, &args(&["--agent-id", "com.example.x"])).is_none());
    }

    #[test]
    fn parse_missing_agent_id_is_none() {
        assert!(parse_runtime_args(1, &args(&["--http-port", "19901"])).is_none());
    }

    #[test]
    fn parse_empty_args_is_none() {
        assert!(parse_runtime_args(1, &[]).is_none());
    }

    fn test_manifest(agent_id: &str) -> acowork_core::AgentManifest {
        acowork_core::AgentManifest::from_toml(&format!(
            "agent_id = \"{agent_id}\"\nversion = \"1.0.0\"\nname = \"Keep\"\ndescription = \"test\"\nauthor = \"test\"\nruntime_version = \"1.0.0\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn classify_adopts_installed_and_skips_unknown() {
        let mut installed: HashMap<String, InstalledAgent> = HashMap::new();
        installed.insert(
            "com.example.keep".to_string(),
            InstalledAgent {
                agent_id: "com.example.keep".to_string(),
                version: "1.0.0".to_string(),
                name: "Keep".to_string(),
                install_path: "/tmp/com.example.keep".to_string(),
                manifest: test_manifest("com.example.keep"),
            },
        );

        let candidates = vec![
            RuntimeCandidate {
                pid: 10,
                agent_id: "com.example.keep".to_string(),
                http_port: 19901,
                dev_mode: false,
            },
            RuntimeCandidate {
                pid: 11,
                agent_id: "com.example.stale".to_string(),
                http_port: 19902,
                dev_mode: false,
            },
        ];

        let (adopt, skip) = classify_candidates(candidates, &installed);
        assert_eq!(adopt.len(), 1);
        assert_eq!(adopt[0].agent_id, "com.example.keep");
        assert_eq!(skip.len(), 1);
        assert_eq!(skip[0].agent_id, "com.example.stale");
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_ps_line_extracts_pid_and_args() {
        // Real `ps -axo pid=,args=` shape: right-aligned PID + args.
        let c = parse_ps_line(
            " 4242 /usr/local/bin/acowork-runtime --agent-id com.example.weather --mqtt-port 19875 --http-port 19905 --log-level info",
            "acowork-runtime",
        )
        .unwrap();
        assert_eq!(c.pid, 4242);
        assert_eq!(c.agent_id, "com.example.weather");
        assert_eq!(c.http_port, 19905);
        assert!(!c.dev_mode);
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_ps_line_rejects_non_runtime_binary() {
        // A `grep`/`sh` process mentioning the binary name must not match.
        assert!(parse_ps_line(" 100 /usr/bin/grep acowork-runtime", "acowork-runtime").is_none());
        // A different binary with the same flags must not match either.
        assert!(
            parse_ps_line(" 100 /usr/bin/other --agent-id com.x --http-port 1", "acowork-runtime")
                .is_none()
        );
    }
}
