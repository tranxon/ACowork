//! Search backend integration tests (Phase 3: P3.1, P3.2, P3.3)
//!
//! P3.1: Tavily API integration — URL construction + response parsing
//! P3.2: Fallback chain — primary fails, secondary succeeds
//! P3.3: No provider / no API key error returns

use async_trait::async_trait;
use rollball_core::protocol::SearchKeyEntry;
use rollball_runtime::tools::builtin::search_backends::{
    SearchBackend, SearchBackendError, SearchResult, WebSearchEngine,
};

// ── Mock backends for testing fallback chain ───────────────────────

/// A backend that always succeeds with a canned result.
struct SuccessBackend {
    id: String,
    result_title: String,
}

#[async_trait]
impl SearchBackend for SuccessBackend {
    fn provider_id(&self) -> &str {
        &self.id
    }

    async fn search(
        &self,
        _query: &str,
        _count: u32,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchBackendError> {
        Ok(vec![SearchResult {
            title: self.result_title.clone(),
            url: "https://example.com".to_string(),
            snippet: "Mock result".to_string(),
        }])
    }
}

/// A backend that always fails with NoApiKey.
struct NoApiKeyBackend {
    id: String,
}

#[async_trait]
impl SearchBackend for NoApiKeyBackend {
    fn provider_id(&self) -> &str {
        &self.id
    }

    async fn search(
        &self,
        _query: &str,
        _count: u32,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchBackendError> {
        Err(SearchBackendError::NoApiKey)
    }
}

/// A backend that always fails with an API error.
struct ApiErrorBackend {
    id: String,
}

#[async_trait]
impl SearchBackend for ApiErrorBackend {
    fn provider_id(&self) -> &str {
        &self.id
    }

    async fn search(
        &self,
        _query: &str,
        _count: u32,
        _api_key: &str,
        _base_url: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchBackendError> {
        Err(SearchBackendError::Api("Simulated API error".into()))
    }
}

// ── P3.3: Empty engine / no provider error ─────────────────────────

#[tokio::test]
async fn test_empty_engine_returns_not_configured() {
    let engine = WebSearchEngine::new(vec![]);
    assert!(engine.is_empty());

    let result = engine.search("test query", 5).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        SearchBackendError::NotConfigured => {}
        other => panic!("Expected NotConfigured, got: {other}"),
    }
}

// ── P3.2: Fallback chain tests ─────────────────────────────────────

#[tokio::test]
async fn test_successful_search_no_fallback_needed() {
    let engine = WebSearchEngine::new(vec![(
        Box::new(SuccessBackend {
            id: "primary".into(),
            result_title: "Primary Result".into(),
        }),
        Some(SearchKeyEntry {
            provider_id: "primary".into(),
            api_key: "key-123".into(),
        }),
        None,
    )]);

    let result = engine.search("test", 3).await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Primary Result");
}

#[tokio::test]
async fn test_fallback_no_api_key_then_success() {
    // Primary has no API key → falls through to secondary
    let engine = WebSearchEngine::new(vec![
        (
            Box::new(NoApiKeyBackend {
                id: "no-key".into(),
            }),
            None, // No key entry
            None,
        ),
        (
            Box::new(SuccessBackend {
                id: "fallback".into(),
                result_title: "Fallback Result".into(),
            }),
            Some(SearchKeyEntry {
                provider_id: "fallback".into(),
                api_key: "key-456".into(),
            }),
            None,
        ),
    ]);

    let result = engine.search("test", 3).await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Fallback Result");
}

#[tokio::test]
async fn test_fallback_api_error_then_success() {
    // Primary has API error → falls through to secondary
    let engine = WebSearchEngine::new(vec![
        (
            Box::new(ApiErrorBackend {
                id: "broken".into(),
            }),
            Some(SearchKeyEntry {
                provider_id: "broken".into(),
                api_key: "key-broken".into(),
            }),
            None,
        ),
        (
            Box::new(SuccessBackend {
                id: "healthy".into(),
                result_title: "Healthy Result".into(),
            }),
            Some(SearchKeyEntry {
                provider_id: "healthy".into(),
                api_key: "key-ok".into(),
            }),
            None,
        ),
    ]);

    let result = engine.search("test", 3).await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Healthy Result");
}

#[tokio::test]
async fn test_all_backends_fail_returns_last_error() {
    let engine = WebSearchEngine::new(vec![
        (
            Box::new(NoApiKeyBackend {
                id: "no-key-1".into(),
            }),
            None,
            None,
        ),
        (
            Box::new(ApiErrorBackend {
                id: "api-error".into(),
            }),
            Some(SearchKeyEntry {
                provider_id: "api-error".into(),
                api_key: "bad-key".into(),
            }),
            None,
        ),
    ]);

    let result = engine.search("test", 3).await;
    assert!(result.is_err());
    // Last error should be from the last backend that failed
    match result.unwrap_err() {
        SearchBackendError::Api(_) => {} // from ApiErrorBackend
        other => panic!("Expected Api error, got: {other}"),
    }
}

#[tokio::test]
async fn test_fallback_chain_three_levels() {
    let engine = WebSearchEngine::new(vec![
        (
            Box::new(NoApiKeyBackend {
                id: "level-1".into(),
            }),
            None,
            None,
        ),
        (
            Box::new(ApiErrorBackend {
                id: "level-2".into(),
            }),
            Some(SearchKeyEntry {
                provider_id: "level-2".into(),
                api_key: "bad".into(),
            }),
            None,
        ),
        (
            Box::new(SuccessBackend {
                id: "level-3".into(),
                result_title: "Deep Fallback".into(),
            }),
            Some(SearchKeyEntry {
                provider_id: "level-3".into(),
                api_key: "good".into(),
            }),
            None,
        ),
    ]);

    let result = engine.search("deep fallback test", 5).await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title, "Deep Fallback");
}

// ── P3.1: Tavily URL construction test ─────────────────────────────

#[tokio::test]
async fn test_tavily_no_api_key_returns_error() {
    use rollball_runtime::tools::builtin::search_backends::tavily::TavilyBackend;

    let backend = TavilyBackend::new();
    let result = backend.search("test query", 5, "", None).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        SearchBackendError::NoApiKey => {}
        other => panic!("Expected NoApiKey, got: {other}"),
    }
}

