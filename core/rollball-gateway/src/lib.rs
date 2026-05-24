//! rollball-gateway — Gateway library
//!
//! Long-running system process: manages Agent lifecycle, Intent routing, key distribution, budget coordination.

pub mod gateway;
pub mod package_manager;
pub mod lifecycle;
pub mod intent;
pub mod capability;
pub mod budget;
pub mod rate;
pub mod vault;
pub mod ipc;
pub mod config;
pub mod cli;
pub mod error;
pub mod cron;
pub mod http;
pub mod grpc;
pub mod resource_cache;

/// Type alias for the tracing reload handle used to dynamically change log levels.
pub type LogReloadHandle = tracing_subscriber::reload::Handle<
    tracing_subscriber::EnvFilter,
    tracing_subscriber::Registry,
>;
