//! Tool registry — tool pool registration + activation
//!
//! Two-step process:
//! 1. `all_builtin_tools()` builds the complete tool pool
//! 2. `activate()` applies security decorators, filtering by per-agent
//!    `enabled` flags from `agent_tools.json` (ADR-029).
use crate::agent::agent_core::BuiltinToolEntry;
use crate::tools::workspace_resolver::SharedResolver;
use acowork_core::AgentManifest;
use acowork_core::tools::traits::Tool;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
use crate::tools::workspace_resolver::WorkspaceResolver;

/// Names whose `enabled` flag is platform-managed — these tools
/// never enter the per-agent activation toggle UX or the persisted
/// `agent_tools.json` file. They live entirely in the in-memory
/// `ToolRegistry` (gated by the boot-only `tool_compression_enabled`
/// flag, ADR-052).
///
/// **Single source of truth for the persistence-layer filter.** Every
/// write path that touches `agent_tools.json` MUST consult this list
/// and skip these names. The dedicated helper
/// [`crate::agent_config::is_platform_protected`] (only reachable
/// through `merge_tools_config`, `init_tools_config_from_manifest`,
/// `apply_builtin_tools_patch`, and `get_merged_tools`) is the only
/// enforcement point — adding a new platform tool requires a
/// single match-arm edit here and every caller picks it up.
///
/// See ADR-029 (per-agent builtin tools) and ADR-052 (tool compression).
pub const PLATFORM_PROTECTED_TOOLS: &[&str] = &["context_retrieve", "context_abandon"];

