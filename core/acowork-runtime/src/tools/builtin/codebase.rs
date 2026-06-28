//! Codebase tool — LSP-powered code intelligence for Agent Runtime.
//!
//! Connects to the LSP Relay's `/api/codebase/rpc` JSON-RPC endpoint to
//! perform code intelligence operations: go-to-definition, find references,
//! hover information, workspace symbol search, and diagnostics.
//!
//! ## Architecture
//!
//! ```text
//! Agent Runtime (codebase tool)
//!     │
//!     │ POST /api/codebase/rpc  (JSON-RPC over HTTP)
//!     ▼
//! acowork-lsp-relay
//!     │
//!     │ stdin/stdout (LSP protocol)
//!     ▼
//! Language Server (rust-analyzer, pyright, etc.)
//! ```
//!
//! The tool is conditionally registered: only when the Gateway reports
//! a running LSP Relay via `AgentHelloConfig.lsp_relay_endpoint`.

use std::time::Duration;

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Timeout for individual LSP requests via the relay.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The codebase tool — proxies LSP requests to the LSP Relay.
pub struct CodebaseTool {
    /// LSP Relay HTTP endpoint (e.g. "http://127.0.0.1:19878").
    relay_endpoint: String,
    /// HTTP client with timeout.
    client: reqwest::Client,
}

impl CodebaseTool {
    /// Create a new codebase tool connected to the given LSP Relay endpoint.
    pub fn new(relay_endpoint: String) -> Self {
        Self {
            relay_endpoint,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("Failed to build codebase HTTP client"),
        }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "codebase".to_string(),
            description: "Query the codebase using LSP (Language Server Protocol). \
                Supports: definition (go to definition), references (find all references), \
                hover (type info and documentation), symbol (workspace-wide symbol search), \
                diagnostic (get diagnostics for a file). \
                Requires a language server to be installed for the target language."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["definition", "references", "hover", "symbol", "diagnostic"],
                        "description": "The code intelligence action to perform"
                    },
                    "language": {
                        "type": "string",
                        "description": "Language id (e.g. 'rust', 'python', 'typescript')"
                    },
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the workspace (e.g. 'core/acowork-runtime/src/main.rs')"
                    },
                    "line": {
                        "type": "integer",
                        "description": "1-based line number (required for definition, references, hover)"
                    },
                    "character": {
                        "type": "integer",
                        "description": "1-based character/column number (required for definition, references, hover)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query for workspace/symbol (required for symbol action)"
                    }
                },
                "required": ["action", "language"]
            }),
        }
    }
}

#[async_trait]
impl Tool for CodebaseTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let action = params["action"].as_str().unwrap_or("");
        let language = params["language"].as_str().unwrap_or("");
        let file = params["file"].as_str().unwrap_or("");
        let line = params["line"].as_u64();
        let character = params["character"].as_u64();
        let query = params["query"].as_str().unwrap_or("");

        if language.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'language' parameter".to_string()),
                token_usage: None,
            });
        }

        // Resolve workspace root from work_dir (the agent's working directory).
        let workspace_root = work_dir.unwrap_or(".");

        // Build the LSP request based on the action.
        let (method, lsp_params) = match action {
            "definition" => {
                if file.is_empty() || line.is_none() || character.is_none() {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "Action 'definition' requires 'file', 'line', and 'character'"
                                .to_string(),
                        ),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/definition",
                    serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line.unwrap() - 1, "character": character.unwrap() - 1 }
                    }),
                )
            }
            "references" => {
                if file.is_empty() || line.is_none() || character.is_none() {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "Action 'references' requires 'file', 'line', and 'character'"
                                .to_string(),
                        ),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/references",
                    serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line.unwrap() - 1, "character": character.unwrap() - 1 },
                        "context": { "includeDeclaration": true }
                    }),
                )
            }
            "hover" => {
                if file.is_empty() || line.is_none() || character.is_none() {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "Action 'hover' requires 'file', 'line', and 'character'"
                                .to_string(),
                        ),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/hover",
                    serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line.unwrap() - 1, "character": character.unwrap() - 1 }
                    }),
                )
            }
            "symbol" => {
                if query.is_empty() {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some("Action 'symbol' requires 'query'".to_string()),
                        token_usage: None,
                    });
                }
                (
                    "workspace/symbol",
                    serde_json::json!({ "query": query }),
                )
            }
            "diagnostic" => {
                if file.is_empty() {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some("Action 'diagnostic' requires 'file'".to_string()),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/diagnostic",
                    serde_json::json!({
                        "textDocument": { "uri": uri }
                    }),
                )
            }
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Unknown action '{}'. Supported: definition, references, hover, symbol, diagnostic",
                        action
                    )),
                    token_usage: None,
                });
            }
        };

        // Call the LSP Relay.
        let url = format!("{}/api/codebase/rpc", self.relay_endpoint);
        let request_body = serde_json::json!({
            "language": language,
            "workspace_root": workspace_root,
            "method": method,
            "params": lsp_params,
            "expect_response": true,
        });

        match self
            .client
            .post(&url)
            .json(&request_body)
            .send()
            .await
        {
            Ok(resp) => {
                let body: Value = resp.json().await.unwrap_or_default();
                let success = body["success"].as_bool().unwrap_or(false);
                if success {
                    let result = body.get("result").cloned().unwrap_or(Value::Null);
                    let content = serde_json::to_string_pretty(&result).unwrap_or_default();
                    Ok(ToolResult {
                        ok: true,
                        content,
                        error: None,
                        token_usage: None,
                    })
                } else {
                    let error_msg = body["error"]
                        .as_str()
                        .unwrap_or("Unknown LSP error")
                        .to_string();
                    Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(error_msg),
                        token_usage: None,
                    })
                }
            }
            Err(e) => Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!("Failed to reach LSP Relay: {e}")),
                token_usage: None,
            }),
        }
    }
}

/// Build a `file://` URI from a workspace root and relative file path.
fn build_file_uri(workspace_root: &str, file: &str) -> String {
    let root = workspace_root.trim_end_matches('/');
    let file = file.trim_start_matches('/');
    format!("file://{}/{}", root, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_file_uri() {
        let uri = build_file_uri("/home/user/project", "src/main.rs");
        assert_eq!(uri, "file:///home/user/project/src/main.rs");
    }

    #[test]
    fn test_build_file_uri_trailing_slash() {
        let uri = build_file_uri("/home/user/project/", "/src/main.rs");
        assert_eq!(uri, "file:///home/user/project/src/main.rs");
    }

    #[test]
    fn test_spec_value() {
        let spec = CodebaseTool::spec_value();
        assert_eq!(spec.name, "codebase");
        assert!(spec.description.contains("LSP"));
    }
}
