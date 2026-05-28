//! Resource cache — versioned provider, MCP, and search provider lists for AgentHello diff sync.
//!
//! Gateway maintains three versioned resource lists on disk:
//! - `provider_list.json`: `{ version: N, providers: [ProviderListItem, ...] }`
//! - `mcp_list.json`:    `{ version: N, servers: [McpListItem, ...] }`
//! - `search_list.json`: `{ version: N, providers: [SearchProviderListItem, ...] }`
//!
//! These are loaded into memory at startup. HTTP handlers rebuild them
//! (version+1) when the user modifies providers, MCP catalog entries, or
//! search provider keys. The AgentHello handler reads the in-memory cache
//! and delivers changed lists to Runtime via version-driven diff sync.
//!
//! ## Key vaults (provider_key_vault / mcp_key_vault / search_key_vault)
//!
//! Key vaults are NOT versioned — they are always delivered in full on
//! every AgentHello. They are built on-the-fly from Vault + MCP catalog
//! (reading decrypted values) rather than cached on disk.

#[cfg(test)]
use std::collections::HashMap;
use std::path::Path;

use rollball_core::protocol::{
    McpKeyEntry, McpListItem, ProviderListItem, ProviderModelEntry,
    SearchKeyEntry, SearchProviderListItem, UserProfileListFile,
};

/// In-memory resource cache loaded at Gateway startup.
///
/// Provider, MCP, Search, and User Profile lists are versioned; keys are
/// always delivered in full and are NOT stored here (built on-the-fly from Vault).
#[derive(Debug, Clone)]
pub struct ResourceCache {
    pub provider_list: ProviderListFile,
    pub mcp_list: McpListFile,
    pub search_list: SearchListFile,
    pub user_profile_list: UserProfileListFile,
}

/// Versioned provider list persisted to disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderListFile {
    pub version: u64,
    pub providers: Vec<ProviderListItem>,
}

/// Versioned MCP server list persisted to disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpListFile {
    pub version: u64,
    pub servers: Vec<McpListItem>,
}

/// Versioned search provider list persisted to disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchListFile {
    pub version: u64,
    pub providers: Vec<SearchProviderListItem>,
}

// ── File paths ─────────────────────────────────────────────────────────

fn provider_list_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("provider_list.json")
}

fn mcp_list_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("mcp_list.json")
}

fn search_list_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("search_list.json")
}

fn user_profile_list_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("user_profiles.json")
}

// ── Loading ────────────────────────────────────────────────────────────

/// Load the resource cache from disk at Gateway startup.
///
/// Returns empty lists with version 0 if files don't exist.
pub fn load_resource_cache(data_dir: &Path) -> ResourceCache {
    let provider_list = load_provider_list(data_dir);
    let mcp_list = load_mcp_list(data_dir);
    let search_list = load_search_list(data_dir);
    let user_profile_list = load_user_profile_list(data_dir);
    tracing::info!(
        provider_count = provider_list.providers.len(),
        provider_version = provider_list.version,
        mcp_count = mcp_list.servers.len(),
        mcp_version = mcp_list.version,
        search_count = search_list.providers.len(),
        search_version = search_list.version,
        user_profile_count = user_profile_list.users.len(),
        user_profile_version = user_profile_list.version,
        "Resource cache loaded"
    );
    ResourceCache {
        provider_list,
        mcp_list,
        search_list,
        user_profile_list,
    }
}

fn load_provider_list(data_dir: &Path) -> ProviderListFile {
    let path = provider_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse provider_list.json, using empty list"
                );
                ProviderListFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("provider_list.json not found, initializing empty");
            ProviderListFile::default()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read provider_list.json, using empty list"
            );
            ProviderListFile::default()
        }
    }
}

fn load_mcp_list(data_dir: &Path) -> McpListFile {
    let path = mcp_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse mcp_list.json, using empty list"
                );
                McpListFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("mcp_list.json not found, initializing empty");
            McpListFile::default()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read mcp_list.json, using empty list"
            );
            McpListFile::default()
        }
    }
}

fn load_search_list(data_dir: &Path) -> SearchListFile {
    let path = search_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse search_list.json, using empty list"
                );
                SearchListFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("search_list.json not found, initializing empty");
            SearchListFile::default()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read search_list.json, using empty list"
            );
            SearchListFile::default()
        }
    }
}

