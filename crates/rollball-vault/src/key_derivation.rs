//! Key derivation (password → master key) using Argon2id
//!
//! Derives a 256-bit master key from a user password using Argon2id,
//! which is the recommended password hashing algorithm (winner of the
//! Password Hashing Competition 2015).
//!
//! The salt is stored alongside the vault data so that the same password
//! always produces the same master key.

use argon2::{Algorithm, Argon2, Params, Version};
use crate::error::{Result, VaultError};

/// Salt length in bytes (16 bytes = 128 bits)
pub const SALT_LEN: usize = 16;

/// Derived key length in bytes (32 bytes = 256 bits)
pub const KEY_LEN: usize = 32;

/// Argon2id parameters (memory: 64MB, iterations: 3, parallelism: 4)
///
/// These are conservative parameters suitable for a desktop application.
/// The memory cost of 64MB provides good resistance against GPU attacks.
const ARGON2_MEMORY_COST: u32 = 65536; // 64 MB in KiB
const ARGON2_TIME_COST: u32 = 3;
const ARGON2_PARALLELISM: u32 = 4;

/// Derive master key from password using Argon2id
///
/// # Arguments
/// * `password` - User's password
/// * `salt` - Random salt (16 bytes recommended)
///
/// # Returns
/// 32-byte derived key suitable for ChaCha20-Poly1305
pub fn derive_key(password: &str, salt: &[u8]) -> Result<Vec<u8>> {
    let params = Params::new(ARGON2_MEMORY_COST, ARGON2_TIME_COST, ARGON2_PARALLELISM, Some(KEY_LEN))
        .map_err(|e| VaultError::Encryption(format!("Invalid Argon2 params: {e}")))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = vec![0u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| VaultError::Encryption(format!("Key derivation failed: {e}")))?;

    Ok(key)
}

/// Generate random salt (16 bytes)
pub fn generate_salt() -> Vec<u8> {
    let mut salt = vec![0u8; SALT_LEN];
    rand::fill(&mut salt[..]);
    salt
}

/// Salt file name stored in vault directory
pub const SALT_FILE: &str = "salt";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_key_basic() {
        let salt = generate_salt();
        let key = derive_key("my_password", &salt).unwrap();
        assert_eq!(key.len(), KEY_LEN);
    }

    #[test]
    fn test_derive_key_deterministic() {
        let salt = vec![0xAA; SALT_LEN];
        let key1 = derive_key("password123", &salt).unwrap();
        let key2 = derive_key("password123", &salt).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_derive_key_different_passwords() {
        let salt = vec![0xAA; SALT_LEN];
        let key1 = derive_key("password1", &salt).unwrap();
        let key2 = derive_key("password2", &salt).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_derive_key_different_salts() {
        let salt1 = vec![0xAA; SALT_LEN];
        let salt2 = vec![0xBB; SALT_LEN];
        let key1 = derive_key("same_password", &salt1).unwrap();
        let key2 = derive_key("same_password", &salt2).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_generate_salt() {
        let salt1 = generate_salt();
        let salt2 = generate_salt();
        assert_eq!(salt1.len(), SALT_LEN);
        assert_eq!(salt2.len(), SALT_LEN);
        // Two salts should be different (extremely unlikely to collide)
        assert_ne!(salt1, salt2);
    }
}
