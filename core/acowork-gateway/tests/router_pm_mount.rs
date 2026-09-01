//! ADR-061 P1 — `build_router_with_pm` 双路径集成测试。
//!
//! 覆盖：
//!
//! 1. **`Some(pm)` 路径**：传真实 `PmService` 时，`/api/pm/*` 路由被正确挂载，
//!    `GET /api/pm/projects` 返回 PM 端 schema 一致的空数组 JSON。
//! 2. **`None` 路径**：不传 PM 时不 panic、`warn!` 一行，`/api/pm/*` 路由**不**
//!    被挂载（返回 404），其余 Gateway 路由（`/health`、`/api/status`）仍正常工作。
//!
//! 这两条路径在生产 Gateway 启动时都会出现：
//! - `Some`：PM 初始化成功
//! - `None`：PM 初始化失败（PM 是 non-fatal，Gateway 继续跑）或 PM 启动前
//!
//! 参考：
//! - `core/acowork-gateway/src/http/routes.rs::build_router_with_pm`
//! - `core/acowork-pm/src/server.rs::PmService::new`
//! - `core/acowork-pm/src/api/projects.rs::list`

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tokio::sync::RwLock;
use tower::ServiceExt;

use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::http::auth::HttpAuth;
use acowork_gateway::http::routes::{build_router_with_pm, AppState};
use acowork_pm::PmConfig;

/// 构造一个最小的 `AppState`（与 `routes.rs::tests::test_app_state` 等价，
/// 但这里需要公开使用所以独立实现）。
///
/// 每个测试用独立临时目录（`AtomicU64` 保证唯一性），`HttpAuth::new(false)`
/// 表示不强制 token —— 这些测试只关心路由挂载与转发的正确性，
/// 不重复 `routes.rs` 内部的健康检查覆盖。
fn make_test_app_state() -> AppState {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-router-pm-mount-{}-{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let gw_state = GatewayState::new(&dir.to_string_lossy());
    AppState::new(
        Arc::new(RwLock::new(gw_state)),
        Arc::new(HttpAuth::new(false)),
    )
}

/// 构造一个最小可用的 `PmConfig`（`data_dir` 指向独立临时目录）。
///
/// 用 `PmConfig::default()` 后覆写 `data_dir`，因为 `default_data_dir` 走
/// `directories::ProjectDirs` —— 在 CI/sandbox 里可能指向共享路径，
/// 这里用临时目录保证测试隔离。
async fn make_test_pm_service() -> Arc<acowork_pm::PmService> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-router-pm-mount-pm-{}-{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut cfg = PmConfig::default();
    cfg.data_dir = dir;
    cfg.index_rebuild_on_start = false; // 测试不需要重建索引

    let svc = acowork_pm::PmService::new(cfg)
        .await
        .expect("PmService::new should succeed with temp dir");
    Arc::new(svc)
}

/// ── Some(pm) ────────────────────────────────────────────────────────

/// `Some(pm)` 路径：挂载后 `GET /api/pm/projects` 返回 200 + 空数组 JSON。
///
/// 断言点：
/// - 状态码 200
/// - body 是合法 JSON 数组（空数组，因为没有创建项目）
/// - **不**触发 None 路径的 `warn!`（间接通过构造 Some 确认）
#[tokio::test]
async fn build_router_with_pm_some_mounts_routes() {
    let state = make_test_app_state();
    let pm = make_test_pm_service().await;

    let router = build_router_with_pm(state, Some(pm));

    let request = Request::builder()
        .method("GET")
        .uri("/api/pm/projects")
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(request).await.expect("router responds");
    let status = response.status();

    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read response body");
    let body_str = String::from_utf8_lossy(&bytes).to_string();

    assert_eq!(
        status,
        StatusCode::OK,
        "GET /api/pm/projects with Some(pm) should return 200, got {status} body={body_str}"
    );

    // 空数组 JSON —— 与 `acowork_pm::api::projects::list` 返回类型对齐
    let parsed: serde_json::Value =
        serde_json::from_str(&body_str).expect("body should be valid JSON");
    assert!(
        parsed.is_array(),
        "expected JSON array, got: {parsed}"
    );
    assert_eq!(
        parsed.as_array().unwrap().len(),
        0,
        "fresh PM should have zero projects"
    );
}

/// `Some(pm)` 路径：挂载后 Gateway 原有路由（`/health`）仍可用，
/// 验证 `nest_service` 不会破坏外层路由。
#[tokio::test]
async fn build_router_with_pm_some_preserves_gateway_routes() {
    let state = make_test_app_state();
    let pm = make_test_pm_service().await;

    let router = build_router_with_pm(state, Some(pm));

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(request).await.expect("router responds");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/health should still respond 200 when PM is mounted"
    );
}

/// ── None ────────────────────────────────────────────────────────────

/// `None` 路径：不传 PM 不 panic，`/api/pm/projects` 返回 404
/// （路由未挂载）。
///
/// 关键约束：这条路径在 PM 初始化失败时会被 Gateway 走，
/// 必须**不**让整个 HTTP server 启动失败（non-fatal 语义）。
#[tokio::test]
async fn build_router_with_pm_none_does_not_mount_routes() {
    let state = make_test_app_state();

    let router = build_router_with_pm(state, None);

    let request = Request::builder()
        .method("GET")
        .uri("/api/pm/projects")
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(request).await.expect("router responds");

    // 路由未挂载 → axum 默认 fallback 到 `404 Not Found`
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "GET /api/pm/projects with None should return 404, got {}",
        response.status()
    );
}

/// `None` 路径：Gateway 原有路由（`/health`）仍可用，
/// 验证 `None` 不会破坏外层路由（与 `Some` 对照）。
#[tokio::test]
async fn build_router_with_pm_none_preserves_gateway_routes() {
    let state = make_test_app_state();

    let router = build_router_with_pm(state, None);

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(request).await.expect("router responds");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/health should still respond 200 when PM is None"
    );
}

/// `None` 路径：`/api/pm/*` 下其它路径（嵌套子路由）也都不挂载。
///
/// 防止 PM router 内部把整个 `/api/pm` 吞掉变成 405/200 等其他状态码，
/// 这里随机挑一个子路径（`/api/pm/tasks/anything`）确认同样是 404。
#[tokio::test]
async fn build_router_with_pm_none_does_not_mount_nested_routes() {
    let state = make_test_app_state();

    let router = build_router_with_pm(state, None);

    let request = Request::builder()
        .method("GET")
        .uri("/api/pm/tasks/anything")
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(request).await.expect("router responds");

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "nested /api/pm/tasks/* with None should return 404, got {}",
        response.status()
    );
}
