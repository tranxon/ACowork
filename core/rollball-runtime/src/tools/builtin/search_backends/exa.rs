//! Exa.ai Search API backend.
//!
//! Exa is an AI search engine designed for LLMs, returning high-quality
//! web content with extracted text.
//!
//! API docs: https://docs.exa.ai/reference/search

use async_trait::async_trait;
use serde::Deserialize;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Exa search API response structure.
#[derive(Debug, Deserialize)]
struct ExaResponse {
    #[serde(default)]
    results: Vec<ExaResultItem>,
}

#[derive(Debug, Deserialize)]
struct ExaResultItem {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    text: String,
}

pub struct ExaBackend {
    client: reqwest::Client,
}

impl ExaBackend {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ExaBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for ExaBackend {
    fn provider_id(&self) -> &str {
        "exa"
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

        let base = base_url.unwrap_or("https://api.exa.ai");
        let url = format!("{base}/search");

        let body = serde_json::json!({
            "query": query,
            "numResults": count.min(10),
            "type": "auto",
            "contents": {
                "text": true
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-api-key", api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| SearchBackendError::Http(format!("Exa request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Exa returned {status}: {body_text}"
            )));
        }

        let data: ExaResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Exa response: {e}"))
        })?;

        let results: Vec<SearchResult> = data
            .results
            .into_iter()
            .take(count as usize)
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.text,
            })
            .collect();

        Ok(results)
    }
}
