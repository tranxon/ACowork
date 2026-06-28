//! Tool security wrappers — decorator pattern for tool security
//!
//! Adapted from ZeroClaw's RateLimitedTool + PathGuardedTool pattern.
//! ACowork deviation: uses manifest-driven permission checking
//! instead of ZeroClaw's config-driven security policy.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::tools::path_utils;

// Re-export for backward compatibility — existing tests import from wrappers
pub use crate::tools::workspace_resolver::{SharedResolver, WorkspaceAccess, WorkspaceDir, WorkspaceResolver};
use std::time::Instant;

/// Rate-limited tool wrapper
///
/// Enforces a maximum number of calls per minute for a tool.
/// Returns an error when the rate limit is exceeded.
pub struct RateLimitedTool {
    inner: Arc<dyn Tool>,
    max_calls_per_minute: u32,
    call_times: parking_lot::Mutex<Vec<Instant>>,
}

impl RateLimitedTool {
    pub fn new(inner: Arc<dyn Tool>, max_calls_per_minute: u32) -> Self {
        Self {
            inner,
            max_calls_per_minute,
            call_times: parking_lot::Mutex::new(Vec::new()),
        }
    }

    fn check_rate_limit(&self) -> Result<(), String> {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        let mut times = self.call_times.lock();
        times.retain(|t| *t > cutoff);

        if times.len() >= self.max_calls_per_minute as usize {
            return Err(format!(
                "Rate limit exceeded for tool '{}': max {} calls/minute",
                self.inner.name(),
                self.max_calls_per_minute
            ));
        }

        times.push(now);
        Ok(())
    }
}

#[async_trait]
impl Tool for RateLimitedTool {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        if let Err(e) = self.check_rate_limit() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(e),
                token_usage: None,
            });
        }
        self.inner.execute(params, work_dir).await
    }
}

/// Path-guarded tool wrapper
///
/// Restricts filesystem tool access to paths within the agent's
/// allowed working directories. Validates path parameters before
/// executing the inner tool.
///
/// Reads workspace directories fresh from the global resolver on every
/// `execute()` call — no local caching. This ensures hot-reload of
/// workspace access changes (e.g., read-only → read-write toggled
/// by the user via the frontend) takes effect immediately without
/// requiring an agent restart.
pub struct PathGuardedTool {
    inner: Arc<dyn Tool>,
    resolver: SharedResolver,
}

impl PathGuardedTool {
    pub fn new(inner: Arc<dyn Tool>, resolver: SharedResolver) -> Self {
        Self {
            inner,
            resolver,
        }
    }

