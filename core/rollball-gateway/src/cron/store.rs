//! Cron entry persistence store (JSON file backend)
//!
//! Stores cron entries to survive Gateway restarts.
//! Data is stored as a JSON array of `StoredCronEntry` objects,
//! written atomically (write-to-temp + rename).
//!
//! Migration: on first open, if an old `cron_entries.db` SQLite file exists
//! but the JSON file does not, data is migrated automatically.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Persistent cron entry (S5.8 enhanced)
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Timezone for schedule interpretation (None = UTC)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Max retry count on failure (0 = no retry)
    #[serde(default)]
    pub retry_count: u32,
    /// Retry backoff interval in seconds
    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u64,
    /// Max total executions (None = unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u32>,
    /// Current execution count
    #[serde(default)]
    pub run_count: u32,
    /// Expiry timestamp in Unix millis (None = never expires)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

fn default_retry_interval() -> u64 {
    60
}

impl StoredCronEntry {
    /// Create a simple StoredCronEntry with defaults for optional S5.8 fields.
    pub fn simple(id: &str, agent_id: &str, schedule: &str, action: &str, params: &str) -> Self {
        Self {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params: params.to_string(),
            timezone: None,
            retry_count: 0,
            retry_interval_secs: 60,
            max_runs: None,
            run_count: 0,
            expires_at: None,
        }
    }
}

/// Error type for CronStore operations
#[derive(Debug, thiserror::Error)]
pub enum CronStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid cron entry: {0}")]
    InvalidEntry(String),
}

/// Persistent store for cron entries.
#[derive(Debug)]
pub struct CronStore {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    entries: Vec<StoredCronEntry>,
    /// None = in-memory (no file persistence)
    path: Option<PathBuf>,
}

impl CronStore {
    /// Open (or create) the cron store at the given path.
    ///
    /// If an old `cron_entries.db` SQLite file exists and the JSON file does not,
    /// data is migrated automatically and the old DB is renamed to `.db.bak`.
    pub fn open(path: &Path) -> Result<Self, CronStoreError> {
        let db_path = path.with_extension("db");
        let json_path = path.with_extension("json");
        if db_path.exists() && !json_path.exists() {
            if let Err(e) = Self::migrate_from_sqlite(&db_path, &json_path) {
                tracing::warn!(
                    "Failed to migrate cron store from {}: {}. Starting fresh.",
                    db_path.display(),
                    e
                );
            }
        }

        let entries = if json_path.exists() {
            let data = std::fs::read_to_string(&json_path).map_err(CronStoreError::Io)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Self {
            inner: Mutex::new(Inner {
                entries,
                path: Some(json_path),
            }),
        })
    }

