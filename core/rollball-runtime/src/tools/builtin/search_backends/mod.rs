//! Configurable web search backend system.
//!
//! Each provider implements the `SearchBackend` trait.
//! `WebSearchEngine` manages a fallback chain of backends.

use async_trait::async_trait;
use rollball_core::protocol::{SearchKeyEntry, SearchProviderListItem};
use serde::{Deserialize, Serialize};

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

/// Ordered list of backends with API keys for fallback chain execution.
pub struct WebSearchEngine {
    backends: Vec<(Box<dyn SearchBackend>, Option<SearchKeyEntry>, Option<String>)>, // (backend, key, base_url)
}

impl WebSearchEngine {
    /// Create a new engine from configured providers.
    ///
    /// Providers are tried in the given order (first = highest priority).
    pub fn new(
        backends: Vec<(Box<dyn SearchBackend>, Option<SearchKeyEntry>, Option<String>)>,
    ) -> Self {
        Self { backends }
    }

    /// Execute a search with automatic fallback.
    ///
    /// Tries each backend in priority order. On failure (no key, API error, network error),
    /// automatically falls through to the next backend. Returns an error only if ALL backends fail.
    pub async fn search(&self, query: &str, count: u32) -> Result<Vec<SearchResult>, SearchBackendError> {
        if self.backends.is_empty() {
            return Err(SearchBackendError::NotConfigured);
        }

        let mut last_error: Option<SearchBackendError> = None;
        let mut errors: Vec<(String, String)> = Vec::new();

        for (backend, key_entry, base_url) in &self.backends {
            let api_key = key_entry.as_ref().map(|k| k.api_key.as_str()).unwrap_or("");

            match backend.search(query, count, api_key, base_url.as_deref()).await {
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

    /// Check if the engine has any configured backends.
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
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
    ]
}

/// Look up static metadata for a provider.
pub fn lookup_provider_meta(id: &str) -> Option<SearchProviderListItem> {
    search_provider_catalog().into_iter().find(|p| p.id == id)
}