    /// Validate that a path is within any of the allowed directories
    ///
    /// Security: prevents path traversal attacks (e.g., "../../etc/passwd")
    /// and prefix-suffix attacks (e.g., "/tmp/agent-workdir-eval/secret").
    /// Uses path component normalization instead of filesystem canonicalize
    /// so it works for paths that don't exist yet.
    ///
    /// Returns the `WorkspaceAccess` level of the **most specific** (longest
    /// prefix) matching allowed directory. This ensures nested directories
    /// with stricter access take precedence over broader parent dirs.
    fn validate_path(&self, path: &str) -> Result<WorkspaceAccess, String> {
        let allowed_dirs = self.resolver.read().unwrap().allowed_dirs().to_vec();

        if allowed_dirs.is_empty() {
            return Err("No workspace directories configured for this agent".to_string());
        }

        let target = std::path::Path::new(path);

        // Track the best (most specific) match: (prefix_length, access)
        let mut best_match: Option<(usize, WorkspaceAccess)> = None;

        // Check against each allowed directory
        for dir in &allowed_dirs {
            let allowed = std::path::Path::new(&dir.path);

            // Resolve relative paths against this allowed dir
            let target_normalized = if target.is_absolute() {
                target.to_path_buf()
            } else {
                allowed.join(target)
            };

            // Normalize path components to resolve ".." and reject traversal
            let normalized = Self::normalize_path(&target_normalized);

            // Reject if normalization failed (e.g., ".." escaped root)
            if normalized.is_none() {
                continue; // Try next allowed dir
            }

            let normalized = normalized.unwrap();

            // Also normalize the allowed dir for consistent comparison
            let allowed_normalized =
                Self::normalize_path(allowed).unwrap_or_else(|| allowed.to_path_buf());

            // Convert to string and normalize separators for cross-platform comparison
            let target_str = path_utils::normalize_separators(&normalized.to_string_lossy());
            let allowed_str =
                path_utils::normalize_separators(&allowed_normalized.to_string_lossy());

            tracing::debug!(
                target_path = %target_str,
                allowed_path = %allowed_str,
                "PathGuardedTool: validating path"
            );

            // Ensure target starts with allowed dir + separator to prevent
            // prefix-suffix attacks (e.g., "/tmp/agent-workdir-eval" matching "/tmp/agent-workdir")
            if target_str.starts_with(&allowed_str) {
                // Verify the prefix match ends at a path boundary
                let suffix = &target_str[allowed_str.len()..];
                if suffix.is_empty() || suffix.starts_with('/') || suffix.starts_with('\\') {
                    // This is a valid match — keep the most specific (longest) prefix
                    let current_len = allowed_str.len();
                    let is_better = best_match
                        .as_ref()
                        .is_none_or(|(prev_len, _)| current_len > *prev_len);
                    if is_better {
                        best_match = Some((current_len, dir.access.clone()));
                    }
                }
            }
        }

        // Return the access level of the best match, or error if no match
        best_match.map(|(_, access)| access).ok_or_else(|| {
            format!(
                "Path '{}' is outside all allowed workspace directories",
                path
            )
        })
    }

    /// Normalize a path by resolving ".." components without touching the filesystem.
    /// Returns None if ".." escapes the path root (i.e., path traversal).
    fn normalize_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
        let mut components = Vec::new();
        for comp in path.components() {
            match comp {
                std::path::Component::Prefix(p) => components.push(std::path::Component::Prefix(p)),
                std::path::Component::RootDir => components.push(std::path::Component::RootDir),
                std::path::Component::Normal(c) => components.push(std::path::Component::Normal(c)),
                std::path::Component::CurDir => { /* skip current dir */ }
                std::path::Component::ParentDir => {
                    // Pop the last normal component; fail if we'd escape root
                    let popped = components
                        .iter()
                        .rposition(|c| matches!(c, std::path::Component::Normal(_)));
                    let pos = popped?;
                    components.truncate(pos);
                }
            }
        }
        Some(components.iter().collect())
    }

    /// Check if the wrapped tool is a filesystem tool that needs path validation
    fn is_filesystem_tool(&self) -> bool {
        matches!(
            self.inner.name().as_str(),
            "file_read"
                | "file_write"
                | "file_edit"
                | "glob_search"
                | "content_search"
                | "doc_reader"
        )
    }

    /// Check if the wrapped tool performs write operations
    fn is_write_tool(&self) -> bool {
        matches!(self.inner.name().as_str(), "file_write" | "file_edit")
    }
}

