//! Per-agent runtime configuration persistence.
//!
//! Stores per-agent config (max_output_tokens, max_iterations,
//! temperature, system_prompt_override, shell_approval_threshold)
//! as JSON in `{work_dir}/config/agent_config.json`.
//!
//! Also stores per-agent MCP server config in `{work_dir}/config/agent_mcp.json`
//! (dual-list format: `catalog` from Gateway + `local` from agent-installed tools)
//! and per-agent search provider config in `{work_dir}/config/agent_search.json`.
//!
//! Model selection is per-session (ADR-012), persisted in JSONL SessionMetadata.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use acowork_core::protocol::{AgentSearchConfig, McpServerConfigDef};

/// Per-agent MCP config with dual-list format.
///
/// `catalog` is managed by Gateway (pushed via RuntimeConfigUpdate).
/// `local` is managed by mcp_install / mcp_uninstall tools.
/// Both lists are merged at startup and when applying MCP connections.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentMcpConfig {
    /// Gateway-managed catalog MCPs (pushed via RuntimeConfigUpdate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub catalog: Vec<McpServerConfigDef>,
    /// Agent-installed local MCPs (managed by mcp_install / mcp_uninstall tools).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local: Vec<McpServerConfigDef>,
}

impl AgentMcpConfig {
    /// Merge catalog + local into a single flat list for MCP connection.
    /// Local entries take precedence over catalog entries with the same name.
    pub fn merged(&self) -> Vec<McpServerConfigDef> {
        let mut result = self.catalog.clone();
        // Local entries override catalog entries with the same name
        for local_entry in &self.local {
            if let Some(pos) = result.iter().position(|c| c.name == local_entry.name) {
                result[pos] = local_entry.clone();
            } else {
                result.push(local_entry.clone());
            }
        }
        result
    }

    /// Check whether a name exists in either catalog or local.
    pub fn contains_name(&self, name: &str) -> bool {
        self.catalog.iter().any(|c| c.name == name) || self.local.iter().any(|l| l.name == name)
    }

    /// Check whether a name exists in catalog only.
    pub fn is_catalog(&self, name: &str) -> bool {
        self.catalog.iter().any(|c| c.name == name)
    }
}

/// Per-agent configuration persisted to workspace/config/agent_config.json.
///
/// On first start, defaults are generated from manifest.toml and AgentHelloResult.
/// User modifications via the Desktop App are persisted here by the Runtime
/// when Gateway pushes a RuntimeConfigUpdate.
///
/// MCP server configurations are stored separately in agent_mcp.json
/// (see `load_agent_mcp_config` / `save_agent_mcp_config`) per the
/// `agent_*.json` naming convention for per-agent config snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct AgentConfig {
    /// Max output tokens per request (None = use global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,

    /// Max LLM iterations per run (None = use global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,

    /// LLM sampling temperature (0.0 = deterministic, 2.0 = max creative).
    ///
    /// Resolution chain at runtime (Layer 1 = highest priority):
    /// 1. **this field** — user's agent-level setting (set via Agent Setup panel)
    /// 2. `manifest.llm.temperature` — package author default (shipped with agent)
    /// 3. `crate::config::DEFAULT_TEMPERATURE` — hardcoded final fallback (0.3)
    ///
    /// `None` means "I don't have an opinion" — fall through to the next level.
    /// The user can clear this value in the UI to revert to the manifest default
    /// (analogous to how `avatar` reverts when cleared).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Per-agent context window size limit in tokens.
    ///
    /// Resolution chain at runtime (Layer 1 = highest priority):
    /// 1. **this field** — user's agent-level setting (set via Agent Setup panel)
    /// 2. `manifest.llm.context_window` — package author default
    /// 3. `crate::config::DEFAULT_CONTEXT_WINDOW` — hardcoded final fallback (200K)
    ///
    /// `None` means "I don't have an opinion" — fall through to the next level.
    /// `Some(0)` means "no limit" — use model's full context window.
    /// The user can clear this value in the UI to revert to the manifest default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,

    /// System prompt override (None = use manifest-compiled prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,

    /// Shell command approval threshold ("low" | "medium" | "high" | "never").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<String>,

    /// Maximum number of conversation sessions to keep on disk.
    /// When exceeded at session creation, the oldest sessions are archived.
    /// None = use system default (1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,

    /// Custom avatar path (relative to install dir, e.g. "assets/avatar-02.jpg").
    /// When set, takes priority over `builtin_avatar`. Managed via gRPC
    /// (RuntimeConfigUpdate) from the Gateway — the Runtime persists it
    /// to agent_config.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,

    /// Builtin avatar icon ID (e.g. "icon-05"). Mutually exclusive with `avatar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,

    /// Approval timeout in seconds for loop approval. None = use system default (300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout_secs: Option<u64>,

    /// ADR-032: number of recent tool results kept raw (not compressed) at
    /// every trigger point (event / budget / restore / manual).
    ///
    /// Resolution chain at runtime (Layer 1 = highest priority):
    /// 1. **this field** — user's agent-level setting (set via Agent Setup panel)
    /// 2. `RuntimeConfigOverrides::tool_result_keep_recent_n` — runtime push
    ///    from Gateway (Layer 1' when no agent_config value is set, but
    ///    normally agent_config.json shadows it during boot via
    ///    `From<&AgentConfig> for RuntimeConfigOverrides`)
    /// 3. `crate::agent::loop_context::DEFAULT_KEEP_RECENT_N` — ADR-032 hardcoded
    ///    final fallback (3; matches skill-typical tool-call depth)
    ///
    /// Semantics (per ADR-032 core principle #7):
    /// - `None` → fall through to the code default (3)
    /// - `Some(0)` → compress every eligible tool result (most aggressive)
    /// - `Some(n)` → keep the last `n` tool messages raw
    /// - No upper cap is enforced; very large values simply disable compression
    ///   for the recent-N window (the user accepts the trade-off)
    ///
    /// NOTE: this field is **never auto-resolved** to a default in
    /// `session_init.rs`. Unlike `context_window` / `temperature` it has
    /// no manifest-level analogue, so the resolution chain stays 2-level
    /// (config → code default) without a third layer to seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_keep_recent_n: Option<u32>,

    /// ADR-032 C4b: compression trigger mode ("auto" | "manual").
    ///
    /// Resolution chain:
    /// 1. **this field** — user's agent-level setting (set via Agent Setup panel)
    /// 2. `RuntimeConfigOverrides::tool_result_compression_mode` — runtime push
    /// 3. `crate::agent::loop_context::DEFAULT_COMPRESSION_MODE` — hardcoded (Auto)
    ///
    /// `None` falls through to the next level. `Some("auto")` enables automatic
    /// compress_tool_results on events. `Some("manual")` disables event triggers;
    /// user invokes via Gateway API / CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_compression_mode: Option<String>,

    /// ADR-032 C4a: tool-result **soft compression** threshold in characters.
    ///
    /// Any tool-result message whose `content.len()` exceeds this threshold
    /// is replaced with a fixed-length placeholder by `compress_tool_results`.
    /// `2048 chars ≈ 512 tokens`, which matches the typical "LLM context
    /// bloat" threshold — see ADR-032 core principle #7.
    ///
    /// Resolution chain (highest to lowest priority):
    /// 1. `RuntimeConfigOverrides::tool_result_soft_threshold_chars` —
    ///    runtime push from Gateway (Layer 1'). Normally shadowed by
    ///    `agent_config.json` on boot via `apply_runtime_config_override`.
    /// 2. **this field** — user's agent-level setting persisted in
    ///    `agent_config.json`. Editable via the Agent Setup panel.
    /// 3. `crate::agent::loop_context::DEFAULT_SOFT_THRESHOLD_CHARS` —
    ///    hardcoded fallback (2048).
    ///
    /// `None` falls through to the next level. `Some(n)` activates the
    /// user-chosen threshold. Stored as `usize` because `content.len()`
    /// returns byte-equal character counts for the message bodies we
    /// compress (validated against UTF-8 boundaries at the call site —
    /// see ADR-032 "char vs byte" section).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_soft_threshold_chars: Option<usize>,
}

