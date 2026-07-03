//! Language-aware project root discovery.
//!
//! Given a file path and language, walks up the directory tree to find
//! the nearest directory containing a language-specific marker file
//! (tsconfig.json, Cargo.toml, go.mod, etc.). Falls back to the
//! workspace root if no marker is found.
//!
//! This is essential for multi-language monorepos where the workspace
//! root (e.g. a Rust + TypeScript monorepo) does not contain language-
//! specific project files. Without project root discovery, the LSP
//! `rootUri` would point to the monorepo root, and language servers
//! like tsserver would create a default project without the correct
//! `moduleResolution` setting, causing false "Cannot find module"
//! diagnostics.

use std::path::Path;

/// Discover the project root for a given file and language.
///
/// Reads `root_markers` from the LSP server config (`lsp_servers.json`).
/// If the language has no root markers (rootless language), returns
/// `workspace_root` as-is.
///
/// # Arguments
///
/// * `file_path` - Absolute path of the file being opened (e.g.
///   `/Users/foo/project/src/main.ts`).
/// * `language` - Language id (e.g. "typescript", "rust"). Aliases
///   like "js" are canonicalized via [`canonical_language`].
/// * `workspace_root` - Monorepo root (upper bound for the search).
///
/// # Returns
///
/// The project root directory (absolute path). If no marker file is
/// found between the file's directory and the workspace root
/// (inclusive), falls back to `workspace_root`.
pub fn discover_project_root(
    file_path: &str,
    language: &str,
    workspace_root: &str,
) -> String {
    let cfg = crate::config::lsp_servers_config();
    let canonical = crate::config::canonical_language(language);
    let entry = match cfg.servers.get(canonical) {
        Some(e) => e,
        None => return workspace_root.to_string(),
    };

    if entry.root_markers.is_empty() {
        return workspace_root.to_string();
    }

    let ws_root = Path::new(workspace_root);
    let file_dir = Path::new(file_path)
        .parent()
        .unwrap_or(ws_root);

    let mut current = file_dir;
    loop {
        for marker in &entry.root_markers {
            if current.join(marker).is_file() {
                let root = current.to_string_lossy().into_owned();
                tracing::debug!(
                    "[project_root] Found project root for '{}' at '{}' \
                     (marker: '{}', file: '{}')",
                    language,
                    root,
                    marker,
                    file_path
                );
                return root;
            }
        }

        if current == ws_root {
            break;
        }

        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    tracing::debug!(
        "[project_root] No marker found for '{}' between '{}' and '{}', \
         falling back to workspace root",
        language,
        file_dir.display(),
        workspace_root
    );

    workspace_root.to_string()
}

/// Extract a file system path from LSP request params.
///
/// LSP `textDocument/*` methods include a `textDocument.uri` field
/// (e.g. `file:///Users/foo/project/src/file.ts`). This function
/// parses the URI and returns the file system path.
///
/// Returns `None` if the params do not contain a `textDocument.uri`
/// field, or if the URI is not a `file://` URI.
pub fn extract_file_path_from_params(params: &serde_json::Value) -> Option<String> {
    let uri = params
        .get("textDocument")?
        .get("uri")?
        .as_str()?;

    // Convert file:// URI to filesystem path.
    // On Windows: file:///C:/... -> C:/...
    // On Unix:    file:///Users/... -> /Users/...
    let path = uri.strip_prefix("file://")?;
    Some(path.to_string())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Create a directory structure with marker files for testing.
    fn setup_test_tree() -> tempfile::TempDir {
        let tmp = tempdir().expect("tempdir");
        // Create a fake monorepo structure:
        // tmp/
        //   tsconfig.json          (root-level, should NOT be used)
        //   apps/
        //     web/
        //       tsconfig.json      (project root for web files)
        //       src/
        //         component.tsx
        //     server/
        //       go.mod             (project root for server files)
        //       main.go
        //   core/
        //     Cargo.toml           (project root for rust files)
        //     src/
        //       main.rs
        let root = tmp.path();
        fs::write(root.join("tsconfig.json"), "{}").expect("write");
        fs::create_dir_all(root.join("apps/web/src")).expect("mkdir");
        fs::write(root.join("apps/web/tsconfig.json"), "{}").expect("write");
        fs::write(root.join("apps/web/src/component.tsx"), "").expect("write");
        fs::create_dir_all(root.join("apps/server")).expect("mkdir");
        fs::write(root.join("apps/server/go.mod"), "module test").expect("write");
        fs::write(root.join("apps/server/main.go"), "package main").expect("write");
        fs::create_dir_all(root.join("core/src")).expect("mkdir");
        fs::write(root.join("core/Cargo.toml"), "[package]").expect("write");
        fs::write(root.join("core/src/main.rs"), "fn main() {}").expect("write");
        tmp
    }

    #[test]
    fn test_discover_typescript_project_root() {
        let tmp = setup_test_tree();
        let root = tmp.path().to_str().unwrap();
        let file = format!("{root}/apps/web/src/component.tsx");
        let result = discover_project_root(&file, "typescript", root);
        assert_eq!(result, format!("{root}/apps/web"));
    }

    #[test]
    fn test_discover_go_project_root() {
        let tmp = setup_test_tree();
        let root = tmp.path().to_str().unwrap();
        let file = format!("{root}/apps/server/main.go");
        let result = discover_project_root(&file, "go", root);
        assert_eq!(result, format!("{root}/apps/server"));
    }

    #[test]
    fn test_discover_rust_project_root() {
        let tmp = setup_test_tree();
        let root = tmp.path().to_str().unwrap();
        let file = format!("{root}/core/src/main.rs");
        let result = discover_project_root(&file, "rust", root);
        assert_eq!(result, format!("{root}/core"));
    }

    #[test]
    fn test_rootless_language_returns_workspace_root() {
        let tmp = setup_test_tree();
        let root = tmp.path().to_str().unwrap();
        let file = format!("{root}/apps/web/src/component.tsx");
        // JSON has no root_markers — should return workspace root.
        let result = discover_project_root(&file, "json", root);
        assert_eq!(result, root);
    }

    #[test]
    fn test_no_marker_found_falls_back_to_workspace_root() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path().to_str().unwrap();
        fs::create_dir_all(tmp.path().join("subdir")).expect("mkdir");
        let file = format!("{root}/subdir/file.ts");
        // No tsconfig.json anywhere — should fall back to workspace root.
        let result = discover_project_root(&file, "typescript", root);
        assert_eq!(result, root);
    }

    #[test]
    fn test_unknown_language_returns_workspace_root() {
        let result = discover_project_root("/tmp/file.xyz", "brainfuck", "/tmp");
        assert_eq!(result, "/tmp");
    }

    #[test]
    fn test_language_alias_canonicalized() {
        let tmp = setup_test_tree();
        let root = tmp.path().to_str().unwrap();
        let file = format!("{root}/apps/web/src/component.tsx");
        // "javascript" is an alias for "typescript".
        let result = discover_project_root(&file, "javascript", root);
        assert_eq!(result, format!("{root}/apps/web"));
    }

    #[test]
    fn test_extract_file_path_from_text_document() {
        let params = serde_json::json!({
            "textDocument": {
                "uri": "file:///Users/foo/project/src/file.ts"
            }
        });
        let path = extract_file_path_from_params(&params);
        assert_eq!(path, Some("/Users/foo/project/src/file.ts".to_string()));
    }

    #[test]
    fn test_extract_file_path_from_missing_text_document() {
        let params = serde_json::json!({
            "position": { "line": 0, "character": 0 }
        });
        let path = extract_file_path_from_params(&params);
        assert_eq!(path, None);
    }

    #[test]
    fn test_extract_file_path_from_non_file_uri() {
        let params = serde_json::json!({
            "textDocument": {
                "uri": "https://example.com/file.ts"
            }
        });
        let path = extract_file_path_from_params(&params);
        assert_eq!(path, None);
    }
}
