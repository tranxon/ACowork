//! MCP 工具 handlers（**P3 完整实现**）。
//!
//! P0 阶段：路由注册 + 占位实现，确保服务可编译。
//! P3 阶段：逐个工具实现完整业务逻辑。

use axum::extract::State;
use axum::Json;
use serde_json::Value;

use super::{manifest::PM_TOOL_MANIFEST, McpState};

/// `POST /mcp/tools/list`
///
/// 返回 [`PM_TOOL_MANIFEST`]。无需访问 store（manifest 是静态的）。
#[tracing::instrument]
pub async fn list() -> Json<Value> {
    let parsed: Value = serde_json::from_str(PM_TOOL_MANIFEST)
        .expect("PM_TOOL_MANIFEST must be valid JSON (compile-time check)");
    Json(parsed)
}

/// `POST /mcp/tools/call`
///
/// 请求体：
/// ```json
/// { "name": "pm_xxx", "arguments": { ... } }
/// ```
#[tracing::instrument(skip(state))]
pub async fn call(
    State(state): State<McpState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CallRequest>,
) -> Result<Json<CallResponse>, crate::error::PmError> {
    let actor = headers
        .get("x-mcp-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal(
            "missing X-MCP-Actor header".to_string(),
        ))?;

    // TODO(P3): 实现完整工具分发
    // 1. match req.name → 调用对应业务方法（传入 actor）
    // 2. 返回紧凑 JSON（仅 LLM 关心的字段）
    // 3. 错误转成 MCP error 格式
    let _ = (state, actor, req);
    Err(crate::error::PmError::Internal(
        "MCP tools not yet implemented (P3)".to_string(),
    ))
}

#[derive(Debug, serde::Deserialize)]
pub struct CallRequest {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, serde::Serialize)]
pub struct CallResponse {
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Json { data: Value },
}

// 保留 helper：未来可加编译时校验（如 const fn 校验 manifest）
const _: () = ();