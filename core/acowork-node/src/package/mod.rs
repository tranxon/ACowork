//! Local agent package operations (ADR-055 §6.20 — migrated from the
//! Gateway's `package_manager/*` plus the skills / manifest / avatar
//! file operations from `http/{skills_api,agents}.rs`).
//!
//! After Phase 2b the Gateway process contains no `std::fs` call
//! touching any package or workspace path (ADR-034 rule 3,
//! zero-exception enforcement); all of it lives here and executes on
//! the node's own filesystem.

pub mod clone;
pub mod install;
pub mod publish;
pub mod skills;
pub mod uninstall;
pub mod upgrade;

use std::path::Path;

use crate::state::{InstalledAgent, NodeState};

/// Build the retained `InstalledAgentInfo` payload for a locally-installed
/// agent (ADR-055 §6.5). Returns `None` if the manifest fails to serialize
/// (should not happen for a valid installed package).
pub fn build_installed_info(installed: &InstalledAgent) -> Option<acowork_core::mqtt_proto::InstalledAgentInfo> {
    let manifest_toml = installed.manifest.to_toml().ok()?;
    Some(acowork_core::mqtt_proto::InstalledAgentInfo {
        agent_id: installed.agent_id.clone(),
        version: installed.version.clone(),
        name: installed.name.clone(),
        install_path: installed.install_path.clone(),
        manifest_toml,
    })
}

/// Scan the packages directory and rebuild the local install table
/// (ADR-055 §6.5: the node is the authority for its own package
/// inventory). Mirrors the Gateway's pre-hard-cut
/// `restore_installed_agents_static` — called once at daemon startup so
/// a restart re-discovers previously installed agents without a
/// re-install.
pub fn restore_installed_agents(state: &mut NodeState, packages_dir: &Path) {
    if !packages_dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(packages_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let agent_dir = entry.path();
        if !agent_dir.is_dir() {
            continue;
        }
        let manifest_path = agent_dir.join("manifest.toml");
        if !manifest_path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = acowork_core::AgentManifest::from_toml(&content) else {
            continue;
        };
        let info = InstalledAgent {
            agent_id: manifest.agent_id.clone(),
            version: manifest.version.clone(),
            name: manifest.name.clone(),
            install_path: agent_dir.to_string_lossy().to_string(),
            manifest,
        };
        tracing::info!(
            "Restored installed agent on node: {} v{}",
            info.agent_id,
            info.version
        );
        state.add_installed(info);
    }
}
