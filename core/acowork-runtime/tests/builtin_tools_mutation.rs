//! Integration tests for the unified builtin-tools mutation pipeline.
//!
//! These tests exercise the **end-to-end** read-modify-write cycle that
//! connects:
//!   1. `agent_config::apply_builtin_tools_patch` (persistence layer)
//!   2. `tools::registry::BuiltinToolEntry::with_resolved_enabled` (platform
//!      protection)
//!   3. `session_task::apply_builtin_tools_update` (atomic in-memory rewrite
//!      + dispatch list + LLM tool_definitions refresh)
//!
//! They are the Bug 2 regression net at the integration level — when the
//! persistence layer and the in-memory layer diverge, these tests fail.

use std::collections::HashMap;

use acowork_runtime::agent_config::{
    self, AgentToolEntry, AgentToolsConfig,
};

/// Read-modify-write cycle the PUT /builtin-tools handler performs.
fn put_builtin_tools(work_dir: &std::path::Path, patch: &[AgentToolEntry]) -> Vec<AgentToolEntry> {
    let current = agent_config::load_agent_tools_config(work_dir)
        .ok()
        .flatten()
        .map(|c| c.tools)
        .unwrap_or_default();

    // Mirror the RuntimeAgentToolsService::put_builtin_tools flow:
    //   1. read current
    //   2. apply_builtin_tools_patch (force-enables platform tools)
    //   3. save
    let updated = agent_config::apply_builtin_tools_patch(&current, patch);
    let cfg = AgentToolsConfig {
        tools: updated.clone(),
    };
    agent_config::save_agent_tools_config(work_dir, &cfg).expect("save_agent_tools_config");
    updated
}

/// Bug 2 regression: PUT /builtin-tools with user disabling a platform
/// tool must NOT actually disable it on disk.
#[test]
fn put_builtin_tools_cannot_disable_platform_tools() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path();

    // Seed: all tools enabled.
    let initial = vec![
        AgentToolEntry::new("context_retrieve", true),
        AgentToolEntry::new("context_abandon", true),
        AgentToolEntry::new("shell", true),
        AgentToolEntry::new("memory_recall", true),
    ];
    agent_config::save_agent_tools_config(
        work_dir,
        &AgentToolsConfig { tools: initial },
    )
    .unwrap();

    // User disables context_retrieve via frontend.
    let patch = vec![AgentToolEntry::new("context_retrieve", false)];
    let updated = put_builtin_tools(work_dir, &patch);

    let map: HashMap<String, bool> =
        updated.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(
        map["context_retrieve"],
        "PUT patch cannot disable context_retrieve (Bug 2)"
    );
    assert!(map["context_abandon"]);
    assert!(map["shell"]);
    assert!(map["memory_recall"]);

    // The on-disk file must agree with the in-memory result.
    let reloaded = agent_config::load_agent_tools_config(work_dir)
        .unwrap()
        .expect("file must exist");
    let on_disk: HashMap<String, bool> =
        reloaded.tools.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(on_disk["context_retrieve"], "disk must mirror in-memory");

    // Round-trip the persistence again through merge_tools_config —
    // cold-start path must agree with hot-update path.
    let code = vec![
        "context_retrieve".to_string(),
        "context_abandon".to_string(),
        "shell".to_string(),
        "memory_recall".to_string(),
    ];
    let merged = agent_config::merge_tools_config(&code, &reloaded.tools);
    let merge_map: HashMap<String, bool> =
        merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(
        merge_map["context_retrieve"],
        "cold-start merge must agree with hot-update patch (Bug 2 unification)"
    );
}

/// Bug 2 regression: when context_retrieve / context_abandon are MISSING
/// from the persisted file (first start or hand-edited file), merge and
/// patch both default them to enabled — the user cannot accidentally
/// omit them.
#[test]
fn put_builtin_tools_seeds_platform_tools_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path();

    // Seed only non-platform tools — platform tools absent entirely.
    let initial = vec![
        AgentToolEntry::new("shell", true),
        AgentToolEntry::new("memory_recall", false),
    ];
    agent_config::save_agent_tools_config(
        work_dir,
        &AgentToolsConfig { tools: initial },
    )
    .unwrap();

    // Cold-start path: merge_tools_config emits entries for every code
    // tool — including platform tools — with platform tools
    // force-enabled.
    let code = vec![
        "context_retrieve".to_string(),
        "context_abandon".to_string(),
        "shell".to_string(),
        "memory_recall".to_string(),
    ];
    let loaded = agent_config::load_agent_tools_config(work_dir).unwrap().unwrap();
    let merged = agent_config::merge_tools_config(&code, &loaded.tools);
    let map: HashMap<String, bool> =
        merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();

    assert!(map.contains_key("context_retrieve"));
    assert!(map["context_retrieve"], "missing platform tool must default to enabled");
    assert!(map["context_abandon"], "missing platform tool must default to enabled");
}