/// Resolve the effective avatar from agent config and manifest fallback.
///
/// Priority:
/// 1. config.avatar          — user's runtime choice (custom image)
/// 2. config.builtin_avatar  — user's runtime choice (builtin icon)
/// 3. manifest.avatar         — install-time default (custom image)
/// 4. manifest.builtin_avatar — install-time default (builtin icon)
/// 5. fallback (both None)   — caller renders deterministic random icon
///
/// Returns `(avatar, builtin_avatar, source)` where source is
/// `"config"`, `"manifest"`, or `"fallback"`.
pub fn resolve_effective_avatar(
    config: &AgentConfig,
    manifest_avatar: &Option<String>,
    manifest_builtin_avatar: &Option<String>,
) -> (Option<String>, Option<String>, &'static str) {
    if config.avatar.is_some() || config.builtin_avatar.is_some() {
        return (config.avatar.clone(), config.builtin_avatar.clone(), "config");
    }
    if manifest_avatar.is_some() || manifest_builtin_avatar.is_some() {
        return (manifest_avatar.clone(), manifest_builtin_avatar.clone(), "manifest");
    }
    (None, None, "fallback")
}

/// Filename for per-agent config in the workspace config directory.
const AGENT_CONFIG_FILE: &str = "agent_config.json";

/// Build the path to the agent config file.
fn config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_CONFIG_FILE)
}

/// Load per-agent config from workspace/config/agent_config.json.
///
/// Returns `None` if the file does not exist (first start).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_config(work_dir: &Path) -> Result<Option<AgentConfig>, String> {
    let path = config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        "Loaded agent config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent config to workspace/config/agent_config.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_config(work_dir: &Path, cfg: &AgentConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        "Saved agent config to workspace"
    );

    Ok(())
}

// ── Per-agent builtin-tools config (ADR-029) ───────────────────────────

/// Per-agent builtin tools enable/disable configuration.
///
/// Persisted to `{work_dir}/config/agent_tools.json`.
/// Contains ALL builtin tools — each with an `enabled` flag. The file
/// is the single source of truth once it exists; on first start it is
/// generated from `manifest.toml` `[[tools]]` declarations (or, if no
/// `[[tools]]` is present, every builtin tool is enabled by default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentToolsConfig {
    /// All builtin tools with their enable status.
    /// Must include every registered builtin tool — call sites
    /// reconcile this list against `all_builtin_tools()` at load time.
    #[serde(default)]
    pub tools: Vec<AgentToolEntry>,
}

/// A single builtin tool entry in `agent_tools.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolEntry {
    /// Tool name (matches `Tool::name()`).
    pub name: String,
    /// Whether this tool is enabled for the agent.
    /// Defaults to `true` when missing in JSON for backward-compatible
    /// additions of new fields.
    #[serde(default = "default_tool_entry_enabled")]
    pub enabled: bool,
}

fn default_tool_entry_enabled() -> bool {
    true
}

impl AgentToolEntry {
    /// Construct a new entry with the given enabled flag.
    pub fn new(name: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            enabled,
        }
    }
}

/// Filename for per-agent builtin-tools config in the workspace config dir.
const AGENT_TOOLS_CONFIG_FILE: &str = "agent_tools.json";

/// Build the path to the agent builtin-tools config file.
fn tools_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_TOOLS_CONFIG_FILE)
}

/// Load per-agent builtin-tools config from
/// `workspace/config/agent_tools.json`.
///
/// Returns `Ok(None)` if the file does not exist (first start — caller
/// must then initialize from `manifest.toml` or default-enable every
/// tool).
pub fn load_agent_tools_config(work_dir: &Path) -> Result<Option<AgentToolsConfig>, String> {
    let path = tools_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentToolsConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        tool_count = cfg.tools.len(),
        "Loaded agent builtin-tools config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent builtin-tools config to
/// `workspace/config/agent_tools.json`. Atomic write-tmp-rename.
pub fn save_agent_tools_config(
    work_dir: &Path,
    cfg: &AgentToolsConfig,
) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = tools_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent tools config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        tool_count = cfg.tools.len(),
        "Saved agent builtin-tools config to workspace"
    );

    Ok(())
}

