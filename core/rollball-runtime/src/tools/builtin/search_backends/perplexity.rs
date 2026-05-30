//! Perplexity Sonar API backend — AI-powered search with citations.
//!
//! Uses the chat completions API with "sonar" model to get AI answers
//! with inline citations, then extracts the citation URLs as search results.
//!
//! API docs: https://docs.perplexity.ai/api-reference/chat-completions

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

use super::{SearchBackend, SearchBackendError, SearchResult};

/// Internal Perplexity chat completions response.
#[derive(Debug, Deserialize)]
struct PerplexityResponse {
    #[serde(default)]
    citations: Vec<String>,
    #[serde(default)]
    choices: Vec<PerplexityChoice>,
}

#[derive(Debug, Deserialize)]
struct PerplexityChoice {
    message: PerplexityMessage,
}

#[derive(Debug, Deserialize)]
struct PerplexityMessage {
    #[serde(default)]
    content: String,
}

pub struct PerplexityBackend {
    client: reqwest::Client,
    search_timeout: std::time::Duration,
}

impl PerplexityBackend {
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

impl Default for PerplexityBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for PerplexityBackend {
    fn provider_id(&self) -> &str {
        "perplexity"
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

        let base = base_url.unwrap_or("https://api.perplexity.ai");
        let url = format!("{base}/chat/completions");

        let body = serde_json::json!({
            "model": "sonar",
            "messages": [
                {
                    "role": "system",
                    "content": crate::prompt::SEARCH_SYSTEM_PROMPT
                },
                {
                    "role": "user",
                    "content": query
                }
            ],
            "max_tokens": 1024,
            "web_search_options": {
                "search_context_size": "medium"
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
            .map_err(|e| SearchBackendError::Http(format!("Perplexity request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SearchBackendError::Api(format!(
                "Perplexity returned {status}: {body_text}"
            )));
        }

        let data: PerplexityResponse = resp.json().await.map_err(|e| {
            SearchBackendError::Parse(format!("Failed to parse Perplexity response: {e}"))
        })?;

        let limit = count as usize;

        // First, use citations as primary search results.
        if !data.citations.is_empty() {
            let results: Vec<SearchResult> = data
                .citations
                .into_iter()
                .take(limit)
                .enumerate()
                .map(|(i, url)| SearchResult {
                    title: format!("Result {}", i + 1),
                    url,
                    snippet: String::new(),
                })
                .collect();
            return Ok(results);
        }

        // Fallback: extract URLs from the AI-generated content via markdown link pattern.
        let content = data
            .choices
            .first()
            .map(|c| c.message.content.as_str())
            .unwrap_or("");

        let link_re =
            regex::Regex::new(r"\[([^\]]*)\]\(([^)]+)\)").unwrap();
        let results: Vec<SearchResult> = link_re
            .captures_iter(content)
            .take(limit)
            .map(|cap| {
                let title = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let url = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                SearchResult {
                    title: if title.is_empty() { url.clone() } else { title },
                    url,
                    snippet: String::new(),
                }
            })
            .collect();

        Ok(results)
    }
}
