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
use std::path::Path;

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

/// Count non-hidden direct children of a directory; 0 on any error.
///
/// ponytail: reads the directory at request time, so slow / network drives
/// block the handler. Acceptable here because the sibling listing path does
/// the same enumeration per entry. If this becomes a hotspot, switch to a
/// cached dirent handle or an async stream.
fn count_visible_children(path: &Path) -> usize {
    std::fs::read_dir(path)
        .ok()
        .map(|rd| {
            rd.filter(|e| {
                e.as_ref()
                    .map(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
            })
            .count()
        })
        .unwrap_or(0)
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
        entries.push(FsBrowseEntry {
            name,
            entry_type: "directory".to_string(),
            path: home_str.replace('\\', "/"),
            size: None,
            children_count: Some(count_visible_children(home_path)),
        });
    }

    #[cfg(unix)]
    {
        let tmp = "/tmp";
        let tmp_path = Path::new(tmp);
        if tmp_path.is_dir() {
            entries.push(FsBrowseEntry {
                name: "tmp".to_string(),
                entry_type: "directory".to_string(),
                path: tmp.to_string(),
                size: None,
                children_count: Some(count_visible_children(tmp_path)),
            });
        }
    }

    // On Unix: add root `/` and common sibling paths (home and /tmp
    // are above).
    //
    // `/` lists itself in `root_entries()`, so when the user expands
    // its chevron the chevron fetch hits this same listing, which
    // contains `/`, which then expands again to the same listing — a
    // path self-cycle. The Desktop `RemoteFolderPicker`'s tree-flatten
    // walker detects this and skips any child whose `path` matches
    // the parent's, so the cycle is contained on the client side.
    //
    // `/tmp` is already added by the "Temp directory" block above;
    // listing it again here would emit two entries with the same
    // `path`, which the Desktop frontend rejects as a duplicate React
    // key.
    #[cfg(unix)]
    {
        entries.push(FsBrowseEntry {
            name: "/".to_string(),
            entry_type: "directory".to_string(),
            path: "/".to_string(),
            size: None,
            children_count: Some(count_visible_children(Path::new("/"))),
        });
        for (label, path) in [("/var", "/var"), ("/opt", "/opt")] {
            let p = Path::new(path);
            if p.is_dir() {
                entries.push(FsBrowseEntry {
                    name: label.to_string(),
                    entry_type: "directory".to_string(),
                    path: path.to_string(),
                    size: None,
                    children_count: Some(count_visible_children(p)),
                });
            }
        }
    }

    #[cfg(windows)]
    {
        for letter in 'A'..='Z' {
            let drive = format!("{}:/", letter);
            let drive_path = std::path::Path::new(&drive);
            if drive_path.is_dir() {
                let children_count = count_visible_children(drive_path);
                entries.push(FsBrowseEntry {
                    name: format!("{}:", letter),
                    entry_type: "directory".to_string(),
                    path: drive,
                    size: None,
                    children_count: Some(children_count),
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
            dirs.push(FsBrowseEntry {
                name,
                entry_type: "directory".to_string(),
                path: abs_path,
                size: None,
                children_count: Some(count_visible_children(&entry.path())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn fresh_dir(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "acowork-node-fsbrowse-{}-{}-{}-{}",
            label,
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn count_visible_children_excludes_hidden() {
        let dir = fresh_dir("hidden");
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::write(dir.join("b.txt"), "x").unwrap();
        fs::write(dir.join(".hidden"), "x").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();

        assert_eq!(count_visible_children(&dir), 3, "expected 3 visible entries");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn count_visible_children_missing_dir_is_zero() {
        let p = fresh_dir("missing");
        let _ = fs::remove_dir_all(&p);
        assert_eq!(count_visible_children(&p), 0);
    }

    #[test]
    fn root_entries_paths_are_unique() {
        // Regression for the macOS crash: `/tmp` was pushed twice (once
        // by the temp-dir block, once by the /var /tmp /opt loop), so
        // the Desktop frontend saw two entries with the same `path` and
        // crashed on a duplicate React key. Every path must be unique.
        let entries = root_entries();
        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            assert!(
                seen.insert(entry.path.as_str()),
                "duplicate path in root_entries: {}",
                entry.path
            );
        }
    }
}
