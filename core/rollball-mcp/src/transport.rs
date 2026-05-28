//! MCP transport abstraction — supports stdio and HTTP transports.
//!
//! Adapted from zeroclaw/src/tools/mcp_transport.rs
//! Rollball deviation: simplified for Phase 1 (no SSE transport)
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_stream::StreamExt;

use crate::config::McpServerConfig;
use crate::protocol::{JSONRPC_VERSION, JsonRpcRequest, JsonRpcResponse};

/// Maximum bytes for a single JSON-RPC response.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024; // 4 MB

/// Timeout for init/list operations.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Streamable HTTP Accept header required by MCP HTTP transport.
const MCP_STREAMABLE_ACCEPT: &str = "application/json, text/event-stream";
/// Default media type for MCP JSON-RPC request bodies.
const MCP_JSON_CONTENT_TYPE: &str = "application/json";
/// Streamable HTTP session header used to preserve MCP server state.
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";

// ── Transport Trait ──────────────────────────────────────────────────────

/// Abstract transport for MCP communication.
#[async_trait]
pub trait McpTransportConn: Send + Sync {
    /// Send a JSON-RPC request and receive the response.
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse>;

    /// Close the connection.
    async fn close(&mut self) -> Result<()>;
}

// ── Stdio Transport ──────────────────────────────────────────────────────

/// Spawn an MCP server command, with Windows fallback for extension-less
/// commands (e.g. `npx` from NVM symlinks).  On Windows, if the raw command
/// fails to spawn and lacks a file extension, retry with `.cmd` appended.
fn spawn_mcp_command(
    command: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> Result<Child> {
    let build_cmd = |cmd: &str| -> std::io::Result<Child> {
        let mut c = Command::new(cmd);
        c.args(args).envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        c.spawn()
    };

    let result = build_cmd(command);

    // On Windows, commands without extension (like NVM `npx` symlink)
    // can't be spawned directly.  Retry with `.cmd` suffix.
    #[cfg(windows)]
    if result.is_err() && !command.contains('.') {
        let cmd_ext = format!("{command}.cmd");
        if let Ok(child) = build_cmd(&cmd_ext) {
            return Ok(child);
        }
    }

    result.map_err(Into::into)
}

/// Stdio-based transport (spawn local process).
pub struct StdioTransport {
    _child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl StdioTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let server_name = config.name.clone();
        let mut child = spawn_mcp_command(&config.command, &config.args, &config.env)
            .with_context(|| format!("failed to spawn MCP server `{}`", config.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("no stdin on MCP server `{}`", config.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout on MCP server `{}`", config.name))?;
        let stdout_lines = BufReader::new(stdout).lines();

        // Capture stderr in a background task and forward to tracing
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "mcp::stderr", server = %server_name, "{}", line);
                }
            });
        }

        Ok(Self {
            _child: child,
            stdin,
            stdout_lines,
        })
    }

    async fn send_raw(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("failed to write to MCP server stdin")?;
        self.stdin
            .write_all(b"\n")
            .await
            .context("failed to write newline to MCP server stdin")?;
        self.stdin.flush().await.context("failed to flush stdin")?;
        Ok(())
    }

    async fn recv_raw(&mut self) -> Result<String> {
        let line = self
            .stdout_lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("MCP server closed stdout"))?;
        if line.len() > MAX_LINE_BYTES {
            bail!("MCP response too large: {} bytes", line.len());
        }
        Ok(line)
    }
}

#[async_trait]
impl McpTransportConn for StdioTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let line = serde_json::to_string(request)?;
        self.send_raw(&line).await?;
        if request.id.is_none() {
            return Ok(JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: None,
                result: None,
                error: None,
            });
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(RECV_TIMEOUT_SECS);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for MCP response");
            }
            let resp_line = timeout(remaining, self.recv_raw())
                .await
                .context("timeout waiting for MCP response")??;
            let resp: JsonRpcResponse = serde_json::from_str(&resp_line)
                .with_context(|| format!("invalid JSON-RPC response: {}", resp_line))?;
            if resp.id.is_none() {
                // Server-sent notification — skip and keep waiting
                tracing::debug!("MCP stdio: skipping server notification while waiting for response");
                continue;
            }
            return Ok(resp);
        }
    }

    async fn close(&mut self) -> Result<()> {
        let _ = self.stdin.shutdown().await;
        Ok(())
    }
}

