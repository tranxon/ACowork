//! acowork-runtime — Agent Runtime library
//!
//! Unified execution engine for .agent packages.

pub mod agent;
pub mod agent_config;
pub mod cli;
pub mod config;
pub mod conversation;
pub mod debug;
pub mod embedding;
pub mod episode_distill;
pub mod error;
pub mod grpc;
pub mod ipc;
pub mod mcp_notify;
pub mod memory;
pub mod package;
pub mod platform;
pub mod prompt;
pub mod providers;
pub mod security;
pub mod skills;
pub mod token;
pub mod tools;
