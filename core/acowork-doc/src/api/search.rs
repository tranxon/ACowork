//! Search REST handler (design §4: `GET /api/search?keyword=`).

use axum::extract::Query;
use axum::Json;
use serde::Deserialize;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::service::search::SearchService;
use crate::types::SearchHit;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub keyword: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

/// `GET /search?keyword=...&limit=...` — cross-directory keyword search.
pub async fn search(
    state: ApiState,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<SearchHit>>> {
    let hits = state
        .search
        .search(&q.keyword, q.limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(hits))
}
