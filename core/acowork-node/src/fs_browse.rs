//! Node-local filesystem browsing (ADR-055 L7-1).
//!
//! The Gateway's `/api/fs/browse` gains a `?target={node_id}` query;
//! remote targets are reverse-proxied here, where browsing executes
//! against THIS machine's filesystem (directory listing only, same
//! restrictions as the Gateway-side implementation: hidden files skipped,
//! `..` traversal rejected, no file-content access).
//!
//! The response shape mirrors the Gateway's `FsBrowseResponse` exactly
//! (camelCase JSON) so the Desktop frontend is agnostic to whether the
//! listing came from the Gateway machine or a remote node.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::state::NodeHttpState;

/// Query parameters for node-local filesystem browsing.
#[derive(Debug, Deserialize, Default)]
pub struct FsBrowseQuery {
    /// Directory path to browse. Empty or "/" = root (returns home + common dirs).
    #[serde(default)]
    pub path: Option<String>,
}

/// A single entry in a directory listing.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<usize>,
}

/// Response for filesystem browsing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseResponse {
    pub path: String,
    pub entries: Vec<FsBrowseEntry>,
}

/// Common root directories to show when browsing "" or empty path.
fn root_entries() -> Vec<FsBrowseEntry> {
    let mut entries = Vec::new();

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| {
            std::env::var("HOMEDRIVE")
                .and_then(|d| std::env::var("HOMEPATH").map(|p| format!("{}{}", d, p)))
        })
        .ok();

    if let Some(home_str) = &home {
        let home_path = std::path::Path::new(home_str);
        let name = home_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Home".to_string());
        let children_count = std::fs::read_dir(home_path)
            .ok()
            .map(|rd| {
                rd.filter(|e| {
                    e.as_ref()
                        .map(|e| !e.file_name().to_string_lossy().starts_with('.'))
                        .unwrap_or(false)
                })
                .count()
            })
            .unwrap_or(0);
        entries.push(FsBrowseEntry {
            name,
            entry_type: "directory".to_string(),
            path: home_str.replace('\\', "/"),
            size: None,
            children_count: Some(children_count),
        });
    }

    #[cfg(unix)]
    {
        let tmp = "/tmp";
        if std::path::Path::new(tmp).is_dir() {
            entries.push(FsBrowseEntry {
                name: "tmp".to_string(),
                entry_type: "directory".to_string(),
                path: tmp.to_string(),
                size: None,
                children_count: None,
            });
        }
    }

    #[cfg(unix)]
    {
        entries.push(FsBrowseEntry {
            name: "/".to_string(),
            entry_type: "directory".to_string(),
            path: "/".to_string(),
            size: None,
            children_count: None,
        });
        for (label, path) in [("/var", "/var"), ("/tmp", "/tmp"), ("/opt", "/opt")] {
            if std::path::Path::new(path).is_dir() {
                entries.push(FsBrowseEntry {
                    name: label.to_string(),
                    entry_type: "directory".to_string(),
                    path: path.to_string(),
                    size: None,
                    children_count: None,
                });
            }
        }
    }

    #[cfg(windows)]
    {
        for letter in 'A'..='Z' {
            let drive = format!("{}:/", letter);
            if std::path::Path::new(&drive).is_dir() {
                entries.push(FsBrowseEntry {
                    name: format!("{}:", letter),
                    entry_type: "directory".to_string(),
                    path: drive,
                    size: None,
                    children_count: None,
                });
            }
        }
    }

    entries
}

/// Validate a browse path to prevent traversal attacks.
fn validate_path(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("Path traversal (..) not allowed".to_string());
    }
    Ok(())
}

type FsError = (StatusCode, Json<serde_json::Value>);

fn bad_request(msg: &str) -> FsError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg, "code": 400 })),
    )
}

fn internal(msg: &str) -> FsError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg, "code": 500 })),
    )
}

/// `GET /fs/browse` — browse THIS node's filesystem directories.
pub async fn browse_fs(
    State(_state): State<NodeHttpState>,
    Query(query): Query<FsBrowseQuery>,
) -> Result<impl IntoResponse, FsError> {
    let requested_path = query.path.as_deref().unwrap_or("").trim();

    if requested_path.is_empty() || requested_path == "/" {
        return Ok(Json(FsBrowseResponse {
            path: requested_path.to_string(),
            entries: root_entries(),
        }));
    }

    validate_path(requested_path).map_err(|e| bad_request(&e))?;

    let dir_path = std::path::Path::new(requested_path);

    if !dir_path.is_dir() {
        return Err(bad_request(&format!(
            "Path is not a directory: {}",
            requested_path
        )));
    }

    let read_dir = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
            return Err(internal(&format!("Failed to read directory: {}", e)));
        }
    };

    let mut dirs: Vec<FsBrowseEntry> = Vec::new();
    let mut files: Vec<FsBrowseEntry> = Vec::new();

    let base_str = dir_path.to_string_lossy();
    let normalized_base = base_str
        .strip_prefix(r"\\?\")
        .unwrap_or(base_str.as_ref());
    let normalized_base = normalized_base.replace('\\', "/");

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') {
            continue;
        }

        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());

        let abs_path = entry.path().to_string_lossy().replace('\\', "/");

        if is_dir {
            let children_count = std::fs::read_dir(entry.path())
                .ok()
                .map(|rd| {
                    rd.filter(|e| {
                        e.as_ref()
                            .map(|e| !e.file_name().to_string_lossy().starts_with('.'))
                            .unwrap_or(false)
                    })
                    .count()
                })
                .unwrap_or(0);

            dirs.push(FsBrowseEntry {
                name,
                entry_type: "directory".to_string(),
                path: abs_path,
                size: None,
                children_count: Some(children_count),
            });
        } else {
            files.push(FsBrowseEntry {
                name,
                entry_type: "file".to_string(),
                path: abs_path,
                size: metadata.as_ref().map(|m| m.len()),
                children_count: None,
            });
        }
    }

    dirs.sort_by_key(|a| a.name.to_lowercase());
    files.sort_by_key(|a| a.name.to_lowercase());

    let mut entries = dirs;
    entries.append(&mut files);

    Ok(Json(FsBrowseResponse {
        path: normalized_base,
        entries,
    }))
}

/// Node-local filesystem browsing routes (ADR-055 L7-1).
///
/// `state` is threaded through for symmetry with the reverse-proxy router
/// (the Phase 5a auth boundary); browsing itself is stateless.
pub fn router(state: NodeHttpState) -> Router {
    Router::new()
        .route("/fs/browse", get(browse_fs))
        .with_state(state)
}
