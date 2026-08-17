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
    /// ADR-? / Win11-MCP-ToolsBugFix: user-selected subset of catalog names that
    /// are **active** for this agent. When `None` (e.g. older files written
    /// before the field existed) the merged list is treated as fully active
    /// for backward compatibility. When `Some(_)`, only matching entries from
    /// `merged()` are reported as active — e.g. `Some(vec![]) == "no servers
    /// active"`. This is the field persisted by `PUT /agents/{id}/mcp-servers`
    /// (per-agent activation toggle in the Desktop Tools panel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_names: Option<Vec<String>>,
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

    /// Names of servers that should be considered **active** for this agent.
    ///
    /// Resolution order:
    /// 1. If `active_names` is `Some`, return that list verbatim (preserving
    ///    caller's ordering for the Tools panel display).
    /// 2. If `active_names` is `None` (file without the field — initial state
    ///    or legacy migration), return an empty list — no servers are active
    ///    by default. The user must explicitly enable servers via the Tools
    ///    panel.
    ///
    /// `Some(vec![])` therefore means **no servers active** (the user
    /// explicitly unchecked every catalog item).
    pub fn active_server_names(&self) -> Vec<String> {
        match &self.active_names {
            Some(names) => names.clone(),
            None => vec![],
        }
    }

    /// Catalog ∪ local, filtered by `active_server_names()`.
    ///
    /// **This is the single source of truth for "which MCP servers should
    /// be connected for this agent".** All MCP connection code paths
    /// (startup auto-connect, MCP config change watcher, etc.) MUST
    /// go through this method — never `merged()` directly — otherwise
    /// the user's Tools-panel toggles are silently ignored.
    ///
    /// Tool names whose entry appears in `merged()` but not in
    /// `active_server_names()` are filtered out. When
    /// `active_server_names()` is empty (either `None` legacy state or
    /// `Some(vec![])` user-unchecked-everything), the result is empty.
    pub fn active_merged(&self) -> Vec<McpServerConfigDef> {
        let active_names = self.active_server_names();
        let active: std::collections::HashSet<&str> =
            active_names.iter().map(String::as_str).collect();
        self.merged()
            .into_iter()
            .filter(|s| active.contains(s.name.as_str()))
            .collect()
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


    /// ADR-052: Whether context_retrieve + context_abandon tools are registered.
    ///
    /// `None` falls through to `true` (default enabled). When `true`, the
    /// LLM can autonomously compress and retrieve tool results. When `false`,
    /// neither tool is registered and the LLM cannot compress.
    ///
    /// Boot-only: consumed at session creation time when building the
    /// builtin tool list. Changes take effect on the next session restore
    /// or process restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_compression_enabled: Option<bool>,
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
///
/// Wire/JSON shape:
/// - **Persisted file** (`agent_tools.json`): `{ "name": "...", "enabled": bool }`
///   — the platform-protection marker is intentionally NOT written to disk.
///   Enforce lives in [`crate::tools::registry::BuiltinToolEntry::with_resolved_enabled`]
///   (ADR-029 §"Initialization" + ADR-052).
/// - **Runtime API response** (`GET /api/agents/{id}/tools`): the same
///   `{name, enabled}` pair **plus** an optional `platform_protected: true`
///   field when the entry's name is in
///   [`crate::tools::registry::PLATFORM_PROTECTED_TOOLS`]. The desktop UI
///   reads this flag to render a non-interactive `Switch` with a
///   "managed by the Tool Compression global toggle" tooltip, avoiding
///   the previous UX where users could toggle force-enabled tools and
///   see their write get silently ignored (and the file get polluted).
///
/// See ADR-029 (per-agent builtin tools) and ADR-052 (tool compression,
/// which introduces the platform-protected tool set).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentToolEntry {
    /// Tool name (matches `Tool::name()`).
    pub name: String,
    /// Whether this tool is enabled for the agent.
    /// Defaults to `true` when missing in JSON for backward-compatible
    /// additions of new fields.
    #[serde(default = "default_tool_entry_enabled")]
    pub enabled: bool,
}

