//! Package uninstallation

use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use std::path::Path;

/// Uninstall a .agent package
pub fn uninstall_package(
    agent_id: &str,
    _install_dir: &Path,
    state: &mut GatewayState,
) -> Result<(), GatewayError> {
    // Check if agent is installed
    let info = state
        .installed_agents
        .get(agent_id)
        .ok_or_else(|| GatewayError::AgentNotFound(agent_id.to_string()))?
        .clone();

    // Check if agent is running
    if state.is_running(agent_id) {
        return Err(GatewayError::AgentAlreadyRunning(agent_id.to_string()));
    }

    // Remove install directory
    let agent_dir = Path::new(&info.install_path);
    if agent_dir.exists() {
        std::fs::remove_dir_all(agent_dir)
            .map_err(|e| GatewayError::Package(format!("Failed to remove install dir: {}", e)))?;
    }

    // Remove from state
    state.remove_installed(agent_id);

    // S3.3: Clean up cron entries for this agent
    let count = state.cron_scheduler.unregister_agent(agent_id);
    if count > 0 {
        tracing::info!("Cleaned up {} cron entries for agent {}", count, agent_id);
        // Also clean from CronStore
        if let Some(store) = &state.cron_store
            && let Err(e) = store.delete_by_agent(agent_id)
        {
            tracing::warn!(
                "Failed to clean cron entries from store for agent {}: {}",
                agent_id,
                e
            );
        }
    }

    tracing::info!("Uninstalled agent: {}", agent_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::GatewayState;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("acowork-test-uninstall-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_uninstall_not_installed() {
        let vault_dir = temp_vault_dir("not_installed");
        let mut state = GatewayState::new(&vault_dir);
        let install_dir = Path::new("/tmp/nonexistent");
        let result = uninstall_package("com.test.unknown", install_dir, &mut state);
        assert!(result.is_err());
    }
}
