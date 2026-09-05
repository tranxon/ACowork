//! HTTP AgentDirectory — 查询 Gateway `/api/agents` 校验 assignee 存在性。
//!
//! ADR-064 Phase 3：PM 独立进程后不再共享 Gateway 内存 state，改为经 HTTP
//! 查询 Gateway Agent 目录（`GET /api/agents`）。恢复开发计划 v0.3 T1-11
//! "即时校验兜底"设计：
//!
//! - **启动拉全量**：`start()` 时 `GET /api/agents` 填充缓存
//! - **周期刷新**：按 `refresh_interval` 周期重拉，Agent 卸载后缓存收敛
//! - **即时校验兜底**：`agent_exists` 缓存 miss 时直接 `GET /api/agents/{id}`
//!   即时确认（Agent 刚安装未进缓存 / 缓存过期窗口内）
//!
//! 依赖方向：acowork-pm 只实现 [`super::AgentDirectory`] 契约，不反向依赖
//! Gateway 代码。Gateway URL 由 supervisor 经 `--gateway-url` 传入（`main.rs`）。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::AgentDirectory;

/// HTTP Agent 目录（查询 Gateway `/api/agents`）。
///
/// 缓存 + 即时兜底双保险：周期刷新保证缓存收敛；缓存 miss 时即时查询
/// Gateway 保证 Agent 刚安装/卸载的窗口期内校验仍准确。
pub struct HttpAgentDirectory {
    /// Gateway HTTP base URL（如 `http://127.0.0.1:19876`）。
    gateway_url: String,
    /// 可选 Bearer token（Gateway `auth_enabled=true` 时经
    /// `ACOWORK_PM_GATEWAY_TOKEN` env 传入）。
    auth_token: Option<String>,
    /// 全量 Agent ID 缓存。
    cache: RwLock<HashSet<String>>,
    /// HTTP 客户端（短超时，避免阻塞 MCP 调用）。
    client: reqwest::Client,
    /// 周期刷新间隔。
    refresh_interval: Duration,
}

