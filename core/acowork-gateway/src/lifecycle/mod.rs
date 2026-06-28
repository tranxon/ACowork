//! Lifecycle management module

pub mod embed;
pub mod embed_supervisor;
pub mod lsp_relay;
pub mod lsp_relay_supervisor;
pub mod manager;
pub mod process;

pub use manager::SYSTEM_AGENT_ID;
