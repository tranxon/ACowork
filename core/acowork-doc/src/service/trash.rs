//! `TrashService` trait — recycle-bin listing, restore, and purge.
//!
//! Restoring is a *semantic* operation, not a raw file move: we read the
//! slot content, create a fresh document via `DocumentService` in the
//! original directory (mint a new `doc_id`), and then purge the slot. If
//! the original directory no longer exists (or a same-name doc occupies
//! it), the call fails with the underlying `DocError` and the slot is
//! left untouched — the caller can still purge it explicitly.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{DocMeta, TrashEntry};

#[async_trait]
pub trait TrashService: Send + Sync {
    /// List every trash slot, newest first. A lazy 30-day purge runs
    /// before returning (design §3.3).
    async fn list(&self) -> Result<Vec<TrashEntry>>;

    /// Restore a slot: read content, re-create the doc in its original
    /// directory, and remove the slot. Returns the restored document.
    async fn restore(&self, trash_id: &str) -> Result<DocMeta>;

    /// Permanently delete a slot (meta + content) without restoring.
    async fn purge(&self, trash_id: &str) -> Result<()>;

    /// Permanently purge every slot older than the retention window.
    /// Returns the number of slots removed. Callers run this on startup
    /// and lazily before every `list()` / `restore()`.
    async fn purge_expired(&self) -> Result<usize>;
}