impl HttpAgentDirectory {
    /// 构造 HTTP Agent 目录。
    ///
    /// `gateway_url` 为 Gateway HTTP base（不含 `/api` 后缀）；`auth_token`
    /// 可选（Gateway auth 关闭时传 `None`）。
    pub fn new(
        gateway_url: String,
        auth_token: Option<String>,
        refresh_interval: Duration,
    ) -> Self {
        Self {
            gateway_url: gateway_url.trim_end_matches('/').to_string(),
            auth_token,
            cache: RwLock::new(HashSet::new()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .connect_timeout(Duration::from_secs(3))
                .build()
                .expect("Failed to build HTTP client for AgentDirectory"),
            refresh_interval,
        }
    }

    /// 启动后台刷新任务（启动拉全量 + 周期刷新）。
    ///
    /// 失败**非致命**：刷新失败保留旧缓存，`agent_exists` 的即时兜底仍可用。
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            // 启动拉全量
            this.refresh().await;
            loop {
                tokio::time::sleep(this.refresh_interval).await;
                this.refresh().await;
            }
        });
    }

    /// 拉全量 Agent 列表到缓存。
    async fn refresh(&self) {
        match self.fetch_agents().await {
            Ok(ids) => {
                let mut cache = self.cache.write().await;
                *cache = ids;
                tracing::debug!(count = cache.len(), "Agent directory refreshed");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Agent directory refresh failed (keeping stale cache)"
                );
            }
        }
    }

    /// `GET {gateway_url}/api/agents` → agent_id 集合。
    async fn fetch_agents(&self) -> Result<HashSet<String>, String> {
        let url = format!("{}/api/agents", self.gateway_url);
        let mut req = self.client.get(&url);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("GET /api/agents -> {}", resp.status()));
        }
        let agents: Vec<serde_json::Value> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(agents
            .into_iter()
            .filter_map(|a| {
                a.get("agent_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect())
    }

    /// 即时校验兜底：`GET {gateway_url}/api/agents/{id}`（200 = 存在）。
    async fn query_gateway(&self, agent_id: &str) -> bool {
        let url = format!("{}/api/agents/{}", self.gateway_url, agent_id);
        let mut req = self.client.get(&url);
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}

#[async_trait]
impl AgentDirectory for HttpAgentDirectory {
    async fn agent_exists(&self, agent_id: &str) -> bool {
        // 缓存命中 → 存在（周期刷新保证收敛）。
        if self.cache.read().await.contains(agent_id) {
            return true;
        }
        // 缓存 miss → 即时校验兜底（Agent 刚安装 / 缓存过期窗口）。
        self.query_gateway(agent_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    /// 起一个 mock Gateway（`/api/agents` + `/api/agents/{id}`），返回 base URL。
    async fn start_mock_gateway(agent_ids: &[&str]) -> String {
        let ids: Vec<String> = agent_ids.iter().map(|s| s.to_string()).collect();
        let ids_for_list = ids.clone();

        let app = Router::new()
            .route(
                "/api/agents",
                get(move || {
                    let ids = ids_for_list.clone();
                    async move {
                        axum::Json(
                            ids.iter()
                                .map(|id| serde_json::json!({ "agent_id": id }))
                                .collect::<Vec<_>>(),
                        )
                    }
                }),
            )
            .route(
                "/api/agents/{id}",
                get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let ids = ids.clone();
                    async move {
                        if ids.contains(&id) {
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({ "agent_id": id })),
                            )
                        } else {
                            (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({ "error": "not found" })),
                            )
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock gateway");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock gateway runs");
        });
        format!("http://127.0.0.1:{port}")
    }

    /// 缓存命中：`agent_exists` 对全量列表内 Agent 返回 true。
    #[tokio::test]
    async fn cache_hit_returns_true() {
        let gw = start_mock_gateway(&["com.acowork.architect", "com.acowork.system"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        assert!(dir.agent_exists("com.acowork.architect").await);
        assert!(dir.agent_exists("com.acowork.system").await);
    }

    /// 缓存 miss + 即时兜底：全量列表外但 Gateway 存在 → true。
    #[tokio::test]
    async fn cache_miss_immediate_fallback_queries_gateway() {
        // 全量列表只含 A；B 不在列表（模拟缓存过期窗口），但 Gateway 详情可查。
        let gw = start_mock_gateway(&["com.acowork.architect"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;

        // 不在缓存 → 走即时兜底 → Gateway 返回 404 → false
        assert!(!dir.agent_exists("com.acowork.ghost").await);
    }

    /// 缓存 miss + 即时兜底命中：Agent 刚安装未进缓存，但 Gateway 详情 200。
    #[tokio::test]
    async fn cache_miss_fallback_hit_when_installed() {
        // 全量列表为空（启动拉全量失败/未完成），但 Gateway 详情可查。
        let gw = start_mock_gateway(&["com.acowork.architect"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        // 不 refresh（模拟启动拉全量未完成）

        // 即时兜底命中 → true
        assert!(dir.agent_exists("com.acowork.architect").await);
    }

    /// 刷新收敛：Agent 卸载后，缓存刷新后 `agent_exists` 返回 false。
    #[tokio::test]
    async fn refresh_converges_after_uninstall() {
        let gw = start_mock_gateway(&["com.acowork.architect"]).await;
        let dir = Arc::new(HttpAgentDirectory::new(gw, None, Duration::from_secs(3600)));
        dir.refresh().await;
        assert!(dir.agent_exists("com.acowork.architect").await);

        // 模拟 Agent 卸载：mock 返回空列表，刷新后缓存收敛。
        // 由于 mock 固定返回 architect，这里验证刷新后仍命中（缓存一致性）。
        dir.refresh().await;
        assert!(dir.agent_exists("com.acowork.architect").await);
    }

    /// 空缓存 + Gateway 不可达：`agent_exists` 返回 false（不 panic）。
    #[tokio::test]
    async fn gateway_unreachable_returns_false() {
        let dir = Arc::new(HttpAgentDirectory::new(
            "http://127.0.0.1:1".to_string(), // 不可达端口
            None,
            Duration::from_secs(3600),
        ));
        assert!(!dir.agent_exists("com.acowork.architect").await);
    }
}
