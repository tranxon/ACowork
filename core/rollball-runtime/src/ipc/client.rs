//! Shared types for Gateway client communication
//!
//! Contains types used by the gRPC client.
//! The legacy IPC GatewayClient has been removed in favor of gRPC.

use rollball_core::protocol::ModelCapabilitiesInfo;

/// LLM configuration received from Gateway
///
/// Contains the user's configured provider, model, API key, and optional base URL.
/// This is the primary way Agent Runtime gets its LLM credentials (PRD GTW-05, SEC-07).
#[derive(Debug, Clone)]
pub struct LlmConfigReceived {
    /// Provider name (e.g. "minimax", "openai")
    pub provider: String,
    /// Model identifier, or None to use manifest's suggested_model
    pub model: Option<String>,
    /// API key for the provider
    pub api_key: Option<String>,
    /// Base URL override (optional)
    pub base_url: Option<String>,
    /// Available models for this provider (user-selected)
    pub models: Vec<String>,
    /// Model capabilities from models.dev / offline data
    pub model_capabilities: Option<ModelCapabilitiesInfo>,
    /// Global max output tokens limit from Gateway config
    pub max_output_tokens_limit: u64,
}
