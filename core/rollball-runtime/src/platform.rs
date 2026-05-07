//! Platform detection module
//!
//! Centralizes runtime platform detection (OS, architecture, shell) using
//! `std::env::consts::OS` (runtime), NOT `#[cfg]` (compile-time).
//! Shell detection runs once at first access and is cached via `OnceLock`.
//!
//! On Windows, this module also detects Git Bash availability to support
//! multiple shell tools (bash + powershell) for LLM fallback chains.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Detected shell information
#[derive(Debug, Clone)]
pub struct ShellInfo {
    /// Shell binary name (e.g. "pwsh", "bash", "cmd")
    pub binary: &'static str,
    /// Shell argument flag (e.g. "-Command", "-c", "/C")
    pub arg: &'static str,
    /// Human-readable display name (e.g. "PowerShell 7 (pwsh)")
    pub display_name: &'static str,
}

/// Available shell tool descriptors for this platform.
///
/// On Windows, this may include both bash (Git Bash) and powershell.
/// On Linux/macOS, typically just one unified "shell" tool.
#[derive(Debug, Clone)]
pub struct AvailableShell {
    /// Tool name exposed to the LLM (e.g. "bash", "powershell", "shell")
    pub tool_name: String,
    /// Human-readable display name for errors and logging
    pub display_name: String,
    /// Shell binary to pass to Command::new()
    pub binary: String,
    /// Resolved path for existence checks at runtime
    pub path: String,
    /// CLI flag for passing a command string (e.g. "-c", "-Command")
    pub arg: String,
    /// Whether this is the primary/preferred shell on this platform
    pub is_primary: bool,
}

static SHELL_INFO: OnceLock<ShellInfo> = OnceLock::new();
static AVAILABLE_SHELLS: OnceLock<Vec<AvailableShell>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Git Bash detection (Windows only)
// ---------------------------------------------------------------------------

/// Known installation paths for Git Bash on Windows.
///
/// Checked in priority order — first match wins.
fn git_bash_candidates() -> Vec<PathBuf> {
    vec![
        // Default Git for Windows 64-bit
        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
        // Git for Windows 32-bit on 64-bit OS
        PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
        // Chocolatey
        PathBuf::from(r"C:\ProgramData\chocolatey\bin\bash.exe"),
    ]
}

/// Try to locate a Git Bash installation on Windows.
///
/// Checks known install locations first, then falls back to PATH lookup
/// using `where bash` (Windows equivalent of `which`).
/// Returns `(binary_name, full_path)` on success.
pub fn find_git_bash() -> Option<(String, String)> {
    if std::env::consts::OS != "windows" {
        return None;
    }

    // Check known install locations
    for candidate in git_bash_candidates() {
        if candidate.exists() {
            let path = candidate.to_string_lossy().to_string();
            tracing::info!(path = %path, "Found Git Bash at known location");
            return Some(("bash".to_string(), path));
        }
    }

    // Fallback: check PATH using Windows "where" command
    if let Ok(output) = std::process::Command::new("where")
        .arg("bash")
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(path_str) = stdout.lines().next() {
                let trimmed = path_str.trim();
                if !trimmed.is_empty() {
                    tracing::info!(path = %trimmed, "Found Git Bash via PATH (where bash)");
                    return Some(("bash".to_string(), trimmed.to_string()));
                }
            }
        }
    }

    tracing::debug!("Git Bash not found");
    None
}

// ---------------------------------------------------------------------------
// Shell detection (cached)
// ---------------------------------------------------------------------------

/// Detect the best available shell for the current platform.
///
/// Priority order:
/// - Windows: pwsh > powershell > cmd
/// - macOS:    $SHELL (zsh > bash > sh fallback)
/// - Linux:    $SHELL (bash > zsh > sh fallback)
fn detect_shell() -> ShellInfo {
    match std::env::consts::OS {
        "windows" => {
            // Prefer PowerShell 7 (pwsh) over Windows PowerShell 5.1 over cmd
            if std::process::Command::new("pwsh")
                .arg("--version")
                .output()
                .is_ok()
            {
                ShellInfo {
                    binary: "pwsh",
                    arg: "-Command",
                    display_name: "PowerShell 7 (pwsh)",
                }
            } else if std::process::Command::new("powershell")
                .arg("-Command")
                .arg("echo ok")
                .output()
                .is_ok()
            {
                ShellInfo {
                    binary: "powershell",
                    arg: "-Command",
                    display_name: "Windows PowerShell 5.1",
                }
            } else {
                ShellInfo {
                    binary: "cmd",
                    arg: "/C",
                    display_name: "cmd.exe",
                }
            }
        }
        "macos" => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
            if shell.contains("zsh") {
                ShellInfo {
                    binary: "zsh",
                    arg: "-c",
                    display_name: "zsh",
                }
            } else if shell.contains("bash") {
                ShellInfo {
                    binary: "bash",
                    arg: "-c",
                    display_name: "bash",
                }
            } else {
                ShellInfo {
                    binary: "sh",
                    arg: "-c",
                    display_name: "sh",
                }
            }
        }
        _ => {
            // Linux and other Unix-like systems
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            if shell.contains("bash") {
                ShellInfo {
                    binary: "bash",
                    arg: "-c",
                    display_name: "bash",
                }
            } else if shell.contains("zsh") {
                ShellInfo {
                    binary: "zsh",
                    arg: "-c",
                    display_name: "zsh",
                }
            } else {
                ShellInfo {
                    binary: "sh",
                    arg: "-c",
                    display_name: "sh",
                }
            }
        }
    }
}

