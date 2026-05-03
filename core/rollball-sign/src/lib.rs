//! rollball-sign — .agent package signing/verification toolchain
//!
//! Provides three CLI commands:
//! - rollball-keygen: Generate Ed25519 key pairs
//! - rollball-sign: Sign .agent packages
//! - rollball-verify: Verify .agent package signatures

pub mod signing_block;
pub mod keygen;
pub mod sign;
pub mod verify;
pub mod certificate;
pub mod error;
pub mod zip_utils;
pub mod packager;