/// Ensure a tool entry exists in `agent_tools.json`.
///
/// If the tool is already present, its `enabled` flag is preserved
/// (respecting the user's preference). If absent, a new entry is
/// appended with `default_enabled`.
///
/// Used by the `SidecarEndpointUpdate` handler to persist dynamically
/// registered tools (e.g. `codebase` when LSP Relay becomes ready)
/// so that the frontend tool panel and `ConfigSnapshot` queries
/// reflect tools that were not available at startup.
///
/// No-op if `agent_tools.json` does not exist yet (it will be created
/// at next startup via the normal `agent_init.rs` flow).
pub fn ensure_tool_in_config(work_dir: &Path, tool_name: &str, default_enabled: bool) {
    match load_agent_tools_config(work_dir) {
        Ok(Some(mut cfg)) => {
            if let Some(existing) = cfg.tools.iter().find(|e| e.name == tool_name) {
                // Already present - respect user's enabled/disabled preference.
                tracing::info!(
                    tool = tool_name,
                    existing_enabled = existing.enabled,
                    default_enabled,
                    "ensure_tool_in_config: tool already present, preserving existing enabled flag"
                );
                return;
            }
            cfg.tools
                .push(AgentToolEntry::new(tool_name, default_enabled));
            tracing::info!(
                tool = tool_name,
                default_enabled,
                "Adding dynamically registered tool to agent_tools.json"
            );
            if let Err(e) = save_agent_tools_config(work_dir, &cfg) {
                tracing::warn!(
                    error = %e,
                    tool = tool_name,
                    "Failed to persist dynamic tool addition to agent_tools.json"
                );
            }
        }
        Ok(None) => {
            tracing::warn!(
                tool = tool_name,
                work_dir = %work_dir.display(),
                "agent_tools.json not found; skipping dynamic tool persistence \
                 (will be created at next startup)"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                tool = tool_name,
                "Failed to load agent_tools.json for dynamic tool persistence"
            );
        }
    }
}

/// Remove a tool entry from `agent_tools.json`.
///
/// Used by the `SidecarEndpointUpdate` handler when a sidecar goes
/// away (e.g. LSP Relay stopped) so the frontend tool panel no longer
/// shows the now-unavailable tool.
///
/// No-op if the file or the tool entry does not exist.
pub fn remove_tool_from_config(work_dir: &Path, tool_name: &str) {
    match load_agent_tools_config(work_dir) {
        Ok(Some(mut cfg)) => {
            let before = cfg.tools.len();
            cfg.tools.retain(|e| e.name != tool_name);
            if cfg.tools.len() < before {
                tracing::info!(
                    tool = tool_name,
                    "Removing dynamically unregistered tool from agent_tools.json"
                );
                if let Err(e) = save_agent_tools_config(work_dir, &cfg) {
                    tracing::warn!(
                        error = %e,
                        tool = tool_name,
                        "Failed to persist dynamic tool removal to agent_tools.json"
                    );
                }
            }
        }
        Ok(None) => {
            // File doesn't exist - nothing to remove.
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                tool = tool_name,
                "Failed to load agent_tools.json for dynamic tool removal"
            );
        }
    }
}

/// Merge a persisted config with the current set of code-registered
/// builtin tools, producing a fresh list of entries keyed off the
/// *code* registry (not the persisted file) so unknown tools are
/// silently dropped on next save.
///
/// Rules:
/// - Tools present in both -> use persisted `enabled` (user's choice
///   is the single source of truth; never overwrite)
/// - Tools only in code (newly added by Runtime upgrade) -> appended
///   with `enabled = false` (opt-in: new tools require explicit
///   enablement via manifest or frontend)
/// - Tools only in persisted file (removed by Runtime upgrade) ->
///   silently dropped
pub fn merge_tools_config(
    code_tool_names: &[String],          // from `all_builtin_tools()` registry
    persisted: &[AgentToolEntry],        // from agent_tools.json
) -> Vec<AgentToolEntry> {
    let persisted_map: std::collections::HashMap<&str, bool> = persisted
        .iter()
        .map(|e| (e.name.as_str(), e.enabled))
        .collect();

    let code_set: std::collections::HashSet<&str> =
        code_tool_names.iter().map(|s| s.as_str()).collect();

    // Log tools that are in persisted but NOT in code registry (will be dropped)
    for entry in persisted {
        if !code_set.contains(entry.name.as_str()) {
            tracing::warn!(
                tool = %entry.name,
                enabled = entry.enabled,
                "merge_tools_config: DROPPING persisted tool not in code registry"
            );
        }
    }
    // Log tools that are in code but NOT in persisted (new tools, default enabled=false)
    for name in code_tool_names {
        if !persisted_map.contains_key(name.as_str()) {
            tracing::info!(
                tool = %name,
                "merge_tools_config: new code-registered tool, defaulting to enabled=false"
            );
        }
    }

    code_tool_names
        .iter()
        .map(|name| {
            let enabled = persisted_map
                .get(name.as_str())
                .copied()
                .unwrap_or(false); // new tool → disabled (opt-in)
            AgentToolEntry::new(name, enabled)
        })
        .collect()
}

/// Tools that are part of the core platform protocol and must always be
/// enabled. Users cannot disable these via frontend tool settings.
const PLATFORM_TOOLS: &[&str] = &["context_recall"];

