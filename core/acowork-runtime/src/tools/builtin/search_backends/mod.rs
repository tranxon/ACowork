//! Configurable web search backend system.
//!
//! Each provider implements the `SearchBackend` trait.
//! `WebSearchEngine` manages a fallback chain of backends.

use acowork_core::protocol::SearchProviderListItem;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub mod brave;
pub mod exa;
pub mod firecrawl;
pub mod google_cse;
pub mod perplexity;
pub mod searxng;
pub mod serper;
pub mod tavily;

// ── Unified search result ──

/// A single search result item, normalized across all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Result title
    pub title: String,
    /// Result URL
    pub url: String,
    /// Snippet / summary text
    pub snippet: String,
}

// ── SearchBackend trait ──

/// Trait for web search provider backends.
///
/// Each provider (Tavily, Brave, Firecrawl, SearXNG) implements this trait
/// to provide a unified search interface regardless of the underlying API.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    /// Provider identifier (e.g. "tavily", "brave")
    fn provider_id(&self) -> &str;

    /// Execute a web search query.
    ///
    /// # Arguments
    /// * `query` - Search query string
    /// * `count` - Maximum number of results to return
    /// * `api_key` - Decrypted API key from Vault (empty for no-auth providers)
    /// * `base_url` - Optional custom base URL override
    async fn search(
        &self,
        query: &str,
        count: u32,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchBackendError>;
}

// ── Error type ──

/// Errors that can occur during search backend execution.
#[derive(Debug)]
pub enum SearchBackendError {
    /// HTTP-level error (network, timeout, etc.)
    Http(String),
    /// API returned an error response (wrong key, rate limited, etc.)
    Api(String),
    /// Response parsing error (unexpected JSON structure)
    Parse(String),
    /// Provider requires an API key but none was provided
    NoApiKey,
    /// Provider is not configured (no Vault entry)
    NotConfigured,
}

impl std::fmt::Display for SearchBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchBackendError::Http(msg) => write!(f, "HTTP error: {msg}"),
            SearchBackendError::Api(msg) => write!(f, "API error: {msg}"),
            SearchBackendError::Parse(msg) => write!(f, "Parse error: {msg}"),
            SearchBackendError::NoApiKey => write!(f, "No API key configured"),
            SearchBackendError::NotConfigured => write!(f, "Provider not configured"),
        }
    }
}

// ── Fallback engine ──

/// Shared search key vault type (provider_id -> decrypted API key).
pub type SharedSearchKeyVault = Arc<RwLock<HashMap<String, String>>>;

/// Shared search provider list type.
pub type SharedSearchProviderList = Arc<RwLock<Vec<SearchProviderListItem>>>;

/// Fallback engine that resolves backends dynamically from shared state.
///
/// Holds `Arc` references to the same key vault and provider list stored in
/// `AgentCore`. When `SessionManager::update_search_config` writes to these
/// shared Arcs (triggered by MQTT `acowork/global/searches` retained
/// updates), the engine sees the new state immediately on the next
/// `search()` call — no explicit refresh needed.
pub struct WebSearchEngine {
    key_vault: SharedSearchKeyVault,
    provider_list: SharedSearchProviderList,
    search_timeout: Duration,
}

impl WebSearchEngine {
    /// Create a new engine bound to shared key vault and provider list.
    ///
    /// Both arguments are `Arc<RwLock<…>>` shared with `AgentCore` so that
    /// runtime MQTT updates are visible without rebuilding the engine.
    ///
    /// # Arguments
    /// * `key_vault` - Shared map of `provider_id -> decrypted API key`.
    /// * `provider_list` - Shared list of configured search providers.
    /// * `search_timeout` - HTTP timeout applied to every search request.
    pub fn new(
        key_vault: SharedSearchKeyVault,
        provider_list: SharedSearchProviderList,
        search_timeout: Duration,
    ) -> Self {
        Self {
            key_vault,
            provider_list,
            search_timeout,
        }
    }

