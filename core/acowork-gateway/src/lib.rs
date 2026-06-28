//! acowork-gateway — Gateway library
//!
//! Long-running system process: manages Agent lifecycle, Intent routing, key distribution, budget coordination.

pub mod budget;
pub mod capability;
pub mod cli;
pub mod config;
pub mod cron;
pub mod error;
pub mod gateway;
pub mod grpc;
pub mod http;
pub mod intent;
pub mod interaction_store;
pub mod ipc;
pub mod lifecycle;
pub mod package_manager;
pub mod rate;
pub mod resource_cache;
pub mod vault;

/// Type alias for the tracing reload handle used to dynamically change log levels.
pub type LogReloadHandle =
    tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;
