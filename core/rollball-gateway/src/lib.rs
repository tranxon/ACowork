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
pub mod permission_store;
pub mod cron;
pub mod http;
