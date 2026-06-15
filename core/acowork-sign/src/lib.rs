//! acowork-sign — .agent package signing/verification toolchain
//!
//! Provides three CLI commands:
//! - acowork-keygen: Generate Ed25519 key pairs
//! - acowork-sign: Sign .agent packages
//! - acowork-verify: Verify .agent package signatures

pub mod certificate;
pub mod error;
pub mod keygen;
pub mod packager;
pub mod sign;
pub mod signing_block;
pub mod verify;
pub mod zip_utils;
