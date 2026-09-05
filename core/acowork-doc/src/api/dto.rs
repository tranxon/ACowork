//! Wire DTOs — the on-the-wire shape of every REST body.
//!
//! Kept separate from `crate::types` (domain) so the persistence
//! schema and the HTTP schema can evolve independently. Validation
//! happens *before* the DTO reaches a service: empty strings / wrong
//! shapes get rejected by serde / hand-rolled checks here, not inside
//! the business layer.
//!
//! DateTime serialisation uses RFC 3339 (matches design §5.3 examples).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{DirMeta, DocMeta, DocRead, TreeNode};

// ── Request bodies ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateDocBody {
    /// Folder id the new doc lives under (design §4 POST /api/docs).
    pub parent_dir_id: String,
    /// Display title — also the filename stem.
    pub title: String,
    /// Initial Markdown content (may be empty).
    pub content: String,
    /// Optional: where this doc originated (manual / file / url / …).
    #[serde(default)]
    pub import: Option<crate::types::ImportSource>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDocBody {
    /// Last-known version for optimistic concurrency (design §5.4).
    pub base_version: u64,
    /// New title (must remain equal to the filename stem).
    #[serde(default)]
    pub title: Option<String>,
    /// New full Markdown content.
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct RenameDocBody {
    /// Last-known version (required — renaming changes the disk name).
    pub base_version: u64,
    pub new_title: String,
}

#[derive(Debug, Deserialize)]
pub struct MoveDocBody {
    pub target_dir_id: String,
    /// Whether to overwrite an existing doc with the same title (default false).
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateDirBody {
    pub parent_dir_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RenameDirBody {
    pub new_name: String,
}

// ── Response bodies ────────────────────────────────────────────────────

/// Document metadata only — no content.
#[derive(Debug, Serialize)]
pub struct DocMetaDto(pub DocMeta);

impl From<DocMeta> for DocMetaDto {
    fn from(m: DocMeta) -> Self {
        Self(m)
    }
}

/// Document metadata + content.
#[derive(Debug, Serialize)]
pub struct DocReadDto(pub DocRead);

impl From<DocRead> for DocReadDto {
    fn from(r: DocRead) -> Self {
        Self(r)
    }
}

#[derive(Debug, Serialize)]
pub struct DirMetaDto(pub DirMeta);

impl From<DirMeta> for DirMetaDto {
    fn from(m: DirMeta) -> Self {
        Self(m)
    }
}

#[derive(Debug, Serialize)]
pub struct TreeNodeDto(pub TreeNode);

impl From<TreeNode> for TreeNodeDto {
    fn from(t: TreeNode) -> Self {
        Self(t)
    }
}

/// RFC 3339 timestamps are produced by serde's default DateTime
/// serialiser — no custom `serialize_with` needed.
pub type Timestamp = DateTime<Utc>;
