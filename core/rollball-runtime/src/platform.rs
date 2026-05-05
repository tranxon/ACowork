//! Platform detection module
//!
//! Centralizes runtime platform detection (OS, architecture, shell) using
//! `std::env::consts::OS` (runtime), NOT `#[cfg]` (compile-time).
//! Shell detection runs once at first access and is cached via `OnceLock`.

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

static SHELL_INFO: OnceLock<ShellInfo> = OnceLock::new();

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
}
