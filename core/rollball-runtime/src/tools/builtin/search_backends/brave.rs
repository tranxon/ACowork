//! Brave Search API backend.
//!
//! API docs: https://api.search.brave.com/app/documentation/web-search

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Brave Search API response structure.
#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    results: Vec<BraveWebResult>,
}

#[derive(Debug, Deserialize)]
struct BraveWebResult {
    title: String,
    url: String,
    description: String,
}

pub struct BraveBackend {
    client: reqwest::Client,
    search_timeout: std::time::Duration,
}

impl BraveBackend {
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

impl Default for BraveBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for BraveBackend {
    fn provider_id(&self) -> &str {
        "brave"
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

        let base = base_url.unwrap_or("https://api.search.brave.com");
        let url = format!("{base}/res/v1/web/search");

        let resp = self
            .client
            .get(&url)
            .query(&[
                ("q", query),
                ("count", &count.min(10).to_string()),
            ])
            .header("Accept", "application/json")
            .header("Accept-Encoding", "gzip")
            .header("X-Subscription-Token", api_key)
            .timeout(self.search_timeout)
            .send()
            .await
            .map_err(|e| SearchBackendError::Http(format!("Brave request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Brave returned {status}: {body_text}"
            )));
        }

        let data: BraveResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Brave response: {e}"))
        })?;

        let results: Vec<SearchResult> = data
            .web
            .into_iter()
            .flat_map(|w| w.results)
            .take(count as usize)
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.description,
            })
            .collect();

        Ok(results)
    }
}
