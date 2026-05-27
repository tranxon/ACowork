//! Firecrawl Search API backend.
//!
//! API docs: https://docs.firecrawl.dev/api-reference

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Firecrawl search API response structure.
#[derive(Debug, Deserialize)]
struct FirecrawlResponse {
    #[serde(default)]
    data: Vec<FirecrawlDataItem>,
    #[serde(default)]
    success: bool,
}

#[derive(Debug, Deserialize)]
struct FirecrawlDataItem {
    title: Option<String>,
    url: Option<String>,
    markdown: Option<String>,
    description: Option<String>,
}

pub struct FirecrawlBackend {
    client: reqwest::Client,
    search_timeout: std::time::Duration,
}

impl FirecrawlBackend {
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

impl Default for FirecrawlBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for FirecrawlBackend {
    fn provider_id(&self) -> &str {
        "firecrawl"
    }

    async fn search(
        &self,
        query: &str,
        count: u32,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchBackendError> {
        if api_key.is_empty() {
            return Err(SearchBackendError::NoApiKey);
        }

        let base = base_url.unwrap_or("https://api.firecrawl.dev");
        let url = format!("{base}/v1/search");

        let body = serde_json::json!({
            "query": query,
            "limit": count.min(10),
            "scrapeOptions": {
                "formats": ["markdown"]
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .timeout(self.search_timeout)
            .send()
            .await
            .map_err(|e| SearchBackendError::Http(format!("Firecrawl request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Firecrawl returned {status}: {body_text}"
            )));
        }

        let data: FirecrawlResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Firecrawl response: {e}"))
        })?;

        if !data.success && data.data.is_empty() {
            return Ok(Vec::new());
        }

        let results: Vec<SearchResult> = data
            .data
            .into_iter()
            .take(count as usize)
            .map(|r| {
                let snippet = r
                    .markdown
                    .or(r.description)
                    .unwrap_or_default();
                SearchResult {
                    title: r.title.unwrap_or_default(),
                    url: r.url.unwrap_or_default(),
                    snippet,
                }
            })
            .collect();

        Ok(results)
    }
}