/// Detect all available shells for tool registration.
///
/// Returns a list of shell descriptors that should be registered as tools.
/// The first entry (if any `is_primary`) is the preferred shell.
///
/// Platform behavior:
/// - **Windows**: Git Bash (if found) + PowerShell. Bash is primary when available.
/// - **macOS/Linux**: Single "shell" tool using the detected system shell.
pub fn detected_shells() -> Vec<AvailableShell> {
    AVAILABLE_SHELLS
        .get_or_init(|| {
            let mut shells = Vec::new();

            match std::env::consts::OS {
                "windows" => {
                    // Git Bash as primary tool (if available)
                    if let Some((binary, path)) = find_git_bash() {
                        shells.push(AvailableShell {
                            tool_name: "bash".to_string(),
                            display_name: "Git Bash".to_string(),
                            binary,
                            path,
                            arg: "-c".to_string(),
                            is_primary: true,
                        });
                    }

                    // PowerShell as secondary / fallback tool
                    let (pwsh_binary, pwsh_display) = if std::process::Command::new("pwsh")
                        .arg("--version")
                        .output()
                        .is_ok()
                    {
                        ("pwsh", "PowerShell 7 (pwsh)")
                    } else {
                        ("powershell", "Windows PowerShell 5.1")
                    };

                    // Resolve full path for PowerShell using "where" on Windows
                    let pwsh_path = std::process::Command::new("where")
                        .arg(pwsh_binary)
                        .output()
                        .ok()
                        .and_then(|out| {
                            if out.status.success() {
                                let stdout = String::from_utf8_lossy(&out.stdout);
                                stdout.lines().next().map(|s| s.trim().to_string())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| pwsh_binary.to_string());

                    shells.push(AvailableShell {
                        tool_name: "powershell".to_string(),
                        display_name: pwsh_display.to_string(),
                        binary: pwsh_binary.to_string(),
                        path: pwsh_path,
                        arg: "-Command".to_string(),
                        is_primary: shells.is_empty(), // primary only if no Git Bash
                    });
                }
                _ => {
                    // Linux / macOS: single unified "shell" tool
                    let info = detect_shell();
                    shells.push(AvailableShell {
                        tool_name: "shell".to_string(),
                        display_name: info.display_name.to_string(),
                        binary: info.binary.to_string(),
                        path: info.binary.to_string(), // on Unix, binary name is in PATH
                        arg: info.arg.to_string(),
                        is_primary: true,
                    });
                }
            }

            tracing::info!(
                count = shells.len(),
                tools = ?shells.iter().map(|s| s.tool_name.as_str()).collect::<Vec<_>>(),
                "Detected available shells for tool registration"
            );

            shells
        })
        .clone()
}

/// Get the detected shell info (cached after first call).
///
/// Uses `OnceLock` so detection only runs once per process.
pub fn detected_shell() -> &'static ShellInfo {
    SHELL_INFO.get_or_init(detect_shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detected_shell_returns_consistent_result() {
        let info1 = detected_shell();
        let info2 = detected_shell();
        // Same static reference — OnceLock guarantees single init
        assert!(std::ptr::eq(info1, info2));
    }

    #[test]
    fn test_shell_info_fields_are_non_empty() {
        let info = detected_shell();
        assert!(!info.binary.is_empty());
        assert!(!info.arg.is_empty());
        assert!(!info.display_name.is_empty());
    }

    #[test]
    fn test_detect_shell_platform_match() {
        let info = detect_shell();
        match std::env::consts::OS {
            "windows" => {
                assert!(matches!(info.binary, "pwsh" | "powershell" | "cmd"));
            }
            _ => {
                assert!(matches!(info.binary, "bash" | "zsh" | "sh"));
            }
        }
    }

    #[test]
    fn test_detected_shells_returns_at_least_one() {
        let shells = detected_shells();
        assert!(!shells.is_empty(), "Should have at least one shell tool");
    }

    #[test]
    fn test_detected_shells_idempotent() {
        let shells1 = detected_shells();
        let shells2 = detected_shells();
        assert_eq!(shells1.len(), shells2.len());
        for (a, b) in shells1.iter().zip(shells2.iter()) {
            assert_eq!(a.tool_name, b.tool_name);
            assert_eq!(a.binary, b.binary);
        }
    }
}
