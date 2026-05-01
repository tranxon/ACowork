//! Mock Provider for testing
//!
//! Provides a configurable mock LLM provider that returns canned responses.
//! Used for integration testing without real LLM API calls.

use async_trait::async_trait;
use futures_core::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};

use crate::error::Result;
use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall, Provider, StreamEvent,
    ToolCall, UsageInfo,
};
#[cfg(test)]
use crate::providers::traits::MessageRole;

/// A single canned response step for the mock provider
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// Return a text response (ends the conversation)
    Text { content: String },
    /// Return tool calls (continues the loop)
    ToolCalls { tool_calls: Vec<ToolCall>, content: String },
    /// Return an error
    Error { message: String },
}

/// Mock Provider for testing
///
/// Returns a sequence of canned responses, then defaults to text responses.
/// Tracks call count and last request for assertions.
pub struct MockProvider {
    /// Sequence of responses to return
    responses: Mutex<Vec<MockResponse>>,
    /// Number of times `chat()` was called
    call_count: AtomicU64,
    /// Last request received
    last_request: Mutex<Option<ChatRequest>>,
    /// Default model name
    model: String,
}

impl MockProvider {
    /// Create a new mock provider with the given response sequence
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            call_count: AtomicU64::new(0),
            last_request: Mutex::new(None),
            model: "mock-model".to_string(),
        }
    }

    /// Create a mock provider that returns a single text response
    pub fn single_text(content: &str) -> Self {
        Self::new(vec![MockResponse::Text {
            content: content.to_string(),
        }])
    }

    /// Create a mock provider that first calls tools, then returns text
    pub fn tool_call_then_text(
        tool_name: &str,
        tool_args: &str,
        final_text: &str,
    ) -> Self {
        Self::new(vec![
            MockResponse::ToolCalls {
                tool_calls: vec![ToolCall {
                    id: "call_mock_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: tool_name.to_string(),
                        arguments: tool_args.to_string(),
                    },
                }],
                content: String::new(),
            },
            MockResponse::Text {
                content: final_text.to_string(),
            },
        ])
    }

    /// Get the number of times chat() was called
    pub fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Get the last request received
    pub fn last_request(&self) -> Option<ChatRequest> {
        self.last_request.lock().ok().and_then(|req| req.clone())
    }

    /// Get the model name
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        // Store last request
        if let Ok(mut last) = self.last_request.lock() {
            *last = Some(request.clone());
        }

        // Get next response
        let response = {
            let mut responses = self.responses.lock().map_err(|e| {
                crate::error::RollballError::Provider(crate::providers::ProviderError::unknown(format!("Mock lock error: {e}")))
            })?;
            if responses.is_empty() {
                MockResponse::Text {
                    content: "Mock default response.".to_string(),
                }
            } else {
                responses.remove(0)
            }
        };

        match response {
            MockResponse::Text { content } => Ok(ChatResponse {
                content,
                tool_calls: None,
                usage: Some(UsageInfo {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                }),
            }),
            MockResponse::ToolCalls { tool_calls, content } => Ok(ChatResponse {
                content,
                tool_calls: Some(tool_calls),
                usage: Some(UsageInfo {
                    prompt_tokens: 200,
                    completion_tokens: 100,
                    total_tokens: 300,
                }),
            }),
            MockResponse::Error { message } => {
                Err(crate::error::RollballError::Provider(crate::providers::ProviderError::unknown(message)))
            }
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let response = self.chat(request).await?;
        let stream = MockStream {
            response: Some(response),
        };
        Ok(Box::new(stream))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> Result<u64> {
        let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
        Ok((total_chars as u64) / 4)
    }
}

/// Simple mock stream that yields a single Finished event
struct MockStream {
    response: Option<ChatResponse>,
}

impl Stream for MockStream {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.response.take() {
            Some(resp) => Poll::Ready(Some(StreamEvent::Finished(resp))),
            None => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_single_text() {
        let provider = MockProvider::single_text("Hello, world!");
        let request = ChatRequest {
            model: "mock".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "Hi".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: None,
            max_tokens: None,
            tools: None,
        };

        let response = provider.chat(request).await.unwrap();
        assert_eq!(response.content, "Hello, world!");
        assert!(response.tool_calls.is_none());
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn test_mock_tool_call_then_text() {
        let provider = MockProvider::tool_call_then_text(
            "calculator",
            r#"{"expression": "2+2"}"#,
            "The answer is 4.",
        );

        let request = ChatRequest {
            model: "mock".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "What is 2+2?".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: None,
            max_tokens: None,
            tools: None,
        };

        let response = provider.chat(request.clone()).await.unwrap();
        assert!(response.tool_calls.is_some());
        let tool_calls = response.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "calculator");

        let response = provider.chat(request).await.unwrap();
        assert_eq!(response.content, "The answer is 4.");
        assert!(response.tool_calls.is_none());

        assert_eq!(provider.call_count(), 2);
    }

    #[tokio::test]
    async fn test_mock_error() {
        let provider = MockProvider::new(vec![MockResponse::Error {
            message: "API unavailable".to_string(),
        }]);

        let request = ChatRequest {
            model: "mock".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "Hi".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: None,
            max_tokens: None,
            tools: None,
        };

        let result = provider.chat(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_default_response() {
        let provider = MockProvider::new(vec![]);
        let request = ChatRequest {
            model: "mock".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "Hi".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: None,
            max_tokens: None,
            tools: None,
        };

        let response = provider.chat(request).await.unwrap();
        assert_eq!(response.content, "Mock default response.");
    }

    #[tokio::test]
    async fn test_mock_tracks_last_request() {
        let provider = MockProvider::single_text("ok");
        let request = ChatRequest {
            model: "mock".to_string(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                content: "test message".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: Some(0.5),
            max_tokens: None,
            tools: None,
        };

        let _ = provider.chat(request.clone()).await.unwrap();
        let last = provider.last_request().unwrap();
        assert_eq!(last.messages.len(), 1);
        assert_eq!(last.messages[0].content, "test message");
    }
}
