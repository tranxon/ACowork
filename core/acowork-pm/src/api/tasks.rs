//! `tasks` handlers（**P1 实现**）。内部路径不带 `/api` 前缀，
//! 公开路径为 `/api/pm/tasks/*`（见 [`routes::pm_router`]）。

use axum::extract::{Path, State};
use axum::Json;

use crate::types::{
    AttachmentId, CreateTask, ProjectId, ReparentTask, ReviewStatus, Task, TaskFilter, TaskId,
    UpdateTask,
};

use super::ApiState;
use crate::store::tree::PmStore;

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/:pid/tasks
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn list(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    query: axum::extract::Query<TaskFilter>,
) -> Result<Json<Vec<crate::types::TaskResponse>>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let mut filter = query.0;
    filter.project_id = Some(pid);
    let tasks = state.store.find_tasks(&filter).await?;
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        let tid = task.id.clone();
        let depth = state.store.index_entry(&tid).map(|e| e.depth).unwrap_or(0);
        let parent_id = state.store.parent_of(&tid);
        let blocked_by = state.store.compute_blocked_by(&tid).await?;
        out.push(crate::types::TaskResponse {
            task,
            is_blocked: !blocked_by.is_empty(),
            blocked_by,
            depth,
            parent_id,
        });
    }
    Ok(Json(out))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects/:pid/tasks
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn create(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<CreateTask>,
) -> Result<Json<Task>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let created_by = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    let task = state.store.create_task(&pid, input, created_by).await?;
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

/// 获取任务详情（响应中自动补 `is_blocked` / `blocked_by` / `depth`）。
#[tracing::instrument(skip(state))]
pub async fn get(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<crate::types::TaskResponse>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let task = state
        .store
        .get_task(&tid)
        .await?
        .ok_or(crate::error::PmError::TaskNotFound(tid.to_string()))?;
    let depth = state.store.index_entry(&tid).map(|e| e.depth).unwrap_or(0);
    let parent_id = state.store.parent_of(&tid);
    let blocked_by = state.store.compute_blocked_by(&tid).await?;

    Ok(Json(crate::types::TaskResponse {
        task,
        is_blocked: !blocked_by.is_empty(),
        blocked_by,
        depth,
        parent_id,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// PATCH /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn update(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    Json(input): Json<UpdateTask>,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let task = state.store.update_task(&tid, input).await?;
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

/// 删除任务。
///
/// Query: `?cascade=true` 强制级联删除子树（默认 false，子任务被提升为顶层）。
#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    query: axum::extract::Query<DeleteTaskQuery>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    state.store.delete_task(&tid, query.cascade).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteTaskQuery {
    #[serde(default)]
    pub cascade: bool,
    /// `?promote_children=true` —— 子任务提升为父项目的顶层（默认 false，cascade 时忽略）。
    #[serde(default)]
    pub promote_children: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// PATCH /tasks/:tid/parent
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn reparent(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    Json(input): Json<ReparentTask>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    state.store.reparent_task(&tid, input).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/claim
// ────────────────────────────────────────────────────────────────────────────

/// Agent 认领任务（pending → in_progress）。
///
/// Header `X-Actor` 携带 agent_id。返回 409 若被依赖阻塞。
#[tracing::instrument(skip(state))]
pub async fn claim(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let agent_id = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal("missing X-Actor header".to_string()))?;
    let task = state.store.claim_task(&tid, agent_id).await?;
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/submit
// ────────────────────────────────────────────────────────────────────────────

/// Agent 提交结果（in_progress → submitted）。
#[tracing::instrument(skip(state, input))]
pub async fn submit(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<SubmitTaskRequest>,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let agent_id = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal("missing X-Actor header".to_string()))?;
    let task = state
        .store
        .submit_task(&tid, &input.text, input.attachment_ids, agent_id)
        .await?;
    Ok(Json(task))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitTaskRequest {
    pub text: String,
    #[serde(default)]
    pub attachment_ids: Vec<AttachmentId>,
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/review
// ────────────────────────────────────────────────────────────────────────────

/// 人类审核（仅 `type=checkpoint/bug` 等 requires_review=true 的任务）。
#[tracing::instrument(skip(state, input))]
pub async fn review(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<ReviewTaskRequest>,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let reviewer = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal("missing X-Actor header".to_string()))?;
    let task = state.store.review_task(&tid, input.approved, reviewer).await?;
    Ok(Json(task))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTaskRequest {
    pub approved: bool,
    #[serde(default)]
    pub _comment: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// GET /tasks/:tid/children
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn list_children(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<Vec<Task>>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let children = state.store.list_children(&tid).await?;
    Ok(Json(children))
}

/// 待导出辅助：review_status 类型别名（避免 handler 引用未使用项）。
#[allow(dead_code)]
pub type _ReviewStatus = ReviewStatus;
