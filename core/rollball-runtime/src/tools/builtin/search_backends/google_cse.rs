//! Google Custom Search Engine (CSE) backend.
//!
//! Requires both an API key and a Search Engine ID (CX).
//! The api_key field should contain both separated by "|": `API_KEY|CX`.
//!
//! API docs: https://developers.google.com/custom-search/v1/reference/rest/v1/cse/list

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Google CSE API response structure.
#[derive(Debug, Deserialize)]
struct GoogleCseResponse {
    #[serde(default)]
    items: Vec<GoogleCseResultItem>,
}

#[derive(Debug, Deserialize)]
struct GoogleCseResultItem {
    title: String,
    link: String,
    #[serde(default)]
    snippet: String,
}

pub struct GoogleCseBackend {
    client: reqwest::Client,
    search_timeout: std::time::Duration,
}

impl GoogleCseBackend {
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

impl Default for GoogleCseBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for GoogleCseBackend {
    fn provider_id(&self) -> &str {
        "google-cse"
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

        // Parse api_key as "API_KEY|CX" — Google CSE requires both.
        let (key, cx) = match api_key.split_once('|') {
            Some((k, c)) => (k, c),
            None => {
                return Err(SearchBackendError::Api(
                    "Google CSE requires api_key in format 'API_KEY|CX' (Search Engine ID)".to_string(),
                ));
            }
        };

        if cx.is_empty() {
            return Err(SearchBackendError::Api(
                "Google CSE: CX (Search Engine ID) is empty".to_string(),
            ));
        }

        let base = base_url.unwrap_or("https://www.googleapis.com");
        let url = format!("{base}/customsearch/v1");

        let resp = self
            .client
            .get(&url)
            .query(&[
                ("key", key),
                ("cx", cx),
                ("q", query),
                ("num", &count.min(10).to_string()),
            ])
            .timeout(self.search_timeout)
            .send()
            .await
            .map_err(|e| SearchBackendError::Http(format!("Google CSE request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Google CSE returned {status}: {body_text}"
            )));
        }

        let data: GoogleCseResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Google CSE response: {e}"))
        })?;

        let results: Vec<SearchResult> = data
            .items
            .into_iter()
            .take(count as usize)
            .map(|r| SearchResult {
                title: r.title,
                url: r.link,
                snippet: r.snippet,
            })
            .collect();

        Ok(results)
    }
}
