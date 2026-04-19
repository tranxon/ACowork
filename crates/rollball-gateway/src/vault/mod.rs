//! Vault integration — facade for Key distribution
//!
//! Wraps rollball-vault crate and adds Gateway-specific key distribution logic.

use crate::error::GatewayError;

/// Vault facade for Gateway
pub struct VaultFacade {
    /// Whether the vault is unlocked
    unlocked: bool,
    /// Provider → API Key mappings (in-memory cache after unlock)
    keys: std::collections::HashMap<String, String>,
}

impl VaultFacade {
    /// Create a new locked vault facade
    pub fn new() -> Self {
        Self {
            unlocked: false,
            keys: std::collections::HashMap::new(),
        }
    }

    /// Unlock the vault with a password
    pub fn unlock(&mut self, _password: &str) -> Result<(), GatewayError> {
        // Phase 1: simple in-memory store
        // Phase 2: delegate to rollball-vault crate
        self.unlocked = true;
        Ok(())
    }

    /// Check if vault is unlocked
    pub fn is_unlocked(&self) -> bool {
        self.unlocked
    }

    /// Store an API key for a provider
    pub fn store_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        if !self.unlocked {
            return Err(GatewayError::Vault("Vault is locked".to_string()));
        }
        self.keys.insert(provider.to_string(), api_key.to_string());
        Ok(())
    }

    /// Get an API key for a provider (one-time distribution)
    pub fn get_key(&self, provider: &str) -> Result<String, GatewayError> {
        if !self.unlocked {
            return Err(GatewayError::Vault("Vault is locked".to_string()));
        }
        self.keys.get(provider)
            .cloned()
            .ok_or_else(|| GatewayError::Vault(format!("No key found for provider '{}'", provider)))
    }

    /// List all providers with stored keys (no values returned)
    pub fn list_providers(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }
}

impl Default for VaultFacade {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_locked_by_default() {
        let vault = VaultFacade::new();
        assert!(!vault.is_unlocked());
    }

    #[test]
    fn test_vault_unlock() {
        let mut vault = VaultFacade::new();
        vault.unlock("password123").unwrap();
        assert!(vault.is_unlocked());
    }

    #[test]
    fn test_vault_store_and_get() {
        let mut vault = VaultFacade::new();
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-test-key").unwrap();
        let key = vault.get_key("openai").unwrap();
        assert_eq!(key, "sk-test-key");
    }

    #[test]
    fn test_vault_get_locked_fails() {
        let vault = VaultFacade::new();
        let result = vault.get_key("openai");
        assert!(result.is_err());
    }

    #[test]
    fn test_vault_store_locked_fails() {
        let mut vault = VaultFacade::new();
        let result = vault.store_key("openai", "sk-test-key");
        assert!(result.is_err());
    }

    #[test]
    fn test_vault_get_missing_provider() {
        let mut vault = VaultFacade::new();
        vault.unlock("password123").unwrap();
        let result = vault.get_key("anthropic");
        assert!(result.is_err());
    }

    #[test]
    fn test_vault_list_providers() {
        let mut vault = VaultFacade::new();
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-key1").unwrap();
        vault.store_key("ollama", "").unwrap();
        let providers = vault.list_providers();
        assert_eq!(providers.len(), 2);
    }
}
