//! Permission persistence store (JSON file backend)
//!
//! Stores user authorization decisions per Agent: granted permissions,
//! scope constraints, expiry, and revocation. Used by Gateway to
//! persist authorization decisions and answer Runtime permission queries.
//!
//! Data is stored as a JSON array of `PermissionGrant` objects,
//! written atomically (write-to-temp + rename).
//!
//! Migration: on first open, if an old `permissions.db` SQLite file exists
//! but the JSON file does not, data is migrated automatically.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rollball_core::permission::{Permission, PermissionGrant};

// ── Store ────────────────────────────────────────────────────────────────

/// Persistent store for permission grants.
///
/// Internally uses `Mutex<Vec<PermissionGrant>>` with JSON file persistence.
/// In-memory mode (for tests) skips file I/O entirely.
#[derive(Debug)]
pub struct PermissionStore {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    grants: Vec<PermissionGrant>,
    /// None = in-memory (no file persistence)
    path: Option<PathBuf>,
}

impl PermissionStore {
    /// Open (or create) the permission store at the given path.
    ///
    /// If an old `permissions.db` SQLite file exists and the JSON file does not,
    /// data is migrated automatically and the old DB is renamed to `.db.bak`.
    pub fn open(path: &Path) -> Result<Self, PermissionStoreError> {
        // Attempt migration from old SQLite DB
        let db_path = path.with_extension("db");
        let json_path = path.with_extension("json");
        if db_path.exists() && !json_path.exists() {
            if let Err(e) = Self::migrate_from_sqlite(&db_path, &json_path) {
                tracing::warn!(
                    "Failed to migrate permission store from {}: {}. Starting fresh.",
                    db_path.display(),
                    e
                );
            }
        }

        let grants = if json_path.exists() {
            let data = std::fs::read_to_string(&json_path).map_err(PermissionStoreError::Io)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Self {
            inner: Mutex::new(Inner {
                grants,
                path: Some(json_path),
            }),
        })
    }

    /// Open an in-memory store (for testing). No file I/O.
    pub fn open_in_memory() -> Result<Self, PermissionStoreError> {
        Ok(Self {
            inner: Mutex::new(Inner {
                grants: Vec::new(),
                path: None,
            }),
        })
    }

    /// Check if the store is healthy (file is readable if on-disk).
    pub fn health_check(&self) -> Result<(), PermissionStoreError> {
        let inner = self.inner.lock().unwrap();
        if let Some(ref path) = inner.path {
            if path.exists() {
                let _ = std::fs::read_to_string(path).map_err(PermissionStoreError::Io)?;
            }
        }
        Ok(())
    }

    // ── CRUD ─────────────────────────────────────────────────────────

