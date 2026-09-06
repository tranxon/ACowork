//! Atomic file I/O — the only module that performs `write + rename` on
//! `library.json` / `.requests/*.json` files.
//!
//! Strategy (design §3.3 "write: file lock + atomic replace"):
//! - Write to a sibling `*.tmp` first.
//! - `rename` is atomic on the same filesystem (POSIX guarantee, NTFS
//!   equivalent), so observers never see a half-written file.
//! - On crash *before* rename the original is intact; *after* rename the
//!   new content is fully visible. There is no torn-write state.
//!
//! Cross-filesystem fallback (rare on local install; possible on network
//! drives) uses `copy + remove`. Mirrors `acowork-pm` so both services
//! handle the same boundary the same way.
//!
//! Note: a process-level file lock is intentionally omitted. doc is a
//! single-instance service (design §3.3 "单机单实例"), and intra-process
//! multi-thread races are resolved at the service layer via the
//! per-directory call serialization in `service::document`. If a future
//! multi-instance deployment is needed, add `fs2` then (YAGNI today).

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{DocError, Result};

/// Atomically write a JSON-serialisable value to `path`.
///
/// Writes `path.with_extension("json.tmp")` first, then `rename`s onto
/// `path`. The rename is atomic on the same filesystem.
pub async fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, json.as_bytes()).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

/// Read and parse a JSON file at `path`.
///
/// Returns [`DocError::CorruptIndex`] if the file does not exist (caller
/// can decide whether to treat that as "empty library" or fatal).
pub async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Err(DocError::CorruptIndex(format!(
            "json not found: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).await?;
    let value = serde_json::from_slice(&bytes)?;
    Ok(value)
}

/// Atomic rename with cross-filesystem fallback.
///
/// `tokio::fs::rename` (and `std::fs::rename`) returns `EXDEV` /
/// `CrossesDevices` when source and target sit on different filesystems.
/// In that case we copy then remove — not atomic, but only triggered on
/// boundary mounts.
pub async fn rename_or_fallback(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(e)
            if e.raw_os_error() == Some(18 /* EXDEV */)
                || e.kind() == std::io::ErrorKind::CrossesDevices =>
        {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::copy(from, to).await?;
            fs::remove_dir_all(from).await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Append `.tmp` to a path (e.g. `library.json` → `library.json.tmp`).
///
/// Currently re-exposed only because callers building write paths prefer
/// to keep the `.tmp` suffix co-located with the atomic helper.
pub fn tmp_sibling(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Sample {
        n: i64,
        s: String,
    }

    #[tokio::test]
    async fn atomic_write_then_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("library.json");
        let v = Sample {
            n: 42,
            s: "hi".into(),
        };
        atomic_write_json(&p, &v).await.unwrap();
        let back: Sample = read_json(&p).await.unwrap();
        assert_eq!(v, back);
        // No .tmp file left behind.
        assert!(!tmp_sibling(&p).exists());
    }

    #[tokio::test]
    async fn atomic_write_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("library.json");
        atomic_write_json(&p, &Sample { n: 1, s: "a".into() })
            .await
            .unwrap();
        atomic_write_json(&p, &Sample { n: 2, s: "b".into() })
            .await
            .unwrap();
        let back: Sample = read_json(&p).await.unwrap();
        assert_eq!(back, Sample { n: 2, s: "b".into() });
    }

    #[tokio::test]
    async fn read_json_missing_returns_corrupt_index() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nope.json");
        let err = read_json::<Sample>(&p).await.unwrap_err();
        assert!(matches!(err, DocError::CorruptIndex(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn rename_or_fallback_same_filesystem() {
        let dir = TempDir::new().unwrap();
        let from = dir.path().join("a.json");
        let to = dir.path().join("b.json");
        fs::write(&from, b"x").await.unwrap();
        rename_or_fallback(&from, &to).await.unwrap();
        assert!(!from.exists());
        assert!(to.exists());
    }

    #[tokio::test]
    async fn tmp_sibling_appends_extension() {
        let p = std::path::PathBuf::from("/var/lib/acowork/library.json");
        assert_eq!(
            tmp_sibling(&p),
            std::path::PathBuf::from("/var/lib/acowork/library.json.tmp")
        );
    }
}