fn load_user_profile_list(data_dir: &Path) -> UserProfileListFile {
    let path = user_profile_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse user_profiles.json, using empty list"
                );
                UserProfileListFile { version: 0, users: Vec::new() }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("user_profiles.json not found, initializing empty");
            UserProfileListFile { version: 0, users: Vec::new() }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read user_profiles.json, using empty list"
            );
            UserProfileListFile { version: 0, users: Vec::new() }
        }
    }
}

// ── Saving ─────────────────────────────────────────────────────────────

/// Save the provider list to disk.
pub fn save_provider_list(data_dir: &Path, list: &ProviderListFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize provider list: {}", e))?;
    std::fs::write(provider_list_path(data_dir), &json)
        .map_err(|e| format!("Failed to write provider_list.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.providers.len(),
        "Provider list saved"
    );
    Ok(())
}

/// Save the MCP list to disk.
pub fn save_mcp_list(data_dir: &Path, list: &McpListFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize MCP list: {}", e))?;
    std::fs::write(mcp_list_path(data_dir), &json)
        .map_err(|e| format!("Failed to write mcp_list.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.servers.len(),
        "MCP list saved"
    );
    Ok(())
}

/// Save the search provider list to disk.
pub fn save_search_list(data_dir: &Path, list: &SearchListFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize search list: {}", e))?;
    std::fs::write(search_list_path(data_dir), &json)
        .map_err(|e| format!("Failed to write search_list.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.providers.len(),
        "Search list saved"
    );
    Ok(())
}

/// Save the user profile list to disk.
pub fn save_user_profile_list(data_dir: &Path, list: &UserProfileListFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize user profile list: {}", e))?;
    std::fs::write(user_profile_list_path(data_dir), &json)
        .map_err(|e| format!("Failed to write user_profiles.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.users.len(),
        "User profile list saved"
    );
    Ok(())
}

// ── Building ───────────────────────────────────────────────────────────

/// Rebuild provider_list.json from all Vault provider entries + models.dev data.
///
/// Called by vault_api.rs handlers after add/update/delete provider key.
/// Updates the in-memory `gw.resource_cache.provider_list` and persists to disk.
pub(crate) async fn rebuild_and_save_provider_cache(
    gw: &mut crate::gateway::state::GatewayState,
    data_dir: &Path,
    _models_cache: &crate::http::models_api::ModelsCache,
) {
    let max_output_tokens = gw
        .config
        .as_ref()
        .map(|c| c.max_output_tokens_limit)
        .unwrap_or(32_768);

    let provider_names = gw.vault.list_providers();
    let mut providers = Vec::with_capacity(provider_names.len());

    for name in &provider_names {
        let entry = match gw.vault.get_provider(name) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Look up protocol type and base API URL from models.dev data.
        // Note: lookup_protocol_info is sync (uses offline data).
        let (protocol_type, api_base_url) =
            crate::http::models_api::lookup_protocol_info(name, None);
        let base_url = entry
            .base_url
            .clone()
            .or(api_base_url)
            .unwrap_or_default();

        // Build model list with capabilities.
        // Priority: user-stored capabilities > models.dev lookup > minimal fallback.
        let mut models = Vec::with_capacity(entry.models.len());
        for model_id in &entry.models {
            let capabilities = if let Some(cap) = entry.model_capabilities.get(model_id) {
                rollball_core::protocol::ModelCapabilitiesInfo::from(cap.clone())
            } else {
                crate::http::models_api::lookup_model_capabilities(name, model_id)
                    .unwrap_or(rollball_core::protocol::ModelCapabilitiesInfo {
                        context_window: 128_000,
                        max_output_tokens: 16_384,
                        max_input_tokens: None,
                        supports_tool_calling: true,
                        supports_reasoning: None,
                        supports_attachment: None,
                        supports_temperature: None,
                        cost: None,
                        modalities: None,
                        name: None,
                        family: None,
                        knowledge_cutoff: None,
                    })
            };
            models.push(ProviderModelEntry {
                id: model_id.clone(),
                capabilities,
                max_output_tokens_limit: max_output_tokens,
            });
        }

        providers.push(ProviderListItem {
            id: name.clone(),
            base_url,
            protocol_type,
            models,
            compact_model: entry.compact_model.clone(),
        });
    }

    let new_version = gw.resource_cache.provider_list.version.wrapping_add(1);
    let new_list = ProviderListFile {
        version: new_version,
        providers,
    };

    if let Err(e) = save_provider_list(data_dir, &new_list) {
        tracing::error!(error = %e, "Failed to save provider_list.json after vault change");
    }
    gw.resource_cache.provider_list = new_list;
}

/// Rebuild mcp_list.json from MCP catalog entries and update in-memory cache.
///
/// Called by mcp_catalog_api.rs handlers after catalog add/update/delete.
pub fn rebuild_and_save_mcp_cache(
    gw: &mut crate::gateway::state::GatewayState,
    data_dir: &Path,
    catalog: &[rollball_core::protocol::McpServerConfigDef],
) {
    let servers = build_mcp_list_from_catalog(catalog);
    let new_version = gw.resource_cache.mcp_list.version.wrapping_add(1);
    let new_list = McpListFile {
        version: new_version,
        servers,
    };

    if let Err(e) = save_mcp_list(data_dir, &new_list) {
        tracing::error!(error = %e, "Failed to save mcp_list.json after catalog change");
    }
    gw.resource_cache.mcp_list = new_list;
}

/// Rebuild search_list.json from search provider configurations.
///
/// Called when user adds/updates/removes search API keys in Vault.
/// Uses the built-in search provider catalog for static metadata, then
/// applies user-configured API keys from Vault.
pub fn rebuild_and_save_search_cache(
    gw: &mut crate::gateway::state::GatewayState,
    data_dir: &Path,
) {
    // Build the search provider list from Vault entries + static catalog
    let mut providers = Vec::new();

    // Iterate through the built-in catalog and pair with vault keys
    let catalog = vec![
        SearchProviderListItem {
            id: "tavily".to_string(),
            name: "Tavily Search".to_string(),
            description: "AI-optimized real-time search API built for AI agents".to_string(),
            requires_api_key: true,
            base_url: "https://api.tavily.com".to_string(),
        },
        SearchProviderListItem {
            id: "brave".to_string(),
            name: "Brave Search".to_string(),
            description: "Privacy-first web search with independent index".to_string(),
            requires_api_key: true,
            base_url: "https://api.search.brave.com".to_string(),
        },
        SearchProviderListItem {
            id: "serper".to_string(),
            name: "Serper.dev".to_string(),
            description: "Fast Google Search API with structured results".to_string(),
            requires_api_key: true,
            base_url: "https://google.serper.dev".to_string(),
        },
        SearchProviderListItem {
            id: "perplexity".to_string(),
            name: "Perplexity Sonar".to_string(),
            description: "AI-powered search with inline citations and answers".to_string(),
            requires_api_key: true,
            base_url: "https://api.perplexity.ai".to_string(),
        },
        SearchProviderListItem {
            id: "exa".to_string(),
            name: "Exa.ai".to_string(),
            description: "AI search engine with extracted web content for LLMs".to_string(),
            requires_api_key: true,
            base_url: "https://api.exa.ai".to_string(),
        },
        SearchProviderListItem {
            id: "google-cse".to_string(),
            name: "Google CSE".to_string(),
            description: "Google Custom Search Engine — requires API key + Search Engine ID (CX)".to_string(),
            requires_api_key: true,
            base_url: "https://www.googleapis.com".to_string(),
        },
        SearchProviderListItem {
            id: "firecrawl".to_string(),
            name: "Firecrawl".to_string(),
            description: "Web scraping and search with markdown output".to_string(),
            requires_api_key: true,
            base_url: "https://api.firecrawl.dev".to_string(),
        },
        SearchProviderListItem {
            id: "searxng".to_string(),
            name: "SearXNG".to_string(),
            description: "Self-hosted privacy-respecting metasearch engine".to_string(),
            requires_api_key: false,
            base_url: String::new(),
        },
    ];

    for item in catalog {
        // Only include providers that have a configured key in the vault.
        // For providers that require an API key (tavily, brave, etc.), this
        // checks that the user has stored a key. For SearXNG (requires_api_key:
        // false), this checks that the user has configured a base_url — without
        // one, SearXNG cannot function and should not appear in agent setup.
        let has_key = gw.vault.get_search_key(&item.id).is_ok();
        if has_key {
            providers.push(item);
        }
    }

    let new_version = gw.resource_cache.search_list.version.wrapping_add(1);
    let new_list = SearchListFile {
        version: new_version,
        providers,
    };

    if let Err(e) = save_search_list(data_dir, &new_list) {
        tracing::error!(error = %e, "Failed to save search_list.json after vault change");
    }
    gw.resource_cache.search_list = new_list;
}

/// Rebuild user_profiles.json with a bumped version and save to disk.
///
/// Called by users_api.rs handlers after create/update/activate.
/// The caller updates the users Vec before calling this function.
pub fn rebuild_and_save_user_profile_cache(
    gw: &mut crate::gateway::state::GatewayState,
    data_dir: &Path,
) {
    let new_version = gw.resource_cache.user_profile_list.version.wrapping_add(1);
    gw.resource_cache.user_profile_list.version = new_version;
    let list = gw.resource_cache.user_profile_list.clone();

    if let Err(e) = save_user_profile_list(data_dir, &list) {
        tracing::error!(error = %e, "Failed to save user_profiles.json after profile change");
    }
}

/// Build search key vault from Vault entries.
///
/// Reads decrypted API keys from Vault for each configured search provider.
pub fn build_search_key_vault(
    gw: &crate::gateway::state::GatewayState,
) -> Vec<SearchKeyEntry> {
    let providers = &["tavily", "brave", "firecrawl", "searxng"];
    providers
        .iter()
        .filter_map(|id| {
            gw.vault.get_search_key(id).ok().map(|entry| SearchKeyEntry {
                provider_id: id.to_string(),
                api_key: entry.api_key,
            })
        })
        .collect()
}

/// Convert MCP catalog entries (McpServerConfigDef) to McpListItem entries.
///
/// MCP keys are built on-the-fly by extracting env vars and headers that
/// contain credentials (api_key, token, etc).
pub fn build_mcp_list_from_catalog(
    catalog: &[rollball_core::protocol::McpServerConfigDef],
) -> Vec<McpListItem> {
    catalog
        .iter()
        .map(|def| McpListItem {
            id: def.name.clone(),
            name: def.name.clone(),
            transport: def.transport.clone(),
            url: def.url.clone(),
            command: def.command.clone(),
            args: def.args.clone(),
            env: def.env.clone(),
            headers: def.headers.clone(),
            tool_timeout_secs: def.tool_timeout_secs,
        })
        .collect()
}

/// Build MCP key vault from catalog entries.
///
/// Extracts potential API keys from env vars and headers.
pub fn build_mcp_key_vault(
    catalog: &[rollball_core::protocol::McpServerConfigDef],
) -> Vec<McpKeyEntry> {
    catalog
        .iter()
        .map(|def| {
            // Extract api key from env vars or headers
            let api_key = extract_api_key_from_mcp_config(def);
            McpKeyEntry {
                mcp_id: def.name.clone(),
                api_key,
            }
        })
        .collect()
}

/// Try to extract an API key from MCP server config env vars and headers.
fn extract_api_key_from_mcp_config(
    config: &rollball_core::protocol::McpServerConfigDef,
) -> Option<String> {
    let key_patterns = ["api_key", "api-key", "token", "auth", "secret", "password"];

    // Check env vars
    for (k, v) in &config.env {
        let lower = k.to_lowercase();
        if key_patterns.iter().any(|p| lower.contains(p)) && !v.is_empty() {
            return Some(v.clone());
        }
    }
    // Check headers
    for (k, v) in &config.headers {
        let lower = k.to_lowercase();
        if key_patterns.iter().any(|p| lower.contains(p)) && !v.is_empty() {
            return Some(v.clone());
        }
    }
    None
}

// ── Defaults ───────────────────────────────────────────────────────────

impl Default for ProviderListFile {
    fn default() -> Self {
        Self {
            version: 0,
            providers: Vec::new(),
        }
    }
}

impl Default for McpListFile {
    fn default() -> Self {
        Self {
            version: 0,
            servers: Vec::new(),
        }
    }
}

impl Default for SearchListFile {
    fn default() -> Self {
        Self {
            version: 0,
            providers: Vec::new(),
        }
    }
}

impl Default for ResourceCache {
    fn default() -> Self {
        Self {
            provider_list: ProviderListFile::default(),
            mcp_list: McpListFile::default(),
            search_list: SearchListFile::default(),
            user_profile_list: UserProfileListFile { version: 0, users: Vec::new() },
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rollball-test-resource-cache-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_default_provider_list() {
        let list = ProviderListFile::default();
        assert_eq!(list.version, 0);
        assert!(list.providers.is_empty());
    }

    #[test]
    fn test_save_and_load_provider_list() {
        let dir = temp_dir("save-provider");
        let list = ProviderListFile {
            version: 1,
            providers: vec![ProviderListItem {
                id: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                protocol_type: rollball_core::protocol::ProtocolType::OpenAI,
                compact_model: None,
                models: vec![ProviderModelEntry {
                    id: "gpt-4o".to_string(),
                    capabilities: rollball_core::protocol::ModelCapabilitiesInfo {
                        context_window: 128000,
                        max_output_tokens: 16384,
                        max_input_tokens: Some(120000),
                        supports_tool_calling: true,
                        supports_reasoning: None,
                        supports_attachment: Some(true),
                        supports_temperature: None,
                        cost: None,
                        modalities: None,
                        name: Some("GPT-4o".to_string()),
                        family: Some("gpt".to_string()),
                        knowledge_cutoff: Some("2025-04".to_string()),
                    },
                    max_output_tokens_limit: 32768,
                }],
            }],
        };

        save_provider_list(&dir, &list).unwrap();
        let loaded = load_provider_list(&dir);
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].id, "openai");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent_provider_list() {
        let dir = temp_dir("nonexistent-provider");
        let loaded = load_provider_list(&dir);
        assert_eq!(loaded.version, 0);
        assert!(loaded.providers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_mcp_list() {
        let dir = temp_dir("save-mcp");
        let list = McpListFile {
            version: 2,
            servers: vec![McpListItem {
                id: "github".to_string(),
                name: "GitHub MCP".to_string(),
                transport: rollball_core::protocol::McpTransportDef::Stdio,
                url: None,
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@modelcontextprotocol/server-github".to_string()],
                env: HashMap::from([("GITHUB_TOKEN".to_string(), "ghp_xxx".to_string())]),
                headers: HashMap::new(),
                tool_timeout_secs: Some(30),
            }],
        };

        save_mcp_list(&dir, &list).unwrap();
        let loaded = load_mcp_list(&dir);
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "GitHub MCP");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_mcp_list_from_catalog() {
        let defs = vec![rollball_core::protocol::McpServerConfigDef {
            name: "test-server".to_string(),
            transport: rollball_core::protocol::McpTransportDef::Stdio,
            command: "node".to_string(),
            args: vec!["server.js".to_string()],
            ..Default::default()
        }];
        let items = build_mcp_list_from_catalog(&defs);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "test-server");
    }

    #[test]
    fn test_extract_api_key_from_mcp() {
        let config = rollball_core::protocol::McpServerConfigDef {
            name: "test".to_string(),
            env: HashMap::from([
                ("API_KEY".to_string(), "secret-123".to_string()),
                ("OTHER_VAR".to_string(), "visible".to_string()),
            ]),
            ..Default::default()
        };
        let key = extract_api_key_from_mcp_config(&config);
        assert_eq!(key, Some("secret-123".to_string()));
    }

    #[test]
    fn test_extract_api_key_from_headers() {
        let config = rollball_core::protocol::McpServerConfigDef {
            name: "test".to_string(),
            headers: HashMap::from([
                ("Authorization".to_string(), "Bearer token-456".to_string()),
            ]),
            ..Default::default()
        };
        let key = extract_api_key_from_mcp_config(&config);
        assert_eq!(key, Some("Bearer token-456".to_string()));
    }

    #[test]
    fn test_load_resource_cache() {
        let dir = temp_dir("load-cache");
        let cache = load_resource_cache(&dir);
        assert_eq!(cache.provider_list.version, 0);
        assert_eq!(cache.mcp_list.version, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
