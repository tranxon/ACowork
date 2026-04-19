//! rollball-gateway — Gateway library
//!
//!常驻系统级进程，管理 Agent 生命周期、Intent 路由、密钥分发、预算协调。

pub mod gateway;
pub mod package_manager;
pub mod lifecycle;
pub mod intent;
pub mod budget;
pub mod rate;
pub mod vault;
pub mod ipc;
pub mod config;
pub mod cli;
pub mod error;
