//! Certificate handling
//!
//! Phase 1 uses a simplified self-signed certificate format (JSON).
//! Full X.509 certificate support will be added in Phase 2+ for the Agent Store.

use crate::error::{Result, SignError};
use crate::keygen::{SelfSignedCert, KeyType};

/// Parse a self-signed certificate from JSON
pub fn parse_certificate(data: &[u8]) -> Result<CertificateInfo> {
    let cert: SelfSignedCert = serde_json::from_slice(data)
        .map_err(|e| SignError::Certificate(format!("Failed to parse certificate: {e}")))?;

    Ok(CertificateInfo {
        subject: format!("{:?}", cert.key_type),
        issuer: format!("{:?}", cert.key_type), // self-signed
        fingerprint: cert.fingerprint,
        valid_from: cert.created_at.clone(),
        valid_until: "never".to_string(), // self-signed certs don't expire in Phase 1
    })
}

/// Certificate information
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    pub subject: String,
    pub issuer: String,
    pub fingerprint: String,
    pub valid_from: String,
    pub valid_until: String,
}

/// Verify certificate chain
///
/// Phase 1 only supports:
/// - Developer self-signed certificates (always valid)
/// - Platform certificates (must match built-in root certificate)
pub fn verify_chain(certificates: &[Vec<u8>]) -> Result<bool> {
    if certificates.is_empty() {
        return Err(SignError::Certificate("No certificates provided".into()));
    }

    // Parse the first certificate
    let cert: SelfSignedCert = serde_json::from_slice(&certificates[0])
        .map_err(|e| SignError::Certificate(format!("Failed to parse certificate: {e}")))?;

    // Phase 1: Developer self-signed certificates are always accepted
    match cert.key_type {
        KeyType::Developer => Ok(true),
        KeyType::Platform => {
            // In Phase 1, platform certificates are also self-signed
            // Phase 2 will add proper root CA verification
            Ok(true)
        }
    }
}

/// Verify that a certificate matches the expected identity
pub fn verify_identity(cert_data: &[u8], expected_identity: KeyType) -> Result<bool> {
    let cert: SelfSignedCert = serde_json::from_slice(cert_data)
        .map_err(|e| SignError::Certificate(format!("Failed to parse certificate: {e}")))?;

    Ok(cert.key_type == expected_identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keygen::KeyPair;
    use crate::keygen::hex;

    #[test]
    fn test_parse_certificate() {
        let kp = KeyPair::generate().unwrap();
        let cert = SelfSignedCert {
            key_type: KeyType::Developer,
            public_key: hex::encode(kp.public_key_bytes()),
            fingerprint: kp.fingerprint(),
            created_at: "2026-04-17T12:00:00Z".into(),
        };

        let cert_json = serde_json::to_vec(&cert).unwrap();
        let info = parse_certificate(&cert_json).unwrap();

        assert_eq!(info.subject, "Developer");
        assert_eq!(info.issuer, "Developer");
        assert_eq!(info.fingerprint, kp.fingerprint());
    }

    #[test]
    fn test_verify_chain_developer() {
        let kp = KeyPair::generate().unwrap();
        let cert = SelfSignedCert {
            key_type: KeyType::Developer,
            public_key: hex::encode(kp.public_key_bytes()),
            fingerprint: kp.fingerprint(),
            created_at: "2026-04-17T12:00:00Z".into(),
        };

        let cert_json = serde_json::to_vec(&cert).unwrap();
        let result = verify_chain(&[cert_json]).unwrap();
        assert!(result);
    }

    #[test]
    fn test_verify_identity() {
        let kp = KeyPair::generate().unwrap();
        let cert = SelfSignedCert {
            key_type: KeyType::Platform,
            public_key: hex::encode(kp.public_key_bytes()),
            fingerprint: kp.fingerprint(),
            created_at: "2026-04-17T12:00:00Z".into(),
        };

        let cert_json = serde_json::to_vec(&cert).unwrap();

        assert!(verify_identity(&cert_json, KeyType::Platform).unwrap());
        assert!(!verify_identity(&cert_json, KeyType::Developer).unwrap());
    }
}
