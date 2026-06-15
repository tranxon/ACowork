//! Tool registry — tool pool registration + activation
//!
//! Two-step process:
//! 1. `all_builtin_tools()` builds the complete tool pool
//! 2. `activate()` applies security decorators (all builtin tools are always active)
use crate::tools::workspace_resolver::SharedResolver;
use crate::tools::wrappers::{PathGuardedTool, RateLimitedTool};
use acowork_core::AgentManifest;
use acowork_core::tools::traits::Tool;
use std::sync::Arc;

#[cfg(test)]
use crate::tools::workspace_resolver::{WorkspaceAccess, WorkspaceDir, WorkspaceResolver};

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

    /// Activate all registered tools with security decorators.
    ///
    /// All builtin tools are always active — manifest `[[tools]]` is reserved
    /// for future scope restriction, not activation filtering.
    ///
    /// Tool activation IS authorization — no separate permission check needed.
    ///
    /// Steps:
    /// 1. Load workspace directories from the shared resolver
    /// 2. Apply security decorators: PathGuarded → RateLimited
    pub fn activate(
        &self,
        _manifest: &AgentManifest,
        resolver: &SharedResolver,
        max_calls_per_minute: u32,
    ) -> Vec<Arc<dyn Tool>> {
        // Use the shared workspace resolver (single source of truth for directory resolution)
        let allowed_dirs = resolver.read().unwrap().allowed_dirs().to_vec();

        // Apply security decorators to all registered tools
        self.tools
            .iter()
            .map(|tool| {
                // Layer 1: Path guard (for filesystem tools)
                let path_guarded =
                    Arc::new(PathGuardedTool::new(tool.clone(), allowed_dirs.clone()))
                        as Arc<dyn Tool>;

                // Layer 2: Rate limit
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

#[allow(clippy::items_after_test_module)]
#[cfg(test)]

mod tests {

    use super::*;
    use acowork_core::tools::traits::{ToolResult, ToolSpec};
    use async_trait::async_trait;
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

        async fn execute(
            &self,
            _params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
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
    fn test_registry_activate_returns_all_tools() {
        let reg = create_registry();
        let manifest = manifest_with_tools(&["shell", "calculator"]);
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));
        let activated = reg.activate(&manifest, &resolver, 60);
        // All tools are always active, regardless of manifest
        assert_eq!(activated.len(), 4);
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
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));
        let activated = reg.activate(&manifest, &resolver, 60);
        assert_eq!(activated.len(), 4); // All tools available
    }

    #[test]
    fn test_registry_default() {
        let reg = ToolRegistry::default();
        assert!(reg.all().is_empty());
    }
}
