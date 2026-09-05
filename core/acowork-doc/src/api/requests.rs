//! Update-request REST handlers — the human review queue (design §4/§5).
//!
//! Thin shells only: parse + validate input, delegate to
//! `RequestService`, render results as DTOs. No handler touches
//! `crate::store::*` / `crate::types` directly.
//!
//! Routes (mounted without `/api` prefix — Gateway strips it):
//! - `POST   /requests`                  submit (agent path)
//! - `GET    /requests?status=`          review queue
//! - `GET    /requests/{id}`             request detail / status check
//! - `POST   /requests/{id}/approve`     review-approve (merge + version+1)
//! - `POST   /requests/{id}/reject`      review-reject (with note)
//! - `GET    /docs/{doc_id}/requests`    per-document history (router-side)

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::api::dto::{
    ApproveDto, ReviewBody, SubmitRequestBody, UpdateRequestDto,
};
use crate::api::{ApiError, ApiResult, ApiState};
use crate::path::{validate_doc_id, validate_request_id};
use crate::service::request::{RequestService, SubmitRequestInput};
use crate::types::RequestStatus;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Optional filter: pending | approved | rejected | expired.
    #[serde(default)]
    pub status: Option<RequestStatus>,
}

/// `POST /requests` — agent submits a PR-style update proposal.
pub async fn submit_request(
    state: ApiState,
    Json(body): Json<SubmitRequestBody>,
) -> ApiResult<(StatusCode, Json<UpdateRequestDto>)> {
    validate_doc_id(&body.doc_id)?;
    let req = state
        .requests
        .submit(SubmitRequestInput {
            doc_id: body.doc_id,
            base_version: body.base_version,
            content: body.content,
            submitted_by: body.submitted_by,
        })
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(req.into())))
}

/// `GET /requests?status=pending` — human review queue.
pub async fn list_requests(
    state: ApiState,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<UpdateRequestDto>>> {
    let reqs = state
        .requests
        .list(q.status)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(reqs.into_iter().map(Into::into).collect()))
}

/// `GET /requests/{id}` — request detail / status check (doc_check_request).
pub async fn get_request(
    state: ApiState,
    Path(request_id): Path<String>,
) -> ApiResult<Json<UpdateRequestDto>> {
    validate_request_id(&request_id)?;
    let req = state
        .requests
        .get(&request_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(req.into()))
}

/// `POST /requests/{id}/approve` — human approves; merges into the doc.
pub async fn approve_request(
    state: ApiState,
    Path(request_id): Path<String>,
    Json(body): Json<ReviewBody>,
) -> ApiResult<Json<ApproveDto>> {
    validate_request_id(&request_id)?;
    let outcome = state
        .requests
        .approve(&request_id, &body.reviewed_by, body.note.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApproveDto {
        request: outcome.request,
        doc_version: outcome.doc_version,
    }))
}

/// `POST /requests/{id}/reject` — human rejects (keeps content + note).
pub async fn reject_request(
    state: ApiState,
    Path(request_id): Path<String>,
    Json(body): Json<ReviewBody>,
) -> ApiResult<Json<UpdateRequestDto>> {
    validate_request_id(&request_id)?;
    let req = state
        .requests
        .reject(&request_id, &body.reviewed_by, body.note.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(req.into()))
}

/// `GET /docs/{doc_id}/requests` — review history of one document.
pub async fn list_doc_requests(
    state: ApiState,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Vec<UpdateRequestDto>>> {
    validate_doc_id(&doc_id)?;
    let reqs = state
        .requests
        .list_for_doc(&doc_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(reqs.into_iter().map(Into::into).collect()))
}

