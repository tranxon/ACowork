//! Cron entry persistence store (rusqlite backend)
//!
//! Stores cron entries to survive Gateway restarts.
//! Uses the same rusqlite approach as PermissionStore.
//!
//! Schema versioning:
//! - v1: Initial schema (cron_entries table)

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};

/// Current schema version
const SCHEMA_VERSION: u32 = 1;

/// Persistent cron entry stored in the database
#[derive(Debug, Clone)]
pub struct StoredCronEntry {
    /// Unique ID for this cron entry
    pub id: String,
    /// Agent that owns this cron entry
    pub agent_id: String,
    /// Cron schedule expression (5-field)
    pub schedule: String,
    /// Action to fire when the schedule triggers
    pub action: String,
    /// Params to include in the IntentReceived (JSON)
    pub params: String,
}

/// Error type for CronStore operations
#[derive(Debug, thiserror::Error)]
pub enum CronStoreError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid cron entry: {0}")]
    InvalidEntry(String),
}

/// Persistent store for cron entries.
///
/// Schema (v1):
/// ```sql
/// CREATE TABLE cron_entries (
///     id          TEXT PRIMARY KEY,
///     agent_id    TEXT NOT NULL,
///     schedule    TEXT NOT NULL,
///     action      TEXT NOT NULL,
///     params      TEXT NOT NULL DEFAULT '{}',
///     created_at  INTEGER NOT NULL
/// );
/// CREATE INDEX idx_cron_agent ON cron_entries(agent_id);
/// ```
#[derive(Debug)]
pub struct CronStore {
    conn: Mutex<Connection>,
}