/// Apply a partial `RuntimeConfigUpdate.builtin_tools_enabled` payload
/// (only listed tool names are touched) onto an existing
/// `AgentToolsConfig` in place. Returns the new full list — a copy of
/// the caller may then persist via `save_agent_tools_config`.
///
/// Tool names in `patch` that are not present in `current` are
/// silently ignored (defensive: future tools should arrive via
/// `merge_tools_config` at startup, not via this incremental path).
///
/// Platform tools (see `PLATFORM_TOOLS`) are force-enabled and ignore
/// any user-provided patch value.
pub fn apply_builtin_tools_patch(
    current: &[AgentToolEntry],
    patch: &[AgentToolEntry],
) -> Vec<AgentToolEntry> {
    let patch_map: std::collections::HashMap<&str, bool> = patch
        .iter()
        .map(|e| (e.name.as_str(), e.enabled))
        .collect();

    current
        .iter()
        .map(|e| {
            // Platform tools are force-enabled — ignore user overrides.
            if PLATFORM_TOOLS.contains(&e.name.as_str()) {
                return AgentToolEntry::new(&e.name, true);
            }
            let enabled = patch_map
                .get(e.name.as_str())
                .copied()
                .unwrap_or(e.enabled);
            AgentToolEntry::new(&e.name, enabled)
        })
        .collect()
}

/// Initialize an `AgentToolsConfig` from a `manifest.toml`
/// `[[tools]]` tool-names list. Any builtin tool whose name appears
/// in the manifest gets `enabled = true`; everything else gets
/// `enabled = false`. Used when `agent_tools.json` is absent.
pub fn init_tools_config_from_manifest(
    code_tool_names: &[String],
    manifest_tool_names: &[String],
) -> Vec<AgentToolEntry> {
    let manifest_set: std::collections::HashSet<&str> =
        manifest_tool_names.iter().map(|s| s.as_str()).collect();

    code_tool_names
        .iter()
        .map(|name| {
            let enabled = manifest_set.contains(name.as_str());
            AgentToolEntry::new(name, enabled)
        })
        .collect()
}

/// Build the canonical list of builtin tools when no manifest
/// declaration is present: every tool enabled by default
/// (backward-compatible default behavior).
pub fn all_enabled_tools_config(code_tool_names: &[String]) -> Vec<AgentToolEntry> {
    code_tool_names
        .iter()
        .map(|name| AgentToolEntry::new(name, true))
        .collect()
}

// ── Per-agent MCP config (dual-list: catalog + local) ──────────────────

/// Filename for per-agent MCP config in the workspace config directory.
const AGENT_MCP_CONFIG_FILE: &str = "agent_mcp.json";

/// Build the path to the agent MCP config file.
fn mcp_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_MCP_CONFIG_FILE)
}

/// Load per-agent MCP config from workspace/config/agent_mcp.json.
///
/// Returns `None` if the file does not exist (no MCP servers configured).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_mcp_config(work_dir: &Path) -> Result<Option<AgentMcpConfig>, String> {
    let path = mcp_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    // Try new dual-list format first
    if let Ok(cfg) = serde_json::from_str::<AgentMcpConfig>(&raw) {
        tracing::info!(
            work_dir = %work_dir.display(),
            catalog_count = cfg.catalog.len(),
            local_count = cfg.local.len(),
            "Loaded agent MCP config (dual-list format) from workspace"
        );
        return Ok(Some(cfg));
    }

    // Fall back to old format (flat Vec) — migrate all entries to catalog
    if let Ok(old_servers) = serde_json::from_str::<Vec<McpServerConfigDef>>(&raw) {
        tracing::info!(
            work_dir = %work_dir.display(),
            old_count = old_servers.len(),
            "Migrating agent MCP config from old flat format to dual-list format"
        );
        let migrated = AgentMcpConfig {
            catalog: old_servers,
            local: Vec::new(),
        };
        // Auto-save in the new format so we don't need to migrate again
        let _ = save_agent_mcp_config(work_dir, &migrated);
        return Ok(Some(migrated));
    }

    Err(format!(
        "Failed to parse {} as either AgentMcpConfig or Vec<McpServerConfigDef>",
        path.display()
    ))
}

/// Save full per-agent MCP config to workspace/config/agent_mcp.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_mcp_config(work_dir: &Path, cfg: &AgentMcpConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = mcp_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent MCP config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        catalog_count = cfg.catalog.len(),
        local_count = cfg.local.len(),
        "Saved agent MCP config to workspace"
    );

    Ok(())
}

/// Save only the catalog portion of agent MCP config.
///
/// This is used by RuntimeConfigUpdate handler: Gateway pushes catalog MCPs,
/// and we must preserve the `local` list (agent-installed MCPs).
/// Reads the current config, replaces only `catalog`, and saves back.
pub fn save_agent_mcp_config_catalog(
    work_dir: &Path,
    catalog_servers: &[McpServerConfigDef],
) -> Result<(), String> {
    // Load current config to preserve local entries
    let current = load_agent_mcp_config(work_dir)
        .unwrap_or_default()
        .unwrap_or_default();

    let updated = AgentMcpConfig {
        catalog: catalog_servers.to_vec(),
        local: current.local,
    };

    save_agent_mcp_config(work_dir, &updated)
}

/// Load merged MCP configs (catalog + local) from workspace/config/agent_mcp.json.
///
/// Convenience function that loads `AgentMcpConfig` and returns the merged list.
/// Returns an empty vec if the file does not exist.
pub fn load_merged_mcp_configs(work_dir: &Path) -> Vec<McpServerConfigDef> {
    load_agent_mcp_config(work_dir)
        .unwrap_or_default()
        .unwrap_or_default()
        .merged()
}

// ── Per-agent search config ────────────────────────────────────────────

/// Filename for per-agent search config in the workspace config directory.
const AGENT_SEARCH_CONFIG_FILE: &str = "agent_search.json";

/// Build the path to the agent search config file.
fn search_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_SEARCH_CONFIG_FILE)
}

