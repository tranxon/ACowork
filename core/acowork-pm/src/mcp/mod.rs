//! MCP (Model Context Protocol) HTTP Server —— JSON-RPC over streamable HTTP。
//!
//! ## 协议
//!
//! 服务端实现 [MCP streamable HTTP](https://modelcontextprotocol.io/) 子集，
//! 单一 `POST /mcp` 端点，请求/响应均为 JSON-RPC 2.0：
//!
//! | JSON-RPC method | 说明 |
//! |-----------------|------|
//! | `initialize` | 握手：返回协议版本 + `capabilities.tools` + `serverInfo` |
//! | `notifications/initialized` | 客户端初始化完成通知（**不**回响应，返回 202） |
//! | `tools/list` | 列出工具（见 [`manifest::PM_TOOL_MANIFEST`]） |
//! | `tools/call` | 调用工具（见 [`tools::dispatch`]） |
//!
//! 服务端**无状态**（不维护 `Mcp-Session-Id`），每次请求独立鉴权。
//! 响应始终为 `application/json`（客户端 `HttpTransport` 同时支持 JSON 与
//! SSE 响应，取 JSON 即可）。
//!
//! ## 与 REST API 的关系
//!
//! MCP 是 REST 的**语义等价**子集——所有 `pm_*` 工具背后调用同一个
//! [`PmStore`] trait。Agent 经 MCP 调用时服务端自动：
//!
//! - 从 `X-MCP-Actor` header 读取调用方 `agent_id`（由 Gateway catalog
//!   注入，见设计 §6.1 / T3-4）
//! - 执行身份校验（设计 §9.2 / §9.3）：匿名只读；状态变更工具校验
//!   调用者 == 任务 `assignee`；`pm_create_task` 校验 assignee 存在
//! - 返回精简 JSON（避免向 LLM 暴露内部 details）
//!
//! ## 错误语义
//!
//! - 协议级错误（parse / invalid request / method not found / invalid
//!   params）→ 标准 JSON-RPC error code（-32700 / -32600 / -32601 / -32602）
//! - 工具级业务错误（`PmError`，含 `Forbidden` / `DependencyNotSatisfied`
//!   等）→ JSON-RPC error，code 见 [`crate::error::PmError::mcp_error_code`]，
//!   message 为 `"{error_code}: {detail}"`。
//!
//!   之所以走 JSON-RPC error 而非 `isError:true` content：acowork-mcp
//!   `McpToolWrapper` 会把 error message 分类为 `[permission]` / `[permanent]`
//!   / `[transient]` 前缀返回给 LLM，Agent 才能做出正确决策（如 403 不重试、
//!   依赖未满足稍后重试）。
//!
//! ## 挂载
//!
//! `mcp_router` 返回 `Router<()>`（内部 `.with_state(McpState)` 注入 state），
//! 由 [`crate::server::PmService::router`] 与 REST `pm_router` 合并后，经
//! Gateway `nest_service("/api/pm", ...)` 挂载。公开端点：
//! `http://{gw}/api/pm/mcp`（设计 §6 / §8）。
//!
//! ## 设计参考
//!
//! [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §6 / §8 / §9

pub mod manifest;
pub mod tools;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::error::PmError;
use crate::store::tree::TreePmStore;

// ── 状态与契约 ────────────────────────────────────────────────────────────

/// MCP 服务端状态（axum `State<McpState>`）。
#[derive(Clone)]
pub struct McpState {
    pub store: Arc<TreePmStore>,
    pub agent_dir: Arc<dyn AgentDirectory>,
}

/// Agent 目录契约（设计 §9.1）：查询某 `agent_id` 是否已安装/存在。
///
/// **依赖方向**：acowork-pm 只定义契约；Gateway 提供实现（基于其
/// `installed_agents`）。pm 侧不反向依赖 gateway。
///
/// `pm_create_task` 指派 `assignee` 时校验其存在；目录不可用（`Noop`）
/// 时跳过校验（保持当前宽松行为）。
#[async_trait]
pub trait AgentDirectory: Send + Sync {
    async fn agent_exists(&self, agent_id: &str) -> bool;
}

/// 默认（宽松）Agent 目录：不校验存在性。`PmService::new` 未注入目录时使用。
pub struct NoopAgentDirectory;

