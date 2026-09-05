//! End-to-end MCP ↔ REST review-loop integration tests (D3-5).
//!
//! Boots the real doc server (full router: REST + `/mcp`), then drives the
//! **Agent side** over JSON-RPC (with `X-MCP-Actor`, as injected by the
//! Gateway catalog) and the **human side** over REST:
//!
//! ```text
//! Agent doc_mkdir → doc_add → (REST GET 人类可见)
//!      → doc_pull → doc_submit_update → (REST approve)
//!      → doc_check_request=approved → doc_read shows merged v2
//! ```
//!
//! Identity: anonymous callers get read-only tools; writes return JSON-RPC
//! `-32002` (permission), mirroring design §9.

use std::net::SocketAddr;
use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

const AGENT: &str = "com.example.agent";

struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

async fn spawn() -> TestServer {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..Default::default()
    };
    let svc = Arc::new(DocService::new(config).await.unwrap());
    let addr: SocketAddr = svc
        .clone()
        .serve(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _data: data,
    }
}

impl TestServer {
    /// JSON-RPC `tools/call`（模拟 Runtime MCP 客户端 + Gateway 注入身份）。
    async fn tool(&self, actor: Option<&str>, name: &str, args: Value) -> Value {
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let mut req = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json");
        if let Some(a) = actor {
            req = req.header("x-mcp-actor", a);
        }
        let resp = req.json(&payload).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "MCP endpoint always answers 200");
        resp.json::<Value>().await.unwrap()
    }

    /// 成功调用的解包内容（`result.content[0].text` JSON）；错误则 panic。
    async fn ok(&self, actor: Option<&str>, name: &str, args: Value) -> Value {
        let resp = self.tool(actor, name, args).await;
        assert!(resp.get("error").is_none(), "tool {name} failed: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    /// 期望 JSON-RPC error，返回 error code + message。
    async fn tool_err(&self, actor: Option<&str>, name: &str, args: Value) -> (i64, String) {
        let resp = self.tool(actor, name, args).await;
        let code = resp["error"]["code"].as_i64().expect("error code");
        let msg = resp["error"]["message"].as_str().unwrap().to_string();
        (code, msg)
    }

    async fn rest_get(&self, path: &str) -> Value {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {path}");
        resp.json::<Value>().await.unwrap()
    }

    async fn rest_post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let value = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, value)
    }
}

/// D3-5 核心闭环：Agent 经 MCP 建目录/加文档 → 人类 REST 可见 →
/// Agent pull/submit_update → 人类 approve → Agent 轮询到 approved +
/// 文档 v2 合并内容。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_full_review_loop_agent_human() {
    let srv = spawn().await;

    // ── Agent 建目录 + add-to-doc（直接生效，带 import 来源）──────────
    let v = srv.ok(Some(AGENT), "doc_mkdir", json!({ "path": "研发" })).await;
    assert_eq!(v["name"], "研发");

    let v = srv
        .ok(
            Some(AGENT),
            "doc_add",
            json!({
                "path": "研发",
                "title": "需求说明",
                "content": "# v1 需求",
                "source_workspace": "ws-main",
                "source_path": "docs/req.md",
            }),
        )
        .await;
    let doc_id = v["doc_id"].as_str().unwrap().to_string();
    assert_eq!(v["version"], 1);

    // ── 人类可见：REST tree 显示新目录与新文档，import 元数据完整 ──────
    let tree = srv.rest_get("/tree?dir_id=root").await;
    assert_eq!(tree["dirs"][0]["name"], "研发");
    let doc = srv.rest_get(&format!("/docs/{doc_id}")).await;
    assert_eq!(doc["meta"]["version"], 1);
    assert_eq!(doc["content"], "# v1 需求");
    assert_eq!(doc["meta"]["import"]["agent_id"], AGENT);
    assert_eq!(doc["meta"]["import"]["workspace_path"], "ws-main:docs/req.md");

    // ── Agent pull（拿 base_version + 建议缓存路径）→ 提交更新 ──────────
    let v = srv.ok(Some(AGENT), "doc_pull", json!({ "ref": doc_id })).await;
    assert_eq!(v["base_version"], 1);
    assert!(v["cache_path"].as_str().unwrap().contains(doc_id.as_str()));

    let v = srv
        .ok(
            Some(AGENT),
            "doc_submit_update",
            json!({ "ref": doc_id, "content": "# v2 需求（agent 修订）", "base_version": 1 }),
        )
        .await;
    let request_id = v["request_id"].as_str().unwrap().to_string();
    assert_eq!(v["status"], "pending");

    // ── 人类 REST approve → 合并，doc version+1 ────────────────────────
    let (status, body) = srv
        .rest_post(
            &format!("/requests/{request_id}/approve"),
            json!({ "reviewed_by": "human:zhang", "note": "可行" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["doc_version"], 2);

    // ── Agent 轮询：approved + note；read 看到合并后 v2 ────────────────
    let v = srv.ok(None, "doc_check_request", json!({ "request_id": request_id })).await;
    assert_eq!(v["status"], "approved");
    assert_eq!(v["reviewed_by"], "human:zhang");

    let v = srv.ok(None, "doc_read", json!({ "ref": doc_id })).await;
    assert_eq!(v["version"], 2);
    assert!(v["content"].as_str().unwrap().contains("agent 修订"));

    // 人类再次直接 PUT（版本并发仍生效）—— doc 现 v2，stale base 409
    let (status, body) = srv
        .rest_post(
            &format!("/docs/{doc_id}/move"),
            json!({ "target_dir_id": "root", "overwrite": false }),
        )
        .await;
    // move 不需要版本号：目标 root 同名检查 → root 无同名 → 成功
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc = srv.rest_get(&format!("/docs/{doc_id}")).await;
    assert_eq!(doc["meta"]["version"], 2, "move must not bump version");
}

/// 匿名：只读工具可用；写工具 JSON-RPC -32002（permission 分类）。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_anonymous_read_only_writes_forbidden() {
    let srv = spawn().await;

    // 匿名只读（空库 list / search）→ 正常返回
    let v = srv.ok(None, "doc_list", json!({})).await;
    assert_eq!(v["total"], 0);
    let v = srv.ok(None, "doc_search", json!({ "keyword": "x" })).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 0);

    // 匿名写 → -32002 forbidden，message 带 forbidden 前缀
    let (code, msg) = srv.tool_err(None, "doc_mkdir", json!({ "path": "x" })).await;
    assert_eq!(code, -32002, "{msg}");
    assert!(msg.contains("forbidden"), "{msg}");

    let (code, msg) = srv
        .tool_err(None, "doc_submit_update", json!({ "ref": "doc-ffffffffffff", "content": "x", "base_version": 1 }))
        .await;
    assert_eq!(code, -32002, "{msg}");
}

/// stale base_version 提交 → 业务错误（version_conflict 前缀，非协议错误）。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_submit_stale_base_reports_version_conflict() {
    let srv = spawn().await;
    let v = srv
        .ok(Some(AGENT), "doc_add", json!({ "title": "周报", "content": "v1" }))
        .await;
    let doc_id = v["doc_id"].as_str().unwrap().to_string();

    let (code, msg) = srv
        .tool_err(
            Some(AGENT),
            "doc_submit_update",
            json!({ "ref": doc_id, "content": "stale", "base_version": 42 }),
        )
        .await;
    assert!(code != 0, "{msg}");
    assert!(msg.contains("version_conflict"), "{msg}");
}

