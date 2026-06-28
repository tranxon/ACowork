//! acowork-core — Shared types, protocols, and traits for ACowork.AI
//!
//! This crate contains all types shared across the ACowork workspace:
//! - Manifest structures (`.agent` package format)
//! - Protocol messages (Gateway Service API)
//! - Tool and Provider traits
//! - Permission, Identity, Budget types
//! - Unified error types

pub mod proto_bridge;
pub mod proto {
    #![allow(clippy::large_enum_variant)]
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("acowork.ipc.v1");
}

pub mod budget;
pub mod crlf;
pub mod defaults;
pub mod error;
pub mod event_bus;
pub mod health;
pub mod intent;
pub mod logging;
pub mod manifest;
pub mod memory;
pub mod packaging;
pub mod path_utils;
pub mod permission;
pub mod process;
pub mod protocol;
pub mod providers;
pub mod shutdown;
pub mod supervisor;
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
pub use path_utils::{is_absolute, resolve};
pub use permission::{Permission, ShellApprovalThreshold};
pub use process::{IdleTimeoutError, ProcessOutput, run_command_with_idle_timeout, run_with_idle_timeout};
pub use providers::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, ImageUrlPart, Provider, ProviderError,
    ProviderErrorType, StreamEvent,
};
pub use shutdown::{Shutdown, install_signal_handlers};
pub use tools::{Tool, ToolResult, ToolSpec};