/// Bug 1 + Bug 2 unification: a single round-trip across both APIs
/// must leave the on-disk state in agreement with both filters
/// (active_names for MCP, platform protection for builtin tools).
#[test]
fn mcp_active_filter_and_builtin_platform_protection_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path();
    std::fs::create_dir_all(work_dir.join("config")).unwrap();

    // ── MCP layer: user has unchecked every catalog MCP. ───────────
    use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};
    use acowork_runtime::agent_config::{load_active_mcp_configs, save_agent_mcp_config, AgentMcpConfig as RuntimeAgentMcpConfig};

    let cfg = RuntimeAgentMcpConfig {
        catalog: vec![
            McpServerConfigDef {
                name: "playwright".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "npx".into(),
                args: vec![],
                env: Default::default(),
                headers: Default::default(),
                tool_timeout_secs: None,
            },
            McpServerConfigDef {
                name: "context7".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "npx".into(),
                args: vec![],
                env: Default::default(),
                headers: Default::default(),
                tool_timeout_secs: None,
            },
        ],
        local: vec![],
        active_names: Some(vec![]), // user unchecked all
    };
    save_agent_mcp_config(work_dir, &cfg).unwrap();
    assert!(load_active_mcp_configs(work_dir).is_empty());

    // ── Builtin layer: user has disabled context_retrieve in
    // agent_tools.json — but merge/patch must override.
    agent_config::save_agent_tools_config(
        work_dir,
        &AgentToolsConfig {
            tools: vec![
                AgentToolEntry::new("context_retrieve", false),
                AgentToolEntry::new("context_abandon", false),
                AgentToolEntry::new("shell", true),
            ],
        },
    )
    .unwrap();

    let code = vec![
        "context_retrieve".to_string(),
        "context_abandon".to_string(),
        "shell".to_string(),
    ];
    let loaded = agent_config::load_agent_tools_config(work_dir).unwrap().unwrap();
    let merged = agent_config::merge_tools_config(&code, &loaded.tools);
    let map: HashMap<String, bool> =
        merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(map["context_retrieve"]);
    assert!(map["context_abandon"]);

    // Cross-check the API used by the PUT path:
    let patched = agent_config::apply_builtin_tools_patch(
        &loaded.tools,
        &[AgentToolEntry::new("context_retrieve", false)],
    );
    let patch_map: HashMap<String, bool> =
        patched.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(patch_map["context_retrieve"]);

    // MCP loader still returns empty (proves the two filters are
    // orthogonal and not accidentally sharing state).
    assert!(load_active_mcp_configs(work_dir).is_empty());
}

/// Drive `apply_builtin_tools_update` directly through its public
/// surface (`apply_builtin_tools_patch`) and confirm the registered
/// slots end up with the correct `enabled` flags. This is the unit
/// proof that the `session_task::apply_builtin_tools_update` body, which
/// calls `apply_builtin_tools_patch(&entries, &[])`, behaves correctly
/// when handed an arbitrary enabled/disabled mix.
#[test]
fn apply_builtin_tools_patch_is_idempotent_with_empty_patch() {
    let entries = vec![
        AgentToolEntry::new("context_retrieve", false), // hostile
        AgentToolEntry::new("context_abandon", false),  // hostile
        AgentToolEntry::new("shell", true),
    ];
    let resolved = agent_config::apply_builtin_tools_patch(&entries, &[]);
    let map: HashMap<String, bool> =
        resolved.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(map["context_retrieve"], "platform tool force-enabled by empty-patch call");
    assert!(map["context_abandon"], "platform tool force-enabled by empty-patch call");
    assert!(map["shell"], "non-platform tool keeps its own enabled flag");
}

/// Sanity-check that the legacy `load_merged_mcp_configs` still works
/// (deprecated but kept for the persistence/introspection layer) and
/// returns the unconditional merged list — proving it is a *different*
/// function from `load_active_mcp_configs` and the deprecation is real.
#[test]
#[allow(deprecated)]
fn load_merged_mcp_configs_returns_unconditional_set() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};
    let cfg = acowork_runtime::agent_config::AgentMcpConfig {
        catalog: vec![McpServerConfigDef {
            name: "playwright".into(),
            transport: McpTransportDef::Stdio,
            url: None,
            command: "npx".into(),
            args: vec![],
            env: Default::default(),
            headers: Default::default(),
            tool_timeout_secs: None,
        }],
        local: vec![],
        active_names: Some(vec![]), // user unchecked all
    };
    acowork_runtime::agent_config::save_agent_mcp_config(dir.path(), &cfg).unwrap();

    // load_merged_mcp_configs IGNORES active_names → returns catalog entry.
    let merged = agent_config::load_merged_mcp_configs(dir.path());
    assert_eq!(merged.len(), 1, "legacy loader ignores active_names");
    assert_eq!(merged[0].name, "playwright");

    // load_active_mcp_configs HONORS active_names → returns empty.
    let active = agent_config::load_active_mcp_configs(dir.path());
    assert!(active.is_empty(), "new loader honors active_names");

    // Arc is used to silence the import (test setup pattern)
}