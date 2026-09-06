//! `.requests/` storage — one JSON file per update request.
//!
//! Layout (design §5.3):
//! ```text
//! .requests/
//!   r-{12hex}.json     <- `UpdateRequest` verbatim (single source of truth)
//! ```
//!
//! State transitions (pending → approved / rejected / expired) rewrite the
//! file atomically via [`atomic_write_json`], so a torn write never leaves
//! a half-reviewed request. The store is pure I/O — review semantics
//! (base_version pre-emption, TTL expiry) live in `service::request_impl`.

use std::path::PathBuf;

use tokio::fs;

use crate::error::{DocError, Result};
use crate::path::validate_request_id;
use crate::store::atomic::{atomic_write_json, read_json};
use crate::types::UpdateRequest;

/// Owns the `.requests/` directory inside a library root.
pub struct RequestsStore {
    root: PathBuf,
}

impl RequestsStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Absolute path of `.requests/` (sibling of `.trash/`).
    pub fn dir(&self) -> PathBuf {
        self.root.join(".requests")
    }

    /// Ensure `.requests/` exists (idempotent — safe on every startup).
    pub async fn ensure(&self) -> Result<()> {
        fs::create_dir_all(self.dir()).await?;
        Ok(())
    }

    /// Resolve a `request_id` to its JSON file, enforcing the id whitelist
    /// so a caller-supplied id cannot escape the directory.
    fn path_for(&self, request_id: &str) -> Result<PathBuf> {
        validate_request_id(request_id)?;
        Ok(self.dir().join(format!("{}.json", request_id)))
    }

    /// Atomically persist a request (create or state transition).
    pub async fn write(&self, req: &UpdateRequest) -> Result<()> {
        self.ensure().await?;
        let p = self.path_for(&req.request_id)?;
        atomic_write_json(&p, req).await
    }

    /// Load one request by id. A missing file maps to
    /// `DocError::RequestNotFound`; an unparseable file surfaces as
    /// `CorruptIndex` (disk state is broken — do not hide it).
    pub async fn read(&self, request_id: &str) -> Result<UpdateRequest> {
        let p = self.path_for(request_id)?;
        if !p.exists() {
            return Err(DocError::RequestNotFound(request_id.to_string()));
        }
        read_json(&p).await
    }

    /// Read every request on disk, newest first. Corrupt / half-written
    /// files are skipped defensively (same policy as `.trash/`).
    pub async fn list(&self) -> Result<Vec<UpdateRequest>> {
        self.ensure().await?;
        let mut out = Vec::new();
        let mut entries = fs::read_dir(self.dir()).await?;
        while let Some(e) = entries.next_entry().await? {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") {
                continue;
            }
            if let Ok(req) = read_json::<UpdateRequest>(&e.path()).await {
                out.push(req);
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(out)
    }

    /// Delete a request file (used by tests / future admin cleanup).
    pub async fn delete(&self, request_id: &str) -> Result<()> {
        let p = self.path_for(request_id)?;
        match fs::remove_file(&p).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(DocError::RequestNotFound(request_id.to_string()))
            }
            Err(e) => Err(DocError::Io(e)),
        }
    }
}

