//! Remote filesystem browsing API
//!
//! Provides a directory listing endpoint for remote Desktop ↔ Gateway scenarios.
//! When the Desktop App connects to a remote Gateway, it cannot use Tauri's
//! native file dialog to browse the remote server's filesystem. This API
//! enables the frontend to browse the server's directory tree remotely.
//!
//! Security considerations:
//! - Only directory listing is allowed (no file content access)
//! - Hidden files/dirs (starting with '.') are skipped
//! - Path traversal (..) is rejected
//! - Absolute paths on Windows that start with a drive letter are accepted
//! - Root-level browsing returns common starting points (home, root, common paths)

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Query parameters for filesystem browsing
#[derive(Debug, Deserialize, Default)]
pub struct FsBrowseQuery {
    /// Directory path to browse. Empty or "/" = root (returns home + common dirs).
    #[serde(default)]
    pub path: Option<String>,
}

/// A single entry in a directory listing
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseEntry {
    /// File or directory name
    pub name: String,
    /// "file" or "directory"
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Absolute path (for navigation)
    pub path: String,
    /// File size in bytes (None for directories)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Number of direct children (only for directories, for expansion indicator)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<usize>,
}

/// Response for filesystem browsing
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseResponse {
    /// The path that was browsed (echoed back for UI breadcrumb)
    pub path: String,
    /// Directory entries (directories first, then files, both alphabetical)
    pub entries: Vec<FsBrowseEntry>,
}

/// Common root directories to show when browsing "" or empty path
fn root_entries() -> Vec<FsBrowseEntry> {
    let mut entries = Vec::new();

    // User home directory (cross-platform)
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| std::env::var("HOMEDRIVE").and_then(|d| std::env::var("HOMEPATH").map(|p| format!("{}{}", d, p))))
        .ok();

    if let Some(home_str) = &home {
        let home_path = std::path::Path::new(home_str);
        let name = home_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Home".to_string());
        let children_count = std::fs::read_dir(home_path)
            .ok()
            .map(|rd| rd.filter(|e| e.as_ref().map(|e| !e.file_name().to_string_lossy().starts_with('.')).unwrap_or(false)).count())
            .unwrap_or(0);
        entries.push(FsBrowseEntry {
            name,
            entry_type: "directory".to_string(),
            path: home_str.replace('\\', "/"),
            size: None,
            children_count: Some(children_count),
        });
    }

    // Temp directory
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

    // On Unix: add root "/" and common paths
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

    // On Windows: add drive roots
    #[cfg(windows)]
    {
        // List available drive letters
        for letter in 'A'..'Z' {
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

/// Validate a browse path to prevent traversal attacks
fn validate_path(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);

    // Reject path traversal
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("Path traversal (..) not allowed".to_string());
    }

    Ok(())
}

/// `GET /api/fs/browse` — browse remote server filesystem directories
///
/// When `path` is empty or "/", returns a list of common root directories
/// (home, drive roots on Windows, / on Unix). For a specific path, returns
/// the directory contents (directories first, then files, both alphabetical).
pub async fn browse_fs(
    State(_state): State<AppState>,
    Query(query): Query<FsBrowseQuery>,
) -> Result<Json<FsBrowseResponse>, (StatusCode, Json<ApiError>)> {
    let requested_path = query.path.as_deref().unwrap_or("").trim();

    // Root browsing — return common starting points
    if requested_path.is_empty() || requested_path == "/" {
        return Ok(Json(FsBrowseResponse {
            path: requested_path.to_string(),
            entries: root_entries(),
        }));
    }

    // Validate path
    validate_path(requested_path)
        .map_err(|e| ApiError::bad_request(&e))?;

    let dir_path = std::path::Path::new(requested_path);

    // Verify it's a directory
    if !dir_path.is_dir() {
        return Err(ApiError::bad_request(&format!(
            "Path is not a directory: {}",
            requested_path
        )));
    }

    // Read directory entries
    let read_dir = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
            return Err(ApiError::internal(&format!(
                "Failed to read directory: {}",
                e
            )));
        }
    };

    let mut dirs: Vec<FsBrowseEntry> = Vec::new();
    let mut files: Vec<FsBrowseEntry> = Vec::new();

    // Normalize path for consistent output (strip Windows \\?\ prefix)
    let base_str = dir_path.to_string_lossy();
    let normalized_base = if base_str.starts_with(r"\\?\") {
        &base_str[4..]
    } else {
        base_str.as_ref()
    };
    let normalized_base = normalized_base.replace('\\', "/");

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files/dirs (starting with '.')
        if name.starts_with('.') {
            continue;
        }

        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().map_or(false, |m| m.is_dir());

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

    // Sort: directories first, then files — both alphabetical (case-insensitive)
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let mut entries = dirs;
    entries.append(&mut files);

    Ok(Json(FsBrowseResponse {
        path: normalized_base,
        entries,
    }))
}

/// Create filesystem browsing routes
pub fn fs_routes() -> Router<AppState> {
    Router::new().route("/api/fs/browse", get(browse_fs))
}