/// Load per-agent search config from workspace/config/agent_search.json.
///
/// Returns `None` if the file does not exist (no search providers configured).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_search_config(work_dir: &Path) -> Result<Option<AgentSearchConfig>, String> {
    let path = search_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentSearchConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        provider_count = cfg.providers.len(),
        "Loaded agent search config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent search config to workspace/config/agent_search.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_search_config(work_dir: &Path, cfg: &AgentSearchConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = search_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent search config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        provider_count = cfg.providers.len(),
        "Saved agent search config to workspace"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};

    // ── AgentMcpConfig struct ────────────────────────────────────────

    #[test]
    fn agent_mcp_config_default_empty() {
        let cfg = AgentMcpConfig::default();
        assert!(cfg.catalog.is_empty());
        assert!(cfg.local.is_empty());
        assert!(cfg.merged().is_empty());
        assert!(!cfg.contains_name("any"));
        assert!(!cfg.is_catalog("any"));
    }

    #[test]
    fn merged_combines_catalog_and_local() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "catalog-a".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "cmd-a".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
            local: vec![McpServerConfigDef {
                name: "local-b".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "cmd-b".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
        };

        let merged = cfg.merged();
        assert_eq!(merged.len(), 2);
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"catalog-a"));
        assert!(names.contains(&"local-b"));
    }

    #[test]
    fn local_overrides_catalog_by_name() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "dup-name".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "catalog-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
            local: vec![McpServerConfigDef {
                name: "dup-name".into(),
                transport: McpTransportDef::Http,
                url: Some("http://local:8080".into()),
                command: "local-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
        };

        let merged = cfg.merged();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "dup-name");
        assert_eq!(merged[0].command, "local-cmd");
    }

    #[test]
    fn contains_name_checks_both_lists() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "cat".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "x".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
            local: vec![McpServerConfigDef {
                name: "loc".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "y".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
        };

        assert!(cfg.contains_name("cat"));
        assert!(cfg.contains_name("loc"));
        assert!(!cfg.contains_name("nonexistent"));
    }

    #[test]
    fn is_catalog_only_matches_catalog_list() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "cat".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "x".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
            local: vec![McpServerConfigDef {
                name: "loc".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "y".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
        };

        assert!(cfg.is_catalog("cat"));
        assert!(!cfg.is_catalog("loc"));
        assert!(!cfg.is_catalog("nonexistent"));
    }

    // ── Serialize/Deserialize round-trip ──────────────────────────────

    #[test]
    fn serialize_deserialize_empty_agent_mcp_config() {
        let cfg = AgentMcpConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentMcpConfig = serde_json::from_str(&json).unwrap();
        assert!(restored.catalog.is_empty());
        assert!(restored.local.is_empty());
    }

    #[test]
    fn serialize_deserialize_full_agent_mcp_config() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "mcp1".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "/usr/bin/server".into(),
                args: vec!["--port".into(), "8080".into()],
                env: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("KEY".into(), "val".into());
                    m
                },
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: Some(30),
            }],
            local: vec![McpServerConfigDef {
                name: "local1".into(),
                transport: McpTransportDef::Sse,
                url: Some("http://example.com/sse".into()),
                command: "".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("Auth".into(), "Bearer x".into());
                    m
                },
                tool_timeout_secs: None,
            }],
        };

        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentMcpConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.catalog.len(), 1);
        assert_eq!(restored.catalog[0].name, "mcp1");
        assert_eq!(restored.catalog[0].tool_timeout_secs, Some(30));
        assert_eq!(restored.local.len(), 1);
        assert_eq!(restored.local[0].name, "local1");
        assert!(matches!(restored.local[0].transport, McpTransportDef::Sse));
    }

    // ── Backward compat migration ────────────────────────────────────

    #[test]
    fn backward_migration_old_flat_format() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let old_json = serde_json::json!([{
            "name": "old-server",
            "transport": "stdio",
            "command": "cmd",
            "args": []
        }]);
        let mcp_path = config_dir.join("agent_mcp.json");
        std::fs::write(&mcp_path, serde_json::to_string(&old_json).unwrap()).unwrap();

        let loaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.catalog.len(), 1);
        assert_eq!(loaded.catalog[0].name, "old-server");
        assert!(loaded.local.is_empty());

        let raw = std::fs::read_to_string(&mcp_path).unwrap();
        assert!(raw.contains("\"catalog\""));
        // Note: local may be absent when empty (skip_serializing_if)
    }

    // ── save_catalog preserves local ─────────────────────────────────

    #[test]
    fn save_catalog_preserves_local() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let initial = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "orig-cat".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "cat-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
            local: vec![McpServerConfigDef {
                name: "orig-loc".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "loc-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
        };
        save_agent_mcp_config(dir.path(), &initial).unwrap();

        let new_catalog = vec![McpServerConfigDef {
            name: "new-cat".into(),
            transport: McpTransportDef::Stdio,
            url: None,
            command: "new-cmd".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            tool_timeout_secs: None,
        }];
        save_agent_mcp_config_catalog(dir.path(), &new_catalog).unwrap();

        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.catalog.len(), 1);
        assert_eq!(reloaded.catalog[0].name, "new-cat");
        assert_eq!(reloaded.local.len(), 1);
        assert_eq!(reloaded.local[0].name, "orig-loc");
    }

    #[test]
    fn load_merged_mcp_configs_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let merged = load_merged_mcp_configs(dir.path());
        assert!(merged.is_empty());
    }

    // ── AgentConfig context_window serialization (ADR-026) ────────────

    #[test]
    fn agent_config_context_window_round_trip() {
        let cfg = AgentConfig {
            context_window: Some(150_000),
            ..AgentConfig::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(json.contains("context_window"), "JSON should contain context_window: {}", json);

        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.context_window, Some(150_000));
    }

    #[test]
    fn agent_config_context_window_none_omitted() {
        let cfg = AgentConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(!json.contains("context_window"), "context_window=None should be omitted from JSON: {}", json);
    }

    #[test]
    fn agent_config_context_window_zero_preserved() {
        // 0 = "no limit" — must be preserved, not treated as None
        let cfg = AgentConfig {
            context_window: Some(0),
            ..AgentConfig::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(json.contains("context_window"), "context_window=0 should be serialized: {}", json);

        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.context_window, Some(0));
    }

    #[test]
    fn agent_config_context_window_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig {
            context_window: Some(200_000),
            temperature: Some(0.5),
            ..AgentConfig::default()
        };
        save_agent_config(dir.path(), &cfg).unwrap();

        let loaded = load_agent_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.context_window, Some(200_000));
        assert_eq!(loaded.temperature, Some(0.5));
    }

    // ── AgentToolsConfig (ADR-029) ────────────────────────────────────

    fn sample_tools() -> Vec<String> {
        vec![
            "memory_recall".to_string(),
            "memory_store".to_string(),
            "http_request".to_string(),
            "web_fetch".to_string(),
            "shell".to_string(),
        ]
    }

    #[test]
    fn agent_tools_config_default_empty() {
        let cfg = AgentToolsConfig::default();
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn agent_tools_config_serialize_round_trip() {
        let cfg = AgentToolsConfig {
            tools: vec![
                AgentToolEntry::new("memory_recall", true),
                AgentToolEntry::new("http_request", false),
            ],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentToolsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tools.len(), 2);
        assert_eq!(restored.tools[0].name, "memory_recall");
        assert!(restored.tools[0].enabled);
        assert!(!restored.tools[1].enabled);
    }

    #[test]
    fn agent_tool_entry_missing_enabled_defaults_to_true() {
        // Backward-compatible: a stripped entry (`{"name":"x"}`) must
        // deserialize to enabled=true, not the Rust default false.
        let json = r#"[{"name": "memory_recall"}]"#;
        let restored: Vec<AgentToolEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(restored.len(), 1);
        assert!(restored[0].enabled, "missing enabled should default to true");
    }

    #[test]
    fn merge_tools_config_preserves_persisted_state() {
        let code = sample_tools();
        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("http_request", false),
            AgentToolEntry::new("removed_tool", true), // should be dropped
        ];
        let merged = merge_tools_config(&code, &persisted);
        let map: std::collections::HashMap<String, bool> =
            merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert_eq!(merged.len(), 5, "removed_tool must not appear in merge");
        assert!(map["memory_recall"]);
        assert!(!map["http_request"], "persisted false preserved");
        // Tools missing from persisted file are disabled (opt-in)
        assert!(!map["memory_store"], "missing in persisted → disabled (opt-in)");
        assert!(!map["web_fetch"], "missing in persisted → disabled (opt-in)");
        assert!(!map["shell"], "missing in persisted → disabled (opt-in)");
        assert!(!map.contains_key("removed_tool"));
    }

    #[test]
    fn merge_tools_config_new_tool_defaults_disabled() {
        // Simulate a Runtime upgrade that introduced a new tool
        // `brand_new_tool` to the code registry. The persisted file
        // has no knowledge of it.
        let mut code = sample_tools();
        code.push("brand_new_tool".to_string());

        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("memory_store", true),
            AgentToolEntry::new("http_request", true),
            AgentToolEntry::new("web_fetch", true),
            AgentToolEntry::new("shell", true),
        ];
        // New tools default to disabled (opt-in: need explicit
        // enablement via manifest or frontend).
        let merged = merge_tools_config(&code, &persisted);
        let entry = merged.iter().find(|e| e.name == "brand_new_tool").unwrap();
        assert!(
            !entry.enabled,
            "new tool default must be disabled (opt-in): got enabled={}",
            entry.enabled
        );
        // Existing tools should preserve their persisted state.
        let memory_recall = merged.iter().find(|e| e.name == "memory_recall").unwrap();
        assert!(memory_recall.enabled);
    }

    #[test]
    fn init_tools_config_from_manifest_enables_listed_only() {
        let code = sample_tools();
        let manifest_tools = vec!["memory_recall".to_string(), "shell".to_string()];
        let cfg = init_tools_config_from_manifest(&code, &manifest_tools);
        let map: std::collections::HashMap<String, bool> =
            cfg.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert!(map["memory_recall"]);
        assert!(map["shell"]);
        assert!(!map["memory_store"], "not in manifest → disabled");
        assert!(!map["http_request"]);
        assert!(!map["web_fetch"]);
    }

    #[test]
    fn init_tools_config_empty_manifest_disables_all() {
        let code = sample_tools();
        let cfg = init_tools_config_from_manifest(&code, &[]);
        assert!(cfg.iter().all(|e| !e.enabled));
    }

    #[test]
    fn all_enabled_tools_config_enables_everything() {
        let code = sample_tools();
        let cfg = all_enabled_tools_config(&code);
        assert_eq!(cfg.len(), code.len());
        assert!(cfg.iter().all(|e| e.enabled));
    }

    #[test]
    fn apply_builtin_tools_patch_only_touches_listed_tools() {
        let current = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("http_request", true),
            AgentToolEntry::new("shell", false),
        ];
        let patch = vec![AgentToolEntry::new("http_request", false)];
        let next = apply_builtin_tools_patch(&current, &patch);

        let map: std::collections::HashMap<String, bool> =
            next.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert!(map["memory_recall"], "unchanged when not in patch");
        assert!(!map["http_request"], "patched to false");
        assert!(!map["shell"], "unchanged when not in patch");
    }

    #[test]
    fn apply_builtin_tools_patch_unknown_tool_silently_ignored() {
        let current = vec![AgentToolEntry::new("memory_recall", true)];
        let patch = vec![
            AgentToolEntry::new("memory_recall", false),
            AgentToolEntry::new("ghost_tool", true), // not in current
        ];
        let next = apply_builtin_tools_patch(&current, &patch);
        assert_eq!(next.len(), 1, "patch must not add new tools");
        assert!(!next[0].enabled);
    }

    #[test]
    fn load_agent_tools_config_returns_none_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_agent_tools_config(dir.path()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn save_and_load_agent_tools_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentToolsConfig {
            tools: vec![
                AgentToolEntry::new("memory_recall", true),
                AgentToolEntry::new("http_request", false),
                AgentToolEntry::new("shell", true),
            ],
        };
        save_agent_tools_config(dir.path(), &cfg).unwrap();

        let loaded = load_agent_tools_config(dir.path())
            .unwrap()
            .expect("file should exist after save");
        assert_eq!(loaded.tools.len(), 3);
        let map: std::collections::HashMap<String, bool> =
            loaded.tools.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert!(map["memory_recall"]);
        assert!(!map["http_request"]);
        assert!(map["shell"]);
    }

    // ── ADR-032 C4b: live-edit → persistence → restart round-trip ────

    use crate::agent::session::session_manager::RuntimeConfigOverrides;

    /// End-to-end regression test for the live-edit persistence bug.
    ///
    /// Chain under test (mirrors what `cli.rs` does at runtime):
    ///   1. The Gateway PUTs a `RuntimeConfigUpdate` with
    ///      `tool_result_compression_mode = "manual"`.
    ///   2. cli.rs applies the push into a `RuntimeConfigOverrides`.
    ///   3. cli.rs calls `overrides.apply_to(&mut agent_cfg)` — this is
    ///      the fix path that previously had no `compression_mode` branch.
    ///   4. cli.rs calls `save_agent_config(...)` so the on-disk file
    ///      reflects the new mode.
    ///   5. A subsequent `load_agent_config(...)` round-trip (simulating
    ///      a restart) must round-trip the value.
    ///
    /// Pre-fix, step 3 was silently dropping the field, step 4 wrote
    /// an unchanged file, and step 5 returned whatever default was in
    /// place — making the UI "auto" stick on restart.
    #[test]
    fn live_edit_compression_mode_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        // Step 0: empty disk (first start) — user has never saved.
        assert!(load_agent_config(dir.path()).unwrap().is_none());

        // Step 1-2: Desktop PUT pushes "manual" through the gateway.
        let push = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };

        // Step 3: cli.rs persistence — load, apply_to, save.
        let mut cfg = load_agent_config(dir.path())
            .unwrap()
            .unwrap_or_default();
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        // Step 4: simulate restart — load from disk again.
        let reloaded = load_agent_config(dir.path())
            .unwrap()
            .expect("file should exist after save");
        assert_eq!(
            reloaded.tool_result_compression_mode.as_deref(),
            Some("manual"),
            "restart must preserve the user's tool_result_compression_mode"
        );

        // Step 5: From<&AgentConfig> projects the persisted value back
        // into runtime_overrides for the next session.
        let ov = RuntimeConfigOverrides::from(&reloaded);
        assert_eq!(ov.tool_result_compression_mode.as_deref(), Some("manual"));
    }

    /// Companion test: `keep_recent_n` (which is boot-only and has no
    /// proto plumb-through) must survive a partial live-edit push. The
    /// `apply_to` semantic — only write `Some` fields, preserve the rest
    /// — is what guarantees this.
    #[test]
    fn live_edit_preserves_unrelated_disk_fields() {
        let dir = tempfile::tempdir().unwrap();

        // Pre-existing disk: user set keep_recent_n=7 before.
        let pre = AgentConfig {
            tool_result_keep_recent_n: Some(7),
            ..Default::default()
        };
        save_agent_config(dir.path(), &pre).unwrap();

        // Live-edit push: only changes compression_mode, NOT
        // keep_recent_n.
        let push = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };

        // Round-trip through cli.rs persistence logic.
        let mut cfg = load_agent_config(dir.path())
            .unwrap()
            .expect("should load existing");
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        // Reload and verify: only compression_mode changed.
        let after = load_agent_config(dir.path())
            .unwrap()
            .expect("file should exist");
        assert_eq!(
            after.tool_result_keep_recent_n, Some(7),
            "keep_recent_n must survive an unrelated push"
        );
        assert_eq!(
            after.tool_result_compression_mode.as_deref(),
            Some("manual"),
            "compression_mode must reflect the push"
        );
    }

    // ── ADR-032 C4a: tool_result_soft_threshold_chars live-edit ─────
    //
    // Regression coverage for the bug pattern that bit
    // `tool_result_compression_mode` in commit 75e0349: the field was
    // destructured into `_` in cli.rs, so a live-edit push carrying
    // `Some(1024)` was silently dropped before reaching
    // `RuntimeConfigOverrides` — and therefore never landed in
    // `agent_config.json`. The user would set the threshold in the UI,
    // it would appear to save, but a restart would revert to 2048.
    //
    // The fix: cli.rs now binds `tool_result_soft_threshold_chars`
    // directly (not `_`) so the value flows into `RuntimeConfigOverrides`
    // → `apply_to` → `save_agent_config`. These tests pin that contract
    // so it can't regress.

    /// Mirror of `live_edit_compression_mode_round_trips_through_disk`
    /// for the soft-threshold field: a push carrying `Some(1024)` must
    /// round-trip through the load/apply_to/save chain and survive a
    /// restart. Pre-fix, this test failed because the field was
    /// destructured into `_` in cli.rs and never reached the override.
    #[test]
    fn live_edit_soft_threshold_chars_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();

        // Step 1-2: Desktop PUT pushes 1024 through the gateway.
        let push = RuntimeConfigOverrides {
            tool_result_soft_threshold_chars: Some(1024),
            ..Default::default()
        };

        // Step 3: cli.rs persistence — load, apply_to, save.
        let mut cfg = load_agent_config(dir.path())
            .unwrap()
            .unwrap_or_default();
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        // Step 4: simulate restart — load from disk again.
        let reloaded = load_agent_config(dir.path())
            .unwrap()
            .expect("file should exist after save");
        assert_eq!(
            reloaded.tool_result_soft_threshold_chars,
            Some(1024),
            "restart must preserve the user's tool_result_soft_threshold_chars"
        );

        // Step 5: From<&AgentConfig> projects the persisted value back
        // into runtime_overrides for the next session, which is what
        // `AgentCore::tool_result_soft_threshold_chars()` reads.
        let ov = RuntimeConfigOverrides::from(&reloaded);
        assert_eq!(ov.tool_result_soft_threshold_chars, Some(1024));
    }

    /// Companion: a push that does **not** carry
    /// `tool_result_soft_threshold_chars` (None) must NOT clobber a
    /// previously-saved value. This is the partial-PUT semantic that
    /// keeps an unrelated edit (e.g. changing model) from resetting the
    /// threshold back to the default.
    #[test]
    fn live_edit_preserves_soft_threshold_chars_across_unrelated_push() {
        let dir = tempfile::tempdir().unwrap();

        // Pre-existing disk: user set threshold=512 before.
        let pre = AgentConfig {
            tool_result_soft_threshold_chars: Some(512),
            ..Default::default()
        };
        save_agent_config(dir.path(), &pre).unwrap();

        // Live-edit push: only changes compression_mode, NOT threshold.
        let push = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("auto".into()),
            ..Default::default()
        };

        // Round-trip through cli.rs persistence logic.
        let mut cfg = load_agent_config(dir.path())
            .unwrap()
            .expect("should load existing");
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        // Reload and verify: threshold survived, mode updated.
        let after = load_agent_config(dir.path())
            .unwrap()
            .expect("file should exist");
        assert_eq!(
            after.tool_result_soft_threshold_chars,
            Some(512),
            "soft_threshold_chars must survive an unrelated push"
        );
        assert_eq!(
            after.tool_result_compression_mode.as_deref(),
            Some("auto"),
            "compression_mode must reflect the push"
        );
    }

    /// Companion: documents the **partial-push** semantic for
    /// `tool_result_soft_threshold_chars`. When the Desktop UI sends
    /// `None` (i.e. omits the field from the PUT body — see
    /// `AgentSetupTab.tsx` save-guard at
    /// `if (profile.toolResultSoftThresholdChars !== undefined && > 0)`),
    /// the runtime receives a `RuntimeConfigUpdate` without this field.
    /// `apply_to` must then **preserve** the on-disk value rather than
    /// wiping it.
    ///
    /// Companion: documents the **partial-push** semantic for
    /// `tool_result_soft_threshold_chars`. When the Desktop UI does not
    /// include the field in the PUT body (because the user didn't touch
    /// the input), the runtime receives a `RuntimeConfigUpdate` with
    /// the corresponding `RuntimeConfigOverrides` slot = `None`, and
    /// `apply_to` must **preserve** the on-disk value.
    ///
    /// The `compression_mode` companion case
    /// (`live_edit_preserves_unrelated_disk_fields`) uses
    /// `tool_result_keep_recent_n` because it has no live-edit wire
    /// slot at all; this test instead uses `compression_mode` as the
    /// "actually-pushed" field to exercise the same partial-PUT path.
    #[test]
    fn live_edit_partial_push_preserves_soft_threshold_chars() {
        let dir = tempfile::tempdir().unwrap();

        // Pre-existing disk: user had a custom threshold.
        let pre = AgentConfig {
            tool_result_soft_threshold_chars: Some(4096),
            ..Default::default()
        };
        save_agent_config(dir.path(), &pre).unwrap();

        // Live-edit push: only changes `compression_mode`. The
        // threshold field is *omitted* (None), which on the frontend
        // means the user didn't touch the input.
        let push = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };

        let mut cfg = load_agent_config(dir.path()).unwrap().unwrap();
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        // Reload: the threshold is preserved, the mode is updated.
        let after = load_agent_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            after.tool_result_soft_threshold_chars,
            Some(4096),
            "partial push with threshold=None must preserve the on-disk value"
        );
        assert_eq!(
            after.tool_result_compression_mode.as_deref(),
            Some("manual"),
            "the actually-pushed field is updated"
        );
    }

    // ── ADR-032 C4b: explicit-value persistence ─────────────────────
    //
    // Regression coverage for the user's reported bug:
    //   "改成auto，怎么配置文件写的是空"
    //
    // Two separate checks pin the contract:
    //
    //   1. `compression_mode_explicit_value_persists_verbatim` — when the
    //      user picks "auto" in the UI, the runtime must write
    //      `"tool_result_compression_mode": "auto"` to disk, **not**
    //      collapse it to `null` / `""` / `None`. The runtime accessor
    //      `compression_mode()` already maps non-`"manual"` to `Auto`,
    //      so the on-disk form is the user's explicit choice — round-
    //      trippable, self-documenting, never silently mutated.
    //
    //   2. `compression_mode_unset_round_trips_as_none` — `None` on the
    //      push side means "don't change" (partial-PUT semantics), so
    //      a previous on-disk value is preserved across an unrelated
    //      save. Mirrors `live_edit_partial_push_preserves_*` above.

    /// When the user picks `"auto"` and the runtime persists, the file
    /// MUST contain `"auto"` (not `""`, not missing, not `null`). This
    /// pins the contract that `tool_result_compression_mode = Some("auto")`
    /// is **not** collapsed to `None` at any layer.
    #[test]
    fn compression_mode_explicit_value_persists_verbatim() {
        let dir = tempfile::tempdir().unwrap();

        let push = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("auto".into()),
            ..Default::default()
        };

        let mut cfg = load_agent_config(dir.path())
            .unwrap()
            .unwrap_or_default();
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        // Reload and verify the **exact** wire value is on disk.
        let reloaded = load_agent_config(dir.path())
            .unwrap()
            .expect("file should exist after save");
        assert_eq!(
            reloaded.tool_result_compression_mode.as_deref(),
            Some("auto"),
            "explicit user value must round-trip verbatim; never collapse to None / \"\""
        );

        // From<&AgentConfig> must project 1:1 (no normalization).
        let ov = RuntimeConfigOverrides::from(&reloaded);
        assert_eq!(ov.tool_result_compression_mode.as_deref(), Some("auto"));
    }

    /// Mirror of the above for `"manual"` — same 1:1 contract.
    #[test]
    fn compression_mode_explicit_manual_persists_verbatim() {
        let dir = tempfile::tempdir().unwrap();

        let push = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };

        let mut cfg = load_agent_config(dir.path())
            .unwrap()
            .unwrap_or_default();
        push.apply_to(&mut cfg);
        save_agent_config(dir.path(), &cfg).unwrap();

        let reloaded = load_agent_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            reloaded.tool_result_compression_mode.as_deref(),
            Some("manual"),
        );
    }
}
