//! Tavily Search API backend.
//!
//! API docs: https://docs.tavily.com/api-reference

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Tavily API response structure.
#[derive(Debug, Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResultItem>,
}

#[derive(Debug, Deserialize)]
struct TavilyResultItem {
    title: String,
    url: String,
    content: String,
}

pub struct TavilyBackend {
    client: reqwest::Client,
    search_timeout: std::time::Duration,
}

impl TavilyBackend {
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

impl Default for TavilyBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for TavilyBackend {
    fn provider_id(&self) -> &str {
        "tavily"
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

        let base = base_url.unwrap_or("https://api.tavily.com");
        let url = format!("{base}/search");

        let body = serde_json::json!({
            "query": query,
            "search_depth": "basic",
            "max_results": count.min(10),
            "include_answer": false,
            "include_raw_content": false,
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
            .map_err(|e| SearchBackendError::Http(format!("Tavily request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Tavily returned {status}: {body_text}"
            )));
        }

        let data: TavilyResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Tavily response: {e}"))
        })?;

        let results: Vec<SearchResult> = data
            .results
            .into_iter()
            .take(count as usize)
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.content,
            })
            .collect();

        Ok(results)
    }
}