    /// Grant a permission to an agent. Returns a synthetic id.
    pub fn grant(&self, g: &PermissionGrant) -> Result<i64, PermissionStoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.grants.push(g.clone());
        self.save_locked(&inner)?;
        Ok(inner.grants.len() as i64)
    }

    /// Query all active (non-expired) grants for an agent.
    pub fn query_grants(&self, agent_id: &str) -> Result<Vec<PermissionGrant>, PermissionStoreError> {
        let inner = self.inner.lock().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        Ok(inner
            .grants
            .iter()
            .filter(|g| {
                g.agent_id == agent_id
                    && g.expires_at.map_or(true, |exp| exp > now)
            })
            .cloned()
            .collect())
    }

    /// Revoke all grants for an agent, or a specific permission.
    /// If `permission` is Some, only revoke grants matching that permission.
    /// If `permission` is None, revoke all grants for the agent.
    pub fn revoke(
        &self,
        agent_id: &str,
        permission: Option<&Permission>,
    ) -> Result<usize, PermissionStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.grants.len();
        match permission {
            Some(perm) => {
                inner.grants.retain(|g| {
                    !(g.agent_id == agent_id && g.permission == *perm)
                });
            }
            None => {
                inner.grants.retain(|g| g.agent_id != agent_id);
            }
        }
        let removed = before - inner.grants.len();
        self.save_locked(&inner)?;
        Ok(removed)
    }

    /// Reset all grants for an agent (revoke everything).
    pub fn reset(&self, agent_id: &str) -> Result<usize, PermissionStoreError> {
        self.revoke(agent_id, None)
    }

    /// Check if an agent has a specific permission granted (active only).
    pub fn has_permission(
        &self,
        agent_id: &str,
        requested: &Permission,
    ) -> Result<bool, PermissionStoreError> {
        let grants = self.query_grants(agent_id)?;
        Ok(grants.iter().any(|g| g.matches_request(requested)))
    }

    /// Clean up expired grants. Returns the number of removed rows.
    pub fn cleanup_expired(&self) -> Result<usize, PermissionStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let before = inner.grants.len();
        inner.grants.retain(|g| {
            g.expires_at.map_or(true, |exp| exp > now)
        });
        let removed = before - inner.grants.len();
        if removed > 0 {
            self.save_locked(&inner)?;
        }
        Ok(removed)
    }

    // ── Internal helpers ─────────────────────────────────────────────

    /// Save grants to file. Must hold the lock.
    fn save_locked(&self, inner: &Inner) -> Result<(), PermissionStoreError> {
        if let Some(ref path) = inner.path {
            save_json_atomic(path, &inner.grants)?;
        }
        Ok(())
    }

    /// Migrate data from an old permissions.db SQLite file to JSON.
    fn migrate_from_sqlite(db_path: &Path, json_path: &Path) -> Result<(), PermissionStoreError> {
        // Lazy-load rusqlite only for migration
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| PermissionStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;

        let mut stmt = conn
            .prepare(
                "SELECT agent_id, perm_type, perm_value, authorized_by, granted_at, expires_at, scope
                 FROM permission_grants",
            )
            .map_err(|e| PermissionStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|e| PermissionStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;

        let mut grants = Vec::new();
        for row in rows {
            let (agent_id, perm_type, perm_value, authorized_by, granted_at, expires_at, scope) = row
                .map_err(|e| PermissionStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;
            let permission =
                deserialize_permission(&perm_type, perm_value.as_deref())?;
            grants.push(PermissionGrant {
                agent_id,
                permission,
                authorized_by,
                granted_at,
                expires_at,
                scope,
            });
        }

        save_json_atomic(json_path, &grants)?;

        // Rename old DB to .bak instead of deleting
        let bak = db_path.with_extension("db.bak");
        let _ = std::fs::rename(db_path, &bak);

        tracing::info!(
            "Migrated {} permission grants from {} to {}",
            grants.len(),
            db_path.display(),
            json_path.display()
        );
        Ok(())
    }
}

// ── Serialization helpers ─────────────────────────────────────────────

fn deserialize_permission(
    perm_type: &str,
    perm_value: Option<&str>,
) -> Result<Permission, PermissionStoreError> {
    match perm_type {
        "Network" => Ok(Permission::Network(perm_value.map(|s| s.to_string()))),
        "FilesystemRead" => Ok(Permission::FilesystemRead(perm_value.map(|s| s.to_string()))),
        "FilesystemWrite" => Ok(Permission::FilesystemWrite(perm_value.map(|s| s.to_string()))),
        "MemoryRead" => Ok(Permission::MemoryRead),
        "MemoryWrite" => Ok(Permission::MemoryWrite),
        "IntentSend" => Ok(Permission::IntentSend(perm_value.map(|s| s.to_string()))),
        "IntentReceive" => Ok(Permission::IntentReceive(perm_value.map(|s| s.to_string()))),
        "IdentityRead" => Ok(Permission::IdentityRead),
        "IdentityWrite" => Ok(Permission::IdentityWrite),
        "Shell" => Ok(Permission::Shell),
        "Wasm" => Ok(Permission::Wasm),
        other => Err(PermissionStoreError::InvalidPermissionType(other.to_string())),
    }
}

// ── Atomic JSON helpers ──────────────────────────────────────────────

/// Save data to a JSON file atomically (write to `.tmp`, then rename).
fn save_json_atomic<T: serde::Serialize>(
    path: &Path,
    data: &T,
) -> Result<(), PermissionStoreError> {
    let json = serde_json::to_string_pretty(data)
        .map_err(PermissionStoreError::Json)?;
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, &json).map_err(PermissionStoreError::Io)?;
    std::fs::rename(&tmp_path, path).map_err(PermissionStoreError::Io)?;
    Ok(())
}

