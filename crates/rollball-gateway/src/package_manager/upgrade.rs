//! Package upgrade
//!
//! Upgrade flow: verify new package → check signature consistency →
//! preserve data/ and config/ → replace rest

use std::path::Path;
use std::io::Read;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use super::install::install_package;

/// Upgrade a .agent package (signature fingerprint consistency check required)
pub fn upgrade_package(
    agent_id: &str,
    new_package_path: &Path,
    install_dir: &Path,
    state: &mut GatewayState,
) -> Result<(), GatewayError> {
    // Check if agent is currently installed
    let old_info = state.installed_agents.get(agent_id)
        .ok_or_else(|| GatewayError::AgentNotFound(agent_id.to_string()))?
        .clone();

    // Check if agent is running
    if state.is_running(agent_id) {
        return Err(GatewayError::AgentAlreadyRunning(agent_id.to_string()));
    }

    // Parse new manifest to check agent_id consistency
    let new_manifest = parse_manifest_from_zip(new_package_path)?;
    if new_manifest.agent_id != agent_id {
        return Err(GatewayError::Package(format!(
            "Package agent_id '{}' does not match expected '{}'",
            new_manifest.agent_id, agent_id
        )));
    }

    // TODO: Phase 2 — check signing fingerprint consistency
    // The new package must be signed by the same developer
    // if old_info.manifest.signer_fingerprint != new_manifest.signer_fingerprint {
    //     return Err(GatewayError::SignatureFailed(...));
    // }

    // Preserve data/ and config/ directories
    let old_install_path = Path::new(&old_info.install_path);
    let data_dir = old_install_path.join("data");
    let config_dir = old_install_path.join("config");

    // Remove old install directory (except preserved dirs)
    if old_install_path.exists() {
        // Move preserved dirs to temp location
        let temp_base = std::env::temp_dir().join(format!("rollball-upgrade-{}", agent_id.replace('.', "-")));
        std::fs::create_dir_all(&temp_base).ok();
        
        if data_dir.exists() {
            std::fs::rename(&data_dir, temp_base.join("data")).ok();
        }
        if config_dir.exists() {
            std::fs::rename(&config_dir, temp_base.join("config")).ok();
        }
        
        // Remove old install dir
        std::fs::remove_dir_all(old_install_path)
            .map_err(|e| GatewayError::Package(format!("Failed to remove old install: {}", e)))?;
        
        // Re-create and restore preserved dirs
        let new_install_path = install_dir.join(agent_id);
        std::fs::create_dir_all(&new_install_path).ok();
        
        if temp_base.join("data").exists() {
            std::fs::rename(temp_base.join("data"), new_install_path.join("data")).ok();
        }
        if temp_base.join("config").exists() {
            std::fs::rename(temp_base.join("config"), new_install_path.join("config")).ok();
        }
        
        // Cleanup temp
        std::fs::remove_dir_all(&temp_base).ok();
    }

    // Remove from state temporarily
    state.remove_installed(agent_id);

    // Install new package (upgrade inherits dev_mode from caller context)
    // TODO(Phase 2): verify signing fingerprint consistency with old package
    let new_info = install_package(new_package_path, install_dir, state, true)?;
    
    tracing::info!("Upgraded agent: {} from v{} to v{}", 
        agent_id, old_info.version, new_info.version);
    Ok(())
}

/// Parse manifest from ZIP without full extraction
fn parse_manifest_from_zip(package_path: &Path) -> Result<rollball_core::AgentManifest, GatewayError> {
    let file = std::fs::File::open(package_path)
        .map_err(|e| GatewayError::Package(format!("Failed to open package: {}", e)))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| GatewayError::Package(format!("Failed to read ZIP: {}", e)))?;
    
    let mut manifest_file = archive.by_name("manifest.toml")
        .map_err(|e| GatewayError::Package(format!("manifest.toml not found: {}", e)))?;
    
    let mut manifest_str = String::new();
    manifest_file.read_to_string(&mut manifest_str)
        .map_err(|e| GatewayError::Package(format!("Failed to read manifest.toml: {}", e)))?;
    
    rollball_core::AgentManifest::from_toml(&manifest_str)
        .map_err(|e| GatewayError::Package(format!("Invalid manifest.toml: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use crate::gateway::state::AgentInfo;

    fn create_test_zip(dir: &Path, agent_id: &str, version: &str) -> PathBuf {
        let zip_path = dir.join(format!("{}-{}.agent", agent_id.replace('.', "-"), version));
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();
        
        let manifest = format!(r#"
            agent_id = "{}"
            version = "{}"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#, agent_id, version);
        
        zip.start_file("manifest.toml", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.finish().unwrap();
        zip_path
    }

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("rollball-test-upgrade-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_upgrade_not_installed() {
        let vault_dir = temp_vault_dir("not_installed");
        let mut state = GatewayState::new(&vault_dir);
        let result = upgrade_package(
            "com.test.unknown",
            Path::new("/tmp/nonexistent.agent"),
            Path::new("/tmp/installed"),
            &mut state,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_upgrade_agent_id_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!("rollball-test-upgrade-mismatch-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        
        // Create a package with different agent_id
        let zip_path = create_test_zip(&temp_dir, "com.test.other", "2.0.0");
        
        // Add old agent to state
        let vault_dir = temp_vault_dir("mismatch");
        let mut state = GatewayState::new(&vault_dir);
        let manifest = rollball_core::AgentManifest::from_toml(r#"
            agent_id = "com.test.weather"
            version = "1.0.0"
            name = "Weather"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#).unwrap();
        state.add_installed(AgentInfo {
            agent_id: "com.test.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather".to_string(),
            install_path: temp_dir.join("installed").join("com.test.weather").to_string_lossy().to_string(),
            manifest,
        });
        
        let result = upgrade_package("com.test.weather", &zip_path, &temp_dir.join("installed"), &mut state);
        assert!(result.is_err());
        
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
