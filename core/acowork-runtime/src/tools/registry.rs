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
/// `ToolRegistry` (ADR-061 §10.2: `context_retrieve` is always
/// registered, `context_abandon` is not).
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
    /// Decide the canonical `enabled` flag for a builtin tool.
    ///
    /// **Platform-protected tools** (see [`PLATFORM_PROTECTED_TOOLS`]) are
    /// unconditionally enabled here — even though the persistence
    /// layer ([`crate::agent_config::merge_tools_config`] and friends)
    /// strips them from `enabled_entries`, they ARE registered in the
    /// in-memory registry (by [`crate::tools::builtin::all_builtin_tools`],
    /// ADR-061 §10.2: `context_retrieve` always registered) and MUST
    /// reach the LLM when registered. Without this force-enable, the
    /// registry layer's default `user_persisted_enabled = false`
    /// would hide them from `tool_specs` (which filters on `enabled`).
    ///
    /// **Non-platform tools** follow the user's persisted preference
    /// verbatim — there's no second override at this layer.
    ///
    /// See ADR-029 (per-agent builtin tools) and ADR-052 (tool
    /// compression + the platform-protected tool set).
    pub(crate) fn with_resolved_enabled(
        user_persisted_enabled: bool,
        tool: Arc<dyn Tool>,
    ) -> Self {
        let name = tool.name();
        let enabled = PLATFORM_PROTECTED_TOOLS.contains(&name.as_str())
            || user_persisted_enabled;
        Self { enabled, tool }
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
    // by `all_builtin_tools()` (ADR-061 §10.2: `context_retrieve`
    // always registered, `context_abandon` not) and reach the LLM
    // through the registry. The persistence-layer filter
    // (`merge_tools_config`, `init_tools_config_from_manifest`,
    // `apply_builtin_tools_patch`, `all_enabled_tools_config`) keeps
    // them out of `enabled_entries`, but the registry still needs to
    // surface them as `enabled=true` so the LLM-visible `tool_specs`
    // list picks them up. That is what `with_resolved_enabled` does:
    // for platform tools it ignores `user_persisted_enabled` and
    // force-enables, so the absence of an entry in `enabled_entries`
    // doesn't accidentally hide the tool from the LLM.

    use crate::agent_config::AgentToolEntry;
    use crate::agent::agent_core::BuiltinToolEntry;

    fn platform_protected_entry(name: &str) -> Arc<dyn Tool> {
        Arc::new(MockTool { name: name.to_string() })
    }

    #[test]
    fn builtin_tool_entry_force_enables_platform_tools() {
        // `with_resolved_enabled` is the ONLY registry-level enforcement
        // for platform-protection at activation time. Persistence-layer
        // filtering strips these names from `enabled_entries`, so
        // `user_persisted_enabled` will be `false` for them in practice
        // — the force-enable here is what keeps them visible to the LLM.
        let platform_tool: Arc<dyn Tool> = platform_protected_entry("context_retrieve");
        let entry = BuiltinToolEntry::with_resolved_enabled(false, platform_tool);
        assert!(entry.enabled, "platform tool must be force-enabled even when user said false");

        let normal_tool: Arc<dyn Tool> = platform_protected_entry("shell");
        let entry = BuiltinToolEntry::with_resolved_enabled(false, normal_tool.clone());
        assert!(!entry.enabled, "non-platform tool respects user disable");
        let entry = BuiltinToolEntry::with_resolved_enabled(true, normal_tool);
        assert!(entry.enabled, "non-platform tool respects user enable");
    }

    #[test]
    fn builtin_tool_entry_force_enables_all_listed_names() {
        // Iterate over the full list so every platform-protected name
        // is covered. Adding a new platform tool without updating this
        // list will leave a hole — caught at code review.
        for &name in PLATFORM_PROTECTED_TOOLS {
            let tool: Arc<dyn Tool> = platform_protected_entry(name);
            let entry = BuiltinToolEntry::with_resolved_enabled(false, tool);
            assert!(entry.enabled, "PLATFORM_PROTECTED_TOOLS member '{}' must be force-enabled", name);
        }
    }

    #[test]
    fn test_registry_activate_force_enables_platform_tools_even_when_absent_from_enabled_entries() {
        // Regression for the user report: after the persistence-layer
        // filter was added, platform tools stopped reaching the LLM
        // because `resolved_entries` no longer carries them and
        // `with_resolved_enabled` defaulted to `false`. The
        // force-enable here restores the contract: any tool whose
        // name is in PLATFORM_PROTECTED_TOOLS must surface as
        // `enabled=true` regardless of `enabled_entries`.
        let mut reg = ToolRegistry::new();
        reg.register(platform_protected_entry("context_retrieve"));
        reg.register(platform_protected_entry("context_abandon"));
        reg.register(platform_protected_entry("shell"));

        let manifest = manifest_with_tools(&["context_retrieve", "context_abandon", "shell"]);
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));

        // enabled_entries deliberately omits platform tools — this is
        // exactly what merge_tools_config produces (commit 145b9104).
        let enabled = vec![AgentToolEntry::new("shell", true)];
        let activated = reg.activate(&manifest, &resolver, 60, &enabled);
        let map: std::collections::HashMap<String, bool> =
            activated.iter().map(|e| (e.name().to_string(), e.enabled)).collect();

        assert!(map["context_retrieve"], "platform tool must be force-enabled even when not in enabled_entries");
        assert!(map["context_abandon"], "platform tool must be force-enabled even when not in enabled_entries");
        assert!(map["shell"], "non-platform tool follows user enable");
    }

    #[test]
    #[allow(non_snake_case)]
    // Deliberately uppercase to pin the contract to the
    // PLATFORM_PROTECTED_TOOLS constant name.
    fn plATFORM_PROTECTED_TOOLS_is_single_source_of_truth() {
        // Pin the contract: the constant list is what every filter
        // callsite (persistence layer + registry force-enable)
        // consults. Adding a new platform tool requires a single edit
        // here, and merge / init / patch / activate / force-enable all
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
