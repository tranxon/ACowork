//! Workspace directory resolver — single source of truth for "where tools operate".
//!
//! All file-operation tools (content_search, glob_search, file_read/write/edit, shell)
//! must go through `WorkspaceResolver` to determine which directory to act on.
//!
//! ## Priority
//!
//! 1. **`current_dir()`**: the directory the user is "working in".
//!    - Determined by `is_current: true` in `.agent_workspaces.json`
//!    - Falls back to `agent_home` if no workspace is marked current
//!
//! 2. **`agent_home()`**: the agent's install directory.
//!    - Used for runtime data: conversations, memory, logs, identity
//!    - This is the `work_dir` passed from CLI/gRPC
//!
//! 3. **`search_dirs()`**: directories to search (content_search / glob_search).
//!    - All workspace directories (including non-current ones)
//!    - Falls back to `[agent_home]` if no workspaces configured

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Shared, hot-reloadable workspace resolver.
///
/// Wrapped in Arc<RwLock<>> so tools share one reference and see
/// workspace changes immediately when Gateway pushes WorkspaceConfigUpdate.
pub type SharedResolver = Arc<RwLock<WorkspaceResolver>>;

/// Access level for a workspace directory
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

/// A single workspace directory entry
#[derive(Clone, Debug)]
pub struct WorkspaceDir {
    pub path: String,
    pub access: WorkspaceAccess,
}

/// Central resolver for workspace directories.
///
/// Constructed once at startup from the agent's `work_dir`.
/// Reads `.agent_workspaces.json` to discover user-configured directories.
#[derive(Clone, Debug)]
pub struct WorkspaceResolver {
    /// Agent install dir (for logs, conversations, memory, identity)
    agent_home: String,
    /// All allowed dirs from .agent_workspaces.json + fallbacks
    allowed_dirs: Vec<WorkspaceDir>,
    /// Index of the `is_current=true` entry in allowed_dirs, if any
    current_dir_index: Option<usize>,
}

impl WorkspaceResolver {
    /// Build a resolver from the agent's work_dir.
    ///
    /// Reads `.agent_workspaces.json` from `work_dir` to discover
    /// user-configured workspace directories.
    pub fn new(work_dir: &str) -> Self {
        let (allowed_dirs, current_dir_index) = load_workspace_dirs(work_dir);
        Self {
            agent_home: work_dir.to_string(),
            allowed_dirs,
            current_dir_index,
        }
    }

    /// Reload the resolver from disk (re-reads .agent_workspaces.json).
    ///
    /// Used after receiving a `WorkspaceConfigUpdate` from Gateway, which
    /// writes the updated config to disk before calling this.
    pub fn reload(work_dir: &str) -> Self {
        Self::new(work_dir)
    }

    /// The "current working directory" for file operations.
    ///
    /// Priority: first `is_current=true` workspace dir > fallback to agent_home.
    ///
    /// This is what file_read/write/edit/shell use as the base directory,
    /// and what content_search/glob_search use when no `path` param is given.
    pub fn current_dir(&self) -> &str {
        if let Some(idx) = self.current_dir_index {
            &self.allowed_dirs[idx].path
        } else {
            &self.agent_home
        }
    }

    /// Agent home dir (for conversations, memory, logs, identity, etc.)
    pub fn agent_home(&self) -> &str {
        &self.agent_home
    }

    /// All searchable directories (for content_search / glob_search).
    ///
    /// Returns all workspace directories. If no workspaces are configured,
    /// returns `[agent_home]`.
    pub fn search_dirs(&self) -> Vec<&str> {
        if self.allowed_dirs.is_empty() {
            vec![&self.agent_home]
        } else {
            self.allowed_dirs.iter().map(|d| d.path.as_str()).collect()
        }
    }

    /// All allowed dirs (for PathGuardedTool path validation).
    pub fn allowed_dirs(&self) -> &[WorkspaceDir] {
        &self.allowed_dirs
    }
}