// ── HTTP Transport ───────────────────────────────────────────────────────

/// HTTP-based transport (POST requests, streamable HTTP).
pub struct HttpTransport {
    url: String,
    client: reqwest::Client,
    headers: HashMap<String, String>,
    session_id: Option<String>,
}

impl HttpTransport {
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("URL required for HTTP transport"))?
            .clone();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            url,
            client,
            headers: config.headers.clone(),
            session_id: None,
        })
    }

    fn apply_session_header(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(session_id) = self.session_id.as_deref() {
            req.header(MCP_SESSION_ID_HEADER, session_id)
        } else {
            req
        }
    }

    fn update_session_id_from_headers(&mut self, headers: &reqwest::header::HeaderMap) {
        if let Some(session_id) = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }
    }
}

#[async_trait]
impl McpTransportConn for HttpTransport {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let body = serde_json::to_string(request)?;

        let has_accept = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Accept"));
        let has_content_type = self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("Content-Type"));

        let mut req = self.client.post(&self.url).body(body);
        if !has_content_type {
            req = req.header("Content-Type", MCP_JSON_CONTENT_TYPE);
        }
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        req = self.apply_session_header(req);
        if !has_accept {
            req = req.header("Accept", MCP_STREAMABLE_ACCEPT);
        }

        let resp = req
            .send()
            .await
            .context("HTTP request to MCP server failed")?;

        if !resp.status().is_success() {
            bail!("MCP server returned HTTP {}", resp.status());
        }

        self.update_session_id_from_headers(resp.headers());

        if request.id.is_none() {
            return Ok(JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: None,
                result: None,
                error: None,
            });
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));

        if is_sse {
            let maybe_resp = timeout(
                Duration::from_secs(RECV_TIMEOUT_SECS),
                read_first_jsonrpc_from_sse_response(resp),
            )
            .await
            .context("timeout waiting for MCP response from SSE stream")??;
            return maybe_resp
                .ok_or_else(|| anyhow!("MCP server returned no response in SSE stream"));
        }

        let resp_text = resp.text().await.context("failed to read HTTP response")?;
        parse_jsonrpc_response_text(&resp_text)
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

// ── SSE helpers (used by HTTP transport for streamable HTTP responses) ───

fn extract_json_from_sse_text(resp_text: &str) -> std::borrow::Cow<'_, str> {
    let text = resp_text.trim_start_matches('\u{feff}');
    let mut current_data_lines: Vec<&str> = Vec::new();
    let mut last_event_data_lines: Vec<&str> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r').trim_start();
        if line.is_empty() {
            if !current_data_lines.is_empty() {
                last_event_data_lines = std::mem::take(&mut current_data_lines);
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            current_data_lines.push(rest);
        }
    }

    if !current_data_lines.is_empty() {
        last_event_data_lines = current_data_lines;
    }
    if last_event_data_lines.is_empty() {
        return std::borrow::Cow::Borrowed(text.trim());
    }
    if last_event_data_lines.len() == 1 {
        return std::borrow::Cow::Borrowed(last_event_data_lines[0].trim());
    }
    std::borrow::Cow::Owned(last_event_data_lines.join("\n").trim().to_string())
}

fn looks_like_sse_text(text: &str) -> bool {
    text.starts_with("data:")
        || text.starts_with("event:")
        || text.contains("\ndata:")
        || text.contains("\nevent:")
}

fn parse_jsonrpc_response_text(resp_text: &str) -> Result<JsonRpcResponse> {
    let trimmed = resp_text.trim();
    if trimmed.is_empty() {
        bail!("MCP server returned no response");
    }
    let json_text = if looks_like_sse_text(trimmed) {
        extract_json_from_sse_text(trimmed)
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };
    let mcp_resp: JsonRpcResponse = serde_json::from_str(json_text.as_ref())
        .with_context(|| format!("invalid JSON-RPC response: {}", resp_text))?;
    Ok(mcp_resp)
}

