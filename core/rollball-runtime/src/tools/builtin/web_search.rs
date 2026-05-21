//! Web search tool — search the web using Brave/SearXNG

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

use crate::tools::output;

pub struct WebSearchTool { client: reqwest::Client }

impl WebSearchTool {
    pub fn new() -> Self { Self { client: reqwest::Client::new() } }
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "Search the web using a search engine. Returns search results.".to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "query": { "type": "string", "description": "Search query" }, "count": { "type": "integer", "description": "Number of results (default 5)", "default": 5 } }, "required": ["query"] }),
        }
    }
}

impl Default for WebSearchTool { fn default() -> Self { Self::new() } }

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let query = params["query"].as_str().unwrap_or("");
        if query.is_empty() { return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'query'".to_string()), token_usage: None }); }

        // Use DuckDuckGo HTML search as a simple fallback (no API key needed)
        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(query));

        match self.client.get(&url).header("User-Agent", "Mozilla/5.0").send().await {
            Ok(resp) => {
                let html = resp.text().await.unwrap_or_default();
                // Extract result snippets from DDG HTML
                let results = extract_ddg_results(&html);
                if results.is_empty() {
                    Ok(ToolResult { ok: true, content: "No results found".to_string(), error: None, token_usage: None })
                } else {
                    let joined = results.join("\n\n");
                    let (content, _truncated) = output::truncate_output(&joined);
                    Ok(ToolResult { ok: true, content, error: None, token_usage: None })
                }
            }
            Err(e) => Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Search failed: {e}")), token_usage: None }),
        }
    }
}

fn extract_ddg_results(html: &str) -> Vec<String> {
    let mut results = Vec::new();
    // Simple extraction — look for result__a (title links) and result__snippet
    let title_re = regex::Regex::new(r#"class="result__a"[^>]*>(.*?)</a>"#).unwrap();
    let snippet_re = regex::Regex::new(r#"class="result__snippet"[^>]*>(.*?)</[at]"#).unwrap();

    let titles: Vec<String> = title_re.captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| strip_tags(m.as_str())))
        .collect();

    let snippets: Vec<String> = snippet_re.captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| strip_tags(m.as_str())))
        .collect();

    for (i, title) in titles.iter().enumerate().take(5) {
        let raw_snippet = snippets.get(i).cloned().unwrap_or_default();
        let snippet = output::truncate_line(&raw_snippet);
        results.push(format!("{}. {title}\n   {snippet}", i + 1));
    }

    results
}

fn strip_tags(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]*>").unwrap();
    re.replace_all(s, "").trim().to_string()
}

mod urlencoding {
    pub fn encode(s: &str) -> String {
        s.chars().map(|c| {
            if c.is_ascii_alphanumeric() || "-_.~".contains(c) { c.to_string() }
            else { format!("%{:02X}", c as u8) }
        }).collect()
    }
}
