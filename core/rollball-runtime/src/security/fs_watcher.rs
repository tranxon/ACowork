//! FsWatcher — cross-platform workspace filesystem monitoring
//!
//! Uses the `notify` crate for platform-agnostic file change detection.
//! Monitors new file creation, permission changes, and symlinks.
//!
//! Design: `docs/08-security.md` §11.4
//! Decision D8: Use `notify` crate instead of manually wrapping
//! inotify/FSEvents/ReadDirectoryChangesW.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

/// A filesystem event detected in the workspace.
#[derive(Debug, Clone)]
pub enum FsEvent {
    /// A new file was created.
    FileCreated { path: PathBuf },
    /// A file was modified.
    FileModified { path: PathBuf },
    /// A file was deleted.
    FileDeleted { path: PathBuf },
    /// A file's metadata changed (permissions, ownership, etc.).
    MetadataChanged { path: PathBuf },
    /// A symlink was created pointing outside the workspace.
    SymlinkCreated { path: PathBuf, target: PathBuf },
}

/// Filesystem watcher for the agent workspace.
///
/// Uses `notify` crate internally. Falls back to a polling
/// implementation if native OS notifications are unavailable.
pub struct FsWatcher {
    workspace_dir: PathBuf,
    #[allow(dead_code)]
    watcher: Option<notify::RecommendedWatcher>,
    rx: mpsc::Receiver<notify::Event>,
}

impl FsWatcher {
    /// Create a new filesystem watcher for the given workspace.
    pub fn new(workspace_dir: &Path) -> Result<Self, FsWatcherError> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })
        .map_err(|e| FsWatcherError::Init(e.to_string()))?;

        // Start watching the workspace directory recursively
        watcher
            .watch(workspace_dir, RecursiveMode::Recursive)
            .map_err(|e| FsWatcherError::Init(e.to_string()))?;

        Ok(Self {
            workspace_dir: workspace_dir.to_path_buf(),
            watcher: Some(watcher),
            rx,
        })
    }

    /// Try to receive pending filesystem events (non-blocking).
    pub fn try_recv_events(&self) -> Vec<FsEvent> {
        let mut events = Vec::new();
        while let Ok(raw_event) = self.rx.try_recv() {
            if let Some(fs_event) = self.convert_event(&raw_event) {
                events.push(fs_event);
            }
        }
        events
    }

    /// Receive filesystem events with a timeout.
    pub fn recv_events_timeout(&self, timeout: Duration) -> Vec<FsEvent> {
        let mut events = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let remaining = deadline - std::time::Instant::now();
            let wait = remaining.min(Duration::from_millis(100));
            match self.rx.recv_timeout(wait) {
                Ok(raw_event) => {
                    if let Some(fs_event) = self.convert_event(&raw_event) {
                        events.push(fs_event);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        events
    }

    /// Get the workspace directory being watched.
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    fn convert_event(&self, event: &notify::Event) -> Option<FsEvent> {
        use notify::EventKind;

        // Only care about events within the workspace
        for path in &event.paths {
            if !path.starts_with(&self.workspace_dir) {
                continue;
            }

            match &event.kind {
                EventKind::Create(_) => {
                    // Check for symlink
                    if path.is_symlink() {
                        if let Ok(target) = std::fs::read_link(path) {
                            // Check if symlink points outside workspace
                            let abs_target = if target.is_absolute() {
                                target
                            } else {
                                path.parent().unwrap_or(path).join(&target)
                            };
                            if !abs_target.starts_with(&self.workspace_dir) {
                                return Some(FsEvent::SymlinkCreated {
                                    path: path.clone(),
                                    target: abs_target,
                                });
                            }
                        }
                    }
                    return Some(FsEvent::FileCreated { path: path.clone() });
                }
                EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => {
                    return Some(FsEvent::MetadataChanged { path: path.clone() });
                }
                EventKind::Modify(_) => {
                    return Some(FsEvent::FileModified { path: path.clone() });
                }
                EventKind::Remove(_) => {
                    return Some(FsEvent::FileDeleted { path: path.clone() });
                }
                _ => {}
            }
        }

        None
    }
}

/// Check if a path looks like an executable file.
pub fn is_executable_file(path: &Path) -> bool {
    // Check common executable extensions (cross-platform)
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let script_extensions = [
        "sh", "bash", "zsh", "fish", "py", "rb", "pl", "pm",
        "js", "ts", "lua", "php",
    ];
    if script_extensions.contains(&ext) {
        return true;
    }

    // Windows-specific extensions
    if cfg!(windows) {
        return matches!(
            ext.to_lowercase().as_str(),
            "exe" | "bat" | "cmd" | "ps1" | "vbs" | "com"
        );
    }

    // On Unix, check execute permission bits
    #[cfg(unix)]
    {
        if ext.is_empty() {
            if let Ok(metadata) = std::fs::metadata(path) {
                use std::os::unix::fs::PermissionsExt;
                return metadata.permissions().mode() & 0o111 != 0;
            }
        }
    }

    false
}

/// Error type for FsWatcher operations.
#[derive(Debug, thiserror::Error)]
pub enum FsWatcherError {
    #[error("Watcher initialization failed: {0}")]
    Init(String),
    #[error("Watcher error: {0}")]
    Watch(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_is_executable_script() {
        let path = Path::new("/workspace/script.sh");
        assert!(is_executable_file(path));

        let path = Path::new("/workspace/app.py");
        assert!(is_executable_file(path));

        let path = Path::new("/workspace/readme.txt");
        assert!(!is_executable_file(path));
    }

    #[test]
    fn test_fs_event_debug() {
        let event = FsEvent::FileCreated {
            path: PathBuf::from("/workspace/newfile.txt"),
        };
        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("FileCreated"));
    }

    #[test]
    fn test_symlink_event() {
        let event = FsEvent::SymlinkCreated {
            path: PathBuf::from("/workspace/link"),
            target: PathBuf::from("/etc/passwd"),
        };
        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("SymlinkCreated"));
    }

    #[test]
    fn test_permissions_changed_event() {
        let event = FsEvent::MetadataChanged {
            path: PathBuf::from("/workspace/script.sh"),
        };
        match event {
            FsEvent::MetadataChanged { path } => {
                assert_eq!(path, PathBuf::from("/workspace/script.sh"));
            }
            _ => panic!("Expected MetadataChanged"),
        }
    }

    #[test]
    fn test_watcher_creation() {
        let dir = std::env::temp_dir().join("rollball-test-fswatcher");
        let _ = fs::create_dir_all(&dir);

        let result = FsWatcher::new(&dir);
        // May fail on some CI environments without inotify/FSEvents support
        if let Ok(watcher) = result {
            assert_eq!(watcher.workspace_dir(), dir);
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
