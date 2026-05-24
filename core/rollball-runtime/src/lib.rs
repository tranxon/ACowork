//! rollball-runtime — Agent Runtime library
//!
//! Unified execution engine for .agent packages.

pub mod platform;
pub mod agent;
pub mod package;
pub mod providers;
pub mod tools;
pub mod memory;
pub mod skills;
pub mod ipc;
pub mod grpc;
pub mod config;
pub mod agent_config;
pub mod cli;
pub mod error;
pub mod token;
pub mod embedding;
pub mod security;
pub mod conversation;
pub mod episode_distill;
pub mod debug;