// Hand-rolled `Serialize` so we can emit the `platform_protected` flag
// in API responses without writing it to the persisted file. The flag
// is computed on the fly from `PLATFORM_PROTECTED_TOOLS` (single source
// of truth in `tools/registry.rs`) so adding a new protected tool only
// requires editing that one list — every caller picks it up.
impl Serialize for AgentToolEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let platform_protected = crate::tools::registry::PLATFORM_PROTECTED_TOOLS
            .contains(&self.name.as_str());
        let n = if platform_protected { 3 } else { 2 };
        let mut s = serializer.serialize_struct("AgentToolEntry", n)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("enabled", &self.enabled)?;
        if platform_protected {
            s.serialize_field("platform_protected", &true)?;
        }
        s.end()
    }
}

fn default_tool_entry_enabled() -> bool {
    true
}

/// On-disk representation of [`AgentToolsConfig`].
///
/// **Why this exists:** `AgentToolEntry`'s `Serialize` impl emits the
/// `platform_protected: true` hint for tools in
/// `PLATFORM_PROTECTED_TOOLS` so the desktop UI can render a
/// non-interactive Switch (ADR-052). But that hint is a presentation
/// concern only — it must NOT be persisted to `agent_tools.json`. This
/// dedicated shape guarantees the on-disk format stays `{name, enabled}`
/// (the format that [`load_agent_tools_config`] knows how to read), and
/// that future API-only fields added to `AgentToolEntry` cannot leak
/// into the persisted file by accident.
///
/// Use [`save_agent_tools_config`] (which converts via `From<&AgentToolsConfig>`)
/// — never serialize [`AgentToolsConfig`] directly to disk.
#[derive(Debug, Serialize)]
struct PersistedToolsConfig<'a> {
    tools: Vec<PersistedToolEntry<'a>>,
}

#[derive(Debug, Serialize)]
struct PersistedToolEntry<'a> {
    name: &'a str,
    enabled: bool,
}

impl<'a> From<&'a AgentToolsConfig> for PersistedToolsConfig<'a> {
    fn from(cfg: &'a AgentToolsConfig) -> Self {
        Self {
            tools: cfg
                .tools
                .iter()
                .map(|e| PersistedToolEntry {
                    name: &e.name,
                    enabled: e.enabled,
                })
                .collect(),
        }
    }
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

    // Persist via the dedicated on-disk shape (no `platform_protected`
    // hint). The runtime `AgentToolEntry::serialize` injects
    // `platform_protected: true` for tools in `PLATFORM_PROTECTED_TOOLS`
    // so the desktop UI can render non-interactive Switches — but that
    // flag is a presentation hint, never load-bearing. Writing it to
    // disk would be wasted bytes at best, and a maintenance trap at
    // worst (every persisted file would carry platform-internal state
    // the user can never usefully edit). See ADR-029 + ADR-052.
    let on_disk: PersistedToolsConfig<'_> = PersistedToolsConfig::from(cfg);
    let json = serde_json::to_string_pretty(&on_disk)
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
/// - Platform-protected tools (see
///   [`crate::tools::registry::PLATFORM_PROTECTED_TOOLS`]) are
///   always enabled regardless of persisted state — enforced via
///   [`crate::tools::registry::BuiltinToolEntry::with_resolved_enabled`]
///   so this function and `apply_builtin_tools_patch` cannot disagree.
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
            let user_wants = persisted_map
                .get(name.as_str())
                .copied()
                .unwrap_or(false); // new tool → disabled (opt-in)
            // Resolve to the canonical enabled flag — platform-protected
            // tools are force-enabled even if the persisted file says false.
            resolve_enabled(name, user_wants)
        })
        .collect()
}

