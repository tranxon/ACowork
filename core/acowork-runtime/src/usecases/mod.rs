//! UseCase traits — business-logic abstractions for Runtime adapters.
//!
//! ADR-040: adapter modules (HTTP server, gateway loop) depend on these
//! traits rather than calling concrete functions directly.

pub mod agent_token;
pub mod memory_query;
pub mod session_control;
pub mod session_metadata;

// Trait re-exports
pub use agent_token::AgentTokenService;
pub use memory_query::MemoryQueryService;
pub use session_control::SessionControlService;
pub use session_metadata::SessionMetadataService;

// Implementation structs.
pub mod agent_token_impl;
pub mod memory_query_impl;
pub mod session_metadata_impl;

pub use agent_token_impl::RuntimeAgentTokenService;
pub use memory_query_impl::GrafeoMemoryAdapter;
pub use session_metadata_impl::RuntimeSessionMetadataService;
