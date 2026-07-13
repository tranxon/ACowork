//! Unified `/api/global/{kind}` HTTP endpoints (ADR-033 Phase 1).
//!
//! Provides a unified REST namespace for listing global resources:
//!
//! ```text
//! GET    /api/global                    — list all available resource kinds
//! GET    /api/global/{kind}             — list all items of a kind
//! GET    /api/global/{kind}/{id}        — redirect to domain-specific endpoint
//! ```
//!
//! `kind ∈ { providers, mcps, lsps, searches, embedding_models }`
//!
//! In Phase 1, this is a **read-only facade** that returns unified JSON
//! views of the global resources. Mutations (POST/PUT/DELETE) should use
//! the existing domain-specific endpoints (`/api/providers`, `/api/mcps`,
//! etc.) which remain unchanged. The unified namespace mirrors the MQTT
//! topic tree (`acowork/global/{kind}`) for API consistency.
//!
//! See `docs/zh/protocols/mqtt.md` §3.1.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;

use crate::http::routes::{ApiError, AppState};

/// Supported global resource kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalKind {
    Providers,
    Mcps,
    Lsps,
    Searches,
    EmbeddingModels,
}

impl GlobalKind {
    /// Parse a kind from a URL path segment.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "providers" => Some(Self::Providers),
            "mcps" => Some(Self::Mcps),
            "lsps" => Some(Self::Lsps),
            "searches" => Some(Self::Searches),
            "embedding_models" => Some(Self::EmbeddingModels),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Providers => "providers",
            Self::Mcps => "mcps",
            Self::Lsps => "lsps",
            Self::Searches => "searches",
            Self::EmbeddingModels => "embedding_models",
        }
    }

    /// The existing domain-specific endpoint for this kind.
    pub fn domain_endpoint(&self) -> &'static str {
        match self {
            Self::Providers => "/api/providers",
            Self::Mcps => "/api/mcps",
            Self::Lsps => "/api/lsp/endpoint",
            Self::Searches => "/api/search/keys",
            Self::EmbeddingModels => "/api/embedding-models",
        }
    }
}

/// Response for `GET /api/global` — lists all available kinds.
#[derive(Debug, Serialize)]
pub struct GlobalKindsResponse {
    pub kinds: Vec<GlobalKindInfo>,
}

/// Info about a single resource kind.
#[derive(Debug, Serialize)]
pub struct GlobalKindInfo {
    pub kind: &'static str,
    /// The existing domain-specific endpoint for CRUD operations.
    pub endpoint: &'static str,
    /// The corresponding MQTT topic for available state.
    pub mqtt_topic: &'static str,
}

/// Build the unified `/api/global` router.
pub fn global_routes() -> Router<AppState> {
    Router::new()
        .route("/api/global", get(list_kinds))
        .route("/api/global/{kind}", get(list_kind))
}

/// `GET /api/global` — list all available resource kinds.
async fn list_kinds() -> Json<GlobalKindsResponse> {
    let kinds = vec![
        GlobalKindInfo {
            kind: "providers",
            endpoint: "/api/providers",
            mqtt_topic: "acowork/global/providers",
        },
        GlobalKindInfo {
            kind: "mcps",
            endpoint: "/api/mcps",
            mqtt_topic: "acowork/global/mcps",
        },
        GlobalKindInfo {
            kind: "lsps",
            endpoint: "/api/lsp/endpoint",
            mqtt_topic: "acowork/global/lsps",
        },
        GlobalKindInfo {
            kind: "searches",
            endpoint: "/api/search/keys",
            mqtt_topic: "acowork/global/searches",
        },
        GlobalKindInfo {
            kind: "embedding_models",
            endpoint: "/api/embedding-models",
            mqtt_topic: "acowork/global/embedding_models",
        },
    ];
    Json(GlobalKindsResponse { kinds })
}

/// `GET /api/global/{kind}` — list all items of a kind.
///
/// Returns a unified JSON view. For mutations, use the domain-specific
/// endpoint listed in `GET /api/global`.
async fn list_kind(
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiError>)> {
    let kind = GlobalKind::from_str(&kind).ok_or_else(|| {
        ApiError::bad_request(&format!(
            "Unknown global resource kind: '{}'. Valid kinds: providers, mcps, lsps, searches, embedding_models",
            kind
        ))
    })?;

    match kind {
        GlobalKind::Providers => {
            let providers = crate::http::provider_api::list_providers(State(state)).await;
            Ok(providers.into_response())
        }
        GlobalKind::Mcps => {
            let catalog = crate::http::mcp_catalog_api::list_catalog(State(state)).await;
            Ok(catalog.into_response())
        }
        GlobalKind::EmbeddingModels => {
            let models = crate::http::embedding_api::list_embedding_models(State(state)).await;
            Ok(models.into_response())
        }
        GlobalKind::Searches => {
            let searches = crate::http::provider_api::list_search_keys(State(state)).await;
            Ok(searches.into_response())
        }
        GlobalKind::Lsps => {
            // LSP relay is a single sidecar — return its status.
            let gw = state.gateway_state.read().await;
            let lsp = gw.lsp_relay_process.as_ref();
            Ok(Json(serde_json::json!({
                "kind": "lsps",
                "endpoint": lsp.filter(|l| l.ready).map(|l| format!("http://127.0.0.1:{}", l.port)),
                "ready": lsp.map(|l| l.ready).unwrap_or(false),
                "mqtt_topic": "acowork/global/lsps",
            })).into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_kind_from_str() {
        assert_eq!(GlobalKind::from_str("providers"), Some(GlobalKind::Providers));
        assert_eq!(GlobalKind::from_str("mcps"), Some(GlobalKind::Mcps));
        assert_eq!(GlobalKind::from_str("lsps"), Some(GlobalKind::Lsps));
        assert_eq!(GlobalKind::from_str("searches"), Some(GlobalKind::Searches));
        assert_eq!(
            GlobalKind::from_str("embedding_models"),
            Some(GlobalKind::EmbeddingModels)
        );
        assert_eq!(GlobalKind::from_str("unknown"), None);
    }

    #[test]
    fn test_global_kind_as_str() {
        assert_eq!(GlobalKind::Providers.as_str(), "providers");
        assert_eq!(GlobalKind::EmbeddingModels.as_str(), "embedding_models");
    }

    #[test]
    fn test_global_kind_domain_endpoint() {
        assert_eq!(GlobalKind::Providers.domain_endpoint(), "/api/providers");
        assert_eq!(GlobalKind::Mcps.domain_endpoint(), "/api/mcps");
        assert_eq!(GlobalKind::Lsps.domain_endpoint(), "/api/lsp/endpoint");
        assert_eq!(GlobalKind::Searches.domain_endpoint(), "/api/search/keys");
        assert_eq!(
            GlobalKind::EmbeddingModels.domain_endpoint(),
            "/api/embedding-models"
        );
    }
}
