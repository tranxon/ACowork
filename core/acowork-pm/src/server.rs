//! PM 服务 HTTP server 启动入口。
//!
//! 设计：PM 服务**不**独立运行——由 Gateway 监督并反向代理。
//! Gateway 启动时通过 `nest_service("/api/pm", self.router())` 把 PM router 挂到
//! 自身 HTTP 路径下（axum 0.8 `nest`/`merge` 要求内外 state 类型一致，而 PM
//! router 呈现为 `Router<()>`，与 Gateway 的 `Router<AppState>` 不同，故用
//! `nest_service` 接受任意 `Service`）。公开路径统一为 `/api/pm/*`
//! （设计文档 §21：`{gw}/api/pm/...`）。
//!
//! 设计参考：[`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §7

use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::PmConfig;
use crate::error::Result;
use crate::store::tree::TreePmStore;

/// PM 服务运行实例。
///
/// Gateway 持有此句柄，用于：
/// - 启动时调用 [`PmService::start`] 拉起 HTTP server
/// - 运行时通过共享的 [`TreePmStore`] 暴露给 handlers（避免 HTTP 路径上构造 store）
/// - 关闭时调用 [`PmService::shutdown`] 优雅停机（flush 写、关闭 listen socket）
pub struct PmService {
    pub config: PmConfig,
    pub store: Arc<TreePmStore>,
}

impl PmService {
    /// 构造服务实例（**不**启动 server，仅初始化 store + 重建索引）。
    pub async fn new(config: PmConfig) -> Result<Self> {
        config.validate()?;

        let store = Arc::new(TreePmStore::new(config.clone()).await?);

        // 启动时重建索引（如果配置开启）
        if config.index_rebuild_on_start {
            store.rebuild_index().await?;
        }

        Ok(Self { config, store })
    }

    /// 构建 axum router（供 Gateway 挂载）。
    ///
    /// 返回的 router 已包含全部 PM API 路由，**不**启动 listen socket。
    ///
    /// State 类型：`Router`（`Router<()>`）—— PM 内部已 `.with_state(ApiState)`
    /// 注入 state，对外呈现为可 serve 的 router。Gateway 端用
    /// `nest_service("/api/pm", ...)` 挂载（axum 0.8 `nest`/`merge` 要求内外 state
    /// 类型一致，PM 的 `()` 与 Gateway 的 `AppState` 不同）。
    pub fn router(&self) -> axum::Router {
        crate::api::routes::pm_router(self.store.clone(), self.config.clone())
    }

    /// 启动独立的 HTTP server（**仅供开发模式**，生产由 Gateway 托管）。
    ///
    /// 返回监听地址（端口可能为 0 → 由 OS 分配）。
    ///
    /// **P0 阶段**：dev server 只 serve 占位 `/health` 端点 —— 完整 PM routes 由
    /// Gateway 在生产中通过 `nest_service("/api/pm", self.router())` 提供。P0 阶段 handlers
    /// 大多是 `unimplemented!()`，dev server 暴露它们没有价值。
    ///
    /// **TODO(P1)**：完整 serve 全部 PM routes。届时此函数改为
    /// `axum::serve(listener, self.router().with_state(()))`（需先剥离 state，
    /// handlers 改用 Extension）。
    pub async fn start_dev(self: Arc<Self>, bind: SocketAddr) -> Result<SocketAddr> {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;

        // P0 占位 router：仅 `/health`。
        let router = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async { "PM dev server (P0 placeholder)" }),
        );
        tracing::info!(addr = %addr, "PM dev server listening (P0 placeholder)");

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "PM dev server exited with error");
            }
        });

        Ok(addr)
    }

    /// 优雅停机（flush 待写、关闭 store）。
    pub async fn shutdown(&self) -> Result<()> {
        self.store.shutdown().await
    }
}
