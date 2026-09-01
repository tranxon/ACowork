//! MCP 工具分发（**P3 完整实现**）。
//!
//! 所有 `pm_*` 工具在此实现。每个工具：
//!
//! 1. 解析参数（serde，宽松 `default`）
//! 2. 执行身份校验（设计 §9.2 / §9.3）：
//!    - 匿名（无 `X-MCP-Actor`）仅允许只读工具：`pm_list_*` / `pm_get_*`
//!    - 状态变更工具要求身份
//!    - `pm_claim_task` / `pm_submit_task` / `pm_update_task` 要求
//!      调用者 `agent_id` == 任务 `assignee`，否则 403
//!    - `pm_create_task` 的 `assignee` 必须存在于 Agent 目录（§9.1）
//! 3. 调用 [`PmStore`] trait 业务方法
//! 4. 返回精简 JSON（复用 REST `TaskResponse` 形状，仅 LLM 关心的字段）
//!
//! 错误统一返回 [`PmError`]；由 [`crate::mcp::mod::jsonrpc_endpoint`] 包装为
//! JSON-RPC error（message 带 `error_code:` 前缀，客户端据此做
//! `[permission]` / `[transient]` 分类）。

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{PmError, Result};
use crate::mcp::McpState;
use crate::store::tree::{PmStore, TreePmStore};
use crate::types::{
    CreateProject, CreateTask, Dependency, Priority, ProjectId, ProjectStatus, ReparentTask,
    ReviewStatus, Task, TaskFilter, TaskId, TaskResponse, TaskSort, TaskStatus, TaskType,
    UpdateTask, deserialize_clearable,
};

/// 工具分发入口（由 `POST /mcp` 的 `tools/call` 调用）。
pub async fn dispatch(
    state: &McpState,
    actor: Option<&str>,
    name: &str,
    args: Value,
) -> Result<Value> {
    match name {
        // ── 只读（匿名允许，设计 §9.3）──────────────────────────────
        "pm_list_projects" => pm_list_projects(state, args).await,
        "pm_get_project" => pm_get_project(state, args).await,
        "pm_list_tasks" => pm_list_tasks(state, args).await,
        "pm_get_task" => pm_get_task(state, args).await,

        // ── 需要身份的只读（自查）───────────────────────────────────
        "pm_list_my_tasks" => {
            let a = require_actor(actor)?;
            pm_list_my_tasks(state, a, args).await
        }
        "pm_check_task" => {
            let a = require_actor(actor)?;
            pm_check_task(state, a, args).await
        }

        // ── 状态变更（需要身份）──────────────────────────────────────
        "pm_create_project" => {
            let a = require_actor(actor)?;
            pm_create_project(state, a, args).await
        }
        "pm_create_task" => {
            let a = require_actor(actor)?;
            pm_create_task(state, a, args).await
        }
        "pm_update_task" => {
            let a = require_actor(actor)?;
            pm_update_task(state, a, args).await
        }
        "pm_claim_task" => {
            let a = require_actor(actor)?;
            pm_claim_task(state, a, args).await
        }
        "pm_submit_task" => {
            let a = require_actor(actor)?;
            pm_submit_task(state, a, args).await
        }
        "pm_reparent_task" => {
            let a = require_actor(actor)?;
            pm_reparent_task(state, a, args).await
        }

        other => Err(PmError::BadRequest(format!("unknown tool: {other}"))),
    }
}

// ── 鉴权辅助 ──────────────────────────────────────────────────────────────

/// 要求调用方具备身份（匿名 → 401）。所有状态变更工具必须先过此关。
fn require_actor(actor: Option<&str>) -> Result<&str> {
    actor.ok_or_else(|| {
        PmError::Unauthenticated(
            "this tool requires an authenticated agent (send X-MCP-Actor header)".into(),
        )
    })
}

/// 要求调用方 == 任务 assignee（设计 §9.2）。`pm_claim_task` /
/// `pm_submit_task` / `pm_update_task` 使用。
fn ensure_assignee(task: &Task, actor: &str) -> Result<()> {
    match &task.assignee {
        Some(a) if a == actor => Ok(()),
        Some(a) => Err(PmError::Forbidden(format!(
            "task {} is assigned to `{}`, not `{}`; only the assignee can perform this action",
            task.id, a, actor
        ))),
        None => Err(PmError::Forbidden(format!(
            "task {} has no assignee; it must be assigned to `{}` before it can be acted on",
            task.id, actor
        ))),
    }
}

// ── 参数解析辅助 ──────────────────────────────────────────────────────────

fn parse_args<T: for<'de> Deserialize<'de>>(name: &str, args: Value) -> Result<T> {
    serde_json::from_value(args)
        .map_err(|e| PmError::BadRequest(format!("invalid arguments for {name}: {e}")))
}

