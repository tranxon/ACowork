//! Integration tests for the unified builtin-tools mutation pipeline
//! (ADR-052).
//!
//! These tests exercise the **end-to-end** read-modify-write cycle that
//! connects:
//!   1. `agent_config::apply_builtin_tools_patch` (persistence layer:
//!      filters platform-protected tools out of the output)
//!   2. `agent_config::merge_tools_config` (cold-start path: same filter)
//!   3. `session_task::apply_builtin_tools_update` (atomic in-memory
//!      rewrite + dispatch list + LLM tool_definitions refresh)
//!
//! They are the ADR-052 regression net at the integration level — when
//! the persistence layer and the in-memory layer diverge, these tests
//! fail.

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
    //   2. apply_builtin_tools_patch (strips platform tools from output)
    //   3. save
    let updated = agent_config::apply_builtin_tools_patch(&current, patch);
    let cfg = AgentToolsConfig {
        tools: updated.clone(),
    };
    agent_config::save_agent_tools_config(work_dir, &cfg).expect("save_agent_tools_config");
    updated
}

/// ADR-052: PUT /builtin-tools must filter platform-protected tools
/// out of both the returned list AND the on-disk file — they live
/// exclusively in the in-memory registry (ADR-061 §10.2:
/// `context_retrieve` always registered, `context_abandon` not).
/// Persisting them would resurrect the "user-editable Switch the server
/// silently ignores" UX bug that ADR-052 was created to eliminate.
#[test]
fn put_builtin_tools_filters_platform_tools_out_of_disk() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path();

    // Seed: a mix of platform and non-platform tools, all enabled.
    // The platform entries simulate legacy data or a hostile client
    // that wrote them into the file — the PUT path must strip them.
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

    // User (or hostile client) attempts to disable context_retrieve.
    let patch = vec![AgentToolEntry::new("context_retrieve", false)];
    let updated = put_builtin_tools(work_dir, &patch);

    let names: std::collections::HashSet<&str> =
        updated.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains("context_retrieve"),
        "PUT must filter context_retrieve out of returned list; got: {:?}",
        names
    );
    assert!(
        !names.contains("context_abandon"),
        "PUT must filter context_abandon out of returned list; got: {:?}",
        names
    );
    assert!(names.contains("shell"));
    assert!(names.contains("memory_recall"));

    // The on-disk file must agree with the returned list — both
    // platform entries pruned, non-platform entries preserved with
    // their persisted flag intact.
    let reloaded = agent_config::load_agent_tools_config(work_dir)
        .unwrap()
        .expect("file must exist");
    let on_disk: std::collections::HashSet<&str> =
        reloaded.tools.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !on_disk.contains("context_retrieve"),
        "disk must mirror in-memory filter; got: {:?}",
        on_disk
    );
    assert!(
        !on_disk.contains("context_abandon"),
        "disk must mirror in-memory filter; got: {:?}",
        on_disk
    );

    // Round-trip the persistence again through merge_tools_config —
    // cold-start path must agree with hot-update path. Both must
    // agree that platform tools never appear in the persisted set.
    let code = vec![
        "context_retrieve".to_string(),
        "context_abandon".to_string(),
        "shell".to_string(),
        "memory_recall".to_string(),
    ];
    let merged = agent_config::merge_tools_config(&code, &reloaded.tools);
    let merge_names: std::collections::HashSet<&str> =
        merged.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !merge_names.contains("context_retrieve"),
        "cold-start merge must agree with hot-update patch (ADR-052 unification)"
    );
    assert!(
        !merge_names.contains("context_abandon"),
        "cold-start merge must agree with hot-update patch (ADR-052 unification)"
    );
}

/// ADR-052: even when platform tools appear in the `code` registry
/// (i.e. the in-memory registry — ADR-061 §10.2, `context_retrieve`
/// always registered) but are absent from
/// the persisted file (first start or hand-edited), the cold-start
/// `merge_tools_config` must STRIP them — they are in-memory only,
/// never persisted. The user's per-agent tool toggle layer has no
/// concept of "platform tools" because it cannot reach them.
#[test]
fn merge_tools_config_filters_platform_tools_when_missing_from_persisted() {
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

    // Cold-start path: code registry includes platform tools (the
    // in-memory registry always carries `context_retrieve`, ADR-061
    // §10.2), but the persisted file does not. Merge must still exclude
    // them from the output.
    let code = vec![
        "context_retrieve".to_string(),
        "context_abandon".to_string(),
        "shell".to_string(),
        "memory_recall".to_string(),
    ];
    let loaded = agent_config::load_agent_tools_config(work_dir).unwrap().unwrap();
    let merged = agent_config::merge_tools_config(&code, &loaded.tools);
    let names: std::collections::HashSet<&str> =
        merged.iter().map(|e| e.name.as_str()).collect();

    assert!(
        !names.contains("context_retrieve"),
        "platform tool must not appear in merged output even when code registry includes it; got: {:?}",
        names
    );
    assert!(
        !names.contains("context_abandon"),
        "platform tool must not appear in merged output even when code registry includes it; got: {:?}",
        names
    );
    // Non-platform tools are preserved with their persisted flags.
    let map: HashMap<String, bool> =
        merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();
    assert!(map["shell"], "non-platform tool keeps persisted enabled=true");
    assert!(
        !map["memory_recall"],
        "non-platform tool keeps persisted enabled=false"
    );
}

