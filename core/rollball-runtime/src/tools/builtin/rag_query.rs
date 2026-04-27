//! RAG query tool — explicit LLM-triggered enterprise knowledge retrieval
//!
//! This is the 16th built-in tool (only registered when manifest declares RAG).
//! It allows the LLM to explicitly query the enterprise RAG service for
//! targeted deep queries (as opposed to the automatic MemoryManager retrieve).
//!
//! Permission: `rag:query` + `network:<endpoint_url>`

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

use crate::tools::rag::client::RagClient;

/// RAG query tool — explicit enterprise knowledge retrieval
///
/// Registered only when the manifest declares `[[tools]] type = "rag"`.
/// The LLM can call this tool for targeted deep queries with custom
/// parameters (different query, higher top_k, specific filters).
pub struct RagQueryTool {
    /// Shared RagClient instance (shared with MemoryManager for auto-retrieve)
    client: std::sync::Arc<RagClient>,
}

impl RagQueryTool {
    /// Create a new RAG query tool with the given client
    pub fn new(client: std::sync::Arc<RagClient>) -> Self {
        Self { client }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "rag_query".to_string(),
            description: "Query the enterprise knowledge base (RAG). Use this tool for targeted deep queries about enterprise-specific information such as product docs, internal processes, or company knowledge. Results are sourced from the configured RAG service.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query text"
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: from manifest config)",
                        "minimum": 1,
                        "maximum": 50
                    },
                    "score_threshold": {
                        "type": "number",
                        "description": "Minimum relevance score (0.0-1.0). Only results above this threshold are returned.",
                        "minimum": 0.0,
                        "maximum": 1.0
                    },
                    "filters": {
                        "type": "object",
                        "description": "Optional filters for enterprise-specific query refinement",
                        "additionalProperties": true
                    }
                },
                "required": ["query"]
            }),
        }
    }
}

#[async_trait]
impl Tool for RagQueryTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let query = params["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'query' parameter".to_string()),
                token_usage: None,
            });
        }

        let top_k = params["top_k"].as_u64().map(|v| v as u32);
        let score_threshold = params["score_threshold"].as_f64().map(|v| v as f32);
        let filters = params.get("filters").cloned();

        let results = self.client
            .query_with_params(query, top_k, score_threshold, filters)
            .await;

        if results.is_empty() {
            return Ok(ToolResult {
                ok: true,
                content: "No relevant results found in enterprise knowledge base.".to_string(),
                error: None,
                token_usage: None,
            });
        }

        // Format results for LLM consumption with source annotations
        let mut content_parts: Vec<String> = Vec::new();
        for (i, result) in results.iter().enumerate() {
            let mut part = format!(
                "{} [score={:.2}]",
                result.item.content,
                result.item.score
            );
            if let Some(ref source_url) = result.item.source_url {
                part.push_str(&format!(" (source: {source_url})"));
            }
            if let Some(ref chunk_id) = result.item.chunk_id {
                part.push_str(&format!(" [chunk: {chunk_id}]"));
            }
            content_parts.push(format!("{}. {part}", i + 1));
        }

        let content = format!(
            "Enterprise knowledge results for \"{}\":\n{}",
            query,
            content_parts.join("\n")
        );

        Ok(ToolResult {
            ok: true,
            content,
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::rag::client::{RagAuthCredential, RagClientConfig};
    use std::time::Duration;

    fn test_rag_client() -> std::sync::Arc<RagClient> {
        let config = RagClientConfig {
            endpoint: "https://10.255.255.1/v1/query".to_string(), // non-routable
            collection: Some("test_docs".to_string()),
            auth: RagAuthCredential::None,
            default_max_results: 5,
            default_score_threshold: 0.7,
            timeout: Duration::from_millis(100),
            tool_name: "enterprise_knowledge".to_string(),
        };
        std::sync::Arc::new(RagClient::new(config))
    }

    #[test]
    fn test_rag_query_tool_spec() {
        let spec = RagQueryTool::spec_value();
        assert_eq!(spec.name, "rag_query");
        assert!(spec.input_schema["properties"]["query"].is_object());
        assert!(spec.input_schema["properties"]["top_k"].is_object());
        assert!(spec.input_schema["properties"]["filters"].is_object());
        assert!(spec.input_schema["required"].as_array().unwrap().contains(&serde_json::json!("query")));
    }

    #[tokio::test]
    async fn test_rag_query_tool_missing_query() {
        let client = test_rag_client();
        let tool = RagQueryTool::new(client);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing 'query'"));
    }

    #[tokio::test]
    async fn test_rag_query_tool_empty_query() {
        let client = test_rag_client();
        let tool = RagQueryTool::new(client);
        let result = tool.execute(serde_json::json!({ "query": "" })).await.unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_rag_query_tool_timeout_degrades_gracefully() {
        let client = test_rag_client();
        let tool = RagQueryTool::new(client);
        let result = tool
            .execute(serde_json::json!({ "query": "test query" }))
            .await
            .unwrap();
        // RAG unavailable → graceful degradation, ok=true with "no results" message
        assert!(result.ok);
        assert!(result.content.contains("No relevant results"));
    }
}