// ── 响应序列化辅助 ────────────────────────────────────────────────────────

/// 任务 → 精简 JSON（复用 REST `TaskResponse` 形状，含派生字段）。
async fn task_to_value(store: &Arc<TreePmStore>, task: Task) -> Result<Value> {
    let tid = task.id.clone();
    let depth = store.index_entry(&tid).map(|e| e.depth).unwrap_or(0);
    let parent_id = store.parent_of(&tid);
    let blocked_by = store.compute_blocked_by(&tid).await?;
    let resp = TaskResponse {
        task,
        is_blocked: !blocked_by.is_empty(),
        blocked_by,
        depth,
        parent_id,
    };
    serde_json::to_value(resp).map_err(PmError::from)
}

/// 项目 → 精简 JSON。
fn project_to_value(p: crate::types::Project, task_count: usize) -> Value {
    json!({
        "id": p.id,
        "title": p.title,
        "description": p.description,
        "status": p.status,
        "created_by": p.created_by,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
        "task_count": task_count,
    })
}

// ── 只读工具 ──────────────────────────────────────────────────────────────

/// `pm_list_projects` — 项目列表（摘要 + 任务数）。
async fn pm_list_projects(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize, Default)]
    struct Args {
        #[serde(default)]
        include_archived: bool,
    }
    let a: Args = parse_args("pm_list_projects", args)?;

    let projects = state.store.list_projects().await?;
    let out: Vec<Value> = projects
        .into_iter()
        .filter(|p| {
            a.include_archived
                || !matches!(p.status, ProjectStatus::Archived | ProjectStatus::Completed)
        })
        .map(|p| {
            let count = state.store.project_task_count(&p.id);
            project_to_value(p, count)
        })
        .collect();
    Ok(Value::Array(out))
}

/// `pm_get_project` — 项目详情（含任务数分拆）。
async fn pm_get_project(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        project_id: ProjectId,
    }
    let a: Args = parse_args("pm_get_project", args)?;

    let p = state
        .store
        .get_project(&a.project_id)
        .await?
        .ok_or_else(|| PmError::ProjectNotFound(a.project_id.to_string()))?;
    let count = state.store.project_task_count(&a.project_id);
    Ok(project_to_value(p, count))
}

/// `pm_list_tasks` — 项目内任务列表（支持过滤 + limit）。
async fn pm_list_tasks(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        project_id: ProjectId,
        #[serde(default)]
        status: Option<TaskStatus>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        only_blocked: bool,
        #[serde(default = "default_limit")]
        limit: usize,
    }
    fn default_limit() -> usize {
        20
    }
    let a: Args = parse_args("pm_list_tasks", args)?;

    let filter = TaskFilter {
        project_id: Some(a.project_id),
        status: a.status,
        assignee: a.assignee,
        only_blocked: a.only_blocked,
        sort: Some(TaskSort::CreatedAt),
    };
    let tasks = state.store.find_tasks(&filter).await?;
    let mut out = Vec::new();
    for task in tasks.into_iter().take(a.limit.max(1)) {
        out.push(task_to_value(&state.store, task).await?);
    }
    Ok(Value::Array(out))
}

/// `pm_get_task` — 任务详情（含 is_blocked / blocked_by / depth / parent_id）。
async fn pm_get_task(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
    }
    let a: Args = parse_args("pm_get_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    task_to_value(&state.store, task).await
}

/// `pm_list_my_tasks` — Agent 自查：指派给当前调用者的任务。
async fn pm_list_my_tasks(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize, Default)]
    struct Args {
        #[serde(default)]
        status: Option<TaskStatus>,
        #[serde(default = "default_limit")]
        limit: usize,
    }
    fn default_limit() -> usize {
        20
    }
    let a: Args = parse_args("pm_list_my_tasks", args)?;

    let filter = TaskFilter {
        assignee: Some(actor.to_string()),
        status: a.status,
        ..Default::default()
    };
    let tasks = state.store.find_tasks(&filter).await?;
    let mut out = Vec::new();
    for task in tasks.into_iter().take(a.limit.max(1)) {
        out.push(task_to_value(&state.store, task).await?);
    }
    Ok(Value::Array(out))
}

/// `pm_check_task` — 查询自己创建的任务是否被批准（含审核状态）。
async fn pm_check_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
    }
    let a: Args = parse_args("pm_check_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    // 仅允许创建者查询（设计 §6：`Agent 查询自己创建的任务是否被批准`）。
    if task.created_by != actor {
        return Err(PmError::Forbidden(format!(
            "task {} was created by `{}`, not `{}`; only the creator can check its review status",
            task.id, task.created_by, actor
        )));
    }
    Ok(json!({
        "id": task.id,
        "project_id": task.project_id,
        "title": task.title,
        "status": task.status,
        "review_status": task.review_status,
        "approved": matches!(task.review_status, ReviewStatus::Approved),
        "created_by": task.created_by,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
    }))
}

