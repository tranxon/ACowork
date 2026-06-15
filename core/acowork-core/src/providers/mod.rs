//! Provider modules

pub mod aliases;
pub mod error_patterns;
pub mod mock;
pub mod traits;

pub use aliases::{canonical_provider_id, vault_key_candidates};
pub use error_patterns::{classify_stream_error, is_context_overflow, is_stream_decode_error};
pub use mock::{MockProvider, MockResponse};
pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall, ImageUrlPart, MessageRole,
    Provider, ProviderError, ProviderErrorType, StreamError, StreamEvent, ToolCall, UsageInfo,
};