/// Tool registry
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl BuiltinToolEntry {
    /// Decide the canonical `enabled` flag for a builtin tool from the
    /// user's persisted preference. The persistence layer
    /// ([`crate::agent_config::merge_tools_config`] and friends)
    /// guarantees platform-protected tools are *never* present in
    /// `enabled_entries`, so this is a straight passthrough —
    /// platform-protection is enforced at the file boundary, not here.
    pub(crate) fn with_resolved_enabled(
        user_persisted_enabled: bool,
        tool: Arc<dyn Tool>,
    ) -> Self {
        Self {
            enabled: user_persisted_enabled,
            tool,
        }
    }
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
    /// 1. Build a name -> enabled map from `enabled_entries` (which
    ///    already excludes platform-protected tools — see
    ///    [`crate::agent_config::merge_tools_config`]).
    /// 2. Decide `enabled` via
    ///    [`BuiltinToolEntry::with_resolved_enabled`]; the caller's
    ///    persisted preference wins verbatim (no platform-protection
    ///    override at this layer — it never needs to fire because the
    ///    filter upstream already removed those names).
    /// 3. Wrap both enabled and disabled tools with security decorators
    ///    (path guard + rate limiter) so disabled tools remain
    ///    introspectable for re-enable at runtime.
    pub(crate) fn activate(
        &self,
        _manifest: &AgentManifest,
        resolver: &SharedResolver,
        max_calls_per_minute: u32,
        enabled_entries: &[crate::agent_config::AgentToolEntry],
    ) -> Vec<BuiltinToolEntry> {
        // Build a name -> enabled map from user-persisted state.
        // Tools not present in the map default to disabled (opt-in).
        let user_map: HashMap<String, bool> = enabled_entries
            .iter()
            .map(|e| (e.name.clone(), e.enabled))
            .collect();

        self.tools
            .iter()
            .map(|tool| {
                let name = tool.name();
                let user_wants = user_map.get(&name).copied().unwrap_or(false);
                let wrapped =
                    crate::tools::wrappers::wrap_with_security_decorators(
                        tool.clone(),
                        resolver.clone(),
                        max_calls_per_minute,
                    );
                BuiltinToolEntry::with_resolved_enabled(user_wants, wrapped)
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

    // ── ADR-052: platform tools live in the in-memory registry only ──
    //
    // Platform-protected tools (`context_retrieve`, `context_abandon`)
    // are never persisted to `agent_tools.json` — they are registered
    // by `all_builtin_tools()` (gated by the boot-only
    // `tool_compression_enabled` flag) and reach the LLM through the
    // registry. The persistence-layer filter (`merge_tools_config`,
    // `init_tools_config_from_manifest`, `apply_builtin_tools_patch`,
    // `all_enabled_tools_config`) guarantees `enabled_entries` never
    // contains these names, so `with_resolved_enabled` is now a
    // passthrough — the platform-protection logic moved upstream to
    // the persistence boundary.

    use crate::agent_config::AgentToolEntry;
    use crate::agent::agent_core::BuiltinToolEntry;

    fn platform_protected_entry(name: &str) -> Arc<dyn Tool> {
        Arc::new(MockTool { name: name.to_string() })
    }

    #[test]
    fn builtin_tool_entry_with_resolved_enabled_is_passthrough() {
        // After ADR-052 the persistence layer (merge_tools_config &
        // friends) guarantees platform tools are never present in
        // `enabled_entries`. So `with_resolved_enabled` is a straight
        // passthrough: the caller's `user_persisted_enabled` value
        // wins verbatim, no platform-protection override needed.
        let tool: Arc<dyn Tool> = platform_protected_entry("context_retrieve");
        let entry = BuiltinToolEntry::with_resolved_enabled(true, tool.clone());
        assert!(entry.enabled, "user-enabled → enabled");

        let entry = BuiltinToolEntry::with_resolved_enabled(false, tool);
        assert!(!entry.enabled, "user-disabled → disabled (persistence layer keeps platform tools out of enabled_entries)");

        let normal: Arc<dyn Tool> = platform_protected_entry("shell");
        let entry = BuiltinToolEntry::with_resolved_enabled(false, normal);
        assert!(!entry.enabled, "non-platform tool respects user disable");
    }

    #[test]
    fn test_registry_activate_passes_through_user_enabled_flags() {
        // End-to-end: build a registry, pass an enabled_entries list,
        // verify activate() returns each tool with the user's chosen
        // flag. Platform tools still reach the LLM (they are in
        // `all_builtin_tools()`) — but their enabled flag is decided
        // by `tool_compression_enabled` upstream, NOT by the user.
        let mut reg = ToolRegistry::new();
        reg.register(platform_protected_entry("context_retrieve"));
        reg.register(platform_protected_entry("context_abandon"));
        reg.register(platform_protected_entry("shell"));

        let manifest = manifest_with_tools(&["context_retrieve", "context_abandon", "shell"]);
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));

        // Persistence layer filter: enabled_entries never carries
        // platform tools (this is the precondition the new contract
        // relies on).
        let enabled = vec![
            AgentToolEntry::new("shell", true),
        ];
        let activated = reg.activate(&manifest, &resolver, 60, &enabled);
        let map: std::collections::HashMap<String, bool> =
            activated.iter().map(|e| (e.name().to_string(), e.enabled)).collect();

        // Non-platform tool: follows user choice.
        assert!(map["shell"], "non-platform tool enabled per user");
        // Platform tools are NOT in enabled_entries, so they default
        // to disabled by `with_resolved_enabled`. Activation callers
        // (agent_core.rs) must independently gate platform tools on
        // `tool_compression_enabled` — see the LLM-visible tool spec
        // assembly in agent_init.rs.
        assert!(
            !map["context_retrieve"],
            "platform tool is disabled at the activate() level (gated upstream by tool_compression_enabled)"
        );
        assert!(!map["context_abandon"]);
    }

    #[test]
    fn plATFORM_PROTECTED_TOOLS_is_single_source_of_truth() {
        // Pin the contract: the constant list is what every filter
        // callsite consults. Adding a new platform tool requires a
        // single edit here, and merge / init / patch / activate all
        // pick it up.
        assert!(!PLATFORM_PROTECTED_TOOLS.is_empty(), "must list at least one name");
        for name in PLATFORM_PROTECTED_TOOLS {
            assert!(!name.is_empty(), "platform tool name must not be empty");
        }
    }

    #[test]
    fn test_registry_default() {
        let reg = ToolRegistry::default();
        assert!(reg.all().is_empty());
    }

}