    /// Create a `SearchBackend` box for the given provider id, using this engine's timeout.
    ///
    /// Returns `None` if the provider id is not recognized.
    pub fn build_backend(&self, provider_id: &str) -> Option<Box<dyn SearchBackend>> {
        match provider_id {
            "brave" => Some(Box::new(brave::BraveBackend::with_timeout(
                self.search_timeout,
            ))),
            "serper" => Some(Box::new(serper::SerperBackend::with_timeout(
                self.search_timeout,
            ))),
            "tavily" => Some(Box::new(tavily::TavilyBackend::with_timeout(
                self.search_timeout,
            ))),
            "exa" => Some(Box::new(exa::ExaBackend::with_timeout(self.search_timeout))),
            "google-cse" => Some(Box::new(google_cse::GoogleCseBackend::with_timeout(
                self.search_timeout,
            ))),
            "perplexity" => Some(Box::new(perplexity::PerplexityBackend::with_timeout(
                self.search_timeout,
            ))),
            "firecrawl" => Some(Box::new(firecrawl::FirecrawlBackend::with_timeout(
                self.search_timeout,
            ))),
            "searxng" => Some(Box::new(searxng::SearXngBackend::with_timeout(
                self.search_timeout,
            ))),
            _ => None,
        }
    }

    /// Execute a search with automatic fallback.
    ///
    /// Reads the current provider list and key vault at call time, then
    /// tries each provider in list order. On failure (no key, API error,
    /// network error), automatically falls through to the next provider.
    /// Returns an error only if ALL providers fail (or none are configured).
    pub async fn search(
        &self,
        query: &str,
        count: u32,
    ) -> Result<Vec<SearchResult>, SearchBackendError> {
        // Snapshot the shared state into locals to avoid holding locks
        // across the `.await` boundary below.
        let providers: Vec<SearchProviderListItem> = {
            let guard = self
                .provider_list
                .read()
                .map_err(|e| SearchBackendError::Api(format!("Provider list lock poisoned: {e}")))?;
            guard.clone()
        };
        if providers.is_empty() {
            return Err(SearchBackendError::NotConfigured);
        }

        let keys: HashMap<String, String> = {
            let guard = self
                .key_vault
                .read()
                .map_err(|e| SearchBackendError::Api(format!("Key vault lock poisoned: {e}")))?;
            guard.clone()
        };

        let mut last_error: Option<SearchBackendError> = None;
        let mut errors: Vec<(String, String)> = Vec::new();

        for provider in &providers {
            // Resolve API key from vault.
            let api_key: &str = if provider.requires_api_key {
                match keys.get(&provider.id) {
                    Some(k) => k.as_str(),
                    None => {
                        tracing::warn!(
                            provider_id = %provider.id,
                            "Search provider requires API key but none in vault, skipping"
                        );
                        continue;
                    }
                }
            } else {
                ""
            };

            // Build a backend instance for this provider.
            let backend = match self.build_backend(&provider.id) {
                Some(b) => b,
                None => {
                    tracing::warn!(
                        provider_id = %provider.id,
                        "Unknown search provider id, skipping"
                    );
                    continue;
                }
            };

            // Determine base URL override (empty string -> None).
            let base_url_override: Option<&str> = if provider.base_url.is_empty() {
                None
            } else {
                Some(provider.base_url.as_str())
            };

            match backend
                .search(query, count, api_key, base_url_override)
                .await
            {
                Ok(results) => {
                    if !errors.is_empty() {
                        tracing::warn!(
                            provider_id = backend.provider_id(),
                            fallback_errors = ?errors,
                            "Web search fallback succeeded after {} error(s)",
                            errors.len()
                        );
                    }
                    return Ok(results);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    tracing::warn!(
                        provider_id = backend.provider_id(),
                        error = %err_msg,
                        "Web search backend failed, trying next fallback"
                    );
                    errors.push((backend.provider_id().to_string(), err_msg));
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or(SearchBackendError::NotConfigured))
    }

    /// Check if the engine has any configured providers.
    pub fn is_empty(&self) -> bool {
        self.provider_list
            .read()
            .map(|l| l.is_empty())
            .unwrap_or(true)
    }
}

// ── Provider catalog (static metadata) ──

/// Built-in search provider catalog for metadata lookups.
pub fn search_provider_catalog() -> Vec<SearchProviderListItem> {
    vec![
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
            description: "Google Custom Search Engine — requires API key + Search Engine ID (CX)"
                .to_string(),
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
    ]
}

/// Look up static metadata for a provider.
pub fn lookup_provider_meta(id: &str) -> Option<SearchProviderListItem> {
    search_provider_catalog().into_iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_is_empty_when_no_providers() {
        let vault: SharedSearchKeyVault = Arc::new(RwLock::new(HashMap::new()));
        let list: SharedSearchProviderList = Arc::new(RwLock::new(Vec::new()));
        let engine = WebSearchEngine::new(vault, list, Duration::from_secs(10));
        assert!(engine.is_empty());
    }

    #[test]
    fn test_engine_not_empty_with_providers() {
        let vault: SharedSearchKeyVault = Arc::new(RwLock::new(HashMap::new()));
        let list: SharedSearchProviderList = Arc::new(RwLock::new(vec![SearchProviderListItem {
            id: "tavily".to_string(),
            name: "Tavily".to_string(),
            description: String::new(),
            requires_api_key: true,
            base_url: "https://api.tavily.com".to_string(),
        }]));
        let engine = WebSearchEngine::new(vault, list, Duration::from_secs(10));
        assert!(!engine.is_empty());
    }

    #[tokio::test]
    async fn test_search_returns_not_configured_when_empty() {
        let vault: SharedSearchKeyVault = Arc::new(RwLock::new(HashMap::new()));
        let list: SharedSearchProviderList = Arc::new(RwLock::new(Vec::new()));
        let engine = WebSearchEngine::new(vault, list, Duration::from_secs(10));
        let result = engine.search("test query", 5).await;
        assert!(matches!(result, Err(SearchBackendError::NotConfigured)));
    }

    #[tokio::test]
    async fn test_search_skips_provider_without_key() {
        // Provider requires API key but vault is empty -> should skip and
        // return NotConfigured (no providers tried).
        let vault: SharedSearchKeyVault = Arc::new(RwLock::new(HashMap::new()));
        let list: SharedSearchProviderList = Arc::new(RwLock::new(vec![SearchProviderListItem {
            id: "tavily".to_string(),
            name: "Tavily".to_string(),
            description: String::new(),
            requires_api_key: true,
            base_url: "https://api.tavily.com".to_string(),
        }]));
        let engine = WebSearchEngine::new(vault, list, Duration::from_secs(10));
        let result = engine.search("test query", 5).await;
        // All providers were skipped (no key) -> last_error is None -> NotConfigured
        assert!(matches!(result, Err(SearchBackendError::NotConfigured)));
    }

    #[tokio::test]
    async fn test_search_sees_runtime_vault_update() {
        // Start with empty vault, then add a key via the shared Arc.
        // The engine should see the update on the next search() call
        // without re-registration (this is the core design property).
        let vault: SharedSearchKeyVault = Arc::new(RwLock::new(HashMap::new()));
        let list: SharedSearchProviderList = Arc::new(RwLock::new(vec![SearchProviderListItem {
            id: "tavily".to_string(),
            name: "Tavily".to_string(),
            description: String::new(),
            requires_api_key: true,
            base_url: "https://api.tavily.com".to_string(),
        }]));
        let engine = WebSearchEngine::new(vault.clone(), list.clone(), Duration::from_secs(10));

        // First search: no key -> NotConfigured
        let result = engine.search("test", 5).await;
        assert!(matches!(result, Err(SearchBackendError::NotConfigured)));

        // Simulate MQTT update: write key to shared vault
        {
            let mut v = vault.write().unwrap();
            v.insert("tavily".to_string(), "test-key".to_string());
        }

        // Second search: key is now present. The backend will try to
        // call the real Tavily API and fail with an HTTP error (since
        // "test-key" is invalid and the endpoint is real). The important
        // thing is that it does NOT return NotConfigured - it actually
        // tried the backend.
        let result = engine.search("test", 5).await;
        // Should be an HTTP or API error, NOT NotConfigured
        assert!(
            !matches!(result, Err(SearchBackendError::NotConfigured)),
            "Engine should have tried the backend after key was added"
        );
    }

    #[test]
    fn test_build_backend_recognizes_all_catalog_ids() {
        let vault: SharedSearchKeyVault = Arc::new(RwLock::new(HashMap::new()));
        let list: SharedSearchProviderList = Arc::new(RwLock::new(Vec::new()));
        let engine = WebSearchEngine::new(vault, list, Duration::from_secs(10));

        // Every provider in the static catalog should be buildable.
        for provider in search_provider_catalog() {
            assert!(
                engine.build_backend(&provider.id).is_some(),
                "Unknown backend id: {}",
                provider.id
            );
        }
    }
}