impl CronStore {
    /// Open (or create) the cron store at the given path.
    pub fn open(path: &Path) -> Result<Self, CronStoreError> {
        let conn = Connection::open(path)?;
        let store = Self { conn: Mutex::new(conn) };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory store (for testing).
    pub fn open_in_memory() -> Result<Self, CronStoreError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn: Mutex::new(conn) };
        store.init_schema()?;
        Ok(store)
    }

    /// Check if the database connection is alive.
    /// Performs a lightweight `SELECT 1` query.
    pub fn health_check(&self) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT 1", [], |_row| Ok(()))?;
        Ok(())
    }

    fn init_schema(&self) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().unwrap();

        // Create schema_version table if not exists
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );"
        )?;

        // Check current version
        let current_version: Option<u32> = conn
            .query_row(
                "SELECT version FROM schema_version LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();

        match current_version {
            None => {
                // Fresh database — create v1 schema
                conn.execute_batch(
                    "CREATE TABLE cron_entries (
                        id          TEXT PRIMARY KEY,
                        agent_id    TEXT NOT NULL,
                        schedule    TEXT NOT NULL,
                        action      TEXT NOT NULL,
                        params      TEXT NOT NULL DEFAULT '{}',
                        created_at  INTEGER NOT NULL
                    );
                    CREATE INDEX idx_cron_agent ON cron_entries(agent_id);

                    INSERT INTO schema_version (version) VALUES (1);"
                )?;
            }
            Some(v) if v < SCHEMA_VERSION => {
                // Run migrations from v to SCHEMA_VERSION
                self.run_migrations(&conn, v)?;
            }
            Some(_) => {
                // Already at latest version
            }
        }

        Ok(())
    }

    fn run_migrations(
        &self,
        _conn: &Connection,
        from: u32,
    ) -> Result<(), CronStoreError> {
        // Future migrations go here
        let _ = from;
        Ok(())
    }

    /// Insert a new cron entry
    pub fn insert(&self, entry: &StoredCronEntry) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO cron_entries (id, agent_id, schedule, action, params, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![entry.id, entry.agent_id, entry.schedule, entry.action, entry.params, now],
        )?;
        Ok(())
    }

    /// Delete a cron entry by ID
    pub fn delete(&self, cron_id: &str) -> Result<bool, CronStoreError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "DELETE FROM cron_entries WHERE id = ?1",
            params![cron_id],
        )?;
        Ok(rows > 0)
    }

    /// Delete all cron entries for an agent
    pub fn delete_by_agent(&self, agent_id: &str) -> Result<usize, CronStoreError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "DELETE FROM cron_entries WHERE agent_id = ?1",
            params![agent_id],
        )?;
        Ok(rows)
    }

    /// List all cron entries for an agent
    pub fn list_by_agent(&self, agent_id: &str) -> Result<Vec<StoredCronEntry>, CronStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, schedule, action, params FROM cron_entries WHERE agent_id = ?1"
        )?;
        let entries = stmt.query_map(params![agent_id], |row| {
            Ok(StoredCronEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                schedule: row.get(2)?,
                action: row.get(3)?,
                params: row.get(4)?,
            })
        })?.filter_map(|e| e.ok()).collect();
        Ok(entries)
    }

    /// List all cron entries
    pub fn list_all(&self) -> Result<Vec<StoredCronEntry>, CronStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, schedule, action, params FROM cron_entries"
        )?;
        let entries = stmt.query_map([], |row| {
            Ok(StoredCronEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                schedule: row.get(2)?,
                action: row.get(3)?,
                params: row.get(4)?,
            })
        })?.filter_map(|e| e.ok()).collect();
        Ok(entries)
    }

    /// Get a single cron entry by ID
    pub fn get(&self, cron_id: &str) -> Result<Option<StoredCronEntry>, CronStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, schedule, action, params FROM cron_entries WHERE id = ?1"
        )?;
        let result = stmt.query_row(params![cron_id], |row| {
            Ok(StoredCronEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                schedule: row.get(2)?,
                action: row.get(3)?,
                params: row.get(4)?,
            })
        }).ok();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_store_open_in_memory() {
        let store = CronStore::open_in_memory().unwrap();
        let entries = store.list_all().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_cron_store_insert_and_list() {
        let store = CronStore::open_in_memory().unwrap();
        let entry = StoredCronEntry {
            id: "cron-1".to_string(),
            agent_id: "com.example.weather".to_string(),
            schedule: "0 * * * *".to_string(),
            action: "hourly_check".to_string(),
            params: "{}".to_string(),
        };
        store.insert(&entry).unwrap();

        let entries = store.list_by_agent("com.example.weather").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "cron-1");
        assert_eq!(entries[0].schedule, "0 * * * *");
    }

    #[test]
    fn test_cron_store_delete() {
        let store = CronStore::open_in_memory().unwrap();
        let entry = StoredCronEntry {
            id: "cron-2".to_string(),
            agent_id: "com.example.weather".to_string(),
            schedule: "0 9 * * *".to_string(),
            action: "morning_report".to_string(),
            params: "{}".to_string(),
        };
        store.insert(&entry).unwrap();
        assert!(store.delete("cron-2").unwrap());
        assert!(!store.delete("cron-2").unwrap()); // Already deleted
    }

    #[test]
    fn test_cron_store_delete_by_agent() {
        let store = CronStore::open_in_memory().unwrap();
        for i in 1..=3 {
            let entry = StoredCronEntry {
                id: format!("cron-{}", i),
                agent_id: "com.example.weather".to_string(),
                schedule: "0 * * * *".to_string(),
                action: format!("task-{}", i),
                params: "{}".to_string(),
            };
            store.insert(&entry).unwrap();
        }
        let entry = StoredCronEntry {
            id: "cron-4".to_string(),
            agent_id: "com.example.calendar".to_string(),
            schedule: "0 0 * * *".to_string(),
            action: "daily".to_string(),
            params: "{}".to_string(),
        };
        store.insert(&entry).unwrap();

        let count = store.delete_by_agent("com.example.weather").unwrap();
        assert_eq!(count, 3);

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].agent_id, "com.example.calendar");
    }

    #[test]
    fn test_cron_store_get() {
        let store = CronStore::open_in_memory().unwrap();
        let entry = StoredCronEntry {
            id: "cron-10".to_string(),
            agent_id: "com.example.test".to_string(),
            schedule: "*/15 * * * *".to_string(),
            action: "health_check".to_string(),
            params: r#"{"type":"ping"}"#.to_string(),
        };
        store.insert(&entry).unwrap();

        let got = store.get("cron-10").unwrap().unwrap();
        assert_eq!(got.schedule, "*/15 * * * *");
        assert_eq!(got.params, r#"{"type":"ping"}"#);

        assert!(store.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_cron_store_list_all() {
        let store = CronStore::open_in_memory().unwrap();
        for i in 1..=5 {
            let entry = StoredCronEntry {
                id: format!("cron-{}", i),
                agent_id: format!("com.example.agent{}", i % 2),
                schedule: "0 * * * *".to_string(),
                action: format!("task-{}", i),
                params: "{}".to_string(),
            };
            store.insert(&entry).unwrap();
        }
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 5);
    }
}
