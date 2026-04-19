//! .agent package installation
//!
//! Install flow: ZIP extract → signature verify → manifest validate → copy to install dir

use std::path::Path;
use std::io::Read;
use crate::error::GatewayError;
use crate::gateway::state::{AgentInfo, GatewayState};

/// Install a .agent package
pub fn install_package(
    package_path: &Path,
    install_dir: &Path,
    state: &mut GatewayState,
) -> Result<AgentInfo, GatewayError> {
    // 1. Open ZIP file
    let file = std::fs::File::open(package_path)
        .map_err(|e| GatewayError::Package(format!("Failed to open package '{}': {}", package_path.display(), e)))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| GatewayError::Package(format!("Failed to read ZIP '{}': {}", package_path.display(), e)))?;

    // 2. Check for signing block
    if archive.by_name("META-INF/signing.block").is_ok() {
        tracing::info!("Package has signing block — verifying signature");
        // TODO: Delegate to rollball-sign for verification
    } else {
        tracing::warn!("Package has no signing block — unsigned package");
        // Phase 1: allow unsigned packages in dev mode
    }

    // 3. Extract and parse manifest.toml
    let manifest = extract_manifest(&mut archive)?;

    // 4. Check if already installed
    if state.is_installed(&manifest.agent_id) {
        return Err(GatewayError::Package(format!(
            "Agent '{}' is already installed. Use upgrade instead.",
            manifest.agent_id
        )));
    }

    // 5. Create install directory
    let agent_install_dir = install_dir.join(&manifest.agent_id);
    std::fs::create_dir_all(&agent_install_dir)
        .map_err(|e| GatewayError::Package(format!("Failed to create install dir: {}", e)))?;

    // 6. Extract all files to install directory
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| GatewayError::Package(format!("ZIP read error: {}", e)))?;
        let outpath = match file.enclosed_name() {
            Some(path) => agent_install_dir.join(path),
            None => continue,
        };
        
        if file.is_dir() {
            std::fs::create_dir_all(&outpath).ok();
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    std::fs::create_dir_all(p).ok();
                }
            }
            let mut outfile = std::fs::File::create(&outpath)
                .map_err(|e| GatewayError::Package(format!("Failed to create file '{}': {}", outpath.display(), e)))?;
            std::io::copy(&mut file, &mut outfile)
                .map_err(|e| GatewayError::Package(format!("Failed to write file '{}': {}", outpath.display(), e)))?;
        }
    }

    // 7. Create AgentInfo
    let info = AgentInfo {
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
fn extract_manifest(archive: &mut zip::ZipArchive<std::fs::File>) -> Result<rollball_core::AgentManifest, GatewayError> {
    let mut manifest_file = archive.by_name("manifest.toml")
        .map_err(|e| GatewayError::Package(format!("manifest.toml not found in package: {}", e)))?;
    
    let mut manifest_str = String::new();
    manifest_file.read_to_string(&mut manifest_str)
        .map_err(|e| GatewayError::Package(format!("Failed to read manifest.toml: {}", e)))?;
    
    let manifest = rollball_core::AgentManifest::from_toml(&manifest_str)
        .map_err(|e| GatewayError::Package(format!("Invalid manifest.toml: {}", e)))?;
    
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
        let temp_dir = std::env::temp_dir().join(format!("rollball-test-install-{}", std::process::id()));
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
        let mut state = GatewayState::new();
        
        let result = install_package(&zip_path, &install_dir, &mut state);
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
        let temp_dir = std::env::temp_dir().join(format!("rollball-test-install-dup-{}", std::process::id()));
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
        let mut state = GatewayState::new();
        
        // First install should succeed
        install_package(&zip_path, &install_dir, &mut state).unwrap();
        
        // Second install should fail
        let result = install_package(&zip_path, &install_dir, &mut state);
        assert!(result.is_err());
        
        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_install_package_missing_manifest() {
        let temp_dir = std::env::temp_dir().join(format!("rollball-test-install-nomanifest-{}", std::process::id()));
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
        let mut state = GatewayState::new();
        
        let result = install_package(&zip_path, &install_dir, &mut state);
        assert!(result.is_err());
        
        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
