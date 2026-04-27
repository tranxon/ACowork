//! rollball-core — Shared types, protocols, and traits for Rollball.AI
//!
//! This crate contains all types shared across the Rollball workspace:
//! - Manifest structures (`.agent` package format)
//! - Protocol messages (Gateway Service API)
//! - Tool and Provider traits
//! - Permission, Identity, Budget types
//! - Unified error types

pub mod manifest;
pub mod protocol;
pub mod transport;
pub mod intent;
pub mod permission;
pub mod identity;
pub mod budget;
pub mod tools;
pub mod providers;
pub mod memory;
pub mod error;

// Re-exports for convenience
pub use manifest::{AgentManifest, CapabilityDef, LlmConfig, ProviderConfig, RoutingConfig, LlmBudget, RagToolConfig, ToolDeclaration};
pub use protocol::{GatewayRequest, GatewayResponse, Frame};
pub use transport::{AsyncTransportConnection, AsyncTransportServer, TransportKind, classify_endpoint, default_endpoint};
pub use intent::Intent;
pub use permission::Permission;
pub use identity::{Identity, IdentityCategory, IdentityEntry, IdentityQueryResult, IdentityStore, IdentitySubscription, PrivacyLevel};
pub use budget::{Budget, UsageReport};
pub use tools::{Tool, ToolSpec, ToolResult};
pub use providers::{Provider, ChatMessage, ChatRequest, ChatResponse, StreamEvent, ProviderError, ProviderErrorType};
pub use error::RollballError;
