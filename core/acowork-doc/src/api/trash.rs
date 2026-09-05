//! Recycle-bin REST handlers (design §4: `GET /api/trash`,
//! `POST /api/trash/:id/restore`).

use axum::extract::Path;
use axum::http::StatusCode;
use axum::Json;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::service::trash::TrashService;
use crate::types::{DocMeta, TrashEntry};

/// `GET /trash` — list trash slots (newest first; lazy 30-day purge runs).
pub async fn list_trash(state: ApiState) -> ApiResult<Json<Vec<TrashEntry>>> {
    let entries = state.trash.list().await.map_err(ApiError::from)?;
    Ok(Json(entries))
}

/// `POST /trash/{trash_id}/restore` — restore into its original directory.
pub async fn restore_trash(
    state: ApiState,
    Path(trash_id): Path<String>,
) -> ApiResult<Json<DocMeta>> {
    let meta = state
        .trash
        .restore(&trash_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(meta))
}

/// `DELETE /trash/{trash_id}` — permanently delete a slot.
pub async fn purge_trash(
    state: ApiState,
    Path(trash_id): Path<String>,
) -> ApiResult<StatusCode> {
    state.trash.purge(&trash_id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}
