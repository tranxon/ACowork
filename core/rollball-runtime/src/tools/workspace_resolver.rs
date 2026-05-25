//! Workspace directory resolver — manages the list of allowed directories.
//!
//! ## Responsibility split (Session-level workspace refactor)
//!
//! - **WorkspaceResolver** (this module): manages the Agent-level workspace
//!   list — allowed directories, path validation, search directories.
//!   *No longer owns "current directory" selection* — that belongs to
//!   `SessionManager` per session.
//!
//! - **SessionManager**: owns per-session `current_dir` mapping
//!   (`session_id → workspace_id`). Tool execution queries
//!   `SessionManager::current_dir_for(session_id)` instead of
//!   `WorkspaceResolver::current_dir()`.
//!
//! ## Key APIs
//!
//! 1. **`agent_home()`**: the agent's install directory.
//!    Used for runtime data: conversations, memory, logs, identity.
//!
//! 2. **`search_dirs()`**: directories to search (content_search / glob_search).
//!    All workspace directories. Falls back to `[agent_home]`.
//!
//! 3. **`allowed_dirs()`**: all allowed dirs (for PathGuardedTool validation).
//!
//! 4. **`find_by_id()`**: look up a workspace by its ID.

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

impl WorkspaceAccess {
    /// Convert to the string representation used in JSON serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkspaceAccess::ReadOnly => "read-only",
            WorkspaceAccess::ReadWrite => "read-write",
        }
    }
}

impl std::fmt::Display for WorkspaceAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for WorkspaceAccess {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for WorkspaceAccess {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "read-write" => Ok(WorkspaceAccess::ReadWrite),
            _ => Ok(WorkspaceAccess::ReadOnly),
        }
    }
}

/// A single workspace directory entry
#[derive(Clone, Debug)]
pub struct WorkspaceDir {
    pub id: String,
    pub path: String,
    pub access: WorkspaceAccess,
    /// Whether this was the last active workspace when the user last selected it.
    /// Used as the default workspace for new sessions.
    pub last_active: bool,
}

/// Central resolver for workspace directories.
///
/// Constructed once at startup from the agent's `work_dir`.
/// Reads `.agent_workspaces.json` to discover user-configured directories.
///
/// Does NOT manage "current directory" — that is per-session state
/// owned by `SessionManager`.
#[derive(Clone, Debug)]
pub struct WorkspaceResolver {
    /// Agent install dir (for logs, conversations, memory, identity)
    agent_home: String,
    /// All allowed dirs from .agent_workspaces.json + fallbacks
    allowed_dirs: Vec<WorkspaceDir>,
}

impl WorkspaceResolver {
    /// Build a resolver from the agent's work_dir.
    ///
    /// Reads `.agent_workspaces.json` from `work_dir` to discover
    /// user-configured workspace directories.
    pub fn new(work_dir: &str) -> Self {
        let allowed_dirs = load_workspace_dirs(work_dir);
        Self {
            agent_home: work_dir.to_string(),
            allowed_dirs,
        }
    }

    /// Reload the resolver from disk (re-reads .agent_workspaces.json).
    ///
    /// Used after receiving a `WorkspaceConfigUpdate` from Gateway, which
    /// writes the updated config to disk before calling this.
    pub fn reload(work_dir: &str) -> Self {
        Self::new(work_dir)
    }

    /// Agent home dir (for conversations, memory, logs, identity, etc.)
    pub fn agent_home(&self) -> &str {
        &self.agent_home
    }

