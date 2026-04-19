//! Web fetch tool — fetch webpage and convert to plain text

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

pub struct WebFetchTool { client: reqwest::Client }

impl WebFetchTool {
    pub fn new() -> Self { Self { client: reqwest::Client::new() } }
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_string(),
            description: "Fetch a webpage and extract its text content, stripping HTML tags.".to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "url": { "type": "string", "description": "URL to fetch" } }, "required": ["url"] }),
        }
    }
}

impl Default for WebFetchTool { fn default() -> Self { Self::new() } }

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let url = params["url"].as_str().unwrap_or("");
        if url.is_empty() { return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'url'".to_string()), token_usage: None }); }

        match self.client.get(url).send().await {
            Ok(resp) => {
                let html = resp.text().await.unwrap_or_default();
                // Simple HTML-to-text: strip tags
                let text = strip_html_tags(&html);
                let truncated = if text.len() > 10000 { &text[..10000] } else { &text };
                Ok(ToolResult { ok: true, content: truncated.to_string(), error: None, token_usage: None })
            }
            Err(e) => Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Fetch failed: {e}")), token_usage: None }),
        }
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    let mut in_script = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                if result.len() > 0 && !result.ends_with(' ') && !result.ends_with('\n') { result.push(' '); }
            }
            '>' => {
                in_tag = false;
                // Skip script/style content
                if result.to_lowercase().ends_with("<script") || result.to_lowercase().ends_with("<style") { in_script = true; }
            }
            _ if in_tag => {}
            _ if in_script => {}
            _ => result.push(ch),
        }
        if in_script && result.to_lowercase().ends_with("</script>") {
            in_script = false;
        }
    }
    // Collapse whitespace
    let re = regex::Regex::new(r"\s+").unwrap();
    re.replace_all(&result, " ").trim().to_string()
}
