//! Tool registry — tool pool registration + manifest-driven activation
//!
//! Two-step process:
//! 1. `all_builtin_tools()` builds the complete tool pool
//! 2. `activate()` filters by manifest declarations and applies security decorators

use rollball_core::tools::traits::Tool;
use rollball_core::AgentManifest;
use std::sync::Arc;

use crate::tools::wrappers::{PathGuardedTool, PermissionCheckedTool, RateLimitedTool, WorkspaceDir, WorkspaceAccess};

/// Tool registry
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create new empty registry
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Get tool by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Get all registered tools
    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Activate tools based on manifest declarations.
    ///
    /// Steps:
    /// 1. If manifest.tools is non-empty, filter to only declared tools
    /// 2. Load workspace directories from `.agent_workspaces.json`
    /// 3. Apply security decorators: PermissionChecked → PathGuarded → RateLimited
    pub fn activate(
        &self,
        manifest: &AgentManifest,
        work_dir: &str,
        max_calls_per_minute: u32,
    ) -> Vec<Arc<dyn Tool>> {
        let filtered: Vec<Arc<dyn Tool>> = if manifest.tools.is_empty() {
            // No tool declarations → all tools available
            self.tools.clone()
        } else {
            // Filter to only declared tools
            self.tools
                .iter()
                .filter(|tool| manifest.has_tool(&tool.name()))
                .cloned()
                .collect()
        };

        // Load workspace directories from .agent_workspaces.json
        let allowed_dirs = load_workspace_dirs(work_dir);

        // Apply security decorators
        filtered
            .into_iter()
            .map(|tool| {
                // Layer 1: Permission check
                let perm_checked = Arc::new(PermissionCheckedTool::new(
                    tool.clone(),
                    manifest.clone(),
                )) as Arc<dyn Tool>;

                // Layer 2: Path guard (for filesystem tools)
                let path_guarded = Arc::new(PathGuardedTool::new(perm_checked, allowed_dirs.clone())) as Arc<dyn Tool>;

                // Layer 3: Rate limit
                Arc::new(RateLimitedTool::new(path_guarded, max_calls_per_minute)) as Arc<dyn Tool>
            })
            .collect()
    }

    /// List all registered tool names
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name()).collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rollball_core::tools::traits::{ToolResult, ToolSpec};
    use serde_json::Value;

    struct MockTool {
        name: String,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: format!("Mock tool {}", self.name),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _params: Value) -> rollball_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: format!("Mock {} executed", self.name),
                error: None,
                token_usage: None,
            })
        }
    }

    fn create_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(MockTool {
            name: "shell".to_string(),
        }));
        reg.register(Arc::new(MockTool {
            name: "calculator".to_string(),
        }));
        reg.register(Arc::new(MockTool {
            name: "weather".to_string(),
        }));
        reg.register(Arc::new(MockTool {
            name: "memory_store".to_string(),
        }));
        reg
    }

    fn manifest_with_tools(tool_names: &[&str]) -> AgentManifest {
        let tools_toml = tool_names
            .iter()
            .map(|name| format!("[[tools]]\nname = \"{}\"", name))
            .collect::<Vec<_>>()
            .join("\n");

        let toml_str = format!(
            r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Shell"

            [[permissions]]
            type = "Network"

            [[permissions]]
            type = "MemoryWrite"

            {}
            "#,
            tools_toml
        );
        AgentManifest::from_toml(&toml_str).unwrap()
    }

    #[test]
    fn test_registry_register_and_get() {
        let reg = create_registry();
        assert!(reg.get("shell").is_some());
        assert!(reg.get("calculator").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_tool_names() {
        let reg = create_registry();
        let names = reg.tool_names();
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"calculator".to_string()));
    }

    #[test]
    fn test_registry_activate_with_manifest_tools() {
        let reg = create_registry();
        let manifest = manifest_with_tools(&["shell", "calculator"]);
        let activated = reg.activate(&manifest, "/tmp/work", 60);
        assert_eq!(activated.len(), 2);
        let names: Vec<String> = activated.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"calculator".to_string()));
        assert!(!names.contains(&"weather".to_string()));
    }

    #[test]
    fn test_registry_activate_no_manifest_tools() {
        let reg = create_registry();
        let toml_str = r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        let activated = reg.activate(&manifest, "/tmp/work", 60);
        assert_eq!(activated.len(), 4); // All tools available
    }

    #[test]
    fn test_registry_default() {
        let reg = ToolRegistry::default();
        assert!(reg.all().is_empty());
    }
}

/// Load workspace directories from `.agent_workspaces.json`
///
/// Returns the configured workspace directories, or falls back to the agent's
/// work_dir if no workspace config exists.
fn load_workspace_dirs(work_dir: &str) -> Vec<WorkspaceDir> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct WorkspaceConfig {
        version: String,
        #[serde(default)]
        additional_dirs: Vec<WorkspaceDirEntry>,
    }

    #[derive(Deserialize)]
    struct WorkspaceDirEntry {
        id: String,
        path: String,
        alias: Option<String>,
        access: String,
        added_at: String,
    }

    let config_path = std::path::Path::new(work_dir)
        .join(".agent_workspaces.json");

    if !config_path.exists() {
        // Fallback: use work_dir as the only allowed directory (read-write)
        tracing::warn!(
            work_dir,
            config_path = %config_path.display(),
            "No .agent_workspaces.json found, using work_dir as default"
        );
        return vec![WorkspaceDir {
            path: work_dir.to_string(),
            access: WorkspaceAccess::ReadWrite,
        }];
    }

    match std::fs::read_to_string(&config_path) {
        Ok(content) => match serde_json::from_str::<WorkspaceConfig>(&content) {
            Ok(config) => {
                let mut dirs: Vec<WorkspaceDir> = config
                    .additional_dirs
                    .into_iter()
                    .map(|entry| WorkspaceDir {
                        path: entry.path,
                        access: if entry.access == "read-write" {
                            WorkspaceAccess::ReadWrite
                        } else {
                            WorkspaceAccess::ReadOnly
                        },
                    })
                    .collect();

                // Always include work_dir as the base directory
                dirs.push(WorkspaceDir {
                    path: work_dir.to_string(),
                    access: WorkspaceAccess::ReadWrite,
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
                vec![WorkspaceDir {
                    path: work_dir.to_string(),
                    access: WorkspaceAccess::ReadWrite,
                }]
            }
        },
        Err(e) => {
            tracing::error!(
                work_dir,
                error = %e,
                "Failed to read .agent_workspaces.json, using work_dir as default"
            );
            vec![WorkspaceDir {
                path: work_dir.to_string(),
                access: WorkspaceAccess::ReadWrite,
            }]
        }
    }
}
