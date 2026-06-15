//! Agent clone logic
//!
//! Clones an existing agent to a new agent ID with optional full data copy.
//! Used for creating agent variants and safe experimentation.
//!
//! Skeleton clone (mode=skeleton): copies manifest, prompts, config, tools, resources.
//! Full clone (mode=full): additionally copies skills, data, conversations, memory.

use std::path::Path;

use crate::error::GatewayError;
use crate::gateway::state::{AgentInfo, GatewayState};

/// Clone mode: what to copy from the source agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneMode {
    /// Copy manifest + prompts + config + tools + resources only
    Skeleton,
    /// Copy everything: skeleton + skills + data + conversations + memory
    Full,
}

/// Clone a source agent to a new agent ID.
///
/// Returns the new AgentInfo or an error if the clone fails.
///
/// # Constraints
///
/// - System agents (`system = true` in manifest) cannot be cloned
/// - The new agent_id must not already be installed
/// - Cloned agent always has `dev = true` set in its manifest (for safe experimentation)
pub fn clone_agent(
    source_agent_id: &str,
    new_agent_id: &str,
    mode: CloneMode,
    install_dir: &Path,
    state: &mut GatewayState,
) -> Result<AgentInfo, GatewayError> {
    // 1. Validate source exists
    let source_info = state
        .installed_agents
        .get(source_agent_id)
        .ok_or_else(|| GatewayError::AgentNotFound(source_agent_id.to_string()))?;

    // 2. System agent cannot be cloned
    if source_info.manifest.system {
        return Err(GatewayError::Package(format!(
            "System agent '{}' cannot be cloned",
            source_agent_id
        )));
    }

    // 3. Check conflict
    if state.is_installed(new_agent_id) {
        return Err(GatewayError::Package(format!(
            "Agent '{}' is already installed. Uninstall or choose a different ID.",
            new_agent_id
        )));
    }

    // 4. Validate new agent_id (reverse-domain format)
    if !is_valid_agent_id(new_agent_id) {
        return Err(GatewayError::Package(format!(
            "Invalid agent ID '{}': must be reverse-domain format (e.g. com.example.myagent)",
            new_agent_id
        )));
    }

    let source_path = Path::new(&source_info.install_path);
    if !source_path.exists() {
        return Err(GatewayError::Package(format!(
            "Source agent install path does not exist: {}",
            source_path.display()
        )));
    }

    let target_path = install_dir.join(new_agent_id);
    std::fs::create_dir_all(&target_path).map_err(|e| {
        GatewayError::Package(format!(
            "Failed to create target directory '{}': {}",
            target_path.display(),
            e
        ))
    })?;

    // 5. Copy manifest (with modifications)
    let mut new_manifest = source_info.manifest.clone();
    new_manifest.agent_id = new_agent_id.to_string();
    new_manifest.dev = true;

    let manifest_toml = toml::to_string_pretty(&new_manifest)
        .map_err(|e| GatewayError::Package(format!("Failed to serialize manifest: {}", e)))?;
    std::fs::write(target_path.join("manifest.toml"), &manifest_toml)
        .map_err(|e| GatewayError::Package(format!("Failed to write manifest: {}", e)))?;

    // 6. Copy skeleton directories
    let skeleton_dirs = &["prompts", "config", "tools", "resources"];
    let full_only_dirs = &["skills", "data"];

    for dir_name in skeleton_dirs {
        let src = source_path.join(dir_name);
        if src.exists() {
            copy_dir_all(&src, &target_path.join(dir_name))?;
        }
    }

    // 7. Copy full-mode only directories
    if mode == CloneMode::Full {
        for dir_name in full_only_dirs {
            let src = source_path.join(dir_name);
            if src.exists() {
                copy_dir_all(&src, &target_path.join(dir_name))?;
            }
        }

        // Copy conversations/ (JSONL files)
        let conversations_src = source_path.join("conversations");
        if conversations_src.exists() {
            copy_dir_all(&conversations_src, &target_path.join("conversations"))?;
        }

        // Copy memory/private.grafeo
        let memory_src = source_path.join("memory");
        if memory_src.exists() {
            // Only copy the private.grafeo file, not the entire memory dir
            // (workspace memory might contain runtime state we don't want)
            let private_grafeo = memory_src.join("private.grafeo");
            if private_grafeo.exists() {
                let target_memory = target_path.join("memory");
                std::fs::create_dir_all(&target_memory).map_err(|e| {
                    GatewayError::Package(format!("Failed to create memory dir: {}", e))
                })?;
                std::fs::copy(&private_grafeo, target_memory.join("private.grafeo")).map_err(
                    |e| GatewayError::Package(format!("Failed to copy private.grafeo: {}", e)),
                )?;
            }
        }
    }

    // 8. Register cloned agent
    let info = AgentInfo {
        agent_id: new_agent_id.to_string(),
        version: new_manifest.version.clone(),
        name: format!("{} (clone)", new_manifest.name),
        install_path: target_path.to_string_lossy().to_string(),
        manifest: new_manifest,
    };

    tracing::info!(
        "Cloned agent: {} → {} (mode={:?})",
        source_agent_id,
        new_agent_id,
        mode
    );
    state.add_installed(info.clone());

    Ok(info)
}

