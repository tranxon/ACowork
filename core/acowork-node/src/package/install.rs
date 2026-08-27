//! .agent package installation (migrated from gateway
//! `package_manager/install.rs`, ADR-055 §6.20).
//!
//! Install flow: read ZIP from path → signature verify → manifest
//! validate → extract to install dir.
//!
//! All installation paths converge on [`install_package`] which takes a
//! file path. Callers that receive bytes (multipart handlers) must
//! spool to a temp file first.
//!
//! Re-based from `GatewayError`/`GatewayState` to
//! [`crate::error::NodeError`]/[`crate::state::NodeState`]. The
//! Gateway-side cron registration (S3.3) is intentionally NOT carried
//! over — cron scheduling is a Gateway global-resource concern
//! (ADR-055 §6.5); the Gateway registers cron triggers from the
//! install-completed event instead.

use std::io::Read;
use std::path::Path;

use crate::error::{NodeError, Result};
use crate::state::{InstalledAgent, NodeState};

/// Install a .agent package from a file path.
///
/// This is the sole installation entry point. When `dev_mode` is true,
/// unsigned packages are allowed (for local development). In production
/// mode, packages must have a valid signature.
pub fn install_package(
    package_path: &Path,
    install_dir: &Path,
    state: &mut NodeState,
    dev_mode: bool,
) -> Result<InstalledAgent> {
    // 1. Read and open ZIP
    let data = std::fs::read(package_path).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read package '{}': {}",
            package_path.display(),
            e
        ))
    })?;
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read ZIP '{}': {}",
            package_path.display(),
            e
        ))
    })?;

    // 2. Verify package signature (delegate to acowork-sign)
    if dev_mode {
        match acowork_sign::verify::verify_package(package_path) {
            Ok(result) => {
                tracing::info!(
                    "Package signature verified: signer={}, fingerprint={}, sections={}",
                    result.signer,
                    result.certificate_fingerprint,
                    result.sections_count
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Package signature verification failed (dev mode, continuing): {}",
                    e
                );
            }
        }
    } else {
        match acowork_sign::verify::verify_package(package_path) {
            Ok(result) => {
                tracing::info!(
                    "Package signature verified: signer={}, fingerprint={}, sections={}",
                    result.signer,
                    result.certificate_fingerprint,
                    result.sections_count
                );
            }
            Err(e) => {
                tracing::error!("Package signature verification failed: {}", e);
                return Err(NodeError::Package(format!(
                    "Signature verification failed: {}",
                    e
                )));
            }
        }
    }

    // 3. Extract and parse manifest.toml
    let manifest = extract_manifest(&mut archive)?;

    // 4. Check if already installed
    if state.is_installed(&manifest.agent_id) {
        return Err(NodeError::Package(format!(
            "Agent '{}' is already installed. Use upgrade instead.",
            manifest.agent_id
        )));
    }

    // 5. Create install directory
    let agent_install_dir = install_dir.join(&manifest.agent_id);
    std::fs::create_dir_all(&agent_install_dir)
        .map_err(|e| NodeError::Package(format!("Failed to create install dir: {}", e)))?;

    // 6. Extract all files to install directory
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| NodeError::Package(format!("ZIP read error: {}", e)))?;
        let outpath = match file.enclosed_name() {
            Some(path) => agent_install_dir.join(path),
            None => continue,
        };

        if file.is_dir() {
            std::fs::create_dir_all(&outpath).ok();
        } else {
            if let Some(p) = outpath.parent()
                && !p.exists()
            {
                std::fs::create_dir_all(p).ok();
            }
            let mut outfile = std::fs::File::create(&outpath).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to create file '{}': {}",
                    outpath.display(),
                    e
                ))
            })?;
            std::io::copy(&mut file, &mut outfile).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to write file '{}': {}",
                    outpath.display(),
                    e
                ))
            })?;
        }
    }

    // 7. Create InstalledAgent
    let info = InstalledAgent {
        agent_id: manifest.agent_id.clone(),
        version: manifest.version.clone(),
        name: manifest.name.clone(),
        install_path: agent_install_dir.to_string_lossy().to_string(),
        manifest,
    };

    tracing::info!("Installed agent: {} v{}", info.agent_id, info.version);
    state.add_installed(info.clone());

    Ok(info)
}

/// Extract manifest.toml from ZIP archive
fn extract_manifest(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
) -> Result<acowork_core::AgentManifest> {
    let mut manifest_file = archive
        .by_name("manifest.toml")
        .map_err(|e| NodeError::Package(format!("manifest.toml not found in package: {}", e)))?;

    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| NodeError::Package(format!("Failed to read manifest.toml: {}", e)))?;

    let manifest = acowork_core::AgentManifest::from_toml(&manifest_str)
        .map_err(|e| NodeError::Package(format!("Invalid manifest.toml: {}", e)))?;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn create_test_zip(dir: &Path, manifest_toml: &str) -> PathBuf {
        let zip_path = dir.join("test.agent");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        zip.start_file("manifest.toml", options).unwrap();
        zip.write_all(manifest_toml.as_bytes()).unwrap();

        zip.start_file("prompts/default.md", options).unwrap();
        zip.write_all(b"You are a weather agent.").unwrap();

        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn test_install_package_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-install-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let manifest_toml = r#"
            agent_id = "com.test.weather"
            version = "1.0.0"
            name = "Weather Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;

        let zip_path = create_test_zip(&temp_dir, manifest_toml);
        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let result = install_package(&zip_path, &install_dir, &mut state, true);
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_eq!(info.agent_id, "com.test.weather");
        assert_eq!(info.version, "1.0.0");
        assert!(state.is_installed("com.test.weather"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_install_package_already_installed() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-install-dup-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let manifest_toml = r#"
            agent_id = "com.test.dup"
            version = "1.0.0"
            name = "Dup Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;

        let zip_path = create_test_zip(&temp_dir, manifest_toml);
        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        // First install should succeed
        install_package(&zip_path, &install_dir, &mut state, true).unwrap();

        // Second install should fail
        let result = install_package(&zip_path, &install_dir, &mut state, true);
        assert!(result.is_err());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_install_package_missing_manifest() {
        let temp_dir = std::env::temp_dir().join(format!(
            "acowork-test-install-nomanifest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create ZIP without manifest.toml
        let zip_path = temp_dir.join("no-manifest.agent");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("prompts/default.md", options).unwrap();
        zip.write_all(b"Hello").unwrap();
        zip.finish().unwrap();

        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let result = install_package(&zip_path, &install_dir, &mut state, true);
        assert!(result.is_err());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
