//! rollball-core — Shared types, protocols, and traits for Rollball.AI
//!
//! This crate contains all types shared across the Rollball workspace:
//! - Manifest structures (`.agent` package format)
//! - Protocol messages (Gateway Service API)
//! - Tool and Provider traits
//! - Permission, Identity, Budget types
//! - Unified error types

pub mod proto_bridge;
pub mod proto {
    #![allow(clippy::large_enum_variant)]
    tonic::include_proto!("rollball.ipc.v1");
}

pub mod defaults;
pub mod manifest;
pub mod protocol;
pub mod intent;
pub mod permission;
pub mod identity;
pub mod budget;
pub mod tools;
pub mod providers;
pub mod memory;
pub mod packaging;
pub mod error;
pub mod crlf;
pub mod logging;

// Re-exports for convenience
pub use manifest::{AgentManifest, CapabilityDef, LlmConfig, ProviderConfig, RoutingConfig, LlmBudget, RagToolConfig, ToolDeclaration, SkillMode, SkillsConfig};
pub use protocol::{GatewayRequest, GatewayResponse, ModelCapabilitiesInfo, ModelCostInfo, ModelModalities, ProtocolType, SessionInfoDto, ConversationEntryDto};

pub use intent::Intent;
pub use permission::Permission;
pub use identity::{Identity, IdentityCategory, IdentityEntry, IdentityQueryResult, IdentityStore, IdentitySubscription, PrivacyLevel};
pub use budget::{Budget, UsageReport};
pub use tools::{Tool, ToolSpec, ToolResult};
pub use providers::{Provider, ChatMessage, ChatRequest, ChatResponse, StreamEvent, ProviderError, ProviderErrorType};
pub use error::RollballError;
pub use packaging::{PackageOptions, should_exclude_path, PACKAGE_ALWAYS_EXCLUDE_DIRS, PACKAGE_EXCLUDE_PATTERNS, PACKAGE_DEFAULT_EXCLUDE_DIRS};
