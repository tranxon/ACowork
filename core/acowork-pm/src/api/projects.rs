//! `projects` handlers（**P1 实现**）。内部路径不带 `/api` 前缀，
//! 公开路径为 `/api/pm/projects/*`（见 [`routes::pm_router`]）。

use axum::extract::{Path, State};
use axum::Json;

use crate::types::{AddProjectMember, CreateProject, Project, ProjectId, UpdateProject};

use super::ApiState;
use crate::store::tree::PmStore;

// ────────────────────────────────────────────────────────────────────────────
// GET /projects
// ────────────────────────────────────────────────────────────────────────────

/// 列出所有项目。
///
/// 支持未来扩展：`?status=active&include_archived=true`。
#[tracing::instrument(skip(state))]
pub async fn list(State(state): State<ApiState>) -> Result<Json<Vec<Project>>, crate::error::PmError> {
    let projects = state.store.list_projects().await?;
    Ok(Json(projects))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects
// ────────────────────────────────────────────────────────────────────────────

/// 创建项目。
///
/// `created_by` 来自 HTTP header `X-Actor`（Gateway 注入当前用户/Agent ID）。
#[tracing::instrument(skip(state, input))]
pub async fn create(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<CreateProject>,
) -> Result<Json<Project>, crate::error::PmError> {
    let created_by = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    let project = state.store.create_project(input, created_by).await?;
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/:pid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn get(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let project = state
        .store
        .get_project(&pid)
        .await?
        .ok_or(crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// PATCH /projects/:pid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn update(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    Json(input): Json<UpdateProject>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let project = state.store.update_project(&pid, input).await?;
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /projects/:pid
// ────────────────────────────────────────────────────────────────────────────

/// 删除项目。
///
/// Query: `?cascade=true` 强制级联删除所有任务（默认 false，返回 409 若仍有任务）。
#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    query: axum::extract::Query<DeleteProjectQuery>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    state.store.delete_project(&pid, query.cascade).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteProjectQuery {
    #[serde(default)]
    pub cascade: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects/:pid/members
// ────────────────────────────────────────────────────────────────────────────

/// 添加项目成员（Agent 实例）。
///
/// 按 `AgentDirectory::agent_exists`（设计 §9.1 / ADR-073，instance_id 维度）
/// 校验 Agent 存在；重复添加 → 409 `member_already_exists`。
#[tracing::instrument(skip(state, input))]
pub async fn add_member(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    Json(input): Json<AddProjectMember>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // 校验 Agent 存在（宽松目录跳过）。成员必须是真实存在的 Agent 实例。
    if !state.agent_dir.agent_exists(&input.instance_id).await {
        return Err(crate::error::PmError::BadRequest(format!(
            "agent instance not found in agent directory: {}",
            input.instance_id
        )));
    }
    let project = state
        .store
        .add_project_member(&pid, &input.instance_id)
        .await?;
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /projects/:pid/members/:instance_id
// ────────────────────────────────────────────────────────────────────────────

/// 移除项目成员。
///
/// 成员名下仍有未完成任务 → 409 `member_has_open_tasks`（显式失败，
/// 任务要先转走或完成）。成员不存在 → 404 `member_not_found`。
#[tracing::instrument(skip(state))]
pub async fn remove_member(
    State(state): State<ApiState>,
    Path((pid, instance_id)): Path<(String, String)>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let project = state
        .store
        .remove_project_member(&pid, &instance_id)
        .await?;
    Ok(Json(project))
}
