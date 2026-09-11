//! Avatar preference routes — ADR-009 §5.
//!
//! The Runtime is the **owner** and only writer of the instance's
//! user-preference overrides ([`acowork_core::agent_overrides`]): the
//! avatar the user picked and its display name are agent-private data, so
//! the Runtime — not the Gateway — reads and writes them. They live in a
//! sibling of the package dir (`{agent_id}/{instance_id}.overrides.json`)
//! so an upgrade, which replaces the instance dir, cannot wipe them.
//!
//! | Method | Path                          | Handler               |
//! |--------|-------------------------------|-----------------------|
//! | GET    | `/agents/{id}/avatar-config`  | [`get_avatar_config`] |
//! | PUT    | `/agents/{id}/avatar-config`  | [`put_avatar_config`] |
//!
//! Serving the image **bytes** is NOT here: the package (`assets/*`,
//! `manifest.avatar`) belongs to the node, which is the package manager
//! and is up whether or not this process is (review §7.1 / V-P). A second
//! copy of those reads here would be a second code path to keep in sync —
//! the mistake this module's history is made of.

use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use acowork_core::agent_overrides::{AVATAR_EXTENSIONS, has_avatar_extension};
use serde::{Deserialize, Serialize};

use crate::http::server::HttpState;

// ── Wire types ────────────────────────────────────────────────────────

/// `GET/PUT /agents/{id}/avatar-config` response — the **effective**
/// avatar/display name (what the UI renders) plus the **raw** overrides
/// (so the Gateway can mirror them without re-deriving the merge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvatarConfigResponse {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    /// Effective display name: override > `manifest.display_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `"overrides"` | `"manifest"` | `"fallback"`.
    pub source: String,
    /// The raw `.overrides.json` content this response was resolved from.
    /// Opaque to everyone but the Runtime — the Gateway stores it verbatim
    /// (ADR-009 §5) and never interprets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<acowork_core::AgentOverrides>,
}

/// `PUT /agents/{id}/avatar-config` body.
///
/// Semantics per field: absent/`null` = leave unchanged; `""` = clear the
/// override (fall back to the manifest); any other value = set it.
/// Setting `avatar` clears `builtin_avatar` and vice versa.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateAvatarConfigRequest {
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub builtin_avatar: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

// ── Router ────────────────────────────────────────────────────────────

