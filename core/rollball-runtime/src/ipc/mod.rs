//! IPC module
//!
//! Contains shared types for Gateway client communication.
//! The legacy IPC transport and GatewayClient have been removed
//! in favor of gRPC (see crate::grpc).

pub mod client;