    /// Look up a workspace directory by its ID.
    /// Returns None if not found.
    pub fn find_by_id(&self, id: &str) -> Option<&WorkspaceDir> {
        self.allowed_dirs.iter().find(|d| d.id == id)
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

    /// Return the workspace ID of the last active workspace, if any.
    /// Checks the `last_active` flag on each user-configured workspace.
    /// Returns `None` if no workspace has `last_active` set.
    pub fn last_active_workspace_id(&self) -> Option<&str> {
        self.allowed_dirs
            .iter()
            .find(|d| d.last_active)
            .map(|d| d.id.as_str())
    }
}

/// Load workspace directories from `.agent_workspaces.json`.
fn load_workspace_dirs(work_dir: &str) -> Vec<WorkspaceDir> {
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
        last_active: bool,
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
                let mut dirs: Vec<WorkspaceDir> = Vec::new();

                for entry in config.additional_dirs.into_iter() {
                    dirs.push(WorkspaceDir {
                        id: entry.id,
                        path: entry.path,
                        access: if entry.access == "read-write" {
                            WorkspaceAccess::ReadWrite
                        } else {
                            WorkspaceAccess::ReadOnly
                        },
                        last_active: entry.last_active,
                    });
                }

                // Include package root (parent of work_dir) as read-only
                if let Some(package_root) = Path::new(work_dir).parent() {
                    let package_root_str = package_root.to_string_lossy().to_string();
                    if package_root_str != work_dir {
                        dirs.push(WorkspaceDir {
                            id: "__package_root__".to_string(),
                            path: package_root_str,
                            access: WorkspaceAccess::ReadOnly,
                            last_active: false,
                        });
                    }
                }

                // Always include agent_home as read-write
                dirs.push(WorkspaceDir {
                    id: "__agent_home__".to_string(),
                    path: work_dir.to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                });

                tracing::info!(
                    work_dir,
                    count = dirs.len(),
                    dirs = ?dirs.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
                    "Loaded workspace directories from .agent_workspaces.json"
                );

                dirs
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
fn fallback_dirs(work_dir: &str) -> Vec<WorkspaceDir> {
    let mut dirs = vec![];

    // Include package root as read-only
    if let Some(package_root) = Path::new(work_dir).parent() {
        let package_root_str = package_root.to_string_lossy().to_string();
        if package_root_str != work_dir {
            dirs.push(WorkspaceDir {
                id: "__package_root__".to_string(),
                path: package_root_str,
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
            });
        }
    }

    dirs.push(WorkspaceDir {
        id: "__agent_home__".to_string(),
        path: work_dir.to_string(),
        access: WorkspaceAccess::ReadWrite,
        last_active: false,
    });

    dirs
}

// ── Persistence & Formatting ───────────────────────────────────────────────

/// Full workspace directory entry with all metadata (for serialization + formatting).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDirFull {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub access: WorkspaceAccess,
    pub added_at: String,
    /// Whether this was the last active workspace when the user last selected it.
    /// Used as the default workspace for new sessions.
    #[serde(default)]
    pub last_active: bool,
    #[serde(default)]
    pub select_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selected_at: Option<String>,
}

/// Full workspace config (mirrors Gateway's WorkspaceConfig for file persistence).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfigFull {
    pub version: String,
    #[serde(default)]
    pub additional_dirs: Vec<WorkspaceDirFull>,
}

/// Write workspace config JSON to `.agent_workspaces.json` atomically (tmp + rename).
///
/// On Windows, `std::fs::rename` fails if the target file exists and is open
/// (e.g. another thread calling `reload`). We work around this by removing
/// the old file first. The tmp path uses the full base name so it's clear
/// which file the temp belongs to.
pub fn write_workspace_config(work_dir: &str, config_json: &str) -> Result<(), String> {
    let config_dir = Path::new(work_dir).join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir: {}", e))?;

    let config_path = config_dir.join(".agent_workspaces.json");
    let tmp_path = config_dir.join(".agent_workspaces.json.tmp");
    std::fs::write(&tmp_path, config_json)
        .map_err(|e| format!("Failed to write temp config: {}", e))?;
    // Windows: rename fails if target exists and is open, so remove first.
    // On Unix this is a no-op if the rename succeeds, but the remove is
    // harmless and ensures Windows compatibility.
    let _ = std::fs::remove_file(&config_path);
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| format!("Failed to rename config: {}", e))?;

    tracing::info!(
        work_dir,
        "Wrote .agent_workspaces.json from Gateway WorkspaceConfigUpdate"
    );
    Ok(())
}

/// Format workspace context for a specific session.
///
/// Takes the WorkspaceResolver (for the full allowed list + agent_home),
/// and the session's current workspace selection.
/// `current_ws_id` of `"__agent_home__"` means agent home is active.
pub fn format_workspace_context_for_session(
    resolver: &WorkspaceResolver,
    current_ws_id: &str,
) -> String {
    // Determine current directory path
    let (current_path, current_alias, current_access, is_agent_home) =
        if current_ws_id == "__agent_home__" {
            (resolver.agent_home().to_string(), None::<String>, None, true)
        } else {
            match resolver.find_by_id(current_ws_id) {
                Some(dir) => (
                    dir.path.clone(),
                    None, // alias not stored in WorkspaceDir
                    Some(dir.access.clone()),
                    false,
                ),
                None => (
                    resolver.agent_home().to_string(),
                    None,
                    None,
                    true,
                ),
            }
        };

    let mut buf = String::new();
    buf.push_str("## Workspace Environment\n\n");

    // 1. Current Working Directory
    if is_agent_home {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_md(&current_path)
        ));
    } else {
        let alias = current_alias.as_deref().unwrap_or("-");
        let access_str = current_access
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or("read-write");
        buf.push_str(&format!(
            "Current Working Directory: {} ({}, {})\n",
            escape_md(&current_path),
            alias,
            access_str,
        ));
        buf.push_str("This is your currently active workspace.\n\n");
    }

    // 2. Agent Home Directory
    buf.push_str(&format!(
        "Agent Home Directory: {} (installation directory)\n\n",
        escape_md(resolver.agent_home())
    ));

    // 3. Available Workspaces table (user workspaces only, not synthetic entries)
    let user_workspaces: Vec<&WorkspaceDir> = resolver
        .allowed_dirs()
        .iter()
        .filter(|d| d.id != "__agent_home__" && d.id != "__package_root__")
        .collect();

    if !user_workspaces.is_empty() {
        buf.push_str("### Available Workspaces\n");
        buf.push_str("| # | Path | Access |\n");
        buf.push_str("|---|------|--------|\n");

        for (i, ws) in user_workspaces.iter().enumerate() {
            let active_marker = if ws.id == current_ws_id { " *" } else { "" };
            buf.push_str(&format!(
                "| {} | {} | {} |\n",
                i + 1,
                escape_md(&ws.path),
                ws.access,
            ));
            let _ = active_marker;
        }
    }

    buf.push_str("\nIMPORTANT: When performing file operations or running shell commands, ALWAYS use the\n");
    buf.push_str("Current Working Directory path shown above as your starting directory.\n");
    buf.push_str("Do NOT use the Agent Home Directory for project work — it contains the agent's own\n");
    buf.push_str("configuration files, not your project code.\n");
    buf.push_str("All listed directories are authorized for access at the indicated permission level.\n");

    buf
}

