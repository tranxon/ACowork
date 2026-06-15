//! Key pair generation (Ed25519) and X.509 certificate creation
//!
//! Provides CLI commands:
//! - `acowork-keygen --type developer --output-dir <path>`
//! - `acowork-keygen --type platform --output-dir <path>`

use ed25519_dalek::{SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

use crate::error::{Result, SignError};

/// Key pair container
#[derive(Debug)]
pub struct KeyPair {
    pub signing_key: SigningKey,
    pub verifying_key: VerifyingKey,
}

impl KeyPair {
    /// Generate a new Ed25519 key pair
    pub fn generate() -> Result<Self> {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret);
        let signing_key = SigningKey::from_bytes(&secret);
        let verifying_key = signing_key.verifying_key();
        Ok(Self {
            signing_key,
            verifying_key,
        })
    }

    /// Get the public key bytes (32 bytes for Ed25519)
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }

    /// Get the secret key bytes (32 bytes for Ed25519)
    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Get the fingerprint (SHA-256 of public key, hex-encoded)
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.public_key_bytes());
        let hash = hasher.finalize();
        hex::encode(hash)
    }
}

/// Key type (Developer or Platform)
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
pub enum KeyType {
    Developer,
    Platform,
}

impl std::fmt::Display for KeyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyType::Developer => write!(f, "developer"),
            KeyType::Platform => write!(f, "platform"),
        }
    }
}

/// Generate a key pair and save to the specified directory
///
/// Creates two files:
/// - `<output_dir>/<key_type>.key` — secret key (32 bytes, raw)
/// - `<output_dir>/<key_type>.pub` — public key (32 bytes, raw)
///
/// Also creates a simple certificate file:
/// - `<output_dir>/<key_type>.cert` — self-signed certificate (JSON format)
pub fn generate_and_save(output_dir: &Path, key_type: KeyType) -> Result<KeyPair> {
    fs::create_dir_all(output_dir)?;

    let keypair = KeyPair::generate()?;

    // Save secret key
    let secret_path = output_dir.join(format!("{key_type}.key"));
    fs::write(&secret_path, keypair.secret_key_bytes())?;

    // Save public key
    let public_path = output_dir.join(format!("{key_type}.pub"));
    fs::write(&public_path, keypair.public_key_bytes())?;

    // Create and save self-signed certificate
    let cert = SelfSignedCert {
        key_type,
        public_key: hex::encode(keypair.public_key_bytes()),
        fingerprint: keypair.fingerprint(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let cert_path = output_dir.join(format!("{key_type}.cert"));
    let cert_json =
        serde_json::to_string_pretty(&cert).map_err(|e| SignError::Certificate(e.to_string()))?;
    fs::write(&cert_path, cert_json)?;

    Ok(keypair)
}

/// Load a key pair from files
pub fn load_keypair(key_dir: &Path, key_type: KeyType) -> Result<KeyPair> {
    let secret_path = key_dir.join(format!("{key_type}.key"));
    let secret_bytes = fs::read(&secret_path)?;
    if secret_bytes.len() != 32 {
        return Err(SignError::Ed25519(format!(
            "Invalid secret key length: expected 32, got {}",
            secret_bytes.len()
        )));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&secret_bytes);
    let signing_key = SigningKey::from_bytes(&key);
    let verifying_key = signing_key.verifying_key();

    Ok(KeyPair {
        signing_key,
        verifying_key,
    })
}

/// Simple self-signed certificate (JSON format)
///
/// Phase 1 uses a simplified certificate format. Full X.509 certificates
/// will be supported in Phase 2+ for the Agent Store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SelfSignedCert {
    pub key_type: KeyType,
    pub public_key: String,
    pub fingerprint: String,
    pub created_at: String,
}

/// Verify a self-signed certificate matches a public key
pub fn verify_cert(cert: &SelfSignedCert, public_key: &[u8]) -> bool {
    let cert_pubkey = match hex::decode(&cert.public_key) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    if cert_pubkey.len() != 32 || public_key.len() != 32 {
        return false;
    }
    cert_pubkey.as_slice() == public_key
}

// Simple hex encoding (avoid adding hex crate dependency)
pub mod hex {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize] as char);
            s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
        }
        s
    }

    pub fn decode(s: &str) -> std::result::Result<Vec<u8>, super::HexDecodeError> {
        if !s.len().is_multiple_of(2) {
            return Err(super::HexDecodeError);
        }
        let mut bytes = Vec::with_capacity(s.len() / 2);
        for i in (0..s.len()).step_by(2) {
            let high = s.as_bytes()[i].to_ascii_lowercase().wrapping_sub(b'0');
            let low = s.as_bytes()[i + 1].to_ascii_lowercase().wrapping_sub(b'0');
            let high = if high > 9 { high - 39 } else { high };
            let low = if low > 9 { low - 39 } else { low };
            if high > 15 || low > 15 {
                return Err(super::HexDecodeError);
            }
            bytes.push(high << 4 | low);
        }
        Ok(bytes)
    }
}

