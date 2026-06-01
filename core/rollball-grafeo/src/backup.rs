//! Backup and recovery for GrafeoStore.
//!
//! Provides file-level directory copy backup, restore, retention cleanup,
//! and WAL recovery verification.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{GrafeoError, Result};
use crate::grafeo::GrafeoStore;

const BACKUP_METADATA_FILENAME: &str = ".backup_metadata.json";

/// Backup configuration.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Directory for backup files.
    pub backup_dir: PathBuf,
    /// Keep N daily backups.
    pub daily_retention: usize,
    /// Keep N weekly backups.
    pub weekly_retention: usize,
    /// Whether automatic backup is enabled.
    pub auto_backup_enabled: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            backup_dir: PathBuf::from("backups"),
            daily_retention: 7,
            weekly_retention: 4,
            auto_backup_enabled: true,
        }
    }
}

/// Backup type classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackupType {
    /// Automatically created daily backup.
    Daily,
    /// Automatically created weekly backup.
    Weekly,
    /// Manually triggered backup.
    Manual,
}

impl BackupType {
    fn as_str(&self) -> &'static str {
        match self {
            BackupType::Daily => "daily",
            BackupType::Weekly => "weekly",
            BackupType::Manual => "manual",
        }
    }
}

/// Backup metadata stored alongside each backup directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupMetadata {
    /// Unique backup identifier.
    pub backup_id: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Path to the original database.
    pub db_path: String,
    /// Number of nodes at backup time.
    pub node_count: usize,
    /// Total size of the backup in bytes.
    pub size_bytes: u64,
    /// Classification of this backup.
    pub backup_type: BackupType,
}

impl GrafeoStore {
    /// Create a backup of the current database.
    ///
    /// Uses file-level copy of the database directory. The database is
    /// checkpointed before copying to ensure consistency.
    ///
    /// # Errors
    ///
    /// Returns an error if the database is in-memory, checkpoint fails,
    /// or the file copy fails.
    pub fn create_backup(&self, config: &BackupConfig, backup_type: BackupType) -> Result<BackupMetadata> {
        let db_path = self.db.path().ok_or_else(|| {
            GrafeoError::Memory("Cannot backup in-memory database".to_string())
        })?;

        // Checkpoint to ensure on-disk consistency before copying.
        self.db.wal_checkpoint()?;

        let created_at = Utc::now();
        let backup_id = format!(
            "{}_{}",
            created_at.format("%Y-%m-%dT%H-%M-%S%.6fZ"),
            backup_type.as_str()
        );
        let backup_path = config.backup_dir.join(&backup_id);

        fs::create_dir_all(&backup_path)?;
        copy_dir_all(db_path, &backup_path)?;

        let node_count = self.db.node_count();
        let size_bytes = calculate_dir_size(&backup_path)?;

        let metadata = BackupMetadata {
            backup_id: backup_id.clone(),
            created_at,
            db_path: db_path.to_string_lossy().to_string(),
            node_count,
            size_bytes,
            backup_type,
        };

        let metadata_path = backup_path.join(BACKUP_METADATA_FILENAME);
        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        fs::write(metadata_path, metadata_json)?;

        Ok(metadata)
    }

    /// List available backups.
    ///
    /// Reads all backup metadata files in the configured backup directory
    /// and returns them sorted by creation time (newest first).
    ///
    /// # Errors
    ///
    /// Returns an error if the backup directory cannot be read.
    pub fn list_backups(&self, config: &BackupConfig) -> Result<Vec<BackupMetadata>> {
        let mut backups = Vec::new();

        if !config.backup_dir.exists() {
            return Ok(backups);
        }

        for entry in fs::read_dir(&config.backup_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let metadata_path = path.join(BACKUP_METADATA_FILENAME);
            if metadata_path.exists() {
                let content = fs::read_to_string(&metadata_path)?;
                let metadata: BackupMetadata = serde_json::from_str(&content)?;
                backups.push(metadata);
            }
        }

        backups.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(backups)
    }

    /// Restore from a specific backup.
    ///
    /// Closes the current database, removes the current database directory,
    /// and copies the backup files to the original database path.
    ///
    /// **Note**: After restore, the current `GrafeoStore` is closed and
    /// should be dropped. Create a new `GrafeoStore` at the same path to
    /// use the restored database.
    ///
    /// # Errors
    ///
    /// Returns an error if the backup is not found, the database is
    /// in-memory, or the file copy fails.
    pub fn restore_from_backup(&self, config: &BackupConfig, backup_id: &str) -> Result<()> {
        let backup_path = config.backup_dir.join(backup_id);
        if !backup_path.exists() {
            return Err(GrafeoError::Memory(format!(
                "Backup not found: {}",
                backup_id
            )));
        }

        let db_path = self.db.path().ok_or_else(|| {
            GrafeoError::Memory("Cannot restore in-memory database".to_string())
        })?;

        // Close the database to release file handles.
        self.db.close()?;

        // Remove current database directory.
        if db_path.exists() {
            fs::remove_dir_all(db_path)?;
        }

        // Recreate database directory and copy backup contents.
        fs::create_dir_all(db_path)?;
        for entry in fs::read_dir(&backup_path)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if file_name == BACKUP_METADATA_FILENAME {
                continue;
            }

            let src = entry.path();
            let dst = db_path.join(&file_name);
            if src.is_dir() {
                copy_dir_all(&src, &dst)?;
            } else {
                fs::copy(&src, &dst)?;
            }
        }

