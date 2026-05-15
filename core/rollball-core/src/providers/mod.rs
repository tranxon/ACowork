//! Provider modules

pub mod traits;
pub mod mock;
pub mod aliases;
pub mod error_patterns;

pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall, MessageRole, Provider, ProviderError,
    ProviderErrorType, StreamError, StreamEvent, ToolCall, UsageInfo,
};
pub use mock::{MockProvider, MockResponse};
pub use aliases::{canonical_provider_id, vault_key_candidates};
pub use error_patterns::{classify_stream_error, is_context_overflow, is_stream_decode_error};