/// Load workspace directories from `.agent_workspaces.json`.
///
/// Returns `(dirs, current_index)` where `current_index` is the index of the
/// `is_current=true` entry (if any).
fn load_workspace_dirs(work_dir: &str) -> (Vec<WorkspaceDir>, Option<usize>) {
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct WorkspaceConfig {
        version: String,
        #[serde(default)]
        additional_dirs: Vec<WorkspaceDirEntry>,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct WorkspaceDirEntry {
        id: String,
        path: String,
        alias: Option<String>,
        access: String,
        added_at: String,
        #[serde(default)]
        is_current: bool,
    }

    let config_path = Path::new(work_dir).join("config").join(".agent_workspaces.json");

    if !config_path.exists() {
        tracing::warn!(
            work_dir,
            config_path = %config_path.display(),
            "No .agent_workspaces.json found, using work_dir as default"
        );
        return fallback_dirs(work_dir);
    }

    match std::fs::read_to_string(&config_path) {
        Ok(content) => match serde_json::from_str::<WorkspaceConfig>(&content) {
            Ok(config) => {
                let mut current_index = None;
                let mut dirs: Vec<WorkspaceDir> = Vec::new();

                for (i, entry) in config.additional_dirs.into_iter().enumerate() {
                    if entry.is_current && current_index.is_none() {
                        current_index = Some(i);
                    }
                    dirs.push(WorkspaceDir {
                        path: entry.path,
                        access: if entry.access == "read-write" {
                            WorkspaceAccess::ReadWrite
                        } else {
                            WorkspaceAccess::ReadOnly
                        },
                    });
                }

                // Include package root (parent of work_dir) as read-only
                if let Some(package_root) = Path::new(work_dir).parent() {
                    let package_root_str = package_root.to_string_lossy().to_string();
                    if package_root_str != work_dir {
                        dirs.push(WorkspaceDir {
                            path: package_root_str,
                            access: WorkspaceAccess::ReadOnly,
                        });
                    }
                }

                // Always include agent_home as read-write
                dirs.push(WorkspaceDir {
                    path: work_dir.to_string(),
                    access: WorkspaceAccess::ReadWrite,
                });

                tracing::info!(
                    work_dir,
                    count = dirs.len(),
                    current_dir = ?current_index.map(|i| &dirs[i].path),
                    dirs = ?dirs.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
                    "Loaded workspace directories from .agent_workspaces.json"
                );

                (dirs, current_index)
            }
            Err(e) => {
                tracing::error!(
                    work_dir,
                    error = %e,
                    "Failed to parse .agent_workspaces.json, using work_dir as default"
                );
                fallback_dirs(work_dir)
            }
        },
        Err(e) => {
            tracing::error!(
                work_dir,
                error = %e,
                "Failed to read .agent_workspaces.json, using work_dir as default"
            );
            fallback_dirs(work_dir)
        }
    }
}

/// Fallback: use work_dir as the only allowed directory
fn fallback_dirs(work_dir: &str) -> (Vec<WorkspaceDir>, Option<usize>) {
    let mut dirs = vec![];

    // Include package root as read-only
    if let Some(package_root) = Path::new(work_dir).parent() {
        let package_root_str = package_root.to_string_lossy().to_string();
        if package_root_str != work_dir {
            dirs.push(WorkspaceDir {
                path: package_root_str,
                access: WorkspaceAccess::ReadOnly,
            });
        }
    }

    dirs.push(WorkspaceDir {
        path: work_dir.to_string(),
        access: WorkspaceAccess::ReadWrite,
    });

    (dirs, None)
}

// ── Persistence & Formatting ───────────────────────────────────────────────

/// Full workspace directory entry with all metadata (for serialization + formatting).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDirFull {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub access: String,
    pub added_at: String,
    #[serde(default)]
    pub is_current: bool,
    #[serde(default)]
    pub select_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selected_at: Option<String>,
}

/// Full workspace config (mirrors Gateway's WorkspaceConfig for file persistence).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceConfigFull {
    version: String,
    #[serde(default)]
    additional_dirs: Vec<WorkspaceDirFull>,
}

/// Write workspace config JSON to `.agent_workspaces.json` atomically (tmp + rename).
pub fn write_workspace_config(work_dir: &str, config_json: &str) -> Result<(), String> {
    let config_dir = Path::new(work_dir).join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir: {}", e))?;

    let config_path = config_dir.join(".agent_workspaces.json");
    let tmp_path = config_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, config_json)
        .map_err(|e| format!("Failed to write temp config: {}", e))?;
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| format!("Failed to rename config: {}", e))?;

    tracing::info!(
        work_dir,
        "Wrote .agent_workspaces.json from Gateway WorkspaceConfigUpdate"
    );
    Ok(())
}

/// Format workspace context Markdown from the raw config JSON.
///
/// This is the Runtime-side equivalent of what Gateway's `format_workspace_context`
/// used to do. The Runtime now self-formats its LLM context.
///
/// `install_path` is the agent's home directory (for the "Agent Home Directory" label).
pub fn format_workspace_context_from_json(config_json: &str, install_path: &str) -> String {
    let config: WorkspaceConfigFull = match serde_json::from_str(config_json) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse workspace config JSON for formatting");
            return format_workspace_context_fallback(install_path);
        }
    };
    format_workspace_context_full(&config.additional_dirs, install_path)
}

