//! ChaCha20-Poly1305 AEAD encryption layer
//!
//! Adapted from zeroclaw/src/security/secrets.rs
//! Rollball deviation: uses password-derived master key (Argon2id) instead of
//! a random key stored in a file. File format: nonce (12B) + ciphertext + tag (16B).

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};

use crate::error::{Result, VaultError};

/// ChaCha20-Poly1305 nonce length in bytes
pub const NONCE_LEN: usize = 12;

/// Encrypt data with ChaCha20-Poly1305
///
/// Returns: `nonce (12B) || ciphertext || tag (16B)`
///
/// The key must be exactly 32 bytes (256-bit).
pub fn encrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 32 {
        return Err(VaultError::Encryption(
            "Key must be 32 bytes for ChaCha20-Poly1305".into(),
        ));
    }

    let key = Key::from_slice(key);
    let cipher = ChaCha20Poly1305::new(key);

    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, data)
        .map_err(|e| VaultError::Encryption(format!("Encryption failed: {e}")))?;

    // Prepend nonce to ciphertext (nonce || ciphertext || tag)
    let mut result = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Decrypt data with ChaCha20-Poly1305
///
/// Input format: `nonce (12B) || ciphertext || tag (16B)`
pub fn decrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 32 {
        return Err(VaultError::DecryptionFailed(
            "Key must be 32 bytes for ChaCha20-Poly1305".into(),
        ));
    }

    if data.len() < NONCE_LEN {
        return Err(VaultError::DecryptionFailed("Data too short to contain nonce".into()));
    }

    let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let key = Key::from_slice(key);
    let cipher = ChaCha20Poly1305::new(key);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| VaultError::DecryptionFailed("Decryption failed (wrong key or tampered data)".into()))?;

    Ok(plaintext)
}

/// Generate a random 32-byte key
pub fn generate_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::fill(&mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = generate_key();
        let plaintext = b"sk-openai-api-key-12345";
        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertext() {
        let key = generate_key();
        let plaintext = b"same data";
        let enc1 = encrypt(plaintext, &key).unwrap();
        let enc2 = encrypt(plaintext, &key).unwrap();
        // Different nonces should produce different ciphertext
        assert_ne!(enc1, enc2);
    }

    #[test]
    fn test_decrypt_wrong_key_fails() {
        let key1 = generate_key();
        let key2 = generate_key();
        let plaintext = b"secret data";
        let encrypted = encrypt(plaintext, &key1).unwrap();
        let result = decrypt(&encrypted, &key2);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_tampered_data_fails() {
        let key = generate_key();
        let plaintext = b"secret data";
        let mut encrypted = encrypt(plaintext, &key).unwrap();
        // Tamper with the ciphertext
        if encrypted.len() > NONCE_LEN + 1 {
            encrypted[NONCE_LEN + 1] ^= 0xFF;
        }
        let result = decrypt(&encrypted, &key);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_invalid_key_length() {
        let key = [0u8; 16]; // Wrong length
        let result = encrypt(b"data", &key);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_too_short() {
        let key = generate_key();
        let result = decrypt(&[0u8; 5], &key);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypted_format_starts_with_nonce() {
        let key = generate_key();
        let encrypted = encrypt(b"test", &key).unwrap();
        // First 12 bytes should be the nonce (random, not all zeros)
        assert_ne!(&encrypted[0..12], &[0u8; 12]);
        // Total length = 12 (nonce) + 4 (plaintext) + 16 (tag) = 32
        assert_eq!(encrypted.len(), NONCE_LEN + 4 + 16);
    }
}