async fn read_first_jsonrpc_from_sse_response(
    resp: reqwest::Response,
) -> Result<Option<JsonRpcResponse>> {
    let stream = resp
        .bytes_stream()
        .map(|item| item.map_err(std::io::Error::other));
    let reader = tokio_util::io::StreamReader::new(stream);
    let mut lines = BufReader::new(reader).lines();

    let mut cur_event: Option<String> = None;
    let mut cur_data: Vec<String> = Vec::new();

    while let Ok(line_opt) = lines.next_line().await {
        let Some(mut line) = line_opt else { break };
        if line.ends_with('\r') {
            line.pop();
        }
        if line.is_empty() {
            if cur_event.is_none() && cur_data.is_empty() {
                continue;
            }
            let event = cur_event.take().unwrap_or_else(|| "message".to_string());
            let data = cur_data.join("\n");
            cur_data.clear();

            if event.eq_ignore_ascii_case("endpoint") || event.eq_ignore_ascii_case("mcp-endpoint") {
                continue;
            }
            if !event.eq_ignore_ascii_case("message") {
                continue;
            }
            let trimmed = data.trim();
            if trimmed.is_empty() {
                continue;
            }
            let json_str = extract_json_from_sse_text(trimmed);
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(json_str.as_ref()) {
                return Ok(Some(resp));
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            cur_event = Some(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            cur_data.push(rest.to_string());
        }
    }
    Ok(None)
}

// ── Factory ──────────────────────────────────────────────────────────────

/// Create a transport based on config.
pub fn create_transport(config: &McpServerConfig) -> Result<Box<dyn McpTransportConn>> {
    match config.transport {
        crate::config::McpTransportDef::Stdio => Ok(Box::new(StdioTransport::new(config)?)),
        crate::config::McpTransportDef::Http => Ok(Box::new(HttpTransport::new(config)?)),
        crate::config::McpTransportDef::Sse => {
            // SSE transport uses the same HTTP client path for streamable HTTP
            Ok(Box::new(HttpTransport::new(config)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpTransportDef;

    #[test]
    fn test_create_transport_stdio_fails_with_no_command() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransportDef::Stdio,
            command: "".into(),
            ..Default::default()
        };
        assert!(create_transport(&config).is_err());
    }

    #[test]
    fn test_create_transport_http_requires_url() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransportDef::Http,
            ..Default::default()
        };
        assert!(create_transport(&config).is_err());
    }

    #[test]
    fn test_create_transport_http_with_url_succeeds() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransportDef::Http,
            url: Some("http://localhost:9999/mcp".into()),
            ..Default::default()
        };
        assert!(create_transport(&config).is_ok());
    }

    #[test]
    fn test_parse_jsonrpc_response_handles_plain_json() {
        let parsed = parse_jsonrpc_response_text("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
            .expect("plain JSON response should parse");
        assert_eq!(parsed.id, Some(serde_json::json!(1)));
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_parse_jsonrpc_response_handles_sse_framed_json() {
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
        let parsed =
            parse_jsonrpc_response_text(sse).expect("SSE-framed JSON response should parse");
        assert_eq!(parsed.id, Some(serde_json::json!(2)));
        assert_eq!(
            parsed
                .result
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_extract_json_from_sse_data_no_space() {
        let input = "data:{\"jsonrpc\":\"2.0\",\"result\":{}}\n\n";
        let extracted = extract_json_from_sse_text(input);
        let _: JsonRpcResponse = serde_json::from_str(extracted.as_ref()).unwrap();
    }

    #[test]
    fn test_looks_like_sse_detects_data_prefix() {
        assert!(looks_like_sse_text("data:{\"jsonrpc\":\"2.0\"}"));
    }

    #[test]
    fn test_looks_like_sse_plain_json_is_not_sse() {
        assert!(!looks_like_sse_text(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"
        ));
    }
}
