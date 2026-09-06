//! D4 远程场景端到端测试（远程 Runtime 经 advertise endpoint 调用 doc MCP）。
//!
//! 与 pm `remote_e2e.rs` 同构：**真实 TCP server** + reqwest，把 doc router
//! nest 到 `/api/doc` 公开前缀（与 Gateway doc_proxy 剥 `/api/doc` 前的公开
//! 形态一致），模拟「远程 Runtime 拿到 catalog 下发的 `doc_mcp_url`
//! （`http://{advertise_host}:{gw_http_port}/api/doc/mcp`）后经 HTTP MCP 调用」。
//!
//! | 维度 | api_mcp_integration（本地） | 本文件 remote_e2e（远程形态） |
//! |------|----------------------------|------------------------------|
//! | MCP 路径 | `{base}/mcp`（doc 内部路径） | `{base}/api/doc/mcp`（公开 advertise 路径） |
//! | REST 路径 | 直连 doc 内部 `/tree` 等 | 同 base `/api/doc/tree` 等 |
//! | 身份 | `X-MCP-Actor` 直传 | 同（gateway 信任注入由 doc_proxy 单测覆盖） |
//!
//! 覆盖（对齐开发计划 §3.5 D4-2 / D4-3）：
//!
//! 1. **D4-3 远程全链路**：人类 REST 建目录/文档 → 远程 Agent（受信 actor）经
//!    MCP HTTP 读文档 → `doc_submit_update` → 人类 REST approve 合并 → 远程
//!    Agent `doc_check_request` 轮询到 approved + 文档 v2 可见。
//! 2. **D4-2 身份**：匿名经 advertise endpoint 只读通过；匿名写 → JSON-RPC
//!    Forbidden（-32002）——doc 的信任校验点是 Gateway（doc_proxy 剥离未
//!    受信 X-MCP-Actor），doc server 侧按「有 actor=受信 / 无 actor=匿名」。
//!
//! 数据目录用 tempdir，测试自建自清，无持久化副作用。

use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use axum::Router;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

const REMOTE_AGENT: &str = "agent-remote.example"; // 模拟 Gateway 已信任并注入

/// 真实启动：先 bind listener 拿地址，再 spawn serve；router nest 到 `/api/doc`。
async fn spawn_public() -> (String, TempDir) {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..DocConfig::default()
    };
    let svc = Arc::new(DocService::new(config).await.unwrap());
    let app = Router::new().nest("/api/doc", svc.router());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("remote_e2e server exited: {e}");
        }
    });
    (format!("http://{addr}/api/doc"), data)
}

/// JSON-RPC tools/call（模拟 Runtime MCP 客户端，actor=None → 匿名）。
async fn mcp_call(base: &str, actor: Option<&str>, name: &str, args: Value) -> Value {
    let client = reqwest::Client::new();
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let mut req = client
        .post(format!("{base}/mcp"))
        .header("content-type", "application/json");
    if let Some(a) = actor {
        req = req.header("x-mcp-actor", a);
    }
    let resp = req.json(&payload).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "MCP endpoint always answers 200");
    resp.json::<Value>().await.unwrap()
}

