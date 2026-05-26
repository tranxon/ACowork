//! Web search tool — search the web via configurable backends (Tavily, Brave, Firecrawl, SearXNG).
//!
//! Uses the fallback engine from `search_backends::WebSearchEngine` which tries
//! backends in priority order. If all backends fail, returns an error.

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

use super::search_backends::WebSearchEngine;
use crate::tools::output;

/// Web search tool powered by the configurable search backend system.
///
/// The engine is constructed at tool registration time from agent search config.
pub struct WebSearchTool {
    engine: WebSearchEngine,
}

impl WebSearchTool {
    /// Create a new web search tool with the given fallback engine.
    pub fn new(engine: WebSearchEngine) -> Self {
        Self { engine }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "Search the web using configured search engines. Supports Tavily, Brave, Firecrawl, and SearXNG with automatic fallback.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results (default 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let query = params["query"].as_str().unwrap_or("");
        if query.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'query'".to_string()),
                token_usage: None,
            });
        }

        let count = params["count"]
            .as_u64()
            .map(|c| c as u32)
            .unwrap_or(5)
            .max(1)
            .min(10);

        match self.engine.search(query, count).await {
            Ok(results) => {
                if results.is_empty() {
                    return Ok(ToolResult {
                        ok: true,
                        content: "No results found".to_string(),
                        error: None,
                        token_usage: None,
                    });
                }

                let joined: String = results
                    .iter()
                    .enumerate()
                    .map(|(i, r)| format!("{}. {}\n   {}", i + 1, r.title, r.snippet))
                    .collect::<Vec<_>>()
                    .join("\n\n");

                let (content, _truncated) = output::truncate_output(&joined);
                Ok(ToolResult {
                    ok: true,
                    content,
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!("Search failed: {e}")),
                token_usage: None,
            }),
        }
    }
}
