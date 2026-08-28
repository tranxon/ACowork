//! Vault integration — facade for API key distribution
//!
//! Wraps acowork-vault crate and adds Gateway-specific key distribution logic.
//! All API keys are stored encrypted on disk via acowork_vault::Vault.
//!
//! Vault ONLY stores encrypted API keys. All non-secret provider configuration
//! (base_url, models, capabilities, compact_model) is stored in
//! provider_list.json via the resource_cache module.
//!
//! Storage format (encrypted):
//!   Legacy: plain text API key string
//!   Current: JSON { "api_key": "..." }
//! The `get_key` method handles both formats transparently.

use crate::error::GatewayError;
use acowork_core::providers::vault_key_candidates;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

/// Provider entry stored in Vault — only the encrypted API key.
///
/// All other provider configuration (base_url, models, capabilities,
/// compact_model) is stored in provider_list.json, NOT in the Vault.
/// See `resource_cache.rs` for provider configuration management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// API key for the provider
    pub api_key: String,
}

/// Key entry for HTTP API listing (masked preview)
#[derive(Debug, Clone, serde::Serialize)]
pub struct VaultKeyEntry {
    /// Provider name
    pub provider: String,
    /// Masked key preview (first 3 + last 3 chars)
    pub key_preview: String,
}

/// Search API key entry returned by Vault facade.
#[derive(Debug, Clone)]
pub struct SearchKeyStorageEntry {
    /// Decrypted API key
    pub api_key: String,
}

/// Masked search key preview for HTTP API (no decrypted key exposed).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchKeyPreview {
    /// Search provider identifier (e.g. "tavily")
    pub provider: String,
    /// Masked key preview (first 3 + last 3 chars)
    pub key_preview: String,
}

/// Vault facade for Gateway
///
/// Delegates to acowork_vault::Vault for encrypted storage.
pub struct VaultFacade {
    /// Inner vault (encrypted on-disk storage)
    vault: acowork_vault::Vault,
    /// In-memory cache of provider names (not values) for fast listing
    provider_names: Vec<String>,
    /// Directory path where the vault is stored
    vault_dir: String,
}

/// Diagnostic helper: produce a `first_4...last_4` preview consistent with
/// `runtime::providers::openai::send_streaming_request`. Lets us correlate
/// the post-store, post-decrypt, post-publish, and runtime-seen byte
/// ranges by eye (or by `grep` on the log files).
///
/// Short keys return `<N>` (matches the runtime's short-key branch).
/// The preview NEVER leaks the full key — only the very first and very
/// last 4 bytes of the input are reflected.
fn preview_key(k: &str) -> String {
    let len = k.len();
    if len <= 8 {
        format!("<{}>", len)
    } else {
        format!("{}...{}", &k[..4], &k[len - 4..])
    }
}

impl VaultFacade {
    /// Create a new vault facade pointing at the given directory
    ///
    /// The vault starts in a locked state. Call `unlock()` with a password
    /// to derive the master key and enable store/retrieve operations.
    pub fn new(vault_dir: &str) -> Self {
        let vault = acowork_vault::Vault::open(std::path::Path::new(vault_dir))
            .unwrap_or_else(|e| panic!("Failed to open vault directory '{}': {}", vault_dir, e));
        Self {
            vault,
            provider_names: Vec::new(),
            vault_dir: vault_dir.to_string(),
        }
    }

    /// Unlock the vault with a password (delegates to acowork_vault)
    pub fn unlock(&mut self, password: &str) -> Result<(), GatewayError> {
        self.vault
            .unlock(password)
            .map_err(|e| GatewayError::Vault(format!("Failed to unlock vault: {}", e)))?;
        // Refresh provider list after unlock
        self.provider_names = self
            .vault
            .list()
            .map_err(|e| GatewayError::Vault(format!("Failed to list vault keys: {}", e)))?;
        Ok(())
    }

    /// Check if vault is unlocked
    pub fn is_unlocked(&self) -> bool {
        self.vault.is_unlocked()
    }

    /// Lock the vault: zeroize the derived master key and drop the
    /// in-memory provider-name cache.
    ///
    /// On-disk encrypted blobs are untouched — a later `unlock()` with
    /// the same password restores access to everything previously
    /// stored. Idempotent: locking an already-locked vault is a no-op.
    pub fn lock(&mut self) {
        self.vault.lock();
        self.provider_names.clear();
    }

    /// Get the vault directory path
    pub fn dir(&self) -> &std::path::Path {
        std::path::Path::new(&self.vault_dir)
    }

