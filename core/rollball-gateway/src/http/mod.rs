//! HTTP API module
//!
//! Provides REST + WebSocket API for Desktop App and CLI access.
//! Shares `Arc<RwLock<GatewayState>>` with the IPC server.

pub mod server;
pub mod routes;
pub mod auth;
pub mod agents;
pub mod agent_config;
pub mod approval;
pub mod question;
pub mod documents;
pub mod chat;
pub mod vault_api;
pub mod config_api;
pub mod cron_api;
pub mod models_api;
pub mod memory_api;
pub mod skills_api;
pub mod workspaces;
pub mod publish_api;
