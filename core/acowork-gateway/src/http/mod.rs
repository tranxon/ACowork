//! HTTP API module
//!
//! Provides REST + WebSocket API for Desktop App and CLI access.
//! Shares `Arc<RwLock<GatewayState>>` with the IPC server.

pub mod agent_config;
pub mod agents;
pub mod approval;
pub mod auth;
pub mod chat;
pub mod config_api;
pub mod cron_api;
pub mod documents;
pub mod embedding_api;
pub mod fs_browse;
pub mod mcp_catalog_api;
pub mod memory_api;
pub mod models_api;
pub mod publish_api;
pub mod question;
pub mod routes;
pub mod server;
pub mod skills_api;
pub mod users_api;
pub mod vault_api;
pub mod workspaces;
