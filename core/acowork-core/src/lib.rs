//! acowork-core — Shared types, protocols, and traits for ACowork.AI
//!
//! This crate contains all types shared across the ACowork workspace:
//! - Manifest structures (`.agent` package format)
//! - Protocol messages (MQTT payload types, JSON DTOs)
//! - Tool and Provider traits
//! - Permission, Identity, Budget types
//! - Unified error types

pub mod mqtt_proto {
    #![allow(clippy::large_enum_variant)]
    include!(concat!(env!("OUT_DIR"), "/acowork.mqtt.v1.rs"));
}

pub mod budget;
pub mod crlf;
pub mod defaults;
pub mod embedding;
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
pub mod rag;
pub mod shutdown;
pub mod supervisor;
pub mod timeout_config;
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
pub use embedding::{EmbeddingError, EmbeddingProvider};
pub use error::AcoworkError;
pub use intent::Intent;
pub use packaging::{
    PACKAGE_ALWAYS_EXCLUDE_DIRS, PACKAGE_DEFAULT_EXCLUDE_DIRS, PACKAGE_EXCLUDE_PATTERNS,
    PackageOptions, should_exclude_path,
};
pub use path_utils::{is_absolute, resolve};
pub use permission::{Permission, ShellApprovalThreshold};
pub use process::{
    IdleTimeoutError, ProcessOutput, run_command_with_idle_timeout, run_with_idle_timeout,
};
pub use providers::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, ImageUrlPart, Provider, ProviderError,
    ProviderErrorType, StreamEvent,
};
pub use rag::{
    AnnotatedRagResult, PROTOCOL_VERSION as RAG_PROTOCOL_VERSION, RagProvider, RagQueryRequest,
    RagQueryResponse, RagResultItem,
};
pub use shutdown::{Shutdown, install_signal_handlers};
pub use timeout_config::{RetryConfig, Timeouts};
pub use tools::{Tool, ToolResult, ToolSpec};