#[async_trait]
impl AgentDirectory for NoopAgentDirectory {
    async fn agent_exists(&self, _agent_id: &str) -> bool {
        true
    }
}

/// 构建 MCP HTTP Server 路由（JSON-RPC，`POST /mcp`）。
///
/// 返回 `Router<()>`（内部 `.with_state(McpState)`），与 REST `pm_router`
/// 合并后由 Gateway `nest_service("/api/pm", ...)` 挂载。
pub fn mcp_router(store: Arc<TreePmStore>, agent_dir: Arc<dyn AgentDirectory>) -> Router {
    Router::new()
        .route("/mcp", post(jsonrpc_endpoint))
        .with_state(McpState { store, agent_dir })
}

// ── JSON-RPC 协议常量 ────────────────────────────────────────────────────

/// MCP 协议版本（与 acowork-mcp `McpClient::connect` 握手版本一致）。
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC 2.0 协议版本。
pub const JSONRPC_VERSION: &str = "2.0";

// JSON-RPC 标准错误码
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

// 自定义服务错误码（-32000..-32099 是协议保留的服务端错误区段）
const CODE_UNAUTHENTICATED: i32 = -32001;
const CODE_FORBIDDEN: i32 = -32002;

impl PmError {
    /// MCP JSON-RPC error code（协议级之外的业务错误映射）。
    ///
    /// - `Forbidden` / `Unauthenticated` → 自定义 -3200x，供客户端 `[permission]` 分类
    /// - 其余 `PmError` → -32603（内部错误）
    pub fn mcp_error_code(&self) -> i32 {
        match self {
            PmError::Unauthenticated(_) => CODE_UNAUTHENTICATED,
            PmError::Forbidden(_) => CODE_FORBIDDEN,
            _ => INTERNAL_ERROR,
        }
    }
}

// ── JSON-RPC 请求结构 ─────────────────────────────────────────────────────

/// 入站 JSON-RPC 请求（MCP 客户端 → 服务端）。
#[derive(Debug, serde::Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

// ── 端点 handler ──────────────────────────────────────────────────────────

/// `POST /mcp` —— JSON-RPC 单一入口。
///
/// 请求体：`{"jsonrpc":"2.0","id":1,"method":"...","params":{...}}`
/// 身份：`X-MCP-Actor` header（可选；缺失视为匿名）。
#[tracing::instrument(skip(state, body))]
pub async fn jsonrpc_endpoint(
    State(state): State<McpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = extract_actor(&headers);

    // 解析失败 → -32700 Parse error
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(e) => {
            return rpc_error(None, PARSE_ERROR, format!("request body is not UTF-8: {e}"));
        }
    };
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return rpc_error(None, PARSE_ERROR, format!("parse error: {e}"));
        }
    };

    // 批处理不支持（客户端单发）→ -32600 Invalid Request
    if parsed.is_array() {
        return rpc_error(None, INVALID_REQUEST, "batch requests are not supported".into());
    }

    let req: JsonRpcRequest = match serde_json::from_value(parsed) {
        Ok(r) => r,
        Err(e) => return rpc_error(None, INVALID_REQUEST, format!("invalid request: {e}")),
    };

    if req.jsonrpc != JSONRPC_VERSION {
        return rpc_error(req.id.clone(), INVALID_REQUEST, "invalid jsonrpc version".into());
    }

    handle_request(state, actor, req).await
}

/// 从 header 提取调用方 agent_id（`X-MCP-Actor`）。缺失 → 匿名（None）。
fn extract_actor(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-mcp-actor")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn handle_request(state: McpState, actor: Option<String>, req: JsonRpcRequest) -> Response {
    let id = req.id.clone();

    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "acowork-pm",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            rpc_result(id, result)
        }

        // 客户端初始化完成通知——按协议不回响应（202 空体）
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),

        "tools/list" => {
            let tools = manifest::manifest_tools();
            rpc_result(id, json!({ "tools": tools }))
        }

        "tools/call" => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Null);

            let Some(name) = name else {
                return rpc_error(id, INVALID_PARAMS, "tools/call requires a `name` field".into());
            };

            match tools::dispatch(&state, actor.as_deref(), &name, args).await {
                Ok(result) => {
                    // 规范化 MCP content 块：数据序列化为 JSON 文本
                    let text = serde_json::to_string(&result)
                        .unwrap_or_else(|_| "null".to_string());
                    let envelope = json!({
                        "content": [ { "type": "text", "text": text } ],
                        "isError": false
                    });
                    rpc_result(id, envelope)
                }
                Err(e) => {
                    // 业务错误 → JSON-RPC error，message 带错误码前缀
                    // 使客户端 [permission]/[transient] 分类生效
                    let msg = format!("{}: {}", e.error_code(), e);
                    rpc_error(id, e.mcp_error_code(), msg)
                }
            }
        }

        // 未知 method：通知（无 id）按协议静默；请求回 -32601
        other => {
            if id.is_none() {
                StatusCode::ACCEPTED.into_response()
            } else {
                rpc_error(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
            }
        }
    }
}

