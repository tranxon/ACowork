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

use crate::types::{DirMeta, DocMeta, DocRead, TreeNode, UpdateRequest};

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

// ── Update-request bodies (design §5) ────────────────────────────────

/// `POST /api/requests` — agent submits an update proposal.
#[derive(Debug, Deserialize)]
pub struct SubmitRequestBody {
    /// Target document id.
    pub doc_id: String,
    /// Version the agent based its edit on (must match live version).
    pub base_version: u64,
    /// Proposed new full Markdown content.
    pub content: String,
    /// Submitter identity, e.g. `"agent:com.example.agent"`. The MCP
    /// layer (D3) will overwrite this from the authenticated agent; the
    /// REST shell accepts it for curl / integration tests.
    #[serde(default = "default_submitted_by")]
    pub submitted_by: String,
}

fn default_submitted_by() -> String {
    "human:desktop".to_string()
}

/// `POST /api/requests/:id/approve` / `reject` — human review.
#[derive(Debug, Deserialize, Default)]
pub struct ReviewBody {
    /// Reviewer identity (display name or `human:xxx`).
    #[serde(default = "default_reviewed_by")]
    pub reviewed_by: String,
    /// Optional review note stored on the request (design §5.3).
    #[serde(default)]
    pub note: Option<String>,
}

fn default_reviewed_by() -> String {
    "human:desktop".to_string()
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

/// Update request — full wire shape matches the on-disk model
/// (design §5.3), no transformation needed.
#[derive(Debug, Serialize)]
pub struct UpdateRequestDto(pub UpdateRequest);

impl From<UpdateRequest> for UpdateRequestDto {
    fn from(r: UpdateRequest) -> Self {
        Self(r)
    }
}

/// Approve response — reviewed request + the merged document version so
/// agents can refresh their cached copy immediately.
#[derive(Debug, Serialize)]
pub struct ApproveDto {
    pub request: UpdateRequest,
    pub doc_version: u64,
}

/// RFC 3339 timestamps are produced by serde's default DateTime
/// serialiser — no custom `serialize_with` needed.
pub type Timestamp = DateTime<Utc>;
