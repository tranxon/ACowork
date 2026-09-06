//! acowork-doc health-check endpoint (supervisor liveness contract).
//!
//! Reuses the [`acowork_core::health::HealthResponse`] contract: the Gateway
//! supervisor probes `GET /health` to decide whether the doc process is
//! ready / alive (same as PM / embed / LSP relay).

use std::path::PathBuf;

use axum::Json;
use serde_json::json;

/// Build the doc service `/health` route.
///
/// `data_dir` is captured into the `details` for diagnostics so the
/// supervisor log can confirm the data-directory resolution.
pub fn health_route(data_dir: PathBuf) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move || async move {
            Json(acowork_core::health::HealthResponse {
                status: "ok".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                process: "acowork-doc".to_string(),
                details: Some(json!({
                    "data_dir": data_dir.display().to_string(),
                })),
            })
        }),
    )
}