// ── 响应构造 ──────────────────────────────────────────────────────────────

/// 成功响应：`{"jsonrpc":"2.0","id":..,"result":..}`（application/json）。
fn rpc_result(id: Option<Value>, result: Value) -> Response {
    let body = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// 错误响应：`{"jsonrpc":"2.0","id":..,"error":{code,message}}`。
fn rpc_error(id: Option<Value>, code: i32, message: String) -> Response {
    let body = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message },
    });
    (StatusCode::OK, Json(body)).into_response()
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PmConfig;
    use crate::store::tree::PmStore;
    use crate::types::{ProjectStatus, TaskId, UpdateProject};
    use tower::ServiceExt;

    /// 将响应体 `Bytes` 解析为 `Value`。
    async fn body_to_value(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    use axum::body::Body;

    /// 测试用最小状态：tempdir store + Noop AgentDirectory。
    pub(crate) async fn test_mcp_state() -> (McpState, tempfile::TempDir) {
        test_mcp_state_with_dir(Arc::new(NoopAgentDirectory)).await
    }

    /// 测试用最小状态（可注入自定义 Agent 目录）。
    pub(crate) async fn test_mcp_state_with_dir(
        agent_dir: Arc<dyn AgentDirectory>,
    ) -> (McpState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = PmConfig::default();
        cfg.data_dir = tmp.path().to_path_buf();
        cfg.index_rebuild_on_start = false;
        let store = Arc::new(TreePmStore::new(cfg).await.unwrap());
        (McpState { store, agent_dir }, tmp)
    }

    #[tokio::test]
    async fn initialize_handshake_returns_server_info() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_value(resp).await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "acowork-pm");
    }

    #[tokio::test]
    async fn notification_initialized_returns_202_empty() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn tools_list_returns_manifest_tools() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        let v = body_to_value(resp).await;
        let tools = v["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":3,"method":"bogus/method","params":{}}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        let v = body_to_value(resp).await;
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
    }

    // ── P3 e2e：全生命周期 + 鉴权 ─────────────────────────────────────

    /// 便捷：向 router 发一次 `tools/call`，返回完整 JSON-RPC 响应。
    async fn call_tool(
        router: &Router,
        actor: Option<&str>,
        id: u64,
        name: &str,
        args: Value,
    ) -> Value {
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(a) = actor {
            builder = builder.header("x-mcp-actor", a);
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        body_to_value(resp).await
    }

    /// 便捷：解析 `tools/call` 成功响应中的 `result.content[0].text`（JSON 文本）。
    fn tool_text(v: &Value) -> Value {
        serde_json::from_str(
            v["result"]["content"][0]["text"]
                .as_str()
                .expect("content[0].text should be a JSON string"),
        )
        .expect("tool result text should be valid JSON")
    }

    /// 建 router + 空状态（Noop AgentDirectory：assignee 存在性恒真）。
    async fn test_router() -> Router {
        let (state, _tmp) = test_mcp_state().await;
        Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state)
    }

    /// 建 router（可注入自定义 Agent 目录）。
    async fn test_router_with_dir(agent_dir: Arc<dyn AgentDirectory>) -> Router {
        let (state, _tmp) = test_mcp_state_with_dir(agent_dir).await;
        Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state)
    }

    /// 建 router + 暴露 store（供 human review / archive 等 MCP 之外的
    /// 前置状态构造，模拟人类审批侧）。
    async fn test_router_and_store() -> (Router, Arc<TreePmStore>) {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state.clone());
        (router, state.store)
    }

    /// 白名单 Agent 目录（测试桩）：仅当 agent_id 在白名单内才返回 true。
    /// 用于验证 `pm_create_task` 的 assignee 存在性校验（设计 §9.1）。
    struct WhitelistAgentDirectory {
        allowed: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    }

    impl WhitelistAgentDirectory {
        fn new(ids: &[&str]) -> Self {
            Self {
                allowed: std::sync::Arc::new(std::sync::Mutex::new(
                    ids.iter().map(|s| s.to_string()).collect(),
                )),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentDirectory for WhitelistAgentDirectory {
        async fn agent_exists(&self, agent_id: &str) -> bool {
            self.allowed.lock().unwrap().contains(agent_id)
        }
    }

    /// 断言 JSON-RPC 错误：返回 (code, message 前缀, 完整 message)。
    fn assert_rpc_error(v: &Value) -> (i32, String, String) {
        let msg = v["error"]["message"].as_str().unwrap_or_default().to_string();
        let prefix = msg
            .split_once(':')
            .map(|(p, _)| p.trim().to_string())
            .unwrap_or_else(|| msg.clone());
        (
            v["error"]["code"].as_i64().unwrap_or(0) as i32,
            prefix,
            msg,
        )
    }

    /// e2e：Agent 完整生命周期 —— create_project → create_task(assignee+due_at)
    /// → claim → submit → check。断言审核语义（agent 创建 → pending review）。
    #[tokio::test]
    async fn e2e_full_lifecycle_claim_submit_check() {
        let router = test_router().await;
        let agent = "agent-alpha";

        // 1. 创建项目
        let v = call_tool(
            &router,
            Some(agent),
            10,
            "pm_create_project",
            json!({ "title": "P1", "description": "desc" }),
        )
        .await;
        assert!(v["error"].is_null(), "create_project failed: {v}");
        let proj = tool_text(&v);
        let pid = proj["id"].as_str().unwrap().to_string();

        // 2. 创建任务（assignee = 自己 + due_at）
        let v = call_tool(
            &router,
            Some(agent),
            11,
            "pm_create_task",
            json!({
                "project_id": pid,
                "title": "T1",
                "description": "do it",
                "assignee": agent,
                "due_at": "2026-01-01T00:00:00Z",
            }),
        )
        .await;
        assert!(v["error"].is_null(), "create_task failed: {v}");
        let task = tool_text(&v);
        let tid = task["id"].as_str().unwrap().to_string();
        assert_eq!(task["status"], "pending");
        assert_eq!(task["review_status"], "pending"); // agent 创建 → pending 待人类审核
        assert_eq!(task["assignee"], agent);

        // 3. claim（assignee 本人）
        let v = call_tool(&router, Some(agent), 12, "pm_claim_task", json!({ "task_id": tid }))
            .await;
        assert!(v["error"].is_null(), "claim failed: {v}");
        assert_eq!(tool_text(&v)["status"], "in_progress");

        // 4. submit
        let v = call_tool(
            &router,
            Some(agent),
            13,
            "pm_submit_task",
            json!({ "task_id": tid, "text": "done" }),
        )
        .await;
        assert!(v["error"].is_null(), "submit failed: {v}");
        assert_eq!(tool_text(&v)["status"], "submitted");

        // 5. check（创建者可查；人类尚未 approve → approved=false）
        let v = call_tool(&router, Some(agent), 14, "pm_check_task", json!({ "task_id": tid }))
            .await;
        assert!(v["error"].is_null(), "check failed: {v}");
        let chk = tool_text(&v);
        assert_eq!(chk["status"], "submitted");
        assert_eq!(chk["approved"], false);
        assert_eq!(chk["review_status"], "pending");
    }

    /// 鉴权 403：非 assignee 调用 claim/submit/update 一律拒绝。
    #[tokio::test]
    async fn e2e_non_assignee_mutation_forbidden() {
        let router = test_router().await;
        let owner = "agent-owner";
        let intruder = "agent-intruder";

        let v = call_tool(
            &router,
            Some(owner),
            20,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        let v = call_tool(
            &router,
            Some(owner),
            21,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": owner }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 非 assignee：claim / submit / update 均 403
        let v = call_tool(&router, Some(intruder), 22, "pm_claim_task", json!({ "task_id": tid }))
            .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "claim by non-assignee: {v}");

        let v = call_tool(
            &router,
            Some(intruder),
            23,
            "pm_submit_task",
            json!({ "task_id": tid, "text": "x" }),
        )
        .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "submit by non-assignee: {v}");

        let v = call_tool(
            &router,
            Some(intruder),
            24,
            "pm_update_task",
            json!({ "task_id": tid, "title": "hijack" }),
        )
        .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "update by non-assignee: {v}");

        // 无 assignee 的任务也无法被行动（设计 §9.2）
        let v = call_tool(
            &router,
            Some(owner),
            25,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T2" }),
        )
        .await;
        let tid2 = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(&router, Some(owner), 26, "pm_claim_task", json!({ "task_id": tid2 }))
            .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "claim unassigned: {v}");
    }

    /// 匿名只读：list/get 允许；写操作 / 自查工具要求身份（401 语义）。
    #[tokio::test]
    async fn e2e_anonymous_read_only_mutations_rejected() {
        let router = test_router().await;
        let agent = "agent-a";

        // 预置数据（以 agent 身份创建）
        let v = call_tool(
            &router,
            Some(agent),
            30,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            31,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 匿名只读 OK
        for (name, args) in [
            ("pm_list_projects", json!({})),
            ("pm_get_project", json!({ "project_id": pid })),
            ("pm_list_tasks", json!({ "project_id": pid })),
            ("pm_get_task", json!({ "task_id": tid })),
        ] {
            let v = call_tool(&router, None, 32, name, args).await;
            assert!(v["error"].is_null(), "anonymous {name} should be allowed: {v}");
        }

        // 匿名写操作 → 未认证（CODE_UNAUTHENTICATED）
        for (name, args) in [
            ("pm_create_project", json!({ "title": "X" })),
            ("pm_create_task", json!({ "project_id": pid, "title": "X" })),
            ("pm_claim_task", json!({ "task_id": tid })),
            ("pm_submit_task", json!({ "task_id": tid, "text": "x" })),
            ("pm_update_task", json!({ "task_id": tid })),
        ] {
            let v = call_tool(&router, None, 33, name, args).await;
            assert_eq!(
                v["error"]["code"],
                CODE_UNAUTHENTICATED,
                "anonymous {name} should be rejected: {v}"
            );
        }

        // 匿名自查工具（list_my_tasks / check_task）也要身份
        let v = call_tool(&router, None, 34, "pm_list_my_tasks", json!({})).await;
        assert_eq!(v["error"]["code"], CODE_UNAUTHENTICATED);
        let v = call_tool(&router, None, 35, "pm_check_task", json!({ "task_id": tid })).await;
        assert_eq!(v["error"]["code"], CODE_UNAUTHENTICATED);
    }

    // ── P3 e2e 补齐：剩余工具 happy path + 边界场景 ───────────────────

    /// e2e：pm_update_task happy path —— 改 title/status/priority/assignee。
    #[tokio::test]
    async fn e2e_update_task_happy_path() {
        let router = test_router().await;
        let agent = "agent-updater";

        let v = call_tool(
            &router,
            Some(agent),
            40,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            41,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 改 title + priority + status（pending → in_progress 合法）
        let v = call_tool(
            &router,
            Some(agent),
            42,
            "pm_update_task",
            json!({ "task_id": tid, "title": "renamed", "priority": "high", "status": "in_progress" }),
        )
        .await;
        assert!(v["error"].is_null(), "update failed: {v}");
        let up = tool_text(&v);
        assert_eq!(up["title"], "renamed");
        assert_eq!(up["priority"], "high");
        assert_eq!(up["status"], "in_progress");

        // 清空 assignee（assignee=null）
        let v = call_tool(
            &router,
            Some(agent),
            43,
            "pm_update_task",
            json!({ "task_id": tid, "assignee": null }),
        )
        .await;
        assert!(v["error"].is_null(), "clear assignee failed: {v}");
        assert_eq!(tool_text(&v)["assignee"], Value::Null);
    }

    /// e2e：pm_update_task 非法状态流转（done → in_progress 虽合法，但
    /// pending → submitted 非法）返回 400 invalid_state_transition。
    #[tokio::test]
    async fn e2e_update_task_invalid_transition() {
        let router = test_router().await;
        let agent = "agent-bad";

        let v = call_tool(
            &router,
            Some(agent),
            44,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            45,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // pending → submitted 非法（必须先 claim）
        let v = call_tool(
            &router,
            Some(agent),
            46,
            "pm_update_task",
            json!({ "task_id": tid, "status": "submitted" }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "transition error msg: {msg}");
        assert_eq!(prefix, "invalid_state_transition", "unexpected: {msg}");
    }

    /// e2e：pm_reparent_task —— 移动到新父 + 提升根 + 防环 409。
    #[tokio::test]
    async fn e2e_reparent_task_happy_and_cycle() {
        let router = test_router().await;
        let agent = "agent-reparent";

        let v = call_tool(
            &router,
            Some(agent),
            47,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // A 根任务，B 为 A 的子任务
        let v = call_tool(
            &router,
            Some(agent),
            48,
            "pm_create_task",
            json!({ "project_id": pid, "title": "A", "assignee": agent }),
        )
        .await;
        let tid_a = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            49,
            "pm_create_task",
            json!({ "project_id": pid, "title": "B", "assignee": agent, "parent_task_id": tid_a }),
        )
        .await;
        let tid_b = tool_text(&v)["id"].as_str().unwrap().to_string();
        // B 深度 = 1，parent = A
        let v = call_tool(&router, Some(agent), 50, "pm_get_task", json!({ "task_id": tid_b }))
            .await;
        assert_eq!(tool_text(&v)["depth"], 1);
        assert_eq!(tool_text(&v)["parent_id"], tid_a);

        // 防环：把 A 移到自己的子任务 B 下 → cycle_detected 409（B 仍是 A 子树成员）
        let v = call_tool(
            &router,
            Some(agent),
            51,
            "pm_reparent_task",
            json!({ "task_id": tid_a, "new_parent": tid_b }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "cycle error msg: {msg}");
        assert_eq!(prefix, "cycle_detected", "unexpected: {msg}");
        // A 层级不变
        let v = call_tool(&router, Some(agent), 52, "pm_get_task", json!({ "task_id": tid_a }))
            .await;
        assert_eq!(tool_text(&v)["depth"], 0);

        // 提升 B 为根（new_parent=null）
        let v = call_tool(
            &router,
            Some(agent),
            53,
            "pm_reparent_task",
            json!({ "task_id": tid_b, "new_parent": null }),
        )
        .await;
        assert!(v["error"].is_null(), "reparent to root failed: {v}");
        let v = call_tool(&router, Some(agent), 54, "pm_get_task", json!({ "task_id": tid_b }))
            .await;
        assert_eq!(tool_text(&v)["depth"], 0);
        assert_eq!(tool_text(&v)["parent_id"], Value::Null);
    }

    /// e2e：pm_list_my_tasks —— Agent 自查指派给自己的任务 + status 过滤。
    #[tokio::test]
    async fn e2e_list_my_tasks_happy_path() {
        let router = test_router().await;
        let agent = "agent-self";
        let other = "agent-other";

        let v = call_tool(
            &router,
            Some(agent),
            54,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 两个给自己的任务 + 一个给别人的任务
        for i in 0..3 {
            let (title, assignee) = if i < 2 { ("mine", agent) } else { ("others", other) };
            let v = call_tool(
                &router,
                Some(agent),
                55,
                "pm_create_task",
                json!({ "project_id": pid, "title": title, "assignee": assignee }),
            )
            .await;
            assert!(v["error"].is_null(), "create task {i} failed: {v}");
        }

        // 自查：只返回自己的 2 个任务
        let v = call_tool(&router, Some(agent), 56, "pm_list_my_tasks", json!({})).await;
        let mine = tool_text(&v);
        assert_eq!(mine.as_array().unwrap().len(), 2, "my_tasks: {mine}");

        // status 过滤：把第一个 claim 后，pending 只剩 1 个
        let my_tid = mine[0]["id"].as_str().unwrap().to_string();
        let v = call_tool(&router, Some(agent), 57, "pm_claim_task", json!({ "task_id": my_tid }))
            .await;
        assert!(v["error"].is_null(), "claim failed: {v}");
        let v = call_tool(
            &router,
            Some(agent),
            58,
            "pm_list_my_tasks",
            json!({ "status": "pending" }),
        )
        .await;
        let pending = tool_text(&v);
        assert_eq!(pending.as_array().unwrap().len(), 1, "pending my_tasks: {pending}");
    }

    /// e2e：pm_check_task 非创建者 403（设计 §6：仅创建者可查审核状态）。
    #[tokio::test]
    async fn e2e_check_task_non_creator_forbidden() {
        let router = test_router().await;
        let creator = "agent-creator";
        let other = "agent-other";

        let v = call_tool(
            &router,
            Some(creator),
            59,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(creator),
            60,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": creator }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 非创建者 check → 403 forbidden
        let v = call_tool(&router, Some(other), 61, "pm_check_task", json!({ "task_id": tid }))
            .await;
        let (code, prefix, _msg) = assert_rpc_error(&v);
        assert_eq!(code, CODE_FORBIDDEN, "non-creator check should be forbidden");
        assert_eq!(prefix, "forbidden");

        // 创建者本人可查
        let v = call_tool(&router, Some(creator), 62, "pm_check_task", json!({ "task_id": tid }))
            .await;
        assert!(v["error"].is_null());
        assert_eq!(tool_text(&v)["id"], tid);
    }

    /// e2e：pm_create_task assignee 不在 Agent 目录 → 400 bad_request
    /// （设计 §9.1；用白名单目录验证存在性校验）。
    #[tokio::test]
    async fn e2e_create_task_assignee_not_in_directory() {
        let router = test_router_with_dir(Arc::new(WhitelistAgentDirectory::new(&[
            "agent-known",
        ])))
        .await;
        let agent = "agent-known";
        let ghost = "agent-ghost";

        let v = call_tool(
            &router,
            Some(agent),
            63,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // assignee 不存在 → 400 bad_request（错误 message 提及 assignee）
        let v = call_tool(
            &router,
            Some(agent),
            64,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": ghost }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR);
        assert_eq!(prefix, "bad_request", "unexpected: {msg}");
        assert!(msg.contains("assignee agent not found"), "msg: {msg}");

        // assignee 存在于目录 → 成功
        let v = call_tool(
            &router,
            Some(agent),
            65,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T2", "assignee": agent }),
        )
        .await;
        assert!(v["error"].is_null(), "known assignee failed: {v}");
        assert_eq!(tool_text(&v)["assignee"], agent);
    }

    /// e2e：依赖阻塞 —— depends_on(Blocks) 未完成时 claim 409，依赖完成
    /// 后（人类 review → Done）可 claim。
    #[tokio::test]
    async fn e2e_dependency_blocks_claim() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-dep";

        let v = call_tool(
            &router,
            Some(agent),
            66,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // A：前置依赖；B：依赖 A（Blocks）
        let v = call_tool(
            &router,
            Some(agent),
            67,
            "pm_create_task",
            json!({ "project_id": pid, "title": "A", "assignee": agent }),
        )
        .await;
        let tid_a = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            68,
            "pm_create_task",
            json!({
                "project_id": pid, "title": "B", "assignee": agent,
                "depends_on": [{ "task_id": tid_a, "kind": "blocks" }],
            }),
        )
        .await;
        let tid_b = tool_text(&v)["id"].as_str().unwrap().to_string();

        // B 被 A 阻塞 → is_blocked=true
        let v = call_tool(&router, Some(agent), 69, "pm_get_task", json!({ "task_id": tid_b }))
            .await;
        let b = tool_text(&v);
        assert_eq!(b["is_blocked"], true, "B should be blocked: {b}");
        assert_eq!(b["blocked_by"][0], tid_a);

        // claim B → 409 dependency_not_satisfied
        let v = call_tool(&router, Some(agent), 70, "pm_claim_task", json!({ "task_id": tid_b }))
            .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "dep error msg: {msg}");
        assert_eq!(prefix, "dependency_not_satisfied", "unexpected: {msg}");

        // 完成 A：claim → submit → 人类 review approve → Done
        let store = store.clone();
        let tid_a_typed = TaskId(tid_a.clone());
        store.claim_task(&tid_a_typed, agent).await.unwrap();
        store
            .submit_task(&tid_a_typed, "done by agent", vec![], agent)
            .await
            .unwrap();
        store.review_task(&tid_a_typed, true, "human").await.unwrap();
        let v = call_tool(&router, Some(agent), 71, "pm_get_task", json!({ "task_id": tid_a }))
            .await;
        assert_eq!(tool_text(&v)["status"], "done", "A should be done: {v}");

        // B 不再被阻塞 → 可 claim
        let v = call_tool(&router, Some(agent), 72, "pm_claim_task", json!({ "task_id": tid_b }))
            .await;
        assert!(v["error"].is_null(), "claim B after dep done failed: {v}");
        assert_eq!(tool_text(&v)["status"], "in_progress");
    }

    /// e2e：404 —— project / task 不存在返回对应错误。
    #[tokio::test]
    async fn e2e_not_found() {
        let router = test_router().await;
        let agent = "agent-404";

        // project 不存在
        let v = call_tool(
            &router,
            Some(agent),
            72,
            "pm_get_project",
            json!({ "project_id": "p-missing123" }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "404 project msg: {msg}");
        assert_eq!(prefix, "project_not_found", "unexpected: {msg}");

        // task 不存在
        let v = call_tool(
            &router,
            Some(agent),
            73,
            "pm_get_task",
            json!({ "task_id": "t-missing123" }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "404 task msg: {msg}");
        assert_eq!(prefix, "task_not_found", "unexpected: {msg}");
    }

    /// e2e：pm_list_tasks 过滤（status / assignee / limit）+ include_archived。
    #[tokio::test]
    async fn e2e_list_filters() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-filter";
        let other = "agent-other";

        // 项目 1：归档（通过 store 直接归档，模拟人类操作）
        let v = call_tool(
            &router,
            Some(agent),
            74,
            "pm_create_project",
            json!({ "title": "archived-proj" }),
        )
        .await;
        let pid_arch = tool_text(&v)["id"].as_str().unwrap().to_string();
        store
            .update_project(
                &crate::types::ProjectId(pid_arch.clone()),
                UpdateProject {
                    title: None,
                    description: None,
                    status: Some(ProjectStatus::Archived),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        // 项目 2：正常，2 个任务（1 个 assignee=agent）
        let v = call_tool(
            &router,
            Some(agent),
            75,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        for (i, (title, a)) in [("T1", agent), ("T2", other)].into_iter().enumerate() {
            let v = call_tool(
                &router,
                Some(agent),
                76,
                "pm_create_task",
                json!({ "project_id": pid, "title": title, "assignee": a }),
            )
            .await;
            assert!(v["error"].is_null(), "create task {i} failed: {v}");
        }

        // include_archived=false（默认）：归档项目不出现
        let v = call_tool(&router, None, 77, "pm_list_projects", json!({})).await;
        let projs = tool_text(&v);
        let titles: Vec<&str> = projs
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["title"].as_str().unwrap())
            .collect();
        assert!(!titles.contains(&"archived-proj"), "archived shown by default: {projs}");

        // include_archived=true：归档项目出现
        let v = call_tool(
            &router,
            None,
            78,
            "pm_list_projects",
            json!({ "include_archived": true }),
        )
        .await;
        let projs = tool_text(&v);
        assert!(
            projs.as_array().unwrap().iter().any(|p| p["title"] == "archived-proj"),
            "archived missing with include_archived: {projs}"
        );

        // list_tasks 过滤：assignee=agent 只有 1 个
        let v = call_tool(
            &router,
            None,
            79,
            "pm_list_tasks",
            json!({ "project_id": pid, "assignee": agent }),
        )
        .await;
        assert_eq!(tool_text(&v).as_array().unwrap().len(), 1);

        // limit=1
        let v = call_tool(
            &router,
            None,
            80,
            "pm_list_tasks",
            json!({ "project_id": pid, "limit": 1 }),
        )
        .await;
        assert_eq!(tool_text(&v).as_array().unwrap().len(), 1);
    }

    /// e2e：pm_reparent_task 匿名拒绝（§9.3 匿名仅只读）。
    #[tokio::test]
    async fn e2e_reparent_anonymous_rejected() {
        let router = test_router().await;
        let agent = "agent-rp";

        let v = call_tool(
            &router,
            Some(agent),
            81,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            82,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        let v = call_tool(
            &router,
            None,
            83,
            "pm_reparent_task",
            json!({ "task_id": tid, "new_parent": null }),
        )
        .await;
        assert_eq!(v["error"]["code"], CODE_UNAUTHENTICATED, "anonymous reparent: {v}");
    }
}
