//! Vault integration — facade for Key distribution
//!
//! Wraps rollball-vault crate and adds Gateway-specific key distribution logic.
//! All API keys are stored encrypted on disk via rollball_vault::Vault.

use crate::error::GatewayError;
use secrecy::ExposeSecret;

/// Vault facade for Gateway
///
/// Delegates to rollball_vault::Vault for encrypted storage.
pub struct VaultFacade {
    /// Inner vault (encrypted on-disk storage)
    vault: rollball_vault::Vault,
    /// In-memory cache of provider names (not values) for fast listing
    provider_names: Vec<String>,
}

impl VaultFacade {
    /// Create a new vault facade pointing at the given directory
    ///
    /// The vault starts in a locked state. Call `unlock()` with a password
    /// to derive the master key and enable store/retrieve operations.
    pub fn new(vault_dir: &str) -> Self {
        let vault = rollball_vault::Vault::open(std::path::Path::new(vault_dir))
            .expect("Failed to open vault directory");
        Self {
            vault,
            provider_names: Vec::new(),
        }
    }

    /// Unlock the vault with a password (delegates to rollball_vault)
    pub fn unlock(&mut self, password: &str) -> Result<(), GatewayError> {
        self.vault.unlock(password)
            .map_err(|e| GatewayError::Vault(format!("Failed to unlock vault: {}", e)))?;
        // Refresh provider list after unlock
        self.provider_names = self.vault.list()
            .map_err(|e| GatewayError::Vault(format!("Failed to list vault keys: {}", e)))?;
        Ok(())
    }

    /// Check if vault is unlocked
    pub fn is_unlocked(&self) -> bool {
        self.vault.is_unlocked()
    }

    /// Store an API key for a provider (encrypted on disk)
    pub fn store_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        self.vault.store(provider, api_key)
            .map_err(|e| GatewayError::Vault(format!("Failed to store key: {}", e)))?;
        if !self.provider_names.contains(&provider.to_string()) {
            self.provider_names.push(provider.to_string());
        }
        Ok(())
    }

    /// Get an API key for a provider (one-time distribution, decrypted)
    pub fn get_key(&self, provider: &str) -> Result<String, GatewayError> {
        let secret = self.vault.retrieve(provider)
            .map_err(|e| GatewayError::Vault(format!("Failed to retrieve key for '{}': {}", provider, e)))?;
        Ok(secret.expose_secret().to_string())
    }

    /// List all providers with stored keys (no values returned)
    pub fn list_providers(&self) -> Vec<String> {
        self.provider_names.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("rollball-test-vaultfacade-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_vault_locked_by_default() {
        let dir = temp_vault_dir("locked");
        let vault = VaultFacade::new(&dir);
        assert!(!vault.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_unlock() {
        let dir = temp_vault_dir("unlock");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        assert!(vault.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_and_get() {
        let dir = temp_vault_dir("store_get");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-test-key").unwrap();
        let key = vault.get_key("openai").unwrap();
        assert_eq!(key, "sk-test-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_get_locked_fails() {
        let dir = temp_vault_dir("get_locked");
        let vault = VaultFacade::new(&dir);
        let result = vault.get_key("openai");
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_locked_fails() {
        let dir = temp_vault_dir("store_locked");
        let mut vault = VaultFacade::new(&dir);
        let result = vault.store_key("openai", "sk-test-key");
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_get_missing_provider() {
        let dir = temp_vault_dir("missing");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        let result = vault.get_key("anthropic");
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_list_providers() {
        let dir = temp_vault_dir("list");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-key1").unwrap();
        vault.store_key("ollama", "").unwrap();
        let providers = vault.list_providers();
        assert_eq!(providers.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