/// Compute the list of workspaces to inject into LLM context (at most 3).
fn compute_context_workspaces(workspaces: &[WorkspaceDirFull]) -> Vec<usize> {
    if workspaces.is_empty() {
        return Vec::new();
    }

    let current_idx = workspaces.iter().position(|w| w.is_current);

    let max_select = workspaces.iter().map(|w| w.select_count).max().unwrap_or(0);

    let now = chrono::Utc::now();
    let mut scored: Vec<(f64, usize)> = workspaces
        .iter()
        .enumerate()
        .filter(|(_, w)| !w.is_current)
        .map(|(i, w)| {
            let normalized = if max_select > 0 {
                w.select_count as f64 / max_select as f64
            } else {
                0.0
            };

            let days_since = w
                .last_selected_at
                .as_ref()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| {
                    let dur = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
                    (dur.num_days().max(0) as f64)
                })
                .unwrap_or(1e9_f64);

            let recency = 1.0 / (1.0 + days_since);
            (normalized * 0.3 + recency * 0.7, i)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut result = Vec::new();
    if let Some(idx) = current_idx {
        result.push(idx);
    }
    for (_, idx) in scored.into_iter().take(2) {
        if result.len() >= 3 {
            break;
        }
        result.push(idx);
    }
    result
}

/// Escape special characters for Markdown table cells.
fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', "")
}

/// Produce the final Markdown context text.
fn format_workspace_context_full(workspaces: &[WorkspaceDirFull], install_path: &str) -> String {
    let mut buf = String::new();
    buf.push_str("## Workspace Environment\n\n");

    if workspaces.is_empty() {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_md(install_path)
        ));
        buf.push_str("No additional workspaces have been configured. Use the Agent Home Directory above\n");
        buf.push_str("as the default working directory for all file and shell operations.\n");
        return buf;
    }

    let active = workspaces.iter().find(|w| w.is_current);

    // 1. Current Working Directory
    if let Some(current) = active {
        let alias = current.alias.as_deref().unwrap_or("-");
        buf.push_str(&format!(
            "Current Working Directory: {} ({}, {})\n",
            escape_md(&current.path),
            alias,
            current.access,
        ));
        buf.push_str("This is your currently active workspace.\n\n");
    } else {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_md(install_path)
        ));
        buf.push_str("No workspace is currently selected. The agent's home directory is the default working directory.\n\n");
    }

    // 2. Agent Home Directory
    buf.push_str(&format!(
        "Agent Home Directory: {} (installation directory)\n\n",
        escape_md(install_path)
    ));

    // 3. Available Workspaces table
    buf.push_str("### Available Workspaces\n");
    buf.push_str("| # | Alias | Path | Access | Active |\n");
    buf.push_str("|---|-------|------|--------|--------|\n");

    for (i, ws) in workspaces.iter().enumerate() {
        let alias = escape_md(ws.alias.as_deref().unwrap_or("-"));
        let active_marker = if ws.is_current { "*" } else { "" };
        buf.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            alias,
            escape_md(&ws.path),
            ws.access,
            active_marker,
        ));
    }

    buf.push_str("\nIMPORTANT: When performing file operations or running shell commands, ALWAYS use the\n");
    buf.push_str("Current Working Directory path shown above as your starting directory.\n");
    buf.push_str("Do NOT use the Agent Home Directory for project work — it contains the agent's own\n");
    buf.push_str("configuration files, not your project code.\n");
    buf.push_str("All listed directories are authorized for access at the indicated permission level.\n");

    buf
}

/// Fallback context when config JSON is missing or unparseable.
fn format_workspace_context_fallback(install_path: &str) -> String {
    format!(
        "## Workspace Environment\n\n\
         Current Working Directory: {} (agent home)\n\
         No workspace configuration available. The agent's home directory is the default working directory.\n",
        escape_md(install_path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_no_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        // Falls back to work_dir as current_dir
        assert_eq!(resolver.current_dir(), dir.path().to_str().unwrap());
        assert_eq!(resolver.agent_home(), dir.path().to_str().unwrap());
    }

    #[test]
    fn test_resolver_with_current_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\projects\\my-project",
                    "alias": "my-project",
                    "access": "read-write",
                    "added_at": "2026-05-01T00:00:00Z",
                    "is_current": true
                },
                {
                    "id": "ws-2",
                    "path": "D:\\projects\\other",
                    "alias": "other",
                    "access": "read-only",
                    "added_at": "2026-05-01T00:00:00Z"
                }
            ]
        }"#;
        std::fs::write(dir.path().join("config").join(".agent_workspaces.json"), config).unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        assert_eq!(resolver.current_dir(), "D:\\projects\\my-project");
        assert_eq!(resolver.agent_home(), dir.path().to_str().unwrap());
        // search_dirs should include all workspace dirs
        let search = resolver.search_dirs();
        assert!(search.iter().any(|d| d.contains("my-project")));
        assert!(search.iter().any(|d| d.contains("other")));
    }

    #[test]
    fn test_resolver_no_current_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\projects\\other",
                    "alias": "other",
                    "access": "read-only",
                    "added_at": "2026-05-01T00:00:00Z"
                }
            ]
        }"#;
        std::fs::write(dir.path().join("config").join(".agent_workspaces.json"), config).unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        // No is_current=true, so falls back to agent_home
        assert_eq!(resolver.current_dir(), dir.path().to_str().unwrap());
    }
}
