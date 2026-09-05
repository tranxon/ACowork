//! `projects` handlers（**P1 实现**）。内部路径不带 `/api` 前缀，
//! 公开路径为 `/api/pm/projects/*`（见 [`routes::pm_router`]）。

use axum::extract::{Path, State};
use axum::Json;

use crate::types::{CreateProject, Project, ProjectId, UpdateProject};

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
