//! Serper.dev Google Search API backend.
//!
//! API docs: https://serper.dev/api-reference

use async_trait::async_trait;
use serde::Deserialize;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Serper API response structure.
#[derive(Debug, Deserialize)]
struct SerperResponse {
    #[serde(default)]
    organic: Vec<SerperOrganicResult>,
}

#[derive(Debug, Deserialize)]
struct SerperOrganicResult {
    title: String,
    link: String,
    #[serde(default)]
    snippet: String,
}

pub struct SerperBackend {
    client: reqwest::Client,
}

impl SerperBackend {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for SerperBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for SerperBackend {
    fn provider_id(&self) -> &str {
        "serper"
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

        let base = base_url.unwrap_or("https://google.serper.dev");
        let url = format!("{base}/search");

        let body = serde_json::json!({
            "q": query,
            "num": count.min(10),
        });

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-API-KEY", api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| SearchBackendError::Http(format!("Serper request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Serper returned {status}: {body_text}"
            )));
        }

        let data: SerperResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Serper response: {e}"))
        })?;

        let results: Vec<SearchResult> = data
            .organic
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
