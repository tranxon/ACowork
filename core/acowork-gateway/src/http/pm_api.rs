//! PM service HTTP routes mounted into the Gateway's HTTP server.
//!
//! ADR-061: PM service 由 Gateway 监督，本模块提供路由挂载入口。
//!
//! ## Path 前缀约定
//!
//! PM router 内部路径**不带** `/api` 前缀（见
//! [`acowork_pm::api::routes::pm_router`] 文档）。Gateway 在
//! `nest_service("/api/pm", ...)` 挂载时自动加上，公开路径为
//! **`/api/pm/*`**（设计文档 §21：`{gw}/api/pm/...`，保持 PM 命名空间隔离，
//! 避免与 Gateway 现有 `/api/*` 路由冲突）。
//!
//! | PM router 内部 | 公开 URL |
//! |---------------|----------|
//! | `GET /projects` | `GET /api/pm/projects` |
//! | `GET /tasks/:tid` | `GET /api/pm/tasks/:tid` |
//! | `GET /attachments/:aid` | `GET /api/pm/attachments/:aid` |
//!
//! ## State 类型
//!
//! PM router 的 state 是 [`acowork_pm::api::ApiState`]（PM 自己的 state），在
//! PM 内部用 `.with_state(ApiState)` 注入并消耗掉，对外呈现为 `Router<()>`
//! （可 serve 的 router），与 Gateway 的 [`AppState`] 完全解耦。
//!
//! **为什么用 `nest_service` 而不是 `nest`/`merge`**：axum 0.8 的 `nest` 和
//! `merge` 都要求内外 router 的 state 类型**一致**（这里 `Router<AppState>` ≠
//! `Router<()>`）；`nest_service` 接受任意 `T: Service<Request, Error=Infallible>`，
//! 而 `Router<()>` 恰好实现了该 trait，且 `nest_service` 会剥离 `/api/pm` 前缀。
//!
//! ## 启动时序
//!
//! 1. `Gateway::new(config)` 同步构造 state（`pm_service = None`）
//! 2. `Gateway::run` 开头 `PmService::new(config.pm).await` 异步构造实例
//! 3. `state.pm_service = Some(Arc::new(pm_service))` 写入
//! 4. `http::server::start` 启动前调用 `build_router_with_pm(state, pm_service)`
//! 5. 本模块的 `pm_routes(pm_service)` 返回 PM router（`Router<()>`），由 Gateway `nest_service`

use std::sync::Arc;

use axum::Router;
use tokio::sync::RwLock;

use acowork_pm::{AgentDirectory, PmService};

/// 返回 PM service 的 axum `Router`（`Router<()>`，已注入 ApiState），
/// 由 Gateway `nest_service("/api/pm", ...)` 挂载。
///
/// Path 内部不带 `/api` 前缀（`nest_service` 时自动加上 `/api/pm`）。
pub fn pm_routes(pm_service: Arc<PmService>) -> Router {
    pm_service.router()
}

/// Gateway 提供的 [`AgentDirectory`] 实现（设计 §9.1）：基于
/// `GatewayState.installed_agents` 判断某 `agent_id` 是否已安装。
///
/// 用于 `pm_create_task` 指派 `assignee` 时的存在性校验——只有存在于
/// Gateway Agent 目录中的 agent 才能被指派任务（防止幽灵 assignee）。
///
/// **为什么基于 `installed_agents` 而不是 `running_agents`**：目录语义是
/// "已安装即可指派"，与 agent 当前是否在线无关；指派给离线 agent 是合法的
/// （其上线后通过 `pm_list_my_tasks` 可自查）。
pub struct GatewayAgentDirectory {
    state: Arc<RwLock<crate::gateway::state::GatewayState>>,
}

impl GatewayAgentDirectory {
    pub fn new(state: Arc<RwLock<crate::gateway::state::GatewayState>>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl AgentDirectory for GatewayAgentDirectory {
    async fn agent_exists(&self, agent_id: &str) -> bool {
        self.state.read().await.is_installed(agent_id)
    }
}
