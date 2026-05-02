//! Vault integration — facade for Key distribution
//!
//! Wraps rollball-vault crate and adds Gateway-specific key distribution logic.
//! All API keys are stored encrypted on disk via rollball_vault::Vault.
//!
//! Storage format (encrypted):
//!   Legacy: plain text API key string
//!   Current: JSON { "api_key": "...", "base_url": "...", "default_model": "...", "models": ["..."] }
//! The `get_key` method handles both formats transparently.

use crate::error::GatewayError;
use rollball_core::providers::vault_key_candidates;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

/// Full provider configuration stored in Vault
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// API key for the provider
    pub api_key: String,
    /// Base URL override (empty = use default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Default model for this provider (empty = use model from manifest)
    /// Kept for backward compatibility — prefer using `models` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Available models for this provider (user-selected from models.dev).
    /// `models[0]` is the default/active model, consistent with `default_model`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// Key entry for HTTP API listing (masked preview)
#[derive(Debug, Clone, serde::Serialize)]
pub struct VaultKeyEntry {
    /// Provider name
    pub provider: String,
    /// Masked key preview (first 3 + last 3 chars)
    pub key_preview: String,
}

/// Vault facade for Gateway
///
/// Delegates to rollball_vault::Vault for encrypted storage.
pub struct VaultFacade {
    /// Inner vault (encrypted on-disk storage)
    vault: rollball_vault::Vault,
    /// In-memory cache of provider names (not values) for fast listing
    provider_names: Vec<String>,
    /// Directory path where the vault is stored
    vault_dir: String,
}