/// Legacy: format workspace context from the raw config JSON.
/// Kept for backward compatibility during transition.
/// Prefer `format_workspace_context_for_session` for session-aware formatting.
pub fn format_workspace_context_from_json(config_json: &str, install_path: &str) -> String {
    let config: WorkspaceConfigFull = match serde_json::from_str(config_json) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse workspace config JSON for formatting");
            return format_workspace_context_fallback(install_path);
        }
    };
    format_workspace_context_full_legacy(&config.additional_dirs, install_path)
}

/// Escape special characters for Markdown table cells.
fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', "")
}

/// Legacy: format workspace context with the old `last_active` logic.
fn format_workspace_context_full_legacy(workspaces: &[WorkspaceDirFull], install_path: &str) -> String {
    let mut buf = String::new();
    buf.push_str("## Workspace Environment\n\n");

    if workspaces.is_empty() {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_md(install_path)
        ));
        buf.push_str("No additional workspaces have been configured.\n");
        return buf;
    }

    let active = workspaces.iter().find(|w| w.last_active);
    if let Some(current) = active {
        let alias = current.alias.as_deref().unwrap_or("-");
        buf.push_str(&format!(
            "Current Working Directory: {} ({}, {})\n",
            escape_md(&current.path),
            alias,
            current.access,
        ));
    } else {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_md(install_path)
        ));
    }
    buf.push_str(&format!(
        "Agent Home Directory: {} (installation directory)\n\n",
        escape_md(install_path)
    ));
    buf.push_str("### Available Workspaces\n");
    buf.push_str("| # | Alias | Path | Access | Active |\n");
    buf.push_str("|---|-------|------|--------|--------|\n");
    for (i, ws) in workspaces.iter().enumerate() {
        let alias = escape_md(ws.alias.as_deref().unwrap_or("-"));
        let active_marker = if ws.last_active { "*" } else { "" };
        buf.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1, alias, escape_md(&ws.path), ws.access, active_marker,
        ));
    }
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
        assert_eq!(resolver.agent_home(), dir.path().to_str().unwrap());
        // find_by_id should work for synthetic entries
        assert!(resolver.find_by_id("__agent_home__").is_some());
        assert_eq!(
            resolver.find_by_id("__agent_home__").unwrap().path,
            dir.path().to_str().unwrap()
        );
    }

    #[test]
    fn test_resolver_with_workspaces() {
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
                    "last_active": true
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
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config").join(".agent_workspaces.json"), config).unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        assert_eq!(resolver.agent_home(), dir.path().to_str().unwrap());

        // find_by_id should locate user workspaces
        let ws1 = resolver.find_by_id("ws-1").unwrap();
        assert_eq!(ws1.path, "D:\\projects\\my-project");
        let ws2 = resolver.find_by_id("ws-2").unwrap();
        assert_eq!(ws2.path, "D:\\projects\\other");

        // search_dirs should include all workspace dirs
        let search = resolver.search_dirs();
        assert!(search.iter().any(|d| d.contains("my-project")));
        assert!(search.iter().any(|d| d.contains("other")));
    }

    #[test]
    fn test_format_workspace_context_for_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\projects\\my-project",
                    "alias": "my-project",
                    "access": "read-write",
                    "added_at": "2026-05-01T00:00:00Z"
                }
            ]
        }"#;
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config").join(".agent_workspaces.json"), config).unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());

        // Agent home as current
        let ctx = format_workspace_context_for_session(&resolver, "__agent_home__");
        assert!(ctx.contains("(agent home)"));
        assert!(ctx.contains("my-project"));

        // User workspace as current
        let ctx = format_workspace_context_for_session(&resolver, "ws-1");
        assert!(ctx.contains("my-project"));
        assert!(ctx.contains("read-write"));
    }
}
