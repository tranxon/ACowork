//! UseCase traits — business-logic abstractions for Runtime adapters.
//!
//! ADR-040: adapter modules (HTTP server, gateway loop) depend on these
//! traits rather than calling concrete functions directly.

pub mod agent_config;
pub mod agent_token;
pub mod agent_tools;
pub mod memory_query;
pub mod session_control;
pub mod session_metadata;
pub mod workspace_mutation;
pub mod workspace_query;

// Trait re-exports
pub use agent_config::{
    AgentConfigError, AgentConfigService, ConfigField, ConfigFieldPatch, FieldPatch,
    GetAgentConfigResponse, PutAgentConfigBody, PutAgentConfigResult,
};
pub use agent_token::AgentTokenService;
pub use agent_tools::{
    AgentToolsError, AgentToolsService, BuiltinToolsResponse, PutBuiltinToolsBody,
    PutMcpServersBody, PutSearchConfigBody,
};
pub use memory_query::MemoryQueryService;
pub use session_control::SessionControlService;
pub use session_metadata::SessionMetadataService;
pub use workspace_mutation::WorkspaceMutationService;
pub use workspace_query::{WorkspaceError, WorkspaceQueryService};

// Implementation structs.
pub mod agent_config_impl;
pub mod agent_token_impl;
pub mod agent_tools_impl;
pub mod memory_query_impl;
pub mod session_metadata_impl;
pub mod workspace_mutation_impl;
pub mod workspace_query_impl;

pub use agent_config_impl::RuntimeAgentConfigService;
pub use agent_token_impl::RuntimeAgentTokenService;
pub use agent_tools_impl::RuntimeAgentToolsService;
pub use memory_query_impl::GrafeoMemoryAdapter;
pub use session_metadata_impl::RuntimeSessionMetadataService;
pub use workspace_mutation_impl::RuntimeWorkspaceMutationService;
pub use workspace_query_impl::RuntimeWorkspaceQueryService;