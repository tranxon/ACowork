//! acowork-vault — Encrypted API key storage
//!
//! Provides secure storage for LLM API keys with:
//! - Password-derived master key (Argon2id)
//! - ChaCha20-Poly1305 AEAD encryption
//! - One-time key distribution via IPC

pub mod encryption;
pub mod error;
pub mod key_derivation;
pub mod vault;

// Re-export primary type for convenience
pub use vault::Vault;
