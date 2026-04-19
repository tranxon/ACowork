//! Package verification (extract Signing Block + verify)
//!
//! Verifies a .agent package by:
//! 1. Extracting the SigningBlock from the ZIP
//! 2. Recomputing section digests
//! 3. Verifying the Ed25519 signature
//! 4. Validating certificate identity

use ed25519_dalek::{Signature, VerifyingKey, Verifier as Ed25519Verifier};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use crate::error::{Result, SignError};
use crate::sign::create_signature_data;
use crate::signing_block::{SigningBlock, SignerIdentity};

/// Verify a .agent package signature
pub fn verify_package(package_path: &Path) -> Result<VerificationResult> {
    let data = fs::read(package_path)?;

    // Extract signing block from ZIP
    let block = extract_signing_block(&data)?;

    if block.signers.is_empty() {
        return Err(SignError::InvalidPackage("No signers found".into()));
    }

    // Verify the first signer (Phase 1 supports single signer)
    let signer = &block.signers[0];

    // Recompute digests
    let computed_digests = recompute_digests(&data)?;

    // Verify digests match
    if signer.digests.len() != computed_digests.len() {
        return Err(SignError::VerificationFailed(format!(
            "Digest count mismatch: expected {}, got {}",
            signer.digests.len(),
            computed_digests.len()
        )));
    }

    for (expected, computed) in signer.digests.iter().zip(computed_digests.iter()) {
        if expected.section_name != computed.section_name {
            return Err(SignError::VerificationFailed(format!(
                "Section name mismatch: expected '{}', got '{}'",
                expected.section_name, computed.section_name
            )));
        }
        if expected.digest != computed.digest {
            return Err(SignError::VerificationFailed(format!(
                "Digest mismatch for section '{}'",
                expected.section_name
            )));
        }
    }

    // Verify signature
    if signer.certificates.is_empty() {
        return Err(SignError::VerificationFailed(
            "No certificates in signing block".into(),
        ));
    }

    let cert = &signer.certificates[0];
    let public_key_bytes: [u8; 32] = cert
        .data
        .clone()
        .try_into()
        .map_err(|_| SignError::InvalidPackage("Invalid public key length".into()))?;

    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|e| SignError::Ed25519(e.to_string()))?;

    let signature_bytes: [u8; 64] = signer
        .signature
        .clone()
        .try_into()
        .map_err(|_| SignError::InvalidPackage("Invalid signature length".into()))?;

    let signature = Signature::from_bytes(&signature_bytes);

    let signature_data = create_signature_data(&signer.digests, &signer.signed_attrs);

    verifying_key
        .verify(&signature_data, &signature)
        .map_err(|e| SignError::VerificationFailed(format!("Signature verification failed: {e}")))?;

    // Compute fingerprint
    let mut hasher = Sha256::new();
    hasher.update(&public_key_bytes);
    let fingerprint = format!("{:x}", hasher.finalize());

    let signer_name = match cert.identity {
        SignerIdentity::Developer => "developer",
        SignerIdentity::Platform => "platform",
        SignerIdentity::CaIssued => "ca-issued",
    };

    Ok(VerificationResult {
        valid: true,
        signer: signer_name.to_string(),
        certificate_fingerprint: fingerprint,
        sections_count: signer.digests.len(),
    })
}

/// Extract the signing block from a .agent ZIP
fn extract_signing_block(zip_data: &[u8]) -> Result<SigningBlock> {
    let reader = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.name() == "META-INF/SIGNING.BLOCK" {
            let mut block_data = Vec::new();
            file.read_to_end(&mut block_data)?;
            return SigningBlock::from_bytes(&block_data)
                .map_err(|e| SignError::InvalidPackage(format!("Invalid signing block: {e}")));
        }
    }

    Err(SignError::InvalidPackage(
        "No signing block found (META-INF/SIGNING.BLOCK)".into(),
    ))
}

/// Recompute SHA-256 digests for all files in the ZIP (excluding signing block)
fn recompute_digests(zip_data: &[u8]) -> Result<Vec<crate::signing_block::SectionDigest>> {
    let reader = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    let mut digests = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();

        // Skip signing block
        if name == "META-INF/SIGNING.BLOCK" {
            continue;
        }

        let mut content = Vec::new();
        file.read_to_end(&mut content)?;

        let mut hasher = Sha256::new();
        hasher.update(&content);
        let digest = hasher.finalize().to_vec();

        digests.push(crate::signing_block::SectionDigest {
            section_name: name,
            digest,
        });
    }

    digests.sort_by(|a, b| a.section_name.cmp(&b.section_name));
    Ok(digests)
}

/// Verification result
#[derive(Debug)]
pub struct VerificationResult {
    /// Whether the signature is valid
    pub valid: bool,
    /// Signer identity (e.g., "developer", "platform")
    pub signer: String,
    /// SHA-256 fingerprint of the signer's certificate
    pub certificate_fingerprint: String,
    /// Number of sections verified
    pub sections_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keygen::KeyType;

    fn create_test_zip(path: &Path) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        writer.start_file("manifest.toml", options).unwrap();
        writer.write_all(b"agent_id = \"com.test\"").unwrap();

        writer.start_file("prompts/system.md", options).unwrap();
        writer.write_all(b"You are a test agent.").unwrap();

        writer.finish().unwrap();
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let tmp_dir = std::env::temp_dir().join("rollball-test-verify");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create and sign
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        let signed_path = tmp_dir.join("signed.agent");
        crate::sign::sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify
        let result = verify_package(&signed_path).unwrap();
        assert!(result.valid);
        assert_eq!(result.signer, "developer");
        assert_eq!(result.sections_count, 2);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_unsigned_package() {
        let tmp_dir = std::env::temp_dir().join("rollball-test-verify-unsigned");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let zip_path = tmp_dir.join("unsigned.agent");
        create_test_zip(&zip_path);

        let result = verify_package(&zip_path);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_tampered_package() {
        let tmp_dir = std::env::temp_dir().join("rollball-test-verify-tampered");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create and sign
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        let signed_path = tmp_dir.join("signed.agent");
        crate::sign::sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Tamper: create a new ZIP with modified content but no signing block update
        let tampered_path = tmp_dir.join("tampered.agent");
        let signed_data = fs::read(&signed_path).unwrap();
        let reader = Cursor::new(signed_data);
        let mut archive = zip::ZipArchive::new(reader).unwrap();

        let output_file = fs::File::create(&tampered_path).unwrap();
        let mut writer = zip::ZipWriter::new(output_file);
        let options = zip::write::SimpleFileOptions::default();

        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            let name = file.name().to_string();

            if name == "manifest.toml" {
                // Tamper with content
                writer.start_file(&name, options).unwrap();
                writer.write_all(b"TAMPERED CONTENT").unwrap();
            } else if name == "META-INF/SIGNING.BLOCK" {
                // Copy signing block as-is
                let mut block_data = Vec::new();
                file.read_to_end(&mut block_data).unwrap();
                writer.start_file(&name, options).unwrap();
                writer.write_all(&block_data).unwrap();
            } else {
                writer.start_file(&name, options).unwrap();
                std::io::copy(&mut file, &mut writer).unwrap();
            }
        }
        writer.finish().unwrap();

        // Verify should fail
        let result = verify_package(&tampered_path);
        assert!(result.is_err(), "Tampered package should fail verification");

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