#[async_trait]
impl Tool for PathGuardedTool {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let mut params = params;
        if self.is_filesystem_tool() {
            // Check path parameter
            if let Some(path) = params["path"].as_str().map(|s| s.to_string()) {
                match self.validate_path(&path) {
                    Ok(access) => {
                        // Write tools require ReadWrite access
                        if self.is_write_tool() && access != WorkspaceAccess::ReadWrite {
                            return Ok(ToolResult {
                                ok: false,
                                content: String::new(),
                                error: Some(format!(
                                    "Write access denied for path '{}': directory is read-only",
                                    path
                                )),
                                token_usage: None,
                            });
                        }
                        // Rewrite relative paths to absolute using work_dir.
                        // All filesystem tools route through `path_utils::resolve`
                        // for this exact step, so behaviour stays in lock-step.
                        let abs_path_str = acowork_core::path_utils::resolve(&path, work_dir)
                            .to_string_lossy()
                            .into_owned();
                        // Re-validate the rewritten absolute path to ensure it
                        // still falls within an allowed directory and has the
                        // correct access level for the actual target location.
                        match self.validate_path(&abs_path_str) {
                            Ok(resolved_access) => {
                                if self.is_write_tool()
                                    && resolved_access != WorkspaceAccess::ReadWrite
                                {
                                    return Ok(ToolResult {
                                        ok: false,
                                        content: String::new(),
                                        error: Some(format!(
                                            "Write access denied for resolved path '{}': directory is read-only",
                                            abs_path_str
                                        )),
                                        token_usage: None,
                                    });
                                }
                                params["path"] =
                                    serde_json::Value::String(abs_path_str);
                            }
                            Err(e) => {
                                return Ok(ToolResult {
                                    ok: false,
                                    content: String::new(),
                                    error: Some(e),
                                    token_usage: None,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        return Ok(ToolResult {
                            ok: false,
                            content: String::new(),
                            error: Some(e),
                            token_usage: None,
                        });
                    }
                }
            }
        }
        self.inner.execute(params, work_dir).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::tools::traits::ToolSpec;
    use std::sync::RwLock;

    /// Helper: build a SharedResolver from a Vec of WorkspaceDir for testing.
    fn test_resolver(dirs: Vec<WorkspaceDir>) -> SharedResolver {
        Arc::new(RwLock::new(WorkspaceResolver::new_for_test(dirs)))
    }

    /// A simple test tool for testing wrappers
    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_string(),
                description: "Echo tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: params.to_string(),
                error: None,
                token_usage: None,
            })
        }
    }