/// The avatar routes (see module docs).
pub(crate) fn avatar_routes() -> Router<HttpState> {
    Router::new().route(
        "/agents/{id}/avatar-config",
        get(get_avatar_config).put(put_avatar_config),
    )
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /agents/{id}/avatar-config` — effective avatar + display name,
/// resolved from the overrides file with the manifest as fallback.
async fn get_avatar_config(State(state): State<HttpState>, Path(id): Path<String>) -> Response {
    if !state.instance_matches(&id) {
        return instance_mismatch(&state, &id);
    }
    match effective_config(&state) {
        Ok(cfg) => Json(cfg).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e),
    }
}

/// `PUT /agents/{id}/avatar-config` — persist the user's pick.
///
/// ADR-009 §5: this route is the **only** writer of
/// `{instance_id}.overrides.json`. The Gateway reverse-proxies it and
/// mirrors the raw `overrides` it returns into its list view; the Gateway
/// never writes the file itself.
async fn put_avatar_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAvatarConfigRequest>,
) -> Response {
    if !state.instance_matches(&id) {
        return instance_mismatch(&state, &id);
    }

    let mut ov = load_overrides(&state);

    // `avatar` and `builtin_avatar` are mutually exclusive — selecting one
    // clears the other, otherwise a stale lower-priority value would
    // resurrect as soon as the newer one is cleared.
    match normalize(&req.avatar) {
        Field::Unchanged => {}
        Field::Clear => ov.avatar = None,
        Field::Set(value) => {
            if !has_avatar_extension(&value) {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!(
                        "Invalid avatar path '{value}': only {} are allowed",
                        AVATAR_EXTENSIONS.join(", ")
                    ),
                );
            }
            ov.avatar = Some(value);
            ov.builtin_avatar = None;
        }
    }
    match normalize(&req.builtin_avatar) {
        Field::Unchanged => {}
        Field::Clear => ov.builtin_avatar = None,
        Field::Set(value) => {
            if !is_plausible_builtin_avatar_id(&value) {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("Invalid builtin_avatar '{value}': expected 'icon-NN' or 1-99"),
                );
            }
            ov.builtin_avatar = Some(value);
            ov.avatar = None;
        }
    }
    match normalize(&req.display_name) {
        Field::Unchanged => {}
        Field::Clear => ov.display_name = None,
        Field::Set(value) => {
            if value.chars().count() > MAX_DISPLAY_NAME_CHARS {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!(
                        "display_name too long ({} > {MAX_DISPLAY_NAME_CHARS} chars)",
                        value.chars().count()
                    ),
                );
            }
            ov.display_name = Some(value);
        }
    }

    if let Err(e) = save_overrides(&state, &ov) {
        tracing::warn!(error = %e, "avatar-config: failed to persist overrides");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("Failed to persist overrides: {e}"),
        );
    }

    match effective_config(&state) {
        Ok(cfg) => Json(cfg).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e),
    }
}

// ── User-preference overrides (ADR-009 §5) ────────────────────────────

/// Reject absurd display names at the trust boundary. Generous enough for
/// any real name; the point is to stop an unbounded string from reaching
/// the file and every list response.
const MAX_DISPLAY_NAME_CHARS: usize = 100;

/// Tri-state for one incoming field: absent, explicitly cleared (`""`), or set.
enum Field {
    Unchanged,
    Clear,
    Set(String),
}

fn normalize(value: &Option<String>) -> Field {
    match value {
        None => Field::Unchanged,
        Some(v) if v.trim().is_empty() => Field::Clear,
        Some(v) => Field::Set(v.trim().to_string()),
    }
}

/// Loose syntactic check for builtin_avatar values: `icon-NN` (1-99) or a
/// bare number 1-99. The Desktop's bundled icon set remains the source of
/// truth for which ids exist; this only rejects obvious typos.
fn is_plausible_builtin_avatar_id(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    match lower.strip_prefix("icon-") {
        Some(num) => num.parse::<u32>().is_ok_and(|n| (1..=99).contains(&n)),
        None => lower.parse::<u32>().is_ok_and(|n| (1..=99).contains(&n)),
    }
}

/// Absolute path of this instance's overrides file.
fn overrides_path_for(state: &HttpState) -> Result<PathBuf, String> {
    acowork_core::overrides_path(&state.package_dir, &state.instance_id).ok_or_else(|| {
        format!(
            "package dir {} has no parent — cannot locate the overrides file",
            state.package_dir.display()
        )
    })
}

/// Load the overrides. A missing / unreadable / malformed file means "no
/// overrides": a corrupt preference degrades to the manifest default,
/// never to an unusable agent.
fn load_overrides(state: &HttpState) -> acowork_core::AgentOverrides {
    let path = match overrides_path_for(state) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!(error = %e, "overrides: cannot locate file");
            return acowork_core::AgentOverrides::default();
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => acowork_core::AgentOverrides::from_json(&raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            acowork_core::AgentOverrides::default()
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "overrides: read failed");
            acowork_core::AgentOverrides::default()
        }
    }
}

/// Persist the overrides atomically (tmp + rename). An empty override set
/// removes the file instead of writing `{}` — "no overrides" has exactly
/// one representation on disk.
fn save_overrides(state: &HttpState, ov: &acowork_core::AgentOverrides) -> Result<(), String> {
    let path = overrides_path_for(state)?;
    if ov.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Failed to remove {}: {e}", path.display())),
        };
    }
    let json = serde_json::to_string_pretty(ov).map_err(|e| format!("serialize overrides: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to commit {}: {e}", path.display())
    })
}

/// The effective avatar / display name for this instance.
fn effective_config(state: &HttpState) -> Result<AvatarConfigResponse, String> {
    let manifest = crate::package::loader::load_manifest(&state.package_dir)
        .map_err(|e| format!("package manifest unavailable: {e}"))?;
    let overrides = load_overrides(state);
    let (avatar, builtin_avatar, source) = resolve_effective_avatar(&overrides, &manifest);
    let display_name = overrides
        .display_name
        .clone()
        .or_else(|| manifest.display_name.clone());
    Ok(AvatarConfigResponse {
        agent_id: state.instance_id.clone(),
        avatar,
        builtin_avatar,
        display_name,
        source: source.to_string(),
        overrides: (!overrides.is_empty()).then_some(overrides),
    })
}

/// User pick first, packaged default second, deterministic client-side
/// fallback last. Returns `(avatar, builtin_avatar, source)`.
fn resolve_effective_avatar(
    overrides: &acowork_core::AgentOverrides,
    manifest: &acowork_core::AgentManifest,
) -> (Option<String>, Option<String>, &'static str) {
    if overrides.avatar.is_some() || overrides.builtin_avatar.is_some() {
        return (
            overrides.avatar.clone(),
            overrides.builtin_avatar.clone(),
            "overrides",
        );
    }
    if manifest.avatar.is_some() || manifest.builtin_avatar.is_some() {
        return (
            manifest.avatar.clone(),
            manifest.builtin_avatar.clone(),
            "manifest",
        );
    }
    (None, None, "fallback")
}

// ── Helpers ───────────────────────────────────────────────────────────

fn instance_mismatch(state: &HttpState, id: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "instance_id_mismatch",
        format!(
            "path '{id}' is not this runtime's instance id '{}'",
            state.instance_id
        ),
    )
}

fn error(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_whitelist_is_case_insensitive() {
        assert!(has_avatar_extension("assets/avatar.PNG"));
        assert!(has_avatar_extension("avatar-01.webp"));
        assert!(!has_avatar_extension("assets/evil.sh"));
        assert!(!has_avatar_extension("assets/noext"));
    }

}