    /// Open an in-memory store (for testing).
    pub fn open_in_memory() -> Result<Self, CronStoreError> {
        Ok(Self {
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                path: None,
            }),
        })
    }

    /// Check if the store is healthy.
    pub fn health_check(&self) -> Result<(), CronStoreError> {
        let inner = self.inner.lock().unwrap();
        if let Some(ref path) = inner.path {
            if path.exists() {
                let _ = std::fs::read_to_string(path).map_err(CronStoreError::Io)?;
            }
        }
        Ok(())
    }

    /// Insert a new cron entry
    pub fn insert(&self, entry: &StoredCronEntry) -> Result<(), CronStoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.push(entry.clone());
        self.save_locked(&inner)?;
        Ok(())
    }

    /// Delete a cron entry by ID
    pub fn delete(&self, cron_id: &str) -> Result<bool, CronStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.entries.len();
        inner.entries.retain(|e| e.id != cron_id);
        let removed = before != inner.entries.len();
        if removed {
            self.save_locked(&inner)?;
        }
        Ok(removed)
    }

    /// Delete all cron entries for an agent
    pub fn delete_by_agent(&self, agent_id: &str) -> Result<usize, CronStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.entries.len();
        inner.entries.retain(|e| e.agent_id != agent_id);
        let removed = before - inner.entries.len();
        if removed > 0 {
            self.save_locked(&inner)?;
        }
        Ok(removed)
    }

    /// List all cron entries for an agent
    pub fn list_by_agent(&self, agent_id: &str) -> Result<Vec<StoredCronEntry>, CronStoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .entries
            .iter()
            .filter(|e| e.agent_id == agent_id)
            .cloned()
            .collect())
    }

    /// List all cron entries
    pub fn list_all(&self) -> Result<Vec<StoredCronEntry>, CronStoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.entries.clone())
    }

    /// Get a single cron entry by ID
    pub fn get(&self, cron_id: &str) -> Result<Option<StoredCronEntry>, CronStoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.entries.iter().find(|e| e.id == cron_id).cloned())
    }

    // ── Internal ─────────────────────────────────────────────────────

    fn save_locked(&self, inner: &Inner) -> Result<(), CronStoreError> {
        if let Some(ref path) = inner.path {
            save_json_atomic(path, &inner.entries)?;
        }
        Ok(())
    }

    /// Migrate data from an old cron_entries.db SQLite file to JSON.
    fn migrate_from_sqlite(db_path: &Path, json_path: &Path) -> Result<(), CronStoreError> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| CronStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;

        // Check columns: v1 schema has 6 cols, v2 has 12
        let has_v2_cols: bool = conn
            .prepare("SELECT retry_count FROM cron_entries LIMIT 0")
            .is_ok();

        let entries: Vec<StoredCronEntry> = if has_v2_cols {
            Self::migrate_v2_entries(&conn)?
        } else {
            Self::migrate_v1_entries(&conn)?
        };

        save_json_atomic(json_path, &entries)?;

        let bak = db_path.with_extension("db.bak");
        let _ = std::fs::rename(db_path, &bak);

        tracing::info!(
            "Migrated {} cron entries from {} to {}",
            entries.len(),
            db_path.display(),
            json_path.display()
        );
        Ok(())
    }

    fn migrate_v1_entries(
        conn: &rusqlite::Connection,
    ) -> Result<Vec<StoredCronEntry>, CronStoreError> {
        let mut stmt = conn
            .prepare("SELECT id, agent_id, schedule, action, params FROM cron_entries")
            .map_err(|e| CronStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredCronEntry::simple(
                    &row.get::<_, String>(0)?,
                    &row.get::<_, String>(1)?,
                    &row.get::<_, String>(2)?,
                    &row.get::<_, String>(3)?,
                    &row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| CronStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn migrate_v2_entries(
        conn: &rusqlite::Connection,
    ) -> Result<Vec<StoredCronEntry>, CronStoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, agent_id, schedule, action, params,
                        timezone, retry_count, retry_interval_secs,
                        max_runs, run_count, expires_at
                 FROM cron_entries",
            )
            .map_err(|e| CronStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredCronEntry {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    schedule: row.get(2)?,
                    action: row.get(3)?,
                    params: row.get(4)?,
                    timezone: row.get(5)?,
                    retry_count: row.get(6)?,
                    retry_interval_secs: row.get(7)?,
                    max_runs: row.get(8)?,
                    run_count: row.get(9)?,
                    expires_at: row.get(10)?,
                })
            })
            .map_err(|e| CronStoreError::Io(io::Error::new(io::ErrorKind::Other, e)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

// ── Atomic JSON helpers ──────────────────────────────────────────────

fn save_json_atomic<T: Serialize>(path: &Path, data: &T) -> Result<(), CronStoreError> {
    let json = serde_json::to_string_pretty(data).map_err(CronStoreError::Json)?;
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, &json).map_err(CronStoreError::Io)?;
    std::fs::rename(&tmp_path, path).map_err(CronStoreError::Io)?;
    Ok(())
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

    fn make_entry(id: &str, agent_id: &str, schedule: &str, action: &str, params: &str) -> StoredCronEntry {
        StoredCronEntry::simple(id, agent_id, schedule, action, params)
    }

    #[test]
    fn test_cron_store_insert_and_list() {
        let store = CronStore::open_in_memory().unwrap();
        let entry = make_entry("cron-1", "com.example.weather", "0 * * * *", "hourly_check", "{}");
        store.insert(&entry).unwrap();

        let entries = store.list_by_agent("com.example.weather").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "cron-1");
        assert_eq!(entries[0].schedule, "0 * * * *");
    }

    #[test]
    fn test_cron_store_delete() {
        let store = CronStore::open_in_memory().unwrap();
        let entry = make_entry("cron-2", "com.example.weather", "0 9 * * *", "morning_report", "{}");
        store.insert(&entry).unwrap();
        assert!(store.delete("cron-2").unwrap());
        assert!(!store.delete("cron-2").unwrap());
    }

    #[test]
    fn test_cron_store_delete_by_agent() {
        let store = CronStore::open_in_memory().unwrap();
        for i in 1..=3 {
            let entry = make_entry(
                &format!("cron-{}", i),
                "com.example.weather",
                "0 * * * *",
                &format!("task-{}", i),
                "{}",
            );
            store.insert(&entry).unwrap();
        }
        let entry = make_entry("cron-4", "com.example.calendar", "0 0 * * *", "daily", "{}");
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
        let entry = make_entry("cron-10", "com.example.test", "*/15 * * * *", "health_check", r#"{"type":"ping"}"#);
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
            let entry = make_entry(
                &format!("cron-{}", i),
                &format!("com.example.agent{}", i % 2),
                "0 * * * *",
                &format!("task-{}", i),
                "{}",
            );
            store.insert(&entry).unwrap();
        }
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 5);
    }
}