/// Recursively copy a directory
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), GatewayError> {
    std::fs::create_dir_all(dst).map_err(|e| {
        GatewayError::Package(format!(
            "Failed to create directory '{}': {}",
            dst.display(),
            e
        ))
    })?;

    let entries = std::fs::read_dir(src).map_err(|e| {
        GatewayError::Package(format!(
            "Failed to read directory '{}': {}",
            src.display(),
            e
        ))
    })?;

    for entry in entries {
        let entry =
            entry.map_err(|e| GatewayError::Package(format!("Failed to read entry: {}", e)))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                GatewayError::Package(format!(
                    "Failed to copy '{}' to '{}': {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                ))
            })?;
        }
    }

    Ok(())
}

/// Validate agent ID format (reverse-domain style, e.g. com.example.myagent)
fn is_valid_agent_id(agent_id: &str) -> bool {
    if agent_id.is_empty() || agent_id.len() > 128 {
        return false;
    }
    // Must have at least one dot
    if !agent_id.contains('.') {
        return false;
    }
    // Each segment must be non-empty and contain only alphanumeric + hyphen
    agent_id
        .split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_agent(dir: &Path, agent_id: &str, system: bool) {
        let prompts_dir = dir.join("prompts");
        let config_dir = dir.join("config");
        let skills_dir = dir.join("skills");
        let conversations_dir = dir.join("conversations");
        let memory_dir = dir.join("memory");

        for d in &[
            &prompts_dir,
            &config_dir,
            &skills_dir,
            &conversations_dir,
            &memory_dir,
        ] {
            std::fs::create_dir_all(d).unwrap();
        }

        let manifest = format!(
            r#"
            agent_id = "{}"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            {}
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#,
            agent_id,
            if system { "system = true" } else { "" }
        );
        std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
        std::fs::write(prompts_dir.join("system.md"), "You are a test agent.").unwrap();
        std::fs::write(config_dir.join("settings.toml"), "temperature = 0.7").unwrap();
        std::fs::write(skills_dir.join("search.md"), "# Search skill").unwrap();
        std::fs::write(
            conversations_dir.join("session.jsonl"),
            r#"{"role":"user","content":"hello"}"#,
        )
        .unwrap();
        std::fs::write(memory_dir.join("private.grafeo"), b"grafeo-data").unwrap();
    }

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("acowork-test-clone-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn add_agent_to_state(state: &mut GatewayState, agent_id: &str, install_path: &str) {
        let manifest = acowork_core::AgentManifest::from_toml(&format!(
            r#"
            agent_id = "{}"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#,
            agent_id
        ))
        .unwrap();
        state.add_installed(AgentInfo {
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Test Agent".to_string(),
            install_path: install_path.to_string(),
            manifest,
        });
    }

    #[test]
    fn test_clone_skeleton_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-sk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather", false);

        let vault_dir = temp_vault_dir("clone-sk");
        let mut state = GatewayState::new(&vault_dir);
        add_agent_to_state(
            &mut state,
            "com.test.weather",
            &source_dir.to_string_lossy(),
        );

        let result = clone_agent(
            "com.test.weather",
            "com.test.weather-clone",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        assert!(result.is_ok(), "Clone failed: {:?}", result.err());
        let info = result.unwrap();
        assert_eq!(info.agent_id, "com.test.weather-clone");
        assert!(info.manifest.dev, "Cloned agent should have dev=true");
        assert!(state.is_installed("com.test.weather-clone"));

        // Verify skeleton dirs copied
        let target = install_dir.join("com.test.weather-clone");
        assert!(target.join("prompts").exists(), "prompts should be copied");
        assert!(target.join("config").exists(), "config should be copied");
        // Skills should NOT be copied in skeleton mode
        assert!(
            !target.join("skills").exists(),
            "skills should not be copied"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_full_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather", false);

        let vault_dir = temp_vault_dir("clone-full");
        let mut state = GatewayState::new(&vault_dir);
        add_agent_to_state(
            &mut state,
            "com.test.weather",
            &source_dir.to_string_lossy(),
        );

        let result = clone_agent(
            "com.test.weather",
            "com.test.weather-full-clone",
            CloneMode::Full,
            &install_dir,
            &mut state,
        );
        assert!(result.is_ok(), "Full clone failed: {:?}", result.err());
        let info = result.unwrap();
        assert_eq!(info.agent_id, "com.test.weather-full-clone");

        let target = install_dir.join("com.test.weather-full-clone");
        assert!(
            target.join("skills").exists(),
            "skills should be copied in full mode"
        );
        assert!(
            target.join("conversations").exists(),
            "conversations should be copied"
        );
        assert!(
            target.join("memory/private.grafeo").exists(),
            "private.grafeo should be copied"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_system_agent_rejected() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.acowork.system", true);

        let vault_dir = temp_vault_dir("clone-sys");
        let mut state = GatewayState::new(&vault_dir);

        // Add with system=true manifest (read from disk)
        let manifest_toml = std::fs::read_to_string(source_dir.join("manifest.toml")).unwrap();
        let manifest = acowork_core::AgentManifest::from_toml(&manifest_toml).unwrap();
        state.add_installed(AgentInfo {
            agent_id: "com.acowork.system".to_string(),
            version: "1.0.0".to_string(),
            name: "System Agent".to_string(),
            install_path: source_dir.to_string_lossy().to_string(),
            manifest,
        });

        let result = clone_agent(
            "com.acowork.system",
            "com.acowork.system-clone",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        assert!(result.is_err(), "System agent clone should be rejected");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_duplicate_agent_id() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather", false);

        let vault_dir = temp_vault_dir("clone-dup");
        let mut state = GatewayState::new(&vault_dir);
        // Pre-install the target agent ID
        add_agent_to_state(
            &mut state,
            "com.test.weather-clone",
            &source_dir.to_string_lossy(),
        );
        add_agent_to_state(
            &mut state,
            "com.test.weather",
            &source_dir.to_string_lossy(),
        );

        let result = clone_agent(
            "com.test.weather",
            "com.test.weather-clone", // already exists
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        assert!(
            result.is_err(),
            "Clone to existing agent_id should be rejected"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_source_not_found() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-nf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let install_dir = temp_dir.join("installed");
        let vault_dir = temp_vault_dir("clone-nf");
        let mut state = GatewayState::new(&vault_dir);

        let result = clone_agent(
            "com.test.nonexistent",
            "com.test.new",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_valid_agent_id() {
        assert!(is_valid_agent_id("com.example.weather"));
        assert!(is_valid_agent_id("com.test.my-agent"));
        assert!(is_valid_agent_id("io.acowork.system"));
        assert!(!is_valid_agent_id(""));
        assert!(!is_valid_agent_id("no-dots"));
        assert!(!is_valid_agent_id("com..empty"));
        assert!(!is_valid_agent_id("com.invalid!char"));
    }
}
