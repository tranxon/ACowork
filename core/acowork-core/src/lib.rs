//! acowork-core — Shared types, protocols, and traits for AgentCowork.AI
//!
//! This crate contains all types shared across the AgentCowork workspace:
//! - Manifest structures (`.agent` package format)
//! - Protocol messages (Gateway Service API)
//! - Tool and Provider traits
//! - Permission, Identity, Budget types
//! - Unified error types

pub mod proto_bridge;
pub mod proto {
    #![allow(clippy::large_enum_variant)]
    tonic::include_proto!("acowork.ipc.v1");
}

pub mod budget;
pub mod crlf;
pub mod defaults;
pub mod error;
pub mod intent;
pub mod logging;
pub mod manifest;
pub mod memory;
pub mod packaging;
pub mod permission;
pub mod protocol;
pub mod providers;
pub mod tools;

// Re-exports for convenience
pub use manifest::{
    AgentManifest, CapabilityDef, LlmBudget, LlmConfig, ProviderConfig, RagToolConfig,
    RoutingConfig, SkillMode, SkillsConfig, ToolDeclaration,
};
pub use protocol::{
    ConversationEntryDto, GatewayRequest, GatewayResponse, ModelCapabilitiesInfo, ModelCostInfo,
    ModelModalities, ProtocolType, SessionInfoDto, SessionStatusDto,
};

pub use budget::{Budget, UsageReport};
pub use error::AcoworkError;
pub use intent::Intent;
pub use packaging::{
    PACKAGE_ALWAYS_EXCLUDE_DIRS, PACKAGE_DEFAULT_EXCLUDE_DIRS, PACKAGE_EXCLUDE_PATTERNS,
    PackageOptions, should_exclude_path,
};
pub use permission::{Permission, ShellApprovalThreshold};
pub use providers::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, ImageUrlPart, Provider, ProviderError,
    ProviderErrorType, StreamEvent,
};
pub use tools::{Tool, ToolResult, ToolSpec};