// ── 状态变更工具 ──────────────────────────────────────────────────────────

/// `pm_create_project` — 创建项目。
async fn pm_create_project(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        title: String,
        #[serde(default)]
        description: String,
    }
    let a: Args = parse_args("pm_create_project", args)?;

    let input = CreateProject {
        title: a.title,
        description: a.description,
        metadata: Default::default(),
    };
    let p = state.store.create_project(input, actor).await?;
    Ok(project_to_value(p, 0))
}

/// `pm_create_task` — 创建任务（Agent 创建 → `review_status=pending`，待人类审核）。
///
/// `assignee` 若提供则必须存在于 Agent 目录（§9.1）；不要求是调用者本人（§9.2）。
async fn pm_create_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        project_id: ProjectId,
        title: String,
        #[serde(default)]
        description: String,
        #[serde(rename = "type", default)]
        task_type: TaskType,
        #[serde(default)]
        priority: Priority,
        #[serde(default)]
        parent_task_id: Option<TaskId>,
        #[serde(default)]
        depends_on: Vec<Dependency>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let a: Args = parse_args("pm_create_task", args)?;

    // 设计 §9.1：assignee 必须存在
    if let Some(assignee) = &a.assignee {
        if !state.agent_dir.agent_exists(assignee).await {
            return Err(PmError::BadRequest(format!(
                "assignee agent not found in agent directory: {assignee}"
            )));
        }
    }

    let input = CreateTask {
        title: a.title,
        description: a.description,
        task_type: a.task_type,
        priority: a.priority,
        parent_task_id: a.parent_task_id,
        depends_on: a.depends_on,
        attachment_ids: vec![],
        assignee: a.assignee,
        due_at: a.due_at,
    };
    let task = state.store.create_task(&a.project_id, input, actor).await?;
    task_to_value(&state.store, task).await
}

/// `pm_update_task` — 更新任务（仅 assignee 本人，设计 §9.2）。
async fn pm_update_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(rename = "type", default)]
        task_type: Option<TaskType>,
        #[serde(default)]
        status: Option<TaskStatus>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default, deserialize_with = "deserialize_clearable")]
        assignee: Option<Option<String>>,
        #[serde(default, deserialize_with = "deserialize_clearable")]
        due_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
        #[serde(default)]
        depends_on: Option<Vec<Dependency>>,
    }
    let a: Args = parse_args("pm_update_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    ensure_assignee(&task, actor)?;

    let input = UpdateTask {
        title: a.title,
        description: a.description,
        task_type: a.task_type,
        status: a.status,
        priority: a.priority,
        assignee: a.assignee,
        due_at: a.due_at,
        depends_on: a.depends_on,
    };
    let task = state.store.update_task(&a.task_id, input).await?;
    task_to_value(&state.store, task).await
}

/// `pm_claim_task` — 自领（pending → in_progress），仅限 assignee；依赖未满足 409。
async fn pm_claim_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
    }
    let a: Args = parse_args("pm_claim_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    ensure_assignee(&task, actor)?;

    let task = state.store.claim_task(&a.task_id, actor).await?;
    task_to_value(&state.store, task).await
}

/// `pm_submit_task` — 提交结果（in_progress → submitted），仅限 assignee。
async fn pm_submit_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
        text: String,
        #[serde(default)]
        attachment_ids: Vec<crate::types::AttachmentId>,
    }
    let a: Args = parse_args("pm_submit_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    ensure_assignee(&task, actor)?;

    let task = state
        .store
        .submit_task(&a.task_id, &a.text, a.attachment_ids, actor)
        .await?;
    task_to_value(&state.store, task).await
}

/// `pm_reparent_task` — 移动任务到新父下（new_parent=null 提升为根），DFS 防环。
///
/// 需要身份（§9.3 匿名仅只读）；设计 §9.2 未要求 assignee 匹配，但要求已认证。
async fn pm_reparent_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
        #[serde(default)]
        new_parent: Option<TaskId>,
    }
    let a: Args = parse_args("pm_reparent_task", args)?;

    // actor 仅用于满足"已认证"前提（见上方 doc）；任务存在性/防环由 store 校验
    let _actor = actor;

    let input = ReparentTask {
        new_parent: a.new_parent,
    };
    state.store.reparent_task(&a.task_id, input).await?;
    Ok(json!({ "ok": true, "task_id": a.task_id }))
}