/// Apply a partial `RuntimeConfigUpdate.builtin_tools_enabled` payload
/// (only listed tool names are touched) onto an existing
/// `AgentToolsConfig` in place. Returns the new full list — a copy of
/// the caller may then persist via `save_agent_tools_config`.
///
/// Tool names in `patch` that are not present in `current` are
/// silently ignored (defensive: future tools should arrive via
/// `merge_tools_config` at startup, not via this incremental path).
///
/// Platform-protected tools are force-enabled regardless of the
/// patch value — see [`resolve_enabled`].
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
            let patched = patch_map
                .get(e.name.as_str())
                .copied()
                .unwrap_or(e.enabled);
            // Resolve to the canonical enabled flag — platform-protected
            // tools are force-enabled even if the user tried to disable them.
            resolve_enabled(&e.name, patched)
        })
        .collect()
}

/// Single source of truth for "should this builtin tool be enabled?".
///
/// Platform-protected tools (see
/// [`crate::tools::registry::PLATFORM_PROTECTED_TOOLS`]) are always
/// enabled. All other tools use the user-supplied value unchanged.
///
/// Routes through the same [`BuiltinToolEntry`]-level helper that
/// `ToolRegistry::activate` uses, so the cold-start path
/// (`merge_tools_config`) and the hot-update path
/// (`apply_builtin_tools_patch`) and the runtime registry
/// initialization agree on every tool — no duplicated
/// `PLATFORM_TOOLS.contains(...)` decision scattered across modules.
fn resolve_enabled(name: &str, user_persisted_enabled: bool) -> AgentToolEntry {
    let final_enabled = crate::tools::registry::PLATFORM_PROTECTED_TOOLS
        .contains(&name)
        || user_persisted_enabled;
    AgentToolEntry::new(name, final_enabled)
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
            active_names: None,
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
/// This is used by the `acowork/global/mcps` MQTT handler: Gateway pushes
/// catalog MCPs, and we must preserve the user's `active_names` selection
/// and the `local` list (agent-installed MCPs). Reads the current config,
/// replaces only `catalog`, and saves back.
///
/// Note: previously this also reset `active_names = None`, which clobbered
/// the user's Tools-panel selection on every Gateway catalog push (e.g.
/// every Gateway restart). That reset is now gone — the catalog update is
/// orthogonal to the activation toggle.
pub fn save_agent_mcp_config_catalog(
    work_dir: &Path,
    catalog_servers: &[McpServerConfigDef],
) -> Result<(), String> {
    // Load current config to preserve local entries AND active_names.
    let current = load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_mcp.json, using default");
            None
        })
        .unwrap_or_default();

    let updated = AgentMcpConfig {
        catalog: catalog_servers.to_vec(),
        local: current.local,
        active_names: current.active_names,
    };

    save_agent_mcp_config(work_dir, &updated)
}

/// Load active MCP configs (catalog ∪ local, filtered by `active_names`)
/// from workspace/config/agent_mcp.json.
///
/// **This is the single entry point for MCP connection code.** It honors
/// the user's Tools-panel toggles via `active_server_names()`. Returns an
/// empty vec if no servers are active (either no file, legacy state with
/// `active_names = None`, or user explicitly unchecked everything).
pub fn load_active_mcp_configs(work_dir: &Path) -> Vec<McpServerConfigDef> {
    load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_mcp.json for active-merge, using empty list");
            None
        })
        .unwrap_or_default()
        .active_merged()
}

/// Load merged MCP configs (catalog + local) from workspace/config/agent_mcp.json.
///
/// **Deprecated**: this returns the unconditional union of catalog + local
/// and IGNORES `active_names`. New code MUST use [`load_active_mcp_configs`]
/// so users' Tools-panel toggles are honored at MCP connection time.
///
/// Kept only for the persistence / introspection layer that legitimately
/// needs to see the unconditional merged list (e.g. validating that a
/// user-selected name is in fact present in catalog ∪ local). The deprecation
/// lint will catch any new caller trying to use it for connection.
#[deprecated(
    since = "0.1.0",
    note = "ignores active_names — use load_active_mcp_configs so user toggles are honored"
)]
pub fn load_merged_mcp_configs(work_dir: &Path) -> Vec<McpServerConfigDef> {
    load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_mcp.json for merge, using empty list");
            None
        })
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

