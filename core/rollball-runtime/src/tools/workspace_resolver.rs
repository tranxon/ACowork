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

use serde::Deserialize;
use std::path::Path;

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

    let config_path = Path::new(work_dir).join(".agent_workspaces.json");

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
        std::fs::write(dir.path().join(".agent_workspaces.json"), config).unwrap();

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
        std::fs::write(dir.path().join(".agent_workspaces.json"), config).unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        // No is_current=true, so falls back to agent_home
        assert_eq!(resolver.current_dir(), dir.path().to_str().unwrap());
    }
}