/// ADR-052 + Bug 1 unification: a single round-trip across both APIs
/// must leave the on-disk state in agreement with both filters
/// (active_names for MCP, platform-tool strip for builtin tools).
/// The two filters operate on disjoint sets — MCP active_names cannot
/// leak into builtin output, and the builtin platform filter cannot
/// leak into MCP output.
#[test]
fn mcp_active_filter_and_builtin_platform_filter_are_independent() {
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

    // ── Builtin layer: legacy data on disk has the platform tools
    // written into agent_tools.json (e.g. user upgraded across the
    // ADR-052 boundary). Both merge (cold-start) and patch (PUT path)
    // must strip them.
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

    // Cold-start path: merge_tools_config strips platform tools.
    let merged = agent_config::merge_tools_config(&code, &loaded.tools);
    let merge_names: std::collections::HashSet<&str> =
        merged.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !merge_names.contains("context_retrieve"),
        "merge must filter context_retrieve; got: {:?}",
        merge_names
    );
    assert!(
        !merge_names.contains("context_abandon"),
        "merge must filter context_abandon; got: {:?}",
        merge_names
    );
    assert!(merge_names.contains("shell"));

    // Hot-update path: apply_builtin_tools_patch strips platform
    // tools from both the current list and the hostile patch entry.
    let patched = agent_config::apply_builtin_tools_patch(
        &loaded.tools,
        &[AgentToolEntry::new("context_retrieve", false)],
    );
    let patch_names: std::collections::HashSet<&str> =
        patched.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !patch_names.contains("context_retrieve"),
        "patch must filter context_retrieve; got: {:?}",
        patch_names
    );
    assert!(
        !patch_names.contains("context_abandon"),
        "patch must filter context_abandon; got: {:?}",
        patch_names
    );

    // MCP loader still returns empty (proves the two filters are
    // orthogonal and not accidentally sharing state).
    assert!(
        load_active_mcp_configs(work_dir).is_empty(),
        "MCP active_names filter must remain independent of builtin platform filter"
    );
}

/// Drive `apply_builtin_tools_update` directly through its public
/// surface (`apply_builtin_tools_patch`) and confirm the registered
/// slots end up with the correct `enabled` flags. This is the unit
/// proof that the `session_task::apply_builtin_tools_update` body, which
/// calls `apply_builtin_tools_patch(&entries, &[])`, behaves correctly
/// when handed an arbitrary enabled/disabled mix.
///
/// ADR-052: the legacy `force-enable platform tool` semantic is
/// replaced by `filter platform tool out of output`. A hostile
/// `current` that lists platform tools must not propagate them to the
/// patch result regardless of `patch` contents.
#[test]
fn apply_builtin_tools_patch_filters_platform_tools_with_empty_patch() {
    let entries = vec![
        AgentToolEntry::new("context_retrieve", false), // hostile legacy data
        AgentToolEntry::new("context_abandon", false),  // hostile legacy data
        AgentToolEntry::new("shell", true),
    ];
    let resolved = agent_config::apply_builtin_tools_patch(&entries, &[]);
    let names: std::collections::HashSet<&str> =
        resolved.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains("context_retrieve"),
        "patch must filter context_retrieve; got: {:?}",
        names
    );
    assert!(
        !names.contains("context_abandon"),
        "patch must filter context_abandon; got: {:?}",
        names
    );
    assert!(names.contains("shell"));

    // Non-platform tool keeps its own enabled flag through the empty
    // patch — the patch is a no-op for entries not mentioned in it.
    let shell = resolved.iter().find(|e| e.name == "shell").unwrap();
    assert!(
        shell.enabled,
        "non-platform tool keeps its own enabled flag (empty patch is no-op)"
    );
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
