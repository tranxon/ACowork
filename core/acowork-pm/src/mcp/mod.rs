//! MCP (Model Context Protocol) HTTP Server。
//!
//! ## 协议
//!
//! 服务端实现 [`MCP HTTP Server` 协议](https://modelcontextprotocol.io/)，对外暴露：
//!
//! - `POST /mcp/tools/list` —— 列出可用工具（见 [`manifest::PM_TOOL_MANIFEST`]）
//! - `POST /mcp/tools/call` —— 调用工具（见 [`tools`]）
//!
//! ## 与 REST API 的关系
//!
//! MCP 是 REST 的**语义等价**子集——所有 `pm_*` MCP 工具背后都调用同一个 [`PmStore`] trait。
//! Agent 通过 MCP 调用时，服务端自动：
//!
//! - 注入 `created_by` / `submitted_by` / `actor`（来自 MCP 客户端 ID）
//! - 返回精简的 JSON（避免向 LLM 暴露内部细节）
//!
//! ## 设计参考
//!
//! [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §6

pub mod manifest;
pub mod tools;

use std::sync::Arc;

use axum::{routing::post, Router};

use crate::config::PmConfig;
use crate::store::tree::TreePmStore;

/// 构建 MCP HTTP Server 路由。
///
/// Gateway 在独立端口（默认 `:7837`）上挂载此 router。
pub fn mcp_router(store: Arc<TreePmStore>, _config: PmConfig) -> Router {
    Router::new()
        .route("/mcp/tools/list", post(tools::list))
        .route("/mcp/tools/call", post(tools::call))
        .with_state(McpState { store })
}

#[derive(Clone)]
pub struct McpState {
    pub store: Arc<TreePmStore>,
}