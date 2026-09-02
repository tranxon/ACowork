//! P4 远程场景端到端测试（T4-2 / T4-3）。
//!
//! 用**真实 HTTP server + reqwest 客户端**模拟「远程 Runtime 通过 advertise
//! endpoint 调用 pm MCP」的完整链路 —— 这是与现有 oneshot router 测试的本质区别：
//!
//! | 维度 | 现有 handlers_e2e / mcp tests | 本文件 remote_e2e |
//! |------|-------------------------------|-------------------|
//! | 传输 | axum `oneshot`（内存内，不走 TCP） | 真实 `TcpListener` + `axum::serve` |
//! | 客户端 | tower `ServiceExt` | reqwest HTTP 客户端 |
//! | 路径 | 内部路径 `/mcp` | 公开路径 `/api/pm/mcp`（Gateway `nest_service` 形态） |
//! | 身份 | 测试内构造 header | 真实 HTTP `X-MCP-Actor` header 往返 |
//!
//! 覆盖场景（对应开发计划 §3.5 T4-2 / T4-3）：
//!
//! 1. **T4-3 远程全链路**：人类 REST 建项目 + 建任务（指派远程 Agent）→ 远程
//!    Agent 经 MCP HTTP `pm_claim_task` → `pm_submit_task` → 人类 REST 查询
//!    看板确认任务已 submitted（看板刷新）。
//! 2. **T4-2 身份校验**（设计 §9.2）：非 assignee 调 `pm_claim_task` / 
//!    `pm_submit_task` 返回 JSON-RPC Forbidden（-32002）。
//! 3. **§9.3 匿名只读**：无 `X-MCP-Actor` 调 `pm_list_projects` 允许；调
//!    `pm_claim_task` 拒绝（Unauthenticated -32001）。

use std::sync::Arc;

use acowork_pm::{AgentDirectory, PmConfig, PmService};
use axum::Router;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// 白名单 Agent 目录：模拟 Gateway `installed_agents` 视图（设计 §9.1）。
struct WhitelistDir(Vec<String>);

#[async_trait::async_trait]
impl AgentDirectory for WhitelistDir {
    async fn agent_exists(&self, agent_id: &str) -> bool {
        self.0.iter().any(|a| a == agent_id)
    }
}

/// 启动真实 PM HTTP server，router 挂到 `/api/pm` 前缀下（与 Gateway
/// `nest_service("/api/pm", ...)` 生产形态一致）。
///
/// 返回 (公开 base URL, tempdir 句柄)。MCP 端点 = `{base}/mcp`。
async fn start_remote_server() -> (String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = PmConfig::default();
    cfg.data_dir = tmp.path().to_path_buf();
    cfg.index_rebuild_on_start = false;

    let agent_dir: Arc<dyn AgentDirectory> =
        Arc::new(WhitelistDir(vec!["agent-remote".to_string()]));
    let svc = PmService::with_agent_directory(cfg, agent_dir)
        .await
        .expect("PmService should start");

    let app = Router::new().nest_service("/api/pm", svc.router());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "remote e2e server exited");
        }
    });

    (format!("http://{addr}/api/pm"), tmp)
}

/// 发送一次 MCP JSON-RPC `tools/call`，返回完整 JSON-RPC 响应 Value。
async fn mcp_call(
    client: &reqwest::Client,
    base: &str,
    actor: Option<&str>,
    id: u64,
    tool: &str,
    args: Value,
) -> Value {
    let mut req = client
        .post(format!("{base}/mcp"))
        .header("content-type", "application/json")
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            })
            .to_string(),
        );
    if let Some(a) = actor {
        req = req.header("x-mcp-actor", a);
    }
    let resp = req.send().await.expect("mcp http request should succeed");
    assert_eq!(
        resp.status(),
        200,
        "MCP endpoint should return 200 for valid JSON-RPC"
    );
    resp.json::<Value>().await.expect("JSON-RPC response should parse")
}

/// 断言 tools/call 成功，并返回工具结果（`result.content[0].text` 解析后的 Value）。
fn tool_result(v: &Value) -> Value {
    assert!(v["error"].is_null(), "tool call should succeed: {v}");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a JSON string");
    serde_json::from_str(text).expect("tool result text should be valid JSON")
}

/// 断言 tools/call 失败并返回给定 JSON-RPC error code。
fn assert_rpc_error(v: &Value, code: i32) {
    assert!(
        v["error"].is_object(),
        "expected JSON-RPC error, got success: {v}"
    );
    assert_eq!(
        v["error"]["code"].as_i64().unwrap_or(0) as i32,
        code,
        "unexpected error code in {v}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// T4-3：远程 Agent 全链路 —— 人类建项目/任务 → 远程 Agent claim → submit →
// 看板刷新（人类 REST 确认任务状态 = submitted）。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn remote_agent_claim_submit_full_chain() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();
    let actor = "agent-remote";

    // ── 1. 人类（REST，x-actor=human）建项目 ──────────────────────────
    let resp = client
        .post(format!("{base}/projects"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "远程协作项目", "description": "P4 e2e" }).to_string())
        .send()
        .await
        .expect("create project http request");
    assert_eq!(resp.status(), 200, "human creates project via REST");
    let proj: Value = resp.json().await.unwrap();
    let pid = proj["id"].as_str().unwrap().to_string();

    // ── 2. 人类建任务，指派给远程 Agent ────────────────────────────────
    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(
            json!({
                "title": "远程任务",
                "description": "需要远程节点执行",
                "assignee": actor,
            })
            .to_string(),
        )
        .send()
        .await
        .expect("create task http request");
    assert_eq!(resp.status(), 200, "human creates task via REST");
    let task: Value = resp.json().await.unwrap();
    let tid = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["status"], "pending", "human-created task starts as pending");

    // ── 3. 远程 Agent 自查（pm_list_tasks，过滤 assignee 自己）──────────
    let v = mcp_call(
        &client,
        &base,
        Some(actor),
        1,
        "pm_list_tasks",
        json!({ "project_id": pid, "assignee": actor }),
    )
    .await;
    let tasks = tool_result(&v);
    let tids: Vec<String> = tasks
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tids.contains(&tid),
        "remote agent should see its assigned task in pm_list_tasks: {tasks}"
    );

    // ── 4. 远程 Agent claim（pending → in_progress）─────────────────────
    let v = mcp_call(&client, &base, Some(actor), 2, "pm_claim_task", json!({ "task_id": tid }))
        .await;
    let claimed = tool_result(&v);
    assert_eq!(claimed["status"], "in_progress", "claim should move to in_progress");

    // ── 5. 远程 Agent submit（in_progress → submitted）──────────────────
    let v = mcp_call(
        &client,
        &base,
        Some(actor),
        3,
        "pm_submit_task",
        json!({ "task_id": tid, "text": "远程完成，结果已交付" }),
    )
    .await;
    let submitted = tool_result(&v);
    assert_eq!(submitted["status"], "submitted", "submit should move to submitted");

    // ── 6. 看板刷新：人类 REST 查询任务，确认远程结果已落盘 ─────────────
    let resp = client
        .get(format!("{base}/tasks/{tid}"))
        .send()
        .await
        .expect("get task http request");
    assert_eq!(resp.status(), 200, "human board refresh");
    let board: Value = resp.json().await.unwrap();
    assert_eq!(board["status"], "submitted", "board shows submitted");
    assert_eq!(board["result"]["text"], "远程完成，结果已交付");
    assert_eq!(board["result"]["submitted_by"], actor);
}

// ═══════════════════════════════════════════════════════════════════════
// T4-2：身份校验 —— 非 assignee 调用 claim/submit 一律 Forbidden。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn non_assignee_mutation_rejected_over_http() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();
    let owner = "agent-remote";
    let intruder = "agent-intruder";

    // 建项目 + 任务（assignee = agent-remote）
    let resp = client
        .post(format!("{base}/projects"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "P" }).to_string())
        .send()
        .await
        .unwrap();
    let proj: Value = resp.json().await.unwrap();
    let pid = proj["id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "T", "assignee": owner }).to_string())
        .send()
        .await
        .unwrap();
    let task: Value = resp.json().await.unwrap();
    let tid = task["id"].as_str().unwrap().to_string();

    // 入侵者 claim → Forbidden（-32002）
    let v = mcp_call(
        &client,
        &base,
        Some(intruder),
        1,
        "pm_claim_task",
        json!({ "task_id": tid }),
    )
    .await;
    assert_rpc_error(&v, -32002);

    // 任务未被改变（仍是 pending）
    let resp = client.get(format!("{base}/tasks/{tid}")).send().await.unwrap();
    let board: Value = resp.json().await.unwrap();
    assert_eq!(board["status"], "pending", "intruder claim must not mutate");

    // 入侵者 submit → Forbidden（即便 assignee 是别人）
    let v = mcp_call(
        &client,
        &base,
        Some(intruder),
        2,
        "pm_submit_task",
        json!({ "task_id": tid, "text": "hacked" }),
    )
    .await;
    assert_rpc_error(&v, -32002);

    // 拥有者仍可正常 claim → in_progress（身份边界正确）
    let v = mcp_call(&client, &base, Some(owner), 3, "pm_claim_task", json!({ "task_id": tid }))
        .await;
    let claimed = tool_result(&v);
    assert_eq!(claimed["status"], "in_progress");
}

// ═══════════════════════════════════════════════════════════════════════
// §9.3：匿名只读 —— 无 X-MCP-Actor 允许只读工具、拒绝状态变更工具。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn anonymous_readonly_allowed_mutation_rejected() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();

    // 匿名 list 允许
    let v = mcp_call(&client, &base, None, 1, "pm_list_projects", json!({})).await;
    tool_result(&v); // 不 panic 即成功

    // 匿名 get（任意 id）→ 允许只读，404 是业务错误而非鉴权错误
    let v = mcp_call(&client, &base, None, 2, "pm_get_task", json!({ "task_id": "t-nonexistent" }))
        .await;
    assert!(v["error"].is_object(), "anonymous get on missing task returns error");

    // 匿名 claim → Unauthenticated（-32001）
    let v = mcp_call(&client, &base, None, 3, "pm_claim_task", json!({ "task_id": "t-x" }))
        .await;
    assert_rpc_error(&v, -32001);
}