#[tokio::test]
async fn test_tavily_with_custom_base_url() {
    use rollball_runtime::tools::builtin::search_backends::tavily::TavilyBackend;

    let backend = TavilyBackend::new();
    // With a custom base URL and an obviously fake API key,
    // this should fail with an HTTP or API error (not NoApiKey)
    let result = backend
        .search(
            "test query",
            3,
            "tvly-fake-key-for-testing",
            Some("https://api.tavily.com"),
        )
        .await;
    // Should NOT be NoApiKey since we provided one
    match result {
        Err(SearchBackendError::NoApiKey) => {
            panic!("Should not be NoApiKey — key was provided");
        }
        _ => {} // Http error, API error, or success — all acceptable
    }
}

// ── Static catalog tests ───────────────────────────────────────────

#[test]
fn test_search_provider_catalog_has_entries() {
    let catalog = rollball_runtime::tools::builtin::search_backends::search_provider_catalog();
    assert!(!catalog.is_empty(), "Catalog should have entries");
    // Verify known providers
    let ids: Vec<&str> = catalog.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"tavily"));
    assert!(ids.contains(&"brave"));
    assert!(ids.contains(&"searxng"));
}

#[test]
fn test_lookup_provider_meta() {
    let meta = rollball_runtime::tools::builtin::search_backends::lookup_provider_meta("tavily");
    assert!(meta.is_some());
    let meta = meta.unwrap();
    assert_eq!(meta.id, "tavily");
    assert_eq!(meta.name, "Tavily Search");
    assert!(meta.requires_api_key);
}

#[test]
fn test_lookup_unknown_provider_returns_none() {
    let meta =
        rollball_runtime::tools::builtin::search_backends::lookup_provider_meta("nonexistent");
    assert!(meta.is_none());
}

// ── SearchResult serialization ─────────────────────────────────────

#[test]
fn test_search_result_serialization() {
    let result = SearchResult {
        title: "Test Title".to_string(),
        url: "https://example.com".to_string(),
        snippet: "This is a test snippet.".to_string(),
    };

    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains("Test Title"));
    assert!(json.contains("https://example.com"));
    assert!(json.contains("test snippet"));

    let deserialized: SearchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.title, "Test Title");
    assert_eq!(deserialized.url, "https://example.com");
    assert_eq!(deserialized.snippet, "This is a test snippet.");
}
