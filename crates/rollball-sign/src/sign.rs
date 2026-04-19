//! Package signing (insert Signing Block into ZIP)
//!
//! Signs a .agent package by:
//! 1. Reading the ZIP contents
//! 2. Computing SHA-256 digests of each section
//! 3. Creating a SigningBlock with Ed25519 signature
//! 4. Writing the signed ZIP with the SigningBlock inserted before the Central Directory

use ed25519_dalek::Signer as Ed25519Signer;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use crate::error::{Result, SignError};
use crate::keygen::load_keypair;
use crate::keygen::KeyType;
use crate::signing_block::{
    Certificate, DigestAlgorithm, SectionDigest, SignedAttributes, Signer as BlockSigner,
    SigningBlock,
};

/// Sign a .agent package
///
/// Reads the input ZIP, computes digests, signs them, and produces
/// a signed ZIP with the SigningBlock inserted before the Central Directory.
pub fn sign_package(
    input_path: &Path,
    output_path: &Path,
    key_dir: &Path,
    key_type: KeyType,
) -> Result<()> {
    let keypair = load_keypair(key_dir, key_type)?;

    // Read the input ZIP
    let input_data = fs::read(input_path)?;

    // Compute section digests
    let digests = compute_digests(&input_data)?;

    // Create signing attributes
    let signed_attrs = SignedAttributes {
        signing_time: chrono::Utc::now().to_rfc3339(),
    };

    // Sign the concatenated digests
    let signature_data = create_signature_data(&digests, &signed_attrs);
    let signature = keypair.signing_key.sign(&signature_data);

    // Build the signing block
    let block = SigningBlock {
        signers: vec![BlockSigner {
            certificates: vec![Certificate {
                data: keypair.public_key_bytes().to_vec(),
                identity: match key_type {
                    KeyType::Developer => crate::signing_block::SignerIdentity::Developer,
                    KeyType::Platform => crate::signing_block::SignerIdentity::Platform,
                },
            }],
            digest_algorithm: DigestAlgorithm::Sha256,
            digests,
            signature: signature.to_bytes().to_vec(),
            signed_attrs,
        }],
    };

    // Write the signed ZIP
    write_signed_zip(&input_data, &block, output_path)?;

    Ok(())
}

/// Compute SHA-256 digests for each file in the ZIP
fn compute_digests(zip_data: &[u8]) -> Result<Vec<SectionDigest>> {
    let reader = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    let mut digests = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();

        let mut content = Vec::new();
        file.read_to_end(&mut content)?;

        let mut hasher = Sha256::new();
        hasher.update(&content);
        let digest = hasher.finalize().to_vec();

        digests.push(SectionDigest {
            section_name: name,
            digest,
        });
    }

    // Sort by section name for deterministic ordering
    digests.sort_by(|a, b| a.section_name.cmp(&b.section_name));

    Ok(digests)
}

/// Create the data to be signed (concatenation of all digests + signed_attrs JSON)
pub fn create_signature_data(digests: &[SectionDigest], signed_attrs: &SignedAttributes) -> Vec<u8> {
    let mut data = Vec::new();

    for digest in digests {
        data.extend_from_slice(digest.section_name.as_bytes());
        data.extend_from_slice(&digest.digest);
    }

    let attrs_json = serde_json::to_vec(signed_attrs).unwrap_or_default();
    data.extend_from_slice(&attrs_json);

    data
}

/// Write a signed ZIP with the SigningBlock inserted before the Central Directory
///
/// The .agent ZIP format:
/// ```text
/// [Local file headers + file data]     — original ZIP content
/// [SigningBlock]                        — inserted before CD
/// [Central Directory]                   — original CD, updated offsets
/// [End of Central Directory Record]     — updated CD offset
/// ```
fn write_signed_zip(original_data: &[u8], block: &SigningBlock, output_path: &Path) -> Result<()> {
    // For Phase 1, we use a simpler approach: append the signing block
    // as a special entry in the ZIP. This avoids the complexity of
    // manipulating raw ZIP structures.
    //
    // The signing block is stored as a ZIP entry named "META-INF/SIGNING.BLOCK"

    let reader = Cursor::new(original_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    let output_file = fs::File::create(output_path)?;
    let mut writer = zip::ZipWriter::new(output_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);

    // Copy all existing entries
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();

        // Skip existing signing block if re-signing
        if name == "META-INF/SIGNING.BLOCK" {
            continue;
        }

        writer.start_file(&name, options)?;
        std::io::copy(&mut file, &mut writer)?;
    }

    // Write signing block as a new entry
    let block_bytes = block.to_bytes();
    writer.start_file("META-INF/SIGNING.BLOCK", options)?;
    writer.write_all(&block_bytes)?;

    writer.finish()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::io::Write;

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
    fn test_compute_digests() {
        let tmp_dir = std::env::temp_dir().join("rollball-test-sign");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        let zip_data = fs::read(&zip_path).unwrap();
        let digests = compute_digests(&zip_data).unwrap();

        assert_eq!(digests.len(), 2);
        // Should be sorted by name
        assert!(digests[0].section_name < digests[1].section_name);
        // Each digest should be 32 bytes (SHA-256)
        for d in &digests {
            assert_eq!(d.digest.len(), 32);
        }

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_sign_package_roundtrip() {
        let tmp_dir = std::env::temp_dir().join("rollball-test-sign-roundtrip");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create test ZIP
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        // Sign
        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify the signed ZIP exists and contains signing block
        assert!(signed_path.exists());

        // Verify we can read the signing block
        let signed_data = fs::read(&signed_path).unwrap();
        let reader = Cursor::new(signed_data);
        let mut archive = zip::ZipArchive::new(reader).unwrap();

        let mut found = false;
        for i in 0..archive.len() {
            let file = archive.by_index(i).unwrap();
            if file.name() == "META-INF/SIGNING.BLOCK" {
                found = true;
                break;
            }
        }
        assert!(found, "Signing block not found in signed ZIP");

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