/// Save only the catalog portion of agent search config.
///
/// This is the search equivalent of `save_agent_mcp_config_catalog`:
/// called by the MQTT poll loop when it receives `acowork/global/searches`.
/// Replaces only the `catalog` field, preserving `providers`.
pub fn save_agent_search_config_catalog(
    work_dir: &Path,
    catalog_providers: &[acowork_core::protocol::SearchProviderListItem],
) -> Result<(), String> {
    let current = load_agent_search_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_search.json, using default");
            None
        })
        .unwrap_or_default();

    let updated = acowork_core::protocol::AgentSearchConfig {
        providers: current.providers,
        catalog: catalog_providers.to_vec(),
    };

    save_agent_search_config(work_dir, &updated)
}

// ── Per-agent provider config ────────────────────────────────────────────

/// Filename for per-agent provider config in the workspace config directory.
const AGENT_PROVIDER_CONFIG_FILE: &str = "agent_provider.json";

/// Build the path to the agent provider config file.
fn provider_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_PROVIDER_CONFIG_FILE)
}

/// Load per-agent provider config from workspace/config/agent_provider.json.
///
/// Returns `None` if the file does not exist (no providers configured yet).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_provider_config(
    work_dir: &Path,
) -> Result<Option<acowork_core::protocol::AgentProviderConfig>, String> {
    let path = provider_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: acowork_core::protocol::AgentProviderConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        provider_count = cfg.providers.len(),
        version = cfg.version,
        "Loaded agent provider config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent provider config to workspace/config/agent_provider.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_provider_config(
    work_dir: &Path,
    cfg: &acowork_core::protocol::AgentProviderConfig,
) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!("Failed to create config dir {}: {}", config_dir.display(), e)
    })?;

    let path = provider_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent provider config: {}", e))?;

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
        version = cfg.version,
        "Saved agent provider config to workspace"
    );

    Ok(())
}

