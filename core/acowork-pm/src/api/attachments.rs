//! `attachments` handlers + multipart upload（**P1 实现**）。
//! 内部路径不带 `/api` 前缀，公开路径为 `/api/pm/attachments/*`
//! （见 [`routes::pm_router`]）。

use axum::extract::{Path, State};
use axum::Json;

use crate::types::{AttachmentId, AttachmentMeta};

use super::ApiState;
use crate::store::tree::PmStore;

// ────────────────────────────────────────────────────────────────────────────
// GET /tasks/:tid/attachments
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn list(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<Vec<AttachmentMeta>>, crate::error::PmError> {
    let tid = tid.parse::<crate::types::TaskId>()?;
    let metas = state.store.list_attachments(&tid).await?;
    Ok(Json(metas))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/attachments
// ────────────────────────────────────────────────────────────────────────────

/// 上传附件（multipart/form-data）。
///
/// Form 字段：
/// - `file` —— 必需，文件二进制
/// - `kind` —— 可选，`image` | `file`（自动从 MIME 推断）
#[tracing::instrument(skip(state, _multipart))]
pub async fn upload(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    _multipart: axum::extract::Multipart,
) -> Result<Json<AttachmentMeta>, crate::error::PmError> {
    // TODO(P1): 实现 multipart 解析 + 大小校验 + MIME 白名单 +
    // sha256 计算 + 图片缩略图生成 + 物理目录创建 + 元数据注册
    let _ = (state, headers, tid, _multipart);
    Err(crate::error::PmError::Internal(
        "upload attachment not yet implemented (P1)".to_string(),
    ))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /attachments/:aid
// ────────────────────────────────────────────────────────────────────────────

/// 下载附件（`?download=1` 强制 attachment 头）。
#[tracing::instrument(skip(state))]
pub async fn download(
    State(state): State<ApiState>,
    Path(aid): Path<String>,
    query: axum::extract::Query<DownloadQuery>,
) -> Result<axum::response::Response, crate::error::PmError> {
    // TODO(P1): 实现下载（Range 支持？Phase 1 直接 stream 整个文件）
    let _ = (state, aid, query);
    Err(crate::error::PmError::Internal(
        "download attachment not yet implemented (P1)".to_string(),
    ))
}

#[derive(Debug, serde::Deserialize)]
pub struct DownloadQuery {
    #[serde(default)]
    pub download: bool,
    /// `?thumb=1` 返回缩略图（仅图片）。
    #[serde(default)]
    pub thumb: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /attachments/:aid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(aid): Path<String>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let aid = aid.parse::<AttachmentId>()?;
    state.store.delete_attachment(&aid).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