impl VaultFacade {
    /// Create a new vault facade pointing at the given directory
    ///
    /// The vault starts in a locked state. Call `unlock()` with a password
    /// to derive the master key and enable store/retrieve operations.
    pub fn new(vault_dir: &str) -> Self {
        let vault = rollball_vault::Vault::open(std::path::Path::new(vault_dir))
            .unwrap_or_else(|e| panic!("Failed to open vault directory '{}': {}", vault_dir, e));
        Self {
            vault,
            provider_names: Vec::new(),
            vault_dir: vault_dir.to_string(),
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

    /// Get the vault directory path
    pub fn dir(&self) -> &std::path::Path {
        std::path::Path::new(&self.vault_dir)
    }

    /// Store a provider entry (encrypted on disk)
    ///
    /// Stores the full provider configuration as JSON:
    /// `{ "api_key": "...", "base_url": "...", "models": ["..."] }`
    pub fn store_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        self.store_provider(provider, None, &[], api_key)
    }

    /// Store a full provider entry with optional base_url and models list
    pub fn store_provider(
        &mut self,
        provider: &str,
        base_url: Option<&str>,
        models: &[String],
        api_key: &str,
    ) -> Result<(), GatewayError> {
        let default_model = models.first().cloned();
        let entry = ProviderEntry {
            api_key: api_key.to_string(),
            base_url: base_url.map(|s| s.to_string()),
            default_model,
            models: models.to_vec(),
        };
        let json = serde_json::to_string(&entry)
            .map_err(|e| GatewayError::Vault(format!("Failed to serialize provider entry: {}", e)))?;
        self.vault.store(provider, &json)
            .map_err(|e| GatewayError::Vault(format!("Failed to store key: {}", e)))?;
        if !self.provider_names.contains(&provider.to_string()) {
            self.provider_names.push(provider.to_string());
        }
        Ok(())
    }

    /// Get the full provider entry (decrypted)
    ///
    /// Handles both the current JSON format and the legacy plain-text format.
    /// Legacy entries (plain API key) are returned with base_url=None, default_model=None.
    ///
    /// If the provider is not found under its canonical ID, falls back to
    /// trying legacy alias names (e.g. "zhipuai" → try "glm", "zhipu").
    pub fn get_provider(&self, provider: &str) -> Result<ProviderEntry, GatewayError> {
        let candidates = vault_key_candidates(provider);
        for candidate in &candidates {
            match self.vault.retrieve(candidate) {
                Ok(secret) => {
                    let raw = secret.expose_secret();
                    // Try JSON format first (current)
                    if let Ok(entry) = serde_json::from_str::<ProviderEntry>(raw) {
                        return Ok(entry);
                    }
                    // Legacy format: plain text API key
                    return Ok(ProviderEntry {
                        api_key: raw.to_string(),
                        base_url: None,
                        default_model: None,
                        models: Vec::new(),
                    });
                }
                Err(_) => continue, // Try next candidate
            }
        }
        Err(GatewayError::Vault(format!(
            "No key found for provider '{}' (tried: {:?})",
            provider, candidates
        )))
    }

    /// Get just the API key for a provider (one-time distribution, decrypted)
    /// Backward-compatible: works with both JSON and legacy format.
    /// Also tries alias names if the canonical ID is not found in Vault.
    pub fn get_key(&self, provider: &str) -> Result<String, GatewayError> {
        let entry = self.get_provider(provider)?;
        Ok(entry.api_key)
    }

    /// List all providers with stored keys (no values returned)
    pub fn list_providers(&self) -> Vec<String> {
        self.provider_names.clone()
    }

    /// List all keys with masked previews (for HTTP API)
    /// Returns (provider, key_preview) pairs where key_preview shows
    /// first 3 and last 3 characters with *** in between.
    pub fn list_keys(&self) -> Result<Vec<VaultKeyEntry>, GatewayError> {
        let mut entries = Vec::new();
        for provider in &self.provider_names {
            let preview = if self.vault.is_unlocked() {
                match self.get_provider(provider) {
                    Ok(entry) => {
                        let key = &entry.api_key;
                        if key.len() > 6 {
                            format!("{}...{}", &key[..3], &key[key.len()-3..])
                        } else {
                            "***".to_string()
                        }
                    }
                    Err(_) => "***".to_string(),
                }
            } else {
                "***".to_string()
            };
            entries.push(VaultKeyEntry {
                provider: provider.clone(),
                key_preview: preview,
            });
        }
        Ok(entries)
    }

    /// Remove a key for a provider
    ///
    /// Also removes any entries stored under legacy alias names for the
    /// same canonical provider, ensuring a clean removal.
    pub fn remove_key(&mut self, provider: &str) -> Result<(), GatewayError> {
        let candidates = vault_key_candidates(provider);
        let mut removed_any = false;
        for candidate in &candidates {
            if self.vault.exists(candidate) {
                self.vault.delete(candidate)
                    .map_err(|e| GatewayError::Vault(format!(
                        "Failed to remove key for '{}': {}", candidate, e
                    )))?;
                self.provider_names.retain(|p| p != candidate);
                removed_any = true;
            }
        }
        if !removed_any {
            return Err(GatewayError::Vault(format!(
                "No key found for provider '{}' (tried: {:?})",
                provider, candidates
            )));
        }
        Ok(())
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

    #[test]
    fn test_vault_store_provider_full_config() {
        let dir = temp_vault_dir("store_provider");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_provider("deepseek", Some("https://api.deepseek.com/v1"), &["deepseek-chat".to_string()], "sk-abc").unwrap();
        let entry = vault.get_provider("deepseek").unwrap();
        assert_eq!(entry.api_key, "sk-abc");
        assert_eq!(entry.base_url, Some("https://api.deepseek.com/v1".to_string()));
        assert_eq!(entry.default_model, Some("deepseek-chat".to_string()));
        assert_eq!(entry.models, vec!["deepseek-chat"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_provider_minimal() {
        let dir = temp_vault_dir("store_provider_min");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_provider("openai", None, &[], "sk-test").unwrap();
        let entry = vault.get_provider("openai").unwrap();
        assert_eq!(entry.api_key, "sk-test");
        assert_eq!(entry.base_url, None);
        assert_eq!(entry.default_model, None);
        assert!(entry.models.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_legacy_format_compatibility() {
        let dir = temp_vault_dir("legacy");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store using old API (plain key)
        vault.store_key("openai", "sk-legacy-key").unwrap();
        // Retrieve using new API — should work with legacy format
        let entry = vault.get_provider("openai").unwrap();
        assert_eq!(entry.api_key, "sk-legacy-key");
        assert_eq!(entry.base_url, None);
        assert_eq!(entry.default_model, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_get_provider_glm_to_zhipuai() {
        let dir = temp_vault_dir("alias_glm");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under old alias "glm"
        vault.store_key("glm", "sk-glm-key").unwrap();
        // Retrieve using canonical "zhipuai" — should find "glm" via alias
        let entry = vault.get_provider("zhipuai").unwrap();
        assert_eq!(entry.api_key, "sk-glm-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_get_provider_qwen_to_alibaba() {
        let dir = temp_vault_dir("alias_qwen");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under old alias "qwen"
        vault.store_key("qwen", "sk-qwen-key").unwrap();
        // Retrieve using canonical "alibaba" — should find "qwen" via alias
        let entry = vault.get_provider("alibaba").unwrap();
        assert_eq!(entry.api_key, "sk-qwen-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_get_provider_moonshot_to_moonshotai() {
        let dir = temp_vault_dir("alias_moonshot");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under old alias "moonshot"
        vault.store_key("moonshot", "sk-moonshot-key").unwrap();
        // Retrieve using canonical "moonshotai" — should find "moonshot" via alias
        let entry = vault.get_provider("moonshotai").unwrap();
        assert_eq!(entry.api_key, "sk-moonshot-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_canonical_takes_priority() {
        let dir = temp_vault_dir("alias_priority");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under both canonical and alias
        vault.store_key("zhipuai", "sk-canonical-key").unwrap();
        vault.store_key("glm", "sk-alias-key").unwrap();
        // Canonical should take priority
        let entry = vault.get_provider("zhipuai").unwrap();
        assert_eq!(entry.api_key, "sk-canonical-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_remove_cleans_all() {
        let dir = temp_vault_dir("alias_remove");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under both canonical and alias
        vault.store_key("zhipuai", "sk-canonical-key").unwrap();
        vault.store_key("glm", "sk-alias-key").unwrap();
        // Remove using canonical name — should clean up both
        vault.remove_key("zhipuai").unwrap();
        // Both should be gone
        assert!(vault.get_provider("zhipuai").is_err());
        assert!(vault.get_provider("glm").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_reverse_lookup() {
        let dir = temp_vault_dir("alias_reverse");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under canonical "zhipuai"
        vault.store_key("zhipuai", "sk-zhipuai-key").unwrap();
        // Retrieve using old alias "glm" — should still find "zhipuai"
        let entry = vault.get_provider("glm").unwrap();
        assert_eq!(entry.api_key, "sk-zhipuai-key");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
