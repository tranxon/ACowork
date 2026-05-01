//! Shell command execution tool

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

/// Shell tool — executes commands in the system shell
pub struct ShellTool {
    work_dir: String,
}

impl ShellTool {
    /// Create a new shell tool with the given working directory
    pub fn new(work_dir: &str) -> Self {
        Self {
            work_dir: work_dir.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "shell".to_string(),
            description: "Execute a shell command and return the output. Use with caution.".to_string(),
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
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
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

        // Detect platform at runtime and choose shell accordingly
        let (shell, shell_arg) = if std::env::consts::OS == "windows" {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        tracing::debug!(
            command = %command,
            shell = %shell,
            work_dir = %self.work_dir,
            "shell: executing command"
        );

        let output = tokio::process::Command::new(shell)
            .arg(shell_arg)
            .arg(command)
            .current_dir(&self.work_dir)
            .output()
            .await;

        match output {
            Ok(output) => {
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
                    error: if output.status.success() { None } else { Some(format!("Exit code: {}", output.status.code().unwrap_or(-1))) },
                    token_usage: None,
                })
            }
            Err(e) => {
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
        }
    }
}