    /// Store a provider API key (encrypted on disk).
    ///
    /// Stores only the API key. Provider configuration (base_url, models, etc.)
    /// is managed separately via provider_list.json.
    pub fn store_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        // DIAG: log the post-serialisation byte range that is about to be
        // encrypted and persisted. If this preview already looks wrong, the
        // bug is upstream of the vault (HTTP handler, onboarding TS, etc.).
        let in_preview = preview_key(api_key);
        tracing::info!(
            provider = %provider,
            api_key_len = api_key.len(),
            api_key_prefix = %in_preview,
            "vault.store_key: serialising ProviderEntry before encryption"
        );
        let entry = ProviderEntry {
            api_key: api_key.to_string(),
        };
        let json = serde_json::to_string(&entry).map_err(|e| {
            GatewayError::Vault(format!("Failed to serialize provider entry: {}", e))
        })?;
        self.vault
            .store(provider, &json)
            .map_err(|e| GatewayError::Vault(format!("Failed to store key: {}", e)))?;
        if !self.provider_names.contains(&provider.to_string()) {
            self.provider_names.push(provider.to_string());
        }
        Ok(())
    }


    /// Get the API key for a provider (decrypted).
    ///
    /// Handles both the current JSON format and the legacy plain-text format.
    /// Legacy entries (plain API key) are returned as-is.
    ///
    /// If the provider is not found under its canonical ID, falls back to
    /// trying legacy alias names (e.g. "zhipuai" → try "glm", "zhipu").
    pub fn get_provider(&self, provider: &str) -> Result<ProviderEntry, GatewayError> {
        let candidates = vault_key_candidates(provider);
        for candidate in &candidates {
            match self.vault.retrieve(candidate) {
                Ok(secret) => {
                    let raw = secret.expose_secret();
                    // DIAG: log the just-decrypted raw bytes BEFORE we try
                    // to parse them. If this preview already looks wrong,
                    // the bug is in the on-disk encryption / master-key
                    // derivation (Argon2id salt, wrong password, etc.).
                    tracing::info!(
                        provider = %provider,
                        candidate = %candidate,
                        raw_len = raw.len(),
                        raw_preview = %preview_key(raw),
                        "vault.get_provider: decrypted raw bytes"
                    );
                    // Try JSON format first (current)
                    if let Ok(entry) = serde_json::from_str::<ProviderEntry>(raw) {
                        tracing::info!(
                            provider = %provider,
                            candidate = %candidate,
                            api_key_len = entry.api_key.len(),
                            api_key_prefix = %preview_key(&entry.api_key),
                            "vault.get_provider: returning JSON-encoded entry"
                        );
                        return Ok(entry);
                    }
                    // Legacy format: plain text API key
                    tracing::warn!(
                        provider = %provider,
                        candidate = %candidate,
                        raw_len = raw.len(),
                        raw_preview = %preview_key(raw),
                        "vault.get_provider: legacy plaintext fallback (non-JSON payload)"
                    );
                    return Ok(ProviderEntry {
                        api_key: raw.to_string(),
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
                            format!("{}...{}", &key[..3], &key[key.len() - 3..])
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
                self.vault.delete(candidate).map_err(|e| {
                    GatewayError::Vault(format!("Failed to remove key for '{}': {}", candidate, e))
                })?;
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

    // ── Search key CRUD (stored under "_search_" prefix) ─────────────

    const SEARCH_PREFIX: &str = "_search_";

    /// Store a web search provider API key.
    pub fn store_search_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        let key_name = format!("{}{provider}", Self::SEARCH_PREFIX);
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        self.vault
            .store(&key_name, api_key)
            .map_err(|e| GatewayError::Vault(format!("Failed to store search key: {e}")))?;
        Ok(())
    }

    /// Get a web search provider API key (decrypted).
    pub fn get_search_key(&self, provider: &str) -> Result<SearchKeyStorageEntry, GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let key_name = format!("{}{provider}", Self::SEARCH_PREFIX);
        let secret = self
            .vault
            .retrieve(&key_name)
            .map_err(|e| GatewayError::Vault(format!("No search key for '{provider}': {e}")))?;
        Ok(SearchKeyStorageEntry {
            api_key: secret.expose_secret().to_string(),
        })
    }

    /// List all configured search providers with masked key previews.
    /// Returns entries with provider name and masked API key (first 3 + last 3 chars).
    pub fn list_search_keys(&self) -> Result<Vec<SearchKeyPreview>, GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let all_keys = self
            .vault
            .list()
            .map_err(|e| GatewayError::Vault(format!("Failed to list vault keys: {e}")))?;
        let mut entries = Vec::new();
        for key_name in &all_keys {
            if let Some(provider) = key_name.strip_prefix(Self::SEARCH_PREFIX) {
                let preview = match self.vault.retrieve(key_name) {
                    Ok(secret) => {
                        let key = secret.expose_secret();
                        if key.len() > 6 {
                            format!("{}...{}", &key[..3], &key[key.len() - 3..])
                        } else {
                            "***".to_string()
                        }
                    }
                    Err(_) => "***".to_string(),
                };
                entries.push(SearchKeyPreview {
                    provider: provider.to_string(),
                    key_preview: preview,
                });
            }
        }
        Ok(entries)
    }

    /// Remove a web search provider API key.
    pub fn remove_search_key(&mut self, provider: &str) -> Result<(), GatewayError> {
        let key_name = format!("{}{provider}", Self::SEARCH_PREFIX);
        if !self.vault.exists(&key_name) {
            return Err(GatewayError::Vault(format!(
                "No search key for '{provider}'"
            )));
        }
        self.vault
            .delete(&key_name)
            .map_err(|e| GatewayError::Vault(format!("Failed to remove search key: {e}")))?;
        Ok(())
    }
}

/// Unlock the vault and raise every readiness signal that depends on it.
///
/// ADR-059 Phase 5.4: this is the ONE implementation of the
/// "unlock → mark vault ready → republish" sequence. The dev-mode
/// auto-unlock task (cold start) and the HTTP `POST /api/vault/unlock`
/// handler (post-relock recovery) both call it — the plan requires the
/// relock path to reuse the cold-start code rather than duplicate it.
///
/// The Argon2id KDF is deliberately slow (~1 s), so the unlock itself
/// runs on the blocking pool via `blocking_write`; the readiness
/// transitions happen on the async side afterwards. On success the
/// vault subsystem is marked Ready, a global-resources republish is
/// triggered so Runtimes receive the now-decrypted keys, and the
/// publisher ready barrier (cold-start only) is raised if still
/// pending. On failure an `Err` is returned — callers decide whether
/// to mark the subsystem Failed (dev-mode auto-unlock) or surface an
/// HTTP error (lock/unlock API).
pub async fn unlock_vault_and_mark_ready(
    state: std::sync::Arc<tokio::sync::RwLock<crate::gateway::state::GatewayState>>,
    handle: crate::bootstrap::SubsystemHandle,
    trigger: Option<crate::mqtt::MqttPublisherTrigger>,
    password: String,
    detail: String,
) -> Result<(), String> {
    let state_for_kdf = state.clone();
    let password_for_kdf = password.clone();
    let result = tokio::task::spawn_blocking(move || {
        state_for_kdf
            .blocking_write()
            .vault
            .unlock(&password_for_kdf)
    })
    .await;
    match result {
        Ok(Ok(())) => {
            tracing::info!("Vault unlocked");
            handle.mark_ready(Some(detail));
            if let Some(ref t) = trigger {
                t.trigger();
            }
            // Cold-start only: the publisher loop may still be waiting
            // on its ready barrier; raising it now releases the first
            // retained publish with populated `api_key` fields. On a
            // later unlock (relock cycle) the handle is already ready
            // and this is a no-op.
            let gw = state.read().await;
            if let Some(ref h) = gw.mqtt_publisher_handle {
                h.mark_ready();
            }
            Ok(())
        }
        Ok(Err(e)) => Err(format!("Failed to unlock vault: {e}")),
        Err(e) => Err(format!("Vault unlock task panicked: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("acowork-test-vaultfacade-{name}"));
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
    fn test_vault_lock_clears_access_but_keeps_data() {
        let dir = temp_vault_dir("lock_clear");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-lock-key").unwrap();
        assert!(vault.is_unlocked());

        // Lock: master key zeroized, provider-name cache dropped.
        vault.lock();
        assert!(!vault.is_unlocked());
        assert!(vault.get_key("openai").is_err());
        assert!(vault.list_providers().is_empty());

        // The same password restores access to the stored data.
        vault.unlock("password123").unwrap();
        assert!(vault.is_unlocked());
        assert_eq!(vault.get_key("openai").unwrap(), "sk-lock-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_lock_is_idempotent() {
        let dir = temp_vault_dir("lock_idem");
        let mut vault = VaultFacade::new(&dir);
        // Locking an already-locked (freshly constructed) vault is a
        // no-op — the relock path must never panic on double lock.
        vault.lock();
        assert!(!vault.is_unlocked());
        vault.lock();
        assert!(!vault.is_unlocked());
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
    fn test_vault_store_and_get_full() {
        let dir = temp_vault_dir("store_get_full");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("deepseek", "sk-abc").unwrap();
        let key = vault.get_key("deepseek").unwrap();
        assert_eq!(key, "sk-abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_and_get_empty_key() {
        let dir = temp_vault_dir("store_get_empty");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("ollama", "").unwrap();
        let key = vault.get_key("ollama").unwrap();
        assert_eq!(key, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_legacy_format_compatibility() {
        let dir = temp_vault_dir("legacy");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store using the API (JSON format)
        vault.store_key("openai", "sk-legacy-key").unwrap();
        // Retrieve — should work
        let entry = vault.get_provider("openai").unwrap();
        assert_eq!(entry.api_key, "sk-legacy-key");
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