// ── Error type ────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PermissionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid permission type: {0}")]
    InvalidPermissionType(String),
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grant_and_query() {
        let store = PermissionStore::open_in_memory().unwrap();

        let grant = PermissionGrant::new(
            "com.example.weather",
            Permission::Network(Some("https://api.weather.com".into())),
            "user",
        );
        store.grant(&grant).unwrap();

        let grants = store.query_grants("com.example.weather").unwrap();
        assert_eq!(grants.len(), 1);
        assert!(matches!(grants[0].permission, Permission::Network(Some(_))));
        assert_eq!(grants[0].authorized_by, "user");
    }

    #[test]
    fn test_grant_multiple_permissions() {
        let store = PermissionStore::open_in_memory().unwrap();

        store.grant(&PermissionGrant::new("com.example.agent", Permission::Shell, "user")).unwrap();
        store.grant(&PermissionGrant::new("com.example.agent", Permission::MemoryRead, "auto")).unwrap();
        store.grant(&PermissionGrant::new("com.example.agent", Permission::Network(None), "user")).unwrap();

        let grants = store.query_grants("com.example.agent").unwrap();
        assert_eq!(grants.len(), 3);
    }

    #[test]
    fn test_revoke_specific_permission() {
        let store = PermissionStore::open_in_memory().unwrap();

        store.grant(&PermissionGrant::new("com.example.agent", Permission::Shell, "user")).unwrap();
        store.grant(&PermissionGrant::new("com.example.agent", Permission::MemoryRead, "auto")).unwrap();

        let revoked = store.revoke("com.example.agent", Some(&Permission::Shell)).unwrap();
        assert_eq!(revoked, 1);

        let grants = store.query_grants("com.example.agent").unwrap();
        assert_eq!(grants.len(), 1);
        assert!(matches!(grants[0].permission, Permission::MemoryRead));
    }

    #[test]
    fn test_revoke_all_permissions() {
        let store = PermissionStore::open_in_memory().unwrap();

        store.grant(&PermissionGrant::new("com.example.agent", Permission::Shell, "user")).unwrap();
        store.grant(&PermissionGrant::new("com.example.agent", Permission::MemoryRead, "auto")).unwrap();

        let revoked = store.reset("com.example.agent").unwrap();
        assert_eq!(revoked, 2);

        let grants = store.query_grants("com.example.agent").unwrap();
        assert!(grants.is_empty());
    }

    #[test]
    fn test_has_permission() {
        let store = PermissionStore::open_in_memory().unwrap();

        store.grant(&PermissionGrant::new(
            "com.example.agent",
            Permission::Network(None),
            "user",
        )).unwrap();

        assert!(store.has_permission("com.example.agent", &Permission::Network(Some("https://api.weather.com".into()))).unwrap());
        assert!(store.has_permission("com.example.agent", &Permission::Network(None)).unwrap());
        assert!(!store.has_permission("com.example.agent", &Permission::Shell).unwrap());
    }

    #[test]
    fn test_expired_grant_not_returned() {
        let store = PermissionStore::open_in_memory().unwrap();

        let past = chrono::Utc::now().timestamp_millis() - 10000;
        let expired = PermissionGrant::with_expiry(
            "com.example.agent",
            Permission::Shell,
            "user",
            past,
        );
        store.grant(&expired).unwrap();

        let grants = store.query_grants("com.example.agent").unwrap();
        assert!(grants.is_empty());
    }

    #[test]
    fn test_cleanup_expired() {
        let store = PermissionStore::open_in_memory().unwrap();

        let past = chrono::Utc::now().timestamp_millis() - 10000;
        let future = chrono::Utc::now().timestamp_millis() + 86400000;

        store.grant(&PermissionGrant::with_expiry("com.example.agent", Permission::Shell, "user", past)).unwrap();
        store.grant(&PermissionGrant::with_expiry("com.example.agent", Permission::MemoryRead, "auto", future)).unwrap();

        let cleaned = store.cleanup_expired().unwrap();
        assert_eq!(cleaned, 1);

        let grants = store.query_grants("com.example.agent").unwrap();
        assert_eq!(grants.len(), 1);
        assert!(matches!(grants[0].permission, Permission::MemoryRead));
    }

    #[test]
    fn test_different_agents_isolated() {
        let store = PermissionStore::open_in_memory().unwrap();

        store.grant(&PermissionGrant::new("agent.a", Permission::Shell, "user")).unwrap();
        store.grant(&PermissionGrant::new("agent.b", Permission::MemoryRead, "auto")).unwrap();

        let grants_a = store.query_grants("agent.a").unwrap();
        assert_eq!(grants_a.len(), 1);
        assert!(matches!(grants_a[0].permission, Permission::Shell));

        let grants_b = store.query_grants("agent.b").unwrap();
        assert_eq!(grants_b.len(), 1);
        assert!(matches!(grants_b[0].permission, Permission::MemoryRead));
    }

    #[test]
    fn test_open_on_disk_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_perms.json");

        // Create and populate
        {
            let store = PermissionStore::open(&path).unwrap();
            store.grant(&PermissionGrant::new("agent.1", Permission::Shell, "user")).unwrap();
            store.grant(&PermissionGrant::new("agent.1", Permission::MemoryRead, "auto")).unwrap();
        }

        // Reopen and verify
        {
            let store = PermissionStore::open(&path).unwrap();
            let grants = store.query_grants("agent.1").unwrap();
            assert_eq!(grants.len(), 2);
        }
    }
}