        Ok(())
    }

    /// Clean up old backups according to retention policy.
    ///
    /// Retains the most recent `daily_retention` daily backups and
    /// `weekly_retention` weekly backups. Manual backups are never removed.
    ///
    /// Returns the number of backups removed.
    ///
    /// # Errors
    ///
    /// Returns an error if the backup directory cannot be read or a backup
    /// cannot be removed.
    pub fn cleanup_old_backups(&self, config: &BackupConfig) -> Result<usize> {
        let backups = self.list_backups(config)?;
        let mut removed = 0usize;

        // Collect backups by type, already sorted newest-first.
        let daily: Vec<_> = backups
            .iter()
            .filter(|b| matches!(b.backup_type, BackupType::Daily))
            .collect();
        let weekly: Vec<_> = backups
            .iter()
            .filter(|b| matches!(b.backup_type, BackupType::Weekly))
            .collect();

        // Remove excess daily backups.
        if daily.len() > config.daily_retention {
            for backup in daily.iter().skip(config.daily_retention) {
                let path = config.backup_dir.join(&backup.backup_id);
                if path.exists() {
                    fs::remove_dir_all(&path)?;
                    removed += 1;
                }
            }
        }

        // Remove excess weekly backups.
        if weekly.len() > config.weekly_retention {
            for backup in weekly.iter().skip(config.weekly_retention) {
                let path = config.backup_dir.join(&backup.backup_id);
                if path.exists() {
                    fs::remove_dir_all(&path)?;
                    removed += 1;
                }
            }
        }

        Ok(removed)
    }

    /// Attempt automatic WAL recovery after unexpected shutdown.
    ///
    /// GrafeoDB handles WAL replay internally on `open()`; this method
    /// validates that the database is in a healthy state by running a
    /// simple diagnostic query.
    ///
    /// Returns `true` if the database is healthy, `false` if validation
    /// fails. For in-memory databases, always returns `true`.
    ///
    /// # Errors
    ///
    /// Returns an error only for I/O failures during validation.
    pub fn verify_wal_recovery(&self) -> Result<bool> {
        if !self.db.is_persistent() {
            return Ok(true);
        }

        let session = self.db.session();
        match session.execute("MATCH (n) RETURN count(n) AS cnt") {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

/// Recursively copy a directory tree.
fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.as_ref().join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_all(entry.path(), dst_path)?;
        } else {
            fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

/// Calculate the total size of all files in a directory tree.
fn calculate_dir_size(path: impl AsRef<Path>) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total += metadata.len();
        } else if metadata.is_dir() {
            total += calculate_dir_size(entry.path())?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_store(db_dir: &Path) -> GrafeoStore {
        GrafeoStore::open_with_default_config(db_dir).unwrap()
    }

    #[test]
    fn test_create_backup() {
        let db_dir = TempDir::new().unwrap();
        let backup_dir = TempDir::new().unwrap();
        let store = create_test_store(db_dir.path());

        // Add some data via direct API.
        store.db.create_node(&["Test"]);

        let config = BackupConfig {
            backup_dir: backup_dir.path().to_path_buf(),
            daily_retention: 7,
            weekly_retention: 4,
            auto_backup_enabled: true,
        };

        let metadata = store.create_backup(&config, BackupType::Daily).unwrap();

        assert!(metadata.backup_id.contains("daily"));
        assert_eq!(metadata.db_path, db_dir.path().to_string_lossy());
        assert!(metadata.node_count >= 1);
        assert!(metadata.size_bytes > 0);

        let backup_path = backup_dir.path().join(&metadata.backup_id);
        assert!(backup_path.exists());
        assert!(backup_path.join(BACKUP_METADATA_FILENAME).exists());
    }

    #[test]
    fn test_list_backups() {
        let db_dir = TempDir::new().unwrap();
        let backup_dir = TempDir::new().unwrap();
        let store = create_test_store(db_dir.path());

        let config = BackupConfig {
            backup_dir: backup_dir.path().to_path_buf(),
            daily_retention: 7,
            weekly_retention: 4,
            auto_backup_enabled: true,
        };

        // Create multiple backups.
        let meta1 = store.create_backup(&config, BackupType::Daily).unwrap();
        let meta2 = store.create_backup(&config, BackupType::Manual).unwrap();

        let backups = store.list_backups(&config).unwrap();
        assert_eq!(backups.len(), 2);
        // Newest first.
        assert_eq!(backups[0].backup_id, meta2.backup_id);
        assert_eq!(backups[1].backup_id, meta1.backup_id);
    }

    #[test]
    fn test_cleanup_old_backups() {
        let db_dir = TempDir::new().unwrap();
        let backup_dir = TempDir::new().unwrap();
        let store = create_test_store(db_dir.path());

        let config = BackupConfig {
            backup_dir: backup_dir.path().to_path_buf(),
            daily_retention: 2,
            weekly_retention: 1,
            auto_backup_enabled: true,
        };

        // Create 3 daily backups and 2 weekly backups.
        let _d1 = store.create_backup(&config, BackupType::Daily).unwrap();
        let _d2 = store.create_backup(&config, BackupType::Daily).unwrap();
        let _d3 = store.create_backup(&config, BackupType::Daily).unwrap();
        let _w1 = store.create_backup(&config, BackupType::Weekly).unwrap();
        let _w2 = store.create_backup(&config, BackupType::Weekly).unwrap();

        let removed = store.cleanup_old_backups(&config).unwrap();
        assert_eq!(removed, 2); // 1 daily + 1 weekly

        let remaining = store.list_backups(&config).unwrap();
        assert_eq!(remaining.len(), 3); // 2 daily + 1 weekly

        let daily_count = remaining
            .iter()
            .filter(|b| matches!(b.backup_type, BackupType::Daily))
            .count();
        let weekly_count = remaining
            .iter()
            .filter(|b| matches!(b.backup_type, BackupType::Weekly))
            .count();
        assert_eq!(daily_count, 2);
        assert_eq!(weekly_count, 1);
    }
}