/// 成功调用解包；错误则 panic。
async fn ok(base: &str, actor: Option<&str>, name: &str, args: Value) -> Value {
    let resp = mcp_call(base, actor, name, args).await;
    assert!(resp.get("error").is_none(), "tool {name} failed: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

/// 期望 JSON-RPC error，返回 (code, message)。
async fn tool_err(base: &str, actor: Option<&str>, name: &str, args: Value) -> (i64, String) {
    let resp = mcp_call(base, actor, name, args).await;
    let code = resp["error"]["code"].as_i64().expect("error code");
    let msg = resp["error"]["message"].as_str().unwrap().to_string();
    (code, msg)
}

/// REST（人类 Desktop 经 gateway /api/doc/*）。
async fn rest(base: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let client = reqwest::Client::new();
    let mut req = client
        .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{base}{path}"))
        .header("x-actor", "human");
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn rest_get(base: &str, path: &str) -> Value {
    let (s, v) = rest(base, "GET", path, None).await;
    assert_eq!(s, StatusCode::OK, "GET {path}: {v}");
    v
}

async fn rest_post(base: &str, path: &str, body: Value) -> (StatusCode, Value) {
    rest(base, "POST", path, Some(body)).await
}

/// D4-3 远程全链路：人类建目录/文档 → 远程 Agent 读/改 → 人类 approve →
/// 远程 Agent 轮询到 approved + 文档 v2。
#[tokio::test(flavor = "multi_thread")]
async fn remote_agent_full_review_loop_over_public_mcp() {
    let (base, _tmp) = spawn_public().await;

    // ── 人类（Desktop）：REST 建目录 + 文档 ─────────────────
    let (s, dir) = rest_post(&base, "/dirs", json!({ "parent_dir_id": "root", "name": "设计" })).await;
    assert_eq!(s, StatusCode::CREATED, "{dir}");
    let dir_id = dir["dir_id"].as_str().unwrap();

    let (s, doc) = rest_post(
        &base,
        "/docs",
        json!({ "parent_dir_id": dir_id, "title": "远程方案", "content": "# v1 由人类创建" }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{doc}");
    let doc_id = doc["doc_id"].as_str().unwrap();
    assert_eq!(doc["version"], 1);

    // ── 远程 Agent：经 advertise MCP 读文档（人类内容可见）──
    let v = ok(&base, Some(REMOTE_AGENT), "doc_read", json!({ "ref": doc_id })).await;
    assert_eq!(v["version"], 1);
    assert!(v["content"].as_str().unwrap().contains("由人类创建"));

    // ── 远程 Agent：doc_pull（拿 base_version）→ submit_update ──
    let v = ok(&base, Some(REMOTE_AGENT), "doc_pull", json!({ "ref": doc_id })).await;
    assert_eq!(v["base_version"], 1);

    let v = ok(
        &base,
        Some(REMOTE_AGENT),
        "doc_submit_update",
        json!({ "ref": doc_id, "content": "# v1 由人类创建\n\n远程 Agent 补充 v2", "base_version": 1 }),
    )
    .await;
    let request_id = v["request_id"].as_str().unwrap().to_string();
    assert_eq!(v["status"], "pending");

    // ── 人类 REST：pending 队列可见 → approve ──────────────
    let list = rest_get(&base, "/requests?status=pending").await;
    assert!(list.as_array().unwrap().iter().any(|r| r["request_id"] == request_id));

    let (s, approved) = rest_post(
        &base,
        &format!("/requests/{request_id}/approve"),
        json!({ "reviewed_by": "human:desktop", "note": "同意" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{approved}");
    assert_eq!(approved["doc_version"], 2);

    // ── 远程 Agent 轮询：approved + doc v2 合并内容可见 ────
    let v = ok(&base, Some(REMOTE_AGENT), "doc_check_request", json!({ "request_id": request_id })).await;
    assert_eq!(v["status"], "approved");
    assert_eq!(v["reviewed_by"], "human:desktop");

    let v = ok(&base, Some(REMOTE_AGENT), "doc_read", json!({ "ref": doc_id })).await;
    assert_eq!(v["version"], 2);
    assert!(v["content"].as_str().unwrap().contains("远程 Agent 补充"));
}

/// D4-2 身份：匿名经 advertise endpoint 只读允许；写 → -32002 forbidden。
#[tokio::test(flavor = "multi_thread")]
async fn remote_anonymous_read_only_writes_forbidden() {
    let (base, _tmp) = spawn_public().await;

    // 匿名只读（空库）→ 正常
    let v = ok(&base, None, "doc_list", json!({})).await;
    assert_eq!(v["total"], 0);

    // 匿名写 → -32002 forbidden
    let (code, msg) = tool_err(&base, None, "doc_mkdir", json!({ "path": "x" })).await;
    assert_eq!(code, -32002, "{msg}");
    assert!(msg.contains("forbidden"), "{msg}");

    // 受信 actor 写 → 允许（目录创建成功）
    let v = ok(&base, Some(REMOTE_AGENT), "doc_mkdir", json!({ "path": "远程资料" })).await;
    assert_eq!(v["name"], "远程资料");

    // 人类 REST 可见远程 Agent 建的目录（跨侧一致性）
    let tree = rest_get(&base, "/tree?dir_id=root").await;
    assert!(tree["dirs"].as_array().unwrap().iter().any(|d| d["name"] == "远程资料"));
}