/// Error type for hex decoding failures
#[derive(Debug, Clone)]
pub struct HexDecodeError;

impl std::fmt::Display for HexDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid hex string")
    }
}

impl std::error::Error for HexDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keypair_generate() {
        let kp = KeyPair::generate().unwrap();
        assert_eq!(kp.public_key_bytes().len(), 32);
        assert_eq!(kp.secret_key_bytes().len(), 32);
    }

    #[test]
    fn test_keypair_fingerprint() {
        let kp = KeyPair::generate().unwrap();
        let fp = kp.fingerprint();
        // SHA-256 hex = 64 chars
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_keypair_deterministic() {
        let kp = KeyPair::generate().unwrap();
        // Same keypair should give same bytes
        assert_eq!(kp.public_key_bytes(), kp.public_key_bytes());
        assert_eq!(kp.secret_key_bytes(), kp.secret_key_bytes());
    }

    #[test]
    fn test_generate_and_save() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-keygen");
        let _ = fs::remove_dir_all(&tmp_dir);

        let kp = generate_and_save(&tmp_dir, KeyType::Developer).unwrap();

        // Check files exist
        assert!(tmp_dir.join("developer.key").exists());
        assert!(tmp_dir.join("developer.pub").exists());
        assert!(tmp_dir.join("developer.cert").exists());

        // Check key file size
        let secret_data = fs::read(tmp_dir.join("developer.key")).unwrap();
        assert_eq!(secret_data.len(), 32);

        let pub_data = fs::read(tmp_dir.join("developer.pub")).unwrap();
        assert_eq!(pub_data.len(), 32);

        // Load back
        let loaded = load_keypair(&tmp_dir, KeyType::Developer).unwrap();
        assert_eq!(loaded.public_key_bytes(), kp.public_key_bytes());

        // Cleanup
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_self_signed_cert_roundtrip() {
        let kp = KeyPair::generate().unwrap();
        let cert = SelfSignedCert {
            key_type: KeyType::Developer,
            public_key: hex::encode(kp.public_key_bytes()),
            fingerprint: kp.fingerprint(),
            created_at: "2026-04-17T12:00:00Z".into(),
        };

        let json = serde_json::to_string(&cert).unwrap();
        let parsed: SelfSignedCert = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.key_type, KeyType::Developer);
        assert_eq!(parsed.fingerprint, kp.fingerprint());
    }

    #[test]
    fn test_verify_cert() {
        let kp = KeyPair::generate().unwrap();
        let cert = SelfSignedCert {
            key_type: KeyType::Developer,
            public_key: hex::encode(kp.public_key_bytes()),
            fingerprint: kp.fingerprint(),
            created_at: "2026-04-17T12:00:00Z".into(),
        };

        assert!(verify_cert(&cert, &kp.public_key_bytes()));

        // Wrong key should fail
        let kp2 = KeyPair::generate().unwrap();
        assert!(!verify_cert(&cert, &kp2.public_key_bytes()));
    }

    #[test]
    fn test_hex_encode_decode() {
        let bytes = vec![0x00, 0x01, 0xAA, 0xFF];
        let encoded = hex::encode(&bytes);
        assert_eq!(encoded, "0001aaff");
        let decoded = hex::decode(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }
}
