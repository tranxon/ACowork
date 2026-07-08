//! Tool registry — tool pool registration + activation
//!
//! Two-step process:
//! 1. `all_builtin_tools()` builds the complete tool pool
//! 2. `activate()` applies security decorators, filtering by per-agent
//!    `enabled` flags from `agent_tools.json` (ADR-029).
use crate::agent::agent_core::BuiltinToolEntry;
use crate::tools::workspace_resolver::SharedResolver;
use crate::tools::wrappers::{PathGuardedTool, RateLimitedTool};
use acowork_core::AgentManifest;
use acowork_core::tools::traits::Tool;
use std::collections::HashSet;
use std::sync::Arc;

#[cfg(test)]
use crate::tools::workspace_resolver::WorkspaceResolver;

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

    /// Register a tool from an external source (e.g. a SidecarEndpointUpdate
    /// that just told us the LSP relay is now available). If a tool with the
    /// same `name()` is already registered, it is **replaced** in place —
    /// this is the canonical semantics for hot-push updates: the most recent
    /// endpoint wins, no duplicates.
    ///
    /// Returns `true` if a pre-existing tool was replaced, `false` if a new
    /// entry was appended.
    pub fn register_external(&mut self, tool: Arc<dyn Tool>) -> bool {
        let name = tool.name().to_string();
        if let Some(existing) = self.tools.iter().position(|t| t.name() == name) {
            self.tools[existing] = tool;
            tracing::info!(tool = %name, "ToolRegistry: replaced existing tool");
            true
        } else {
            self.tools.push(tool);
            tracing::info!(tool = %name, "ToolRegistry: added new tool");
            false
        }
    }

    /// Remove a tool by name. Returns `true` if a tool was actually removed.
    /// No-op if no tool with that name is present.
    pub fn unregister(&mut self, name: &str) -> bool {
        let before = self.tools.len();
        self.tools.retain(|t| t.name() != name);
        let removed = self.tools.len() < before;
        if removed {
            tracing::info!(tool = %name, "ToolRegistry: removed tool");
        }
        removed
    }

    /// Get tool by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Get all registered tools (regardless of `enabled` flag)
    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// List all registered tool names (unconditional, regardless of enabled)
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Activate tools with security decorators, filtering by per-agent
    /// `enabled` flags. The return value is `Vec<BuiltinToolEntry>`
    /// — both for direct LLM dispatch (filter `enabled` again to get
    /// `Vec<Arc<dyn Tool>>`) and for storing in
    /// `AgentCore.builtin_tools`.
    ///
    /// Tools NOT in the registry but listed in `enabled_entries` are
    /// silently skipped (defensive — see ADR-029 §"Initialization").
    ///
    /// Steps:
    /// 1. Look up the `enabled` flag for every registered tool
    /// 2. Wrap disabled ones with security decorators anyway (so the
    ///    frontend can introspect / so future toggles are cheap) but
    ///    mark them as `enabled = false`
    /// 3. Caller's `AgentCore::rebuild_all_tools` will filter when
    ///    handing the list to the LLM
    pub(crate) fn activate(
        &self,
        _manifest: &AgentManifest,
        resolver: &SharedResolver,
        max_calls_per_minute: u32,
        enabled_entries: &[crate::agent_config::AgentToolEntry],
    ) -> Vec<BuiltinToolEntry> {
        let enabled_set: HashSet<String> = enabled_entries
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.name.clone())
            .collect();

        // Pass the shared resolver directly to PathGuardedTool so it reads
        // workspace directories fresh from the global source of truth on every
        // execute() call. This ensures hot-reload of workspace access changes
        // takes effect immediately without agent restart.
        self.tools
            .iter()
            .map(|tool| {
                let is_enabled = enabled_set.contains(&tool.name());

                if is_enabled {
                    // Layer 1: Path guard (for filesystem tools)
                    let path_guarded =
                        Arc::new(PathGuardedTool::new(tool.clone(), resolver.clone()))
                            as Arc<dyn Tool>;

                    // Layer 2: Rate limit
                    let rate_limited = Arc::new(RateLimitedTool::new(
                        path_guarded,
                        max_calls_per_minute,
                    )) as Arc<dyn Tool>;
                    BuiltinToolEntry {
                        tool: rate_limited,
                        enabled: true,
                    }
                } else {
                    // Disabled tools still get the security decorators so
                    // they remain introspectable for re-enable at runtime.
                    let path_guarded =
                        Arc::new(PathGuardedTool::new(tool.clone(), resolver.clone()))
                            as Arc<dyn Tool>;
                    let rate_limited = Arc::new(RateLimitedTool::new(
                        path_guarded,
                        max_calls_per_minute,
                    )) as Arc<dyn Tool>;
                    BuiltinToolEntry {
                        tool: rate_limited,
                        enabled: false,
                    }
                }
            })
            .collect()
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
        let enabled = vec![
            crate::agent_config::AgentToolEntry::new("shell", true),
            crate::agent_config::AgentToolEntry::new("calculator", true),
            crate::agent_config::AgentToolEntry::new("weather", true),
            crate::agent_config::AgentToolEntry::new("memory_store", true),
        ];
        let activated = reg.activate(&manifest, &resolver, 60, &enabled);
        assert_eq!(activated.len(), 4);
        // All enabled
        assert!(activated.iter().all(|e| e.enabled));
    }

    #[test]
    fn test_registry_activate_filters_by_enabled() {
        let reg = create_registry();
        let manifest = manifest_with_tools(&["shell", "calculator"]);
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));
        // Only enable two tools
        let enabled = vec![
            crate::agent_config::AgentToolEntry::new("shell", true),
            crate::agent_config::AgentToolEntry::new("weather", true),
            crate::agent_config::AgentToolEntry::new("calculator", false),
            crate::agent_config::AgentToolEntry::new("memory_store", false),
        ];
        let activated = reg.activate(&manifest, &resolver, 60, &enabled);
        assert_eq!(activated.len(), 4, "all 4 are returned (disabled ones kept for introspection)");
        let shell = activated.iter().find(|e| e.name() == "shell").unwrap();
        let calc  = activated.iter().find(|e| e.name() == "calculator").unwrap();
        assert!(shell.enabled);
        assert!(!calc.enabled);
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
        // No enabled_entries → all default to false (opt-in semantics)
        let activated = reg.activate(&manifest, &resolver, 60, &[]);
        assert_eq!(activated.len(), 4);
        assert!(activated.iter().all(|e| !e.enabled));
    }

    #[test]
    fn test_registry_default() {
        let reg = ToolRegistry::default();
        assert!(reg.all().is_empty());
    }

    // ── ADR-030 C3: register_external / unregister (hot-push API) ──────

    #[test]
    fn test_registry_register_external_appends_new() {
        let mut reg = ToolRegistry::new();
        let replaced = reg.register_external(Arc::new(MockTool {
            name: "codebase".to_string(),
        }));
        assert!(!replaced, "no pre-existing tool → returns false");
        assert_eq!(reg.all().len(), 1);
        assert!(reg.get("codebase").is_some());
    }

    #[test]
    fn test_registry_register_external_replaces_same_name() {
        let mut reg = ToolRegistry::new();
        reg.register_external(Arc::new(MockTool {
            name: "codebase".to_string(),
        }));
        let replaced = reg.register_external(Arc::new(MockTool {
            name: "codebase".to_string(),
        }));
        assert!(replaced, "same-name registration → returns true");
        // No duplicates — still exactly one entry.
        assert_eq!(reg.all().len(), 1);
        let names: Vec<String> = reg.tool_names();
        let count = names.iter().filter(|n| n.as_str() == "codebase").count();
        assert_eq!(count, 1, "codebase must appear exactly once after replace");
    }

    #[test]
    fn test_registry_unregister_removes_and_returns_flag() {
        let mut reg = ToolRegistry::new();
        reg.register_external(Arc::new(MockTool {
            name: "codebase".to_string(),
        }));
        reg.register_external(Arc::new(MockTool {
            name: "shell".to_string(),
        }));

        assert!(reg.unregister("codebase"));
        assert!(reg.get("codebase").is_none());
        assert!(reg.get("shell").is_some());

        // Idempotent: removing a non-existent tool is a no-op that returns false.
        assert!(!reg.unregister("codebase"));
        assert!(!reg.unregister("never_registered"));
    }
}