    struct FileEchoTool;
    #[async_trait]
    impl Tool for FileEchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "file_read".to_string(),
                description: "File read echo".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: format!("Read: {}", params["path"].as_str().unwrap_or("")),
                error: None,
                token_usage: None,
            })
        }
    }

    #[tokio::test]
    async fn test_rate_limited_allows_within_limit() {
        let inner = Arc::new(EchoTool);
        let tool = RateLimitedTool::new(inner, 3);
        for _ in 0..3 {
            let result = tool
                .execute(serde_json::json!({"msg": "hi"}), None)
                .await
                .unwrap();
            assert!(result.ok);
        }
    }

    #[tokio::test]
    async fn test_rate_limited_blocks_over_limit() {
        let inner = Arc::new(EchoTool);
        let tool = RateLimitedTool::new(inner, 2);
        let _ = tool.execute(serde_json::json!({}), None).await;
        let _ = tool.execute(serde_json::json!({}), None).await;
        let result = tool.execute(serde_json::json!({}), None).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Rate limit"));
    }

    #[tokio::test]
    async fn test_path_guarded_allows_within_dir() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-workdir/file.txt" }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_path_guarded_blocks_outside_dir() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(serde_json::json!({ "path": "/etc/passwd" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("outside all allowed workspace directories")
        );
    }

    #[tokio::test]
    async fn test_path_guarded_relative_path() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(serde_json::json!({ "path": "subdir/file.txt" }), None)
            .await
            .unwrap();
        assert!(result.ok); // relative path resolved within allowed dir
    }

    #[tokio::test]
    async fn test_path_guarded_non_filesystem_tool() {
        let inner = Arc::new(EchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        // echo is not a filesystem tool, so no path check
        let result = tool
            .execute(serde_json::json!({ "path": "/etc/passwd" }), None)
            .await
            .unwrap();
        assert!(result.ok); // Not checked because not a filesystem tool
    }

    #[tokio::test]
    async fn test_path_guarded_blocks_traversal() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        // Path traversal via ".." resolves to /etc/passwd which is outside allowed dir
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-workdir/../../etc/passwd" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("outside all allowed workspace directories")
        );
    }

    #[tokio::test]
    async fn test_path_guarded_blocks_prefix_suffix_attack() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        // Prefix-suffix attack: "/tmp/agent-workdir-eval" starts with "/tmp/agent-workdir"
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-workdir-eval/secret" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("outside all allowed workspace directories")
        );
    }

    #[tokio::test]
    async fn test_readonly_allows_read() {
        // A file_read tool should be allowed in a ReadOnly directory
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-pkg".to_string(),
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/manifest.toml" }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_readonly_blocks_write() {
        // A file_write tool should be blocked in a ReadOnly directory
        struct FileWriteEchoTool;
        #[async_trait]
        impl Tool for FileWriteEchoTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_write".to_string(),
                    description: "File write echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                Ok(ToolResult {
                    ok: true,
                    content: format!("Wrote: {}", params["path"].as_str().unwrap_or("")),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let inner = Arc::new(FileWriteEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-pkg".to_string(),
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/manifest.toml" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn test_readonly_blocks_file_edit() {
        // A file_edit tool should be blocked in a ReadOnly directory
        struct FileEditEchoTool;
        #[async_trait]
        impl Tool for FileEditEchoTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_edit".to_string(),
                    description: "File edit echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                Ok(ToolResult {
                    ok: true,
                    content: format!("Edited: {}", params["path"].as_str().unwrap_or("")),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let inner = Arc::new(FileEditEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-pkg".to_string(),
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/prompts/system.md" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn test_nested_readwrite_overrides_readonly() {
        // When a ReadOnly parent and ReadWrite child both match,
        // the more specific (longest prefix) ReadWrite should win.
        // This simulates: package_root=ReadOnly, workspace=ReadWrite
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![
                WorkspaceDir {
                    id: "rw".to_string(),
                    path: "/tmp/agent-pkg".to_string(),
                    access: WorkspaceAccess::ReadOnly,
                    last_active: false,
                    prompt_file: None,
                },
                WorkspaceDir {
                    id: "ws".to_string(),
                    path: "/tmp/agent-pkg/workspace".to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                },
            ]),
        );
        // Read within workspace should succeed (ReadWrite wins)
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/workspace/file.txt" }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_nested_readonly_overrides_readwrite() {
        // When a ReadWrite parent and ReadOnly child both match,
        // the more specific (longest prefix) ReadOnly should win.
        let inner = Arc::new(FileEchoTool);
        let _tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![
                WorkspaceDir {
                    id: "rw".to_string(),
                    path: "/tmp/agent-pkg".to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                },
                WorkspaceDir {
                    id: "ro".to_string(),
                    path: "/tmp/agent-pkg/readonly".to_string(),
                    access: WorkspaceAccess::ReadOnly,
                    last_active: false,
                    prompt_file: None,
                },
            ]),
        );
        // A write tool should be blocked in the nested ReadOnly directory
        struct FileWriteEchoTool2;
        #[async_trait]
        impl Tool for FileWriteEchoTool2 {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_write".to_string(),
                    description: "File write echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                Ok(ToolResult {
                    ok: true,
                    content: format!("Wrote: {}", params["path"].as_str().unwrap_or("")),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let write_inner = Arc::new(FileWriteEchoTool2);
        let write_tool = PathGuardedTool::new(
            write_inner,
            test_resolver(vec![
                WorkspaceDir {
                    id: "rw".to_string(),
                    path: "/tmp/agent-pkg".to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                },
                WorkspaceDir {
                    id: "ro".to_string(),
                    path: "/tmp/agent-pkg/readonly".to_string(),
                    access: WorkspaceAccess::ReadOnly,
                    last_active: false,
                    prompt_file: None,
                },
            ]),
        );
        // Write within /tmp/agent-pkg/readonly should be blocked
        let result = write_tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/readonly/secret.txt" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("read-only"));
    }
}
