//! Shell command execution tool
//!
//! Platform-aware shell registration: on Windows, Git Bash and PowerShell are
//! registered as separate tools so the LLM can prefer bash and fall back to
//! PowerShell. On Linux/macOS, a single "shell" tool uses the system shell.
//!
//! Runtime safety: each invocation checks whether the shell binary still exists
//! (e.g. user uninstalled Git) and returns an LLM-actionable error pointing to
//! the fallback tool instead of a cryptic "command not found".

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

/// A concrete shell executor registered as a tool.
///
/// Different instances represent different shells (bash, powershell, etc.)
/// so the LLM sees distinct tools with distinct descriptions and can make
/// informed choices about which to use.
pub struct ShellTool {
    /// Tool name exposed to LLM (e.g. "bash", "powershell", "shell")
    tool_name: String,
    /// Human-readable shell identifier for error messages
    shell_name: String,
    /// Shell binary to invoke (e.g. "bash", "pwsh", "/bin/zsh")
    shell_binary: String,
    /// Full path resolved at registration time (used for existence check)
    shell_path: String,
    /// CLI flag for passing a command string (e.g. "-c", "-Command")
    shell_arg: String,
    /// Working directory for command execution
    work_dir: String,
}

impl ShellTool {
    /// Create a shell tool with explicit binary path.
    ///
    /// `shell_path` is the fully-resolved path used for existence checks.
    /// `shell_binary` is what's passed to `std::process::Command::new()`.
    pub fn new(
        tool_name: &str,
        shell_name: &str,
        shell_binary: &str,
        shell_path: &str,
        shell_arg: &str,
        work_dir: &str,
    ) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            shell_name: shell_name.to_string(),
            shell_binary: shell_binary.to_string(),
            shell_path: shell_path.to_string(),
            shell_arg: shell_arg.to_string(),
            work_dir: work_dir.to_string(),
        }
    }

    /// Build the ToolSpec with a platform-appropriate description.
    ///
    /// Bash tools get Unix-style guidance; PowerShell tools get Windows-style
    /// guidance so the LLM produces syntactically correct commands.
    fn build_spec(&self) -> ToolSpec {
        let description = match self.tool_name.as_str() {
            "bash" => format!(
                "Execute a command in Git Bash (Unix-style shell on Windows). \
                 For absolute paths outside the workspace, prefer Windows format (e.g. 'C:/Users/...'). \
                 {fallback}",
                fallback = self.fallback_hint()
            ),
            "powershell" => format!(
                "Execute a command in {shell_name} ({shell_binary}). \
                 Use this if 'bash' is unavailable or for Windows-specific tasks. \
                 {fallback}",
                shell_name = self.shell_name,
                shell_binary = self.shell_binary,
                fallback = self.fallback_hint()
            ),
            _ => format!(
                "Execute a command in {} ({}). Use with caution.",
                self.shell_name, self.shell_binary
            ),
        };

        ToolSpec {
            name: self.tool_name.clone(),
            description,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    /// Hint about fallback tools when this shell is unavailable at runtime.
    fn fallback_hint(&self) -> String {
        match self.tool_name.as_str() {
            "bash" => "If this tool returns an error about 'bash' not found, \
                       use the 'powershell' tool instead — Git Bash may have \
                       been uninstalled or moved.".to_string(),
            "powershell" => "If this tool returns an error about 'powershell' not found, \
                             try the 'bash' tool if available.".to_string(),
            _ => String::new(),
        }
    }

    /// Check whether the shell binary still exists on disk.
    ///
    /// Covers the case where Git (bash.exe) or PowerShell was uninstalled
    /// after the agent process started.
    fn binary_exists(&self) -> bool {
        // Fast path: check the resolved path from registration time
        if Path::new(&self.shell_path).exists() {
            return true;
        }
        // Slow path: try executing a trivial command using the shell's own
        // argument convention. This is the same approach used by detect_shell()
        // and is more reliable than `where`/`which` on Windows where PowerShell
        // may be registered differently.
        let test_cmd = if cfg!(windows) { "echo ok" } else { "true" };
        std::process::Command::new(&self.shell_binary)
            .arg(&self.shell_arg)
            .arg(test_cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        self.build_spec()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let command = params["command"].as_str().unwrap_or("");

        if command.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'command' parameter".to_string()),
                token_usage: None,
            });
        }

        // Runtime existence check — handles post-startup uninstall of Git/PowerShell
        if !self.binary_exists() {
            let hint = self.fallback_hint();
            let error_msg = if hint.is_empty() {
                format!(
                    "Shell binary '{}' ({}) not found. \
                     This may happen if the shell was uninstalled or moved. \
                     This command was NOT executed: {}",
                    self.shell_name, self.shell_path, command
                )
            } else {
                format!(
                    "Shell binary '{}' ({}) not found. {} \
                     This command was NOT executed: {}",
                    self.shell_name, self.shell_path, hint, command
                )
            };
            tracing::warn!(
                tool = %self.tool_name,
                shell_binary = %self.shell_binary,
                shell_path = %self.shell_path,
                "shell: binary not found at runtime"
            );
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(error_msg),
                token_usage: None,
            });
        }

        tracing::debug!(
            command = %command,
            shell = %self.shell_binary,
            work_dir = %self.work_dir,
            "shell: executing command"
        );

        // Spawn child process synchronously, then wait for completion in a
        // blocking thread. We use std::process::Command instead of
        // tokio::process::Command for cross-platform compatibility:
        //
        // - On Windows, tokio::process::Command uses async named-pipe I/O
        //   which is incompatible with MinGW/MSYS2 programs like Git Bash
        //   (bash.exe), causing the process to hang until timeout.
        // - On Linux/macOS, std::process::Command is equally reliable and
        //   avoids an unnecessary dependency on tokio's process layer.
        //
        // Timeout behavior: the outer tokio::time::timeout in AgentLoop
        // cancels the .await on spawn_blocking's JoinHandle, which drops
        // the handle but does NOT interrupt the blocking work. The child
        // process continues until it exits naturally. This is acceptable
        // because:
        // 1. Shell commands are typically short-lived (seconds at most)
        // 2. The LLM receives a proper timeout error and can retry/fallback
        // 3. Tokio's blocking thread pool (default 512 threads) absorbs
        //    occasional stragglers without resource exhaustion
        //
        // For future hardening: consider ProcessGuard with kill-on-drop
        // using Arc<Mutex<Child>> to guarantee cleanup on timeout.
        let shell_path = self.shell_path.clone();
        let shell_arg = self.shell_arg.clone();
        let command_owned = command.to_string();
        let work_dir = self.work_dir.clone();
        let tool_name = self.tool_name.clone();

        let output = tokio::task::spawn_blocking(move || {
            // Use shell_path (fully-resolved path) instead of shell_binary
            // (just "bash") to avoid PATH resolution finding WSL bash
            // instead of Git Bash on Windows.
            let mut cmd = std::process::Command::new(&shell_path);
            cmd.arg(&shell_arg)
                .arg(&command_owned)
                .current_dir(&work_dir);

            // Ensure MSYS2 environment is properly initialized for Git Bash
            // so drive letter mounts (/c/, /d/) and Unix paths work correctly.
            if tool_name == "bash" {
                cmd.env("MSYSTEM", "MINGW64");
                cmd.env("CHERE_INVOKING", "1");
            }

            cmd.output()
        })
        .await;

        match output {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let content = if stderr.is_empty() {
                    stdout
                } else {
                    format!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}")
                };

                Ok(ToolResult {
                    ok: output.status.success(),
                    content,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!(
                            "Exit code: {}",
                            output.status.code().unwrap_or(-1)
                        ))
                    },
                    token_usage: None,
                })
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    command = %command,
                    error = %e,
                    "shell: failed to execute command"
                );
                Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to execute command: {e}")),
                    token_usage: None,
                })
            }
            Err(join_err) => {
                tracing::warn!(
                    command = %command,
                    error = %join_err,
                    "shell: spawn_blocking task panicked"
                );
                Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Internal error: shell task panicked: {join_err}"
                    )),
                    token_usage: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(name: &str, binary: &str) -> ShellTool {
        ShellTool::new(name, name, binary, binary, "-c", "/tmp")
    }

    #[tokio::test]
    async fn test_missing_command_parameter() {
        let tool = make_tool("bash", "bash");
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn test_empty_command_parameter() {
        let tool = make_tool("bash", "bash");
        let result = tool.execute(serde_json::json!({"command": ""})).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn test_runtime_binary_missing_error_includes_hint() {
        // Use a binary name that definitely does not exist
        let tool = ShellTool::new(
            "bash",
            "Git Bash",
            "definitely_does_not_exist_shell_xyz",
            "/definitely/not/a/real/path/bash.exe",
            "-c",
            "/tmp",
        );
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.unwrap();
        assert!(err.contains("not found"), "Error should mention binary not found: {}", err);
        assert!(err.contains("powershell"), "Error should hint at fallback tool: {}", err);
        assert!(err.contains("NOT executed"), "Error should state command was not executed: {}", err);
    }

    #[tokio::test]
    async fn test_valid_command_executes() {
        // Use detected_shells() which provides the fully-resolved path
        let shells = crate::platform::detected_shells();
        let primary = shells.first()
            .expect("Should have at least one available shell");

        let tool = ShellTool::new(
            &primary.tool_name,
            &primary.display_name,
            &primary.binary,
            &primary.path,
            &primary.arg,
            ".",
        );
        let result = tool
            .execute(serde_json::json!({"command": "echo hello_rollball"}))
            .await
            .unwrap();
        assert!(result.ok, "echo should succeed: {:?}", result.error);
        assert!(
            result.content.contains("hello_rollball"),
            "Output should contain echo text: {}",
            result.content
        );
    }
}