/// Save provider config from an MQTT `AvailableProviders` payload.
///
/// This is the provider equivalent of `save_agent_mcp_config_catalog`:
/// called by the MQTT poll loop when it receives `acowork/global/providers`.
/// Only the `providers` list and `version` are replaced — if the file
/// exists, the new payload fully replaces the old one.
pub fn save_agent_provider_config_from_available(
    work_dir: &Path,
    providers: &[acowork_core::protocol::ProviderListItem],
    version: u64,
) -> Result<(), String> {
    let cfg = acowork_core::protocol::AgentProviderConfig {
        providers: providers.to_vec(),
        version,
    };
    save_agent_provider_config(work_dir, &cfg)
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
        
            active_names: None,};

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
        
            active_names: None,};

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
        
            active_names: None,};

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
        
            active_names: None,};

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
        
            active_names: None,};

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

            active_names: None,};
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

    /// Regression: Gateway pushes catalog on every restart (retained
    /// `acowork/global/mcps`). `save_agent_mcp_config_catalog` is invoked
    /// from the Runtime MQTT poll loop in response. The function MUST
    /// preserve the user's `active_names` selection (Tools panel
    /// checkboxes), not reset it. Otherwise every Gateway restart
    /// silently clears the user's MCP activation state.
    ///
    /// History: the original implementation set `active_names = None`
    /// unconditionally, which is exactly the regression this test guards
    /// against. See `mqtt/client.rs` poll loop for the call site.
    #[test]
    fn save_catalog_preserves_active_names() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        // User has previously activated context7 in the Tools panel.
        let initial = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "context7".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "npx".into(),
                args: vec!["-y".into(), "@upstash/context7-mcp".into()],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            }],
            local: vec![],
            active_names: Some(vec!["context7".into()]),
        };
        save_agent_mcp_config(dir.path(), &initial).unwrap();

        // Gateway republishes catalog with a new tool added.
        let new_catalog = vec![
            McpServerConfigDef {
                name: "context7".into(),
                transport: McpTransportDef::Stdio,
                url: None,
                command: "npx".into(),
                args: vec!["-y".into(), "@upstash/context7-mcp".into()],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            },
            McpServerConfigDef {
                name: "new-mcp".into(),
                transport: McpTransportDef::Http,
                url: Some("http://example.com".into()),
                command: String::new(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
            },
        ];
        save_agent_mcp_config_catalog(dir.path(), &new_catalog).unwrap();

        // After sync: catalog replaced, but active_names survives.
        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.catalog.len(), 2);
        assert_eq!(reloaded.catalog[1].name, "new-mcp");
        assert_eq!(
            reloaded.active_names,
            Some(vec!["context7".into()]),
            "active_names must survive a catalog sync (Gateway restart would otherwise wipe user MCP selection)"
        );
    }

    #[test]
    #[allow(deprecated)] // exercising the deprecated loader explicitly
    fn load_merged_mcp_configs_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let merged = load_merged_mcp_configs(dir.path());
        assert!(merged.is_empty());
    }

    // ── Bug 1 regression suite (MCP active_merged) ───────────────────
    //
    // These pin the semantics of `AgentMcpConfig::active_merged()` and
    // `load_active_mcp_configs()` so the user-toggles-active-but-runtime-
    // connects-all bug cannot recur. Any future change that breaks these
    // is by definition a regression.

    fn mcp_def(name: &str) -> McpServerConfigDef {
        McpServerConfigDef {
            name: name.to_string(),
            transport: McpTransportDef::Stdio,
            url: None,
            command: "npx".to_string(),
            args: vec![],
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            tool_timeout_secs: None,
        }
    }

    #[test]
    fn active_merged_returns_only_user_selected_entries() {
        // User toggled context7 ON, playwright OFF in the Tools panel.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: Some(vec!["context7".into()]),
        };
        let active = cfg.active_merged();
        assert_eq!(active.len(), 1, "only context7 should be active");
        assert_eq!(active[0].name, "context7");
    }

    #[test]
    fn active_merged_empty_when_user_unchecked_all() {
        // User explicitly unchecked every catalog item — the "no MCP
        // active" state surfaced in Bug 1. Result MUST be empty so the
        // startup auto-connect path connects nothing.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: Some(vec![]),
        };
        assert!(cfg.active_merged().is_empty());
    }

    #[test]
    fn active_merged_empty_when_active_names_field_missing() {
        // Legacy file without `active_names` field (e.g. pre-ADR-? save).
        // `active_server_names()` returns [] for None, so we must
        // connect nothing — opt-in semantics for MCP.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: None,
        };
        assert!(
            cfg.active_merged().is_empty(),
            "legacy file (no active_names) must default to no servers active"
        );
    }

    #[test]
    fn active_merged_local_overrides_catalog_for_same_name() {
        // Local entry shadows catalog entry with the same name and must
        // remain active even if user listed only the catalog name.
        // (active_names is a name set, not a {catalog,local} pair.)
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("context7")],
            local: vec![{
                let mut d = mcp_def("context7");
                d.command = "local-context7".to_string();
                d
            }],
            active_names: Some(vec!["context7".into()]),
        };
        let active = cfg.active_merged();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].command, "local-context7", "local must shadow catalog");
    }

    #[test]
    fn active_merged_drops_unknown_active_names() {
        // If a stale `active_names` references a catalog entry that no
        // longer exists (e.g. catalog push removed it), active_merged
        // must silently skip it — not panic, not include garbage.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("context7")],
            local: vec![],
            active_names: Some(vec!["context7".into(), "ghost_mcp".into()]),
        };
        let active = cfg.active_merged();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "context7");
    }

    #[test]
    fn load_active_mcp_configs_honors_active_names() {
        // End-to-end: write a config file where the user has unchecked
        // every catalog MCP, then verify the loader returns the empty
        // list. This is the exact Bug 1 repro path.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: Some(vec![]), // user unchecked all
        };
        save_agent_mcp_config(dir.path(), &cfg).unwrap();

        let active = load_active_mcp_configs(dir.path());
        assert!(
            active.is_empty(),
            "Bug 1 regression: load_active_mcp_configs returned {:?}",
            active.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // ── Bug 2 regression suite (PLATFORM_TOOLS force-enable) ─────────
    //
    // Pin the semantics that platform-protected tools
    // (`context_retrieve`, `context_abandon`) are force-enabled through
    // every write path: cold-start merge, hot-update patch, registry
    // activate. Any change that breaks these is by definition a
    // regression.

    #[test]
    fn merge_tools_config_force_enables_platform_tools() {
        // Bug 2 scenario: persisted file says context_retrieve is
        // disabled (legacy data, hand-edited file, or hostile client).
        // Cold-start merge must NOT honor that — platform tools are
        // force-enabled regardless of persisted state.
        let code = vec![
            "context_retrieve".to_string(),
            "context_abandon".to_string(),
            "shell".to_string(),
        ];
        let persisted = vec![
            AgentToolEntry::new("context_retrieve", false), // user-disabled
            AgentToolEntry::new("context_abandon", false),  // user-disabled
            AgentToolEntry::new("shell", false),
        ];
        let merged = merge_tools_config(&code, &persisted);
        let map: std::collections::HashMap<String, bool> =
            merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert!(map["context_retrieve"], "PLATFORM tool must be force-enabled");
        assert!(map["context_abandon"], "PLATFORM tool must be force-enabled");
        assert!(!map["shell"], "non-platform tool follows user choice");
    }

    #[test]
    fn merge_tools_config_force_enables_missing_platform_tools() {
        // First start: persisted file absent → context_retrieve not in
        // persisted at all. merge_tools_config must still emit it as
        // enabled (platform protection applies even on cold start with
        // no user history).
        let code = vec![
            "context_retrieve".to_string(),
            "context_abandon".to_string(),
        ];
        let merged = merge_tools_config(&code, &[]);
        assert!(merged.iter().all(|e| e.enabled), "platform tools default to enabled");
    }

    #[test]
    fn apply_builtin_tools_patch_force_enables_platform_tools() {
        // PUT /builtin-tools path: user explicitly disables
        // context_retrieve via frontend. Patch must NOT honor that —
        // platform tools are force-enabled regardless of patch value.
        let current = vec![
            AgentToolEntry::new("context_retrieve", true),
            AgentToolEntry::new("context_abandon", true),
            AgentToolEntry::new("shell", true),
        ];
        let patch = vec![
            AgentToolEntry::new("context_retrieve", false),
            AgentToolEntry::new("context_abandon", false),
            AgentToolEntry::new("shell", false),
        ];
        let next = apply_builtin_tools_patch(&current, &patch);
        let map: std::collections::HashMap<String, bool> =
            next.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert!(map["context_retrieve"]);
        assert!(map["context_abandon"]);
        assert!(!map["shell"]);
    }

    #[test]
    fn merge_and_patch_agree_on_platform_tools() {
        // The whole point of unification: cold-start (merge) and
        // hot-update (patch) must agree on platform tools. If one
        // returns `enabled=true` and the other returns `enabled=false`
        // for the same persisted entry, the bug recurs.
        let code = vec!["context_retrieve".to_string(), "shell".to_string()];
        let persisted = vec![AgentToolEntry::new("context_retrieve", false)];

        let merged = merge_tools_config(&code, &persisted);
        let ctx_enabled = merged.iter().find(|e| e.name == "context_retrieve").unwrap().enabled;

        let patch = vec![AgentToolEntry::new("context_retrieve", false)];
        let patched = apply_builtin_tools_patch(&persisted, &patch);
        let patch_enabled = patched.iter().find(|e| e.name == "context_retrieve").unwrap().enabled;

        assert_eq!(ctx_enabled, patch_enabled, "cold-start and hot-update must agree");
        assert!(ctx_enabled, "platform tool must be enabled in both paths");
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

    // ── platform_protected serialization hint (ADR-029 + ADR-052) ───

    /// Non-protected tool: serialized JSON must be exactly `{name, enabled}`,
    /// no `platform_protected` field at all (otherwise we'd bloat the
    /// persisted file and force migrations on every release).
    #[test]
    fn agent_tool_entry_serialize_omits_platform_protected_for_unprotected() {
        let entry = AgentToolEntry::new("memory_recall", true);
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();
        let obj = json.as_object().expect("must be a JSON object");
        assert_eq!(obj.len(), 2, "must have exactly 2 keys, got: {:?}", obj.keys().collect::<Vec<_>>());
        assert_eq!(obj.get("name").unwrap(), "memory_recall");
        assert_eq!(obj.get("enabled").unwrap(), true);
        assert!(
            !obj.contains_key("platform_protected"),
            "platform_protected must be omitted for non-protected tools"
        );
    }

    /// Protected tool (`context_retrieve`, `context_abandon` — see
    /// `PLATFORM_PROTECTED_TOOLS`): serialized JSON must include
    /// `"platform_protected": true` so the desktop can render a
    /// non-interactive Switch + tooltip.
    #[test]
    fn agent_tool_entry_serialize_includes_platform_protected_for_protected() {
        for protected_name in crate::tools::registry::PLATFORM_PROTECTED_TOOLS {
            let entry = AgentToolEntry::new(*protected_name, true);
            let json: serde_json::Value = serde_json::to_value(&entry).unwrap();
            let obj = json.as_object().expect("must be a JSON object");
            assert_eq!(
                obj.get("platform_protected").and_then(|v| v.as_bool()),
                Some(true),
                "platform_protected must be emitted as `true` for {:?}",
                protected_name
            );
            assert_eq!(obj.get("name").unwrap(), *protected_name);
            assert_eq!(obj.get("enabled").unwrap(), true);
        }
    }

    /// The persisted file format must remain `{name, enabled}` — even for
    /// protected tools — so `save_agent_tools_config` never writes a
    /// `platform_protected` key. This guards against accidentally
    /// persisting the presentation hint.
    #[test]
    fn save_agent_tools_config_never_writes_platform_protected_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentToolsConfig {
            tools: vec![
                AgentToolEntry::new("context_retrieve", true),
                AgentToolEntry::new("context_abandon", false),
                AgentToolEntry::new("memory_recall", true),
            ],
        };
        save_agent_tools_config(dir.path(), &cfg).unwrap();

        let raw = std::fs::read_to_string(
            dir.path().join("config").join(AGENT_TOOLS_CONFIG_FILE),
        )
        .unwrap();
        assert!(
            !raw.contains("platform_protected"),
            "persisted file must not contain platform_protected; got:\n{}",
            raw
        );
        // Sanity: shape is preserved.
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let tools = parsed["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
    }

    /// Forward compatibility: a hand-crafted JSON payload from a newer
    /// client that *does* include `platform_protected` must still
    /// deserialize cleanly — the field is a presentation hint, never
    /// load-bearing.
    #[test]
    fn agent_tool_entry_deserialize_ignores_unexpected_platform_protected() {
        let json = r#"{"name":"memory_recall","enabled":true,"platform_protected":true}"#;
        let entry: AgentToolEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name, "memory_recall");
        assert!(entry.enabled);
        // We don't surface a field; "platform_protected" must not be
        // honored on the load path (the runtime enforcement is
        // independent, in `with_resolved_enabled`).
    }
}