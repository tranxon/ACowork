//! `RequestService` trait — PR-style update review flow (design §5).
//!
//! The collaboration rule: **agents never write directly**. An agent's
//! edit is submitted as an update request (`pending`), a human reviews
//! it via approve / reject, and only approve merges into the document
//! (version +1). This mirrors git: `base_version` is checked both at
//! submit time and again at approve time, so a request whose base was
//! pre-empted by a concurrent merge is refused (409) and the agent must
//! rebase on the new version (design §5.4).
//!
//! TTL expiry (`DocConfig::request_ttl_hours`, default 72h) is applied
//! lazily: any list / get / approve / reject call first sweeps stale
//! `pending` requests into `expired`.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{DocId, RequestStatus, UpdateRequest};

/// Input for submitting a new update request (agent path).
#[derive(Debug, Clone)]
pub struct SubmitRequestInput {
    /// Target document the agent wants to edit.
    pub doc_id: DocId,
    /// Version the agent based its edit on (`doc_pull` result). Must
    /// equal the document's current version or the submission is
    /// rejected with `DocError::VersionConflict` (design §5.2).
    pub base_version: u64,
    /// New full Markdown content proposed by the agent.
    pub content: String,
    /// Identity of the submitter, e.g. `"agent:com.example.agent"`
    /// (design §5.3). Populated by the MCP layer in D3; the REST shell
    /// falls back to a caller-supplied value for curl / tests.
    pub submitted_by: String,
}

/// Result of an approve: the reviewed request plus the document version
/// after the merge, so the caller can refresh its local copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveOutcome {
    pub request: UpdateRequest,
    pub doc_version: u64,
}

/// Service-trait contract for the review flow.
#[async_trait]
pub trait RequestService: Send + Sync {
    /// Submit a new `pending` update request for `doc_id`. Validates
    /// `base_version` against the document's current version; a stale
    /// base fails with `DocError::VersionConflict` and no request file
    /// is created (the agent must rebase first).
    async fn submit(&self, input: SubmitRequestInput) -> Result<UpdateRequest>;

    /// List update requests, optionally filtered by status. Stale
    /// pending requests are lazily marked `expired` before listing
    /// (design §5.4). Newest first.
    async fn list(&self, status: Option<RequestStatus>) -> Result<Vec<UpdateRequest>>;

    /// Read a single request by id (`doc_check_request`).
    async fn get(&self, request_id: &str) -> Result<UpdateRequest>;

    /// List the review history of one document (design §4
    /// `GET /api/docs/:id/requests`). Newest first.
    async fn list_for_doc(&self, doc_id: &str) -> Result<Vec<UpdateRequest>>;

    /// Review-approve a pending request: re-checks `base_version` against
    /// the live document (pre-empted bases → 409 `VersionConflict`, the
    /// request stays `pending`), then merges content and bumps the
    /// version to `base_version + 1`. Idempotence: reviewing an already
    /// reviewed request fails with `DocError::AlreadyReviewed`.
    async fn approve(
        &self,
        request_id: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<ApproveOutcome>;

    /// Review-reject a pending request. Keeps the submitted content and
    /// the review note on disk for the agent to inspect.
    async fn reject(
        &self,
        request_id: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<UpdateRequest>;
}

