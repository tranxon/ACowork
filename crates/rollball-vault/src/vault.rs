//! Vault main structure
//!
//! Manages encrypted storage of LLM API keys.
//!
//! Directory layout:
//! ```text
//! ~/.config/agent-gateway/vault/
//! ├── salt              — Argon2id salt (16 bytes)
//! ├── openai.enc        — Encrypted API key for OpenAI
//! ├── anthropic.enc     — Encrypted API key for Anthropic
//! └── ...
//! ```

use secrecy::SecretString;
#[cfg(test)]
use secrecy::ExposeSecret;
use zeroize::Zeroize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::encryption;
use crate::key_derivation;
use crate::error::{Result, VaultError};

/// Vault for storing API keys
pub struct Vault {
    vault_dir: PathBuf,
    /// Derived master key (32 bytes, present only when unlocked)
    master_key: Option<Vec<u8>>,
}

impl Vault {
    /// Create or open a Vault at the specified directory
    ///
    /// The vault starts in a locked state. Call `unlock()` with a password
    /// to derive the master key and enable store/retrieve operations.
    pub fn open(vault_dir: &Path) -> Result<Self> {
        fs::create_dir_all(vault_dir)?;
        Ok(Self {
            vault_dir: vault_dir.to_path_buf(),
            master_key: None,
        })
    }

    /// Unlock vault with password (derive master key via Argon2id)
    ///
    /// If no salt file exists, a new one is generated.
    /// If a salt file exists, it is loaded to derive the same key.
    pub fn unlock(&mut self, password: &str) -> Result<()> {
        let salt_path = self.vault_dir.join(key_derivation::SALT_FILE);

        let salt = if salt_path.exists() {
            fs::read(&salt_path)?
        } else {
            let salt = key_derivation::generate_salt();
            fs::write(&salt_path, &salt)?;
            salt
        };

        if salt.len() != key_derivation::SALT_LEN {
            return Err(VaultError::InvalidPassword);
        }

        let master_key = key_derivation::derive_key(password, &salt)?;
        self.master_key = Some(master_key);
        Ok(())
    }

    /// Lock the vault (zeroize master key from memory)
    pub fn lock(&mut self) {
        if let Some(mut key) = self.master_key.take() {
            // Zero out the key material using zeroize to prevent
            // compiler optimization from removing the dead store
            key.zeroize();
        }
    }

    /// Check if the vault is unlocked
    pub fn is_unlocked(&self) -> bool {
        self.master_key.is_some()
    }

    /// Store a key (encrypted)
    ///
    /// The key is encrypted with ChaCha20-Poly1305 and written to
    /// `<vault_dir>/<key_name>.enc`.
    pub fn store(&self, key_name: &str, secret: &str) -> Result<()> {
        let key = self.master_key.as_ref().ok_or(VaultError::NotUnlocked)?;

        let encrypted = encryption::encrypt(secret.as_bytes(), key)?;
        let file_path = self.encrypted_path(key_name);
        fs::write(&file_path, &encrypted)?;

        Ok(())
    }

    /// Retrieve a key (decrypted, returns SecretString)
    ///
    /// The key is read from the encrypted file and decrypted.
    /// Returns a `SecretString` to minimize exposure in memory.
    pub fn retrieve(&self, key_name: &str) -> Result<SecretString> {
        let key = self.master_key.as_ref().ok_or(VaultError::NotUnlocked)?;

        let file_path = self.encrypted_path(key_name);
        if !file_path.exists() {
            return Err(VaultError::KeyNotFound(key_name.to_string()));
        }

        let encrypted = fs::read(&file_path)?;
        let decrypted = encryption::decrypt(&encrypted, key)?;

        let secret_str = String::from_utf8(decrypted)
            .map_err(|_| VaultError::DecryptionFailed("Invalid UTF-8 in decrypted key".into()))?;

        Ok(SecretString::from(secret_str))
    }

    /// Delete a stored key
    pub fn delete(&self, key_name: &str) -> Result<()> {
        let file_path = self.encrypted_path(key_name);
        if !file_path.exists() {
            return Err(VaultError::KeyNotFound(key_name.to_string()));
        }
        fs::remove_file(&file_path)?;
        Ok(())
    }

    /// List all key names (does not return values)
    pub fn list(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();

        if !self.vault_dir.exists() {
            return Ok(keys);
        }

        for entry in fs::read_dir(&self.vault_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(key_name) = name.strip_suffix(".enc") {
                keys.push(key_name.to_string());
            }
        }

        keys.sort();
        Ok(keys)
    }

    /// Check if a key exists
    pub fn exists(&self, key_name: &str) -> bool {
        self.encrypted_path(key_name).exists()
    }

    /// Get the path for an encrypted key file
    fn encrypted_path(&self, key_name: &str) -> PathBuf {
        self.vault_dir.join(format!("{key_name}.enc"))
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        // Ensure master key is zeroed on drop
        self.lock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rollball-test-vault-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_vault_open_creates_directory() {
        let dir = temp_vault_dir("open").join("new_vault");
        let _ = fs::remove_dir_all(&dir);

        let _vault = Vault::open(&dir).unwrap();
        assert!(dir.exists());

        let _ = fs::remove_dir_all(temp_vault_dir("open"));
    }

    #[test]
    fn test_vault_store_retrieve_roundtrip() {
        let dir = temp_vault_dir("roundtrip");
        let mut vault = Vault::open(&dir).unwrap();

        vault.unlock("my_password").unwrap();
        vault.store("openai", "sk-12345").unwrap();

        let retrieved = vault.retrieve("openai").unwrap();
        assert_eq!(retrieved.expose_secret(), "sk-12345");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_not_unlocked() {
        let dir = temp_vault_dir("not_unlocked");
        let vault = Vault::open(&dir).unwrap();

        let result = vault.store("openai", "sk-12345");
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_wrong_password() {
        let dir = temp_vault_dir("wrong_pw");
        let mut vault = Vault::open(&dir).unwrap();

        // Store with password1
        vault.unlock("password1").unwrap();
        vault.store("openai", "sk-12345").unwrap();
        vault.lock();

        // Try to retrieve with password2
        vault.unlock("password2").unwrap();
        let result = vault.retrieve("openai");
        // Wrong password produces different key → decryption fails
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_key_not_found() {
        let dir = temp_vault_dir("not_found");
        let mut vault = Vault::open(&dir).unwrap();

        vault.unlock("password").unwrap();
        let result = vault.retrieve("nonexistent");
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_list_keys() {
        let dir = temp_vault_dir("list");
        let mut vault = Vault::open(&dir).unwrap();

        vault.unlock("password").unwrap();
        vault.store("openai", "sk-openai").unwrap();
        vault.store("anthropic", "sk-ant").unwrap();

        let keys = vault.list().unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"openai".to_string()));
        assert!(keys.contains(&"anthropic".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_delete_key() {
        let dir = temp_vault_dir("delete");
        let mut vault = Vault::open(&dir).unwrap();

        vault.unlock("password").unwrap();
        vault.store("openai", "sk-openai").unwrap();
        assert!(vault.exists("openai"));

        vault.delete("openai").unwrap();
        assert!(!vault.exists("openai"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_lock_clears_key() {
        let dir = temp_vault_dir("lock");
        let mut vault = Vault::open(&dir).unwrap();

        vault.unlock("password").unwrap();
        assert!(vault.is_unlocked());

        vault.lock();
        assert!(!vault.is_unlocked());

        let _ = fs::remove_dir_all(&dir);
    }
}
