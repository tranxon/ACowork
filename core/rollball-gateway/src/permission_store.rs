//! Permission persistence store (rusqlite backend)
//!
//! Stores user authorization decisions per Agent: granted permissions,
//! scope constraints, expiry, and revocation. Used by Gateway to
//! persist authorization decisions and answer Runtime permission queries.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use rollball_core::permission::{Permission, PermissionGrant};

/// Persistent store for permission grants.
///
/// Schema (v1):
/// ```sql
/// CREATE TABLE permission_grants (
///     id          INTEGER PRIMARY KEY AUTOINCREMENT,
///     agent_id    TEXT NOT NULL,
///     perm_type   TEXT NOT NULL,       -- serialized Permission.type
///     perm_value  TEXT,                -- serialized Permission.value (nullable)
///     authorized_by TEXT NOT NULL,     -- "user" / "system" / "auto"
///     granted_at  INTEGER NOT NULL,    -- Unix millis
///     expires_at  INTEGER,             -- Unix millis (nullable = permanent)
///     scope       TEXT                 -- optional scope constraint
/// );
/// CREATE INDEX idx_grants_agent ON permission_grants(agent_id);
/// ```

#[derive(Debug)]
pub struct PermissionStore {
    conn: Mutex<Connection>,
}

impl PermissionStore {
    /// Open (or create) the permission store at the given path.
    ///
    /// If the file does not exist, it will be created with the schema.
    pub fn open(path: &Path) -> Result<Self, PermissionStoreError> {
        let conn = Connection::open(path)?;
        let store = Self { conn: Mutex::new(conn) };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory store (for testing).
    pub fn open_in_memory() -> Result<Self, PermissionStoreError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn: Mutex::new(conn) };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), PermissionStoreError> {
        self.conn.lock().unwrap().execute_batch(
            "CREATE TABLE IF NOT EXISTS permission_grants (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id      TEXT NOT NULL,
                perm_type     TEXT NOT NULL,
                perm_value    TEXT,
                authorized_by TEXT NOT NULL,
                granted_at    INTEGER NOT NULL,
                expires_at    INTEGER,
                scope         TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_grants_agent ON permission_grants(agent_id);",
        )?;
        Ok(())
    }

    /// Grant a permission to an agent. Returns the row id.
    pub fn grant(&self, g: &PermissionGrant) -> Result<i64, PermissionStoreError> {
        let (perm_type, perm_value) = serialize_permission(&g.permission);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO permission_grants (agent_id, perm_type, perm_value, authorized_by, granted_at, expires_at, scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![g.agent_id, perm_type, perm_value, g.authorized_by, g.granted_at, g.expires_at, g.scope],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Query all active (non-expired) grants for an agent.
    pub fn query_grants(&self, agent_id: &str) -> Result<Vec<PermissionGrant>, PermissionStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT agent_id, perm_type, perm_value, authorized_by, granted_at, expires_at, scope
             FROM permission_grants
             WHERE agent_id = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
        )?;
        let rows = stmt.query_map(params![agent_id, now], |row| {
            let perm_type: String = row.get(1)?;
            let perm_value: Option<String> = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?, // agent_id
                perm_type,
                perm_value,
                row.get::<_, String>(3)?, // authorized_by
                row.get::<_, i64>(4)?,    // granted_at
                row.get::<_, Option<i64>>(5)?, // expires_at
                row.get::<_, Option<String>>(6)?, // scope
            ))
        })?;
        let mut grants = Vec::new();
        for row in rows {
            let (agent_id, perm_type, perm_value, authorized_by, granted_at, expires_at, scope) = row?;
            let permission = deserialize_permission(&perm_type, perm_value.as_deref())?;
            grants.push(PermissionGrant {
                agent_id,
                permission,
                authorized_by,
                granted_at,
                expires_at,
                scope,
            });
        }
        Ok(grants)
    }

    /// Revoke all grants for an agent, or a specific permission.
    /// If `permission` is Some, only revoke grants matching that permission.
    /// If `permission` is None, revoke all grants for the agent.
    pub fn revoke(&self, agent_id: &str, permission: Option<&Permission>) -> Result<usize, PermissionStoreError> {
        let conn = self.conn.lock().unwrap();
        let affected = match permission {
            Some(perm) => {
                let (perm_type, perm_value) = serialize_permission(perm);
                conn.execute(
                    "DELETE FROM permission_grants WHERE agent_id = ?1 AND perm_type = ?2 AND ifnull(perm_value, '') = ifnull(?3, '')",
                    params![agent_id, perm_type, perm_value],
                )?
            }
            None => {
                conn.execute(
                    "DELETE FROM permission_grants WHERE agent_id = ?1",
                    params![agent_id],
                )?
            }
        };
        Ok(affected)
    }

    /// Reset all grants for an agent (revoke everything).
    pub fn reset(&self, agent_id: &str) -> Result<usize, PermissionStoreError> {
        self.revoke(agent_id, None)
    }

    /// Check if an agent has a specific permission granted (active only).
    pub fn has_permission(&self, agent_id: &str, requested: &Permission) -> Result<bool, PermissionStoreError> {
        let grants = self.query_grants(agent_id)?;
        Ok(grants.iter().any(|g| g.matches_request(requested)))
    }

    /// Clean up expired grants. Returns the number of removed rows.
    pub fn cleanup_expired(&self) -> Result<usize, PermissionStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let affected = self.conn.lock().unwrap().execute(
            "DELETE FROM permission_grants WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now],
        )?;
        Ok(affected)
    }
}

// ── Serialization helpers ─────────────────────────────────────────────

/// Serialize a Permission into (type, value) for DB storage.
fn serialize_permission(perm: &Permission) -> (String, Option<String>) {
    match perm {
        Permission::Network(v) => ("Network".into(), v.clone()),
        Permission::FilesystemRead(v) => ("FilesystemRead".into(), v.clone()),
        Permission::FilesystemWrite(v) => ("FilesystemWrite".into(), v.clone()),
        Permission::MemoryRead => ("MemoryRead".into(), None),
        Permission::MemoryWrite => ("MemoryWrite".into(), None),
        Permission::IntentSend(v) => ("IntentSend".into(), v.clone()),
        Permission::IntentReceive(v) => ("IntentReceive".into(), v.clone()),
        Permission::IdentityRead => ("IdentityRead".into(), None),
        Permission::IdentityWrite => ("IdentityWrite".into(), None),
        Permission::Shell => ("Shell".into(), None),
        Permission::Wasm => ("Wasm".into(), None),
    }
}

/// Deserialize a Permission from (type, value) stored in DB.
fn deserialize_permission(perm_type: &str, perm_value: Option<&str>) -> Result<Permission, PermissionStoreError> {
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

// ── Error type ────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PermissionStoreError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Invalid permission type: {0}")]
    InvalidPermissionType(String),
}

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
            Permission::Network(None), // broad: all network
            "user",
        )).unwrap();

        // Broad grant matches narrow request
        assert!(store.has_permission("com.example.agent", &Permission::Network(Some("https://api.weather.com".into()))).unwrap());
        // Broad grant matches broad request
        assert!(store.has_permission("com.example.agent", &Permission::Network(None)).unwrap());
        // Different type doesn't match
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
}
