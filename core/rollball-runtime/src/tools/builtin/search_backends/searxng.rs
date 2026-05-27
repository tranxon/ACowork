//! SearXNG metasearch engine backend.
//!
//! SearXNG is self-hosted, so no API key is required.
//! API docs: https://docs.searxng.org/dev/search_api.html

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal SearXNG search API response structure.
#[derive(Debug, Deserialize)]
struct SearxngResponse {
    results: Vec<SearxngResultItem>,
}

#[derive(Debug, Deserialize)]
struct SearxngResultItem {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    snippet: Option<String>,
}

pub struct SearXngBackend {
    client: reqwest::Client,
    search_timeout: std::time::Duration,
}

impl SearXngBackend {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TOOL_HTTP_TIMEOUT)
    }

    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            client: reqwest::Client::new(),
            search_timeout: timeout,
        }
    }
}

impl Default for SearXngBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for SearXngBackend {
    fn provider_id(&self) -> &str {
        "searxng"
    }

    async fn search(
        &self,
        query: &str,
        count: u32,
        _api_key: &str,
        base_url: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchBackendError> {
        let base = base_url.unwrap_or("");
        if base.is_empty() {
            return Err(SearchBackendError::NoApiKey);
        }

        let url = format!("{base}/search");

        let resp = self
            .client
            .get(&url)
            .query(&[
                ("q", query),
                ("format", "json"),
                ("categories", "general"),
            ])
            .timeout(self.search_timeout)
            .send()
            .await
            .map_err(|e| SearchBackendError::Http(format!("SearXNG request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "SearXNG returned {status}: {body_text}"
            )));
        }

        let data: SearxngResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse SearXNG response: {e}"))
        })?;

        let results: Vec<SearchResult> = data
            .results
            .into_iter()
            .take(count as usize)
            .map(|r| {
                let snippet = r.snippet.unwrap_or(r.content);
                SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet,
                }
            })
            .collect();

        Ok(results)
    }
}
