//! MemoryAdminService trait - administrative interface for memory engines.
//!
//! This trait provides management/admin operations used by HTTP admin
//! endpoints (`/memory/nodes`, `/memory/stats`, `/memory/consolidate`)
//! and embedding-dimension migration. It is intentionally separate from
//! [`crate::MemoryProvider`] (which serves the agent loop) to follow
//! Interface Segregation:
//!
//! - `MemoryProvider`: retrieve, inject, record, consolidation lifecycle
//!   — used by `loop_memory.rs`, `MemoryManager`, tools.
//! - `MemoryAdminService`: list/get/create/update/delete nodes, stats,
//!   consolidation trigger, embedding migration — used by HTTP admin
//!   endpoints and session bootstrap.
//!
//! A concrete engine (e.g., `GrafeoStore`) typically implements both
//! traits. A remote/test provider may implement only `MemoryProvider`
//! and skip `MemoryAdminService` (admin endpoints will report
//! "unavailable").
//!
//! Design ref: ADR-051 §4.4.1

use std::collections::HashMap;

use acowork_core::error::Result;

// ── Admin types (engine-independent) ──────────────────────────────────

/// One row in a memory list response.
///
/// Mirrors the wire-format contract consumed by the Desktop Memory panel.
/// Fields are engine-agnostic: no grafeo types leak through.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminNodeRecord {
    pub node_id: u64,
    pub node_type: String,
    pub content: String,
    pub confidence: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
}

/// Parameters for [`MemoryAdminService::list_nodes`].
#[derive(Debug, Clone, Default)]
pub struct AdminListNodesParams {
    /// 1-based page number.
    pub page: u32,
    /// Page size (clamped to 1..=100 by the implementation).
    pub size: u32,
    /// Filter by node type ("Episodic" / "Knowledge" / "Procedural" /
    /// "Autobiographical"). Empty string = no filter.
    pub node_type: String,
    /// Case-insensitive substring filter applied to the rendered content.
    /// Empty string = no filter.
    pub keyword: String,
    /// Time-range bucket. Supported values: "1h", "1d", "7d", "30d",
    /// "all", "". Empty string = no filter.
    pub time_range: String,
}

/// Result of a list-nodes query.
#[derive(Debug, Clone)]
pub struct AdminListNodesOutput {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub nodes: Vec<AdminNodeRecord>,
    /// When `Some`, the unfiltered scan was rejected because the database
    /// exceeds the implementation's safety limit.
    pub rejected_unfiltered: Option<u64>,
}

/// Result of a single-node GET query.
///
/// `found == false` with `message == "Memory store not available"`
/// indicates the engine has not been initialised yet.
#[derive(Debug, Clone)]
pub struct AdminNodeDetail {
    pub node_id: u64,
    pub found: bool,
    pub node_type: String,
    pub content: String,
    pub confidence: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
    /// All node properties, serialised as JSON values. Empty when
    /// `found == false`.
    pub properties: HashMap<String, serde_json::Value>,
    pub message: String,
}

/// Detailed memory statistics for the admin UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminStats {
    pub total_nodes: u64,
    pub storage_bytes: u64,
    pub by_type: HashMap<String, u64>,
    pub by_status: HashMap<String, u64>,
    pub avg_decay_score: f64,
    pub index_health: String,
    /// Embedding dimension stored in the engine's vector indexes.
    pub stored_dim: u64,
    /// Number of nodes that have an embedding vector.
    pub nodes_with_embedding: u64,
}

/// Result of a consolidation trigger.
#[derive(Debug, Clone)]
pub struct AdminConsolidateResult {
    /// Number of episodes consolidated into knowledge nodes.
    pub episodes_consolidated: u64,
}

/// Statistics returned by embedding dimension migration.
///
/// Mirrors `GrafeoStore::RebuildStats` but without engine-specific types.
#[derive(Debug, Clone, Default)]
pub struct RebuildStats {
    pub total_scanned: u64,
    pub rebuilt: u64,
    pub skipped_no_embedding: u64,
    pub skipped_no_content: u64,
    pub errors: u64,
}

// ── Trait definition ──────────────────────────────────────────────────

/// Administrative interface for memory storage engines.
///
/// All methods are synchronous because they perform in-process graph
/// operations without I/O. Implementations that need async (e.g., a
/// remote HTTP backend) can bridge internally.
///
/// Design ref: ADR-051 §4.4.1
pub trait MemoryAdminService: Send + Sync {
    // ── Node CRUD ─────────────────────────────────────────────────────

    /// List memory nodes with pagination, filtering, and search.
    fn list_nodes(&self, params: &AdminListNodesParams) -> AdminListNodesOutput;

    /// Get a single node's full detail by numeric ID.
    fn get_node(&self, node_id: u64) -> AdminNodeDetail;

    /// Create a new memory node with the given label and property map.
    ///
    /// Returns the new `node_id`.
    fn create_node(
        &self,
        label: &str,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<u64>;

    /// Update (merge) properties on an existing node.
    ///
    /// Returns `Err` if the node does not exist (so the HTTP layer can
    /// map it to a 404).
    fn update_node(
        &self,
        node_id: u64,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<()>;

    /// Delete a memory node by ID. Returns `true` if found and deleted.
    fn delete_node(&self, node_id: u64) -> bool;

    // ── Statistics ────────────────────────────────────────────────────

    /// Collect detailed memory statistics for the admin UI.
    fn get_stats(&self) -> AdminStats;

    // ── Consolidation ────────────────────────────────────────────────

    /// Trigger offline memory consolidation.
    ///
    /// `force = true` short-circuits the `min_pending_age_hours` guard.
    fn consolidate(&self, force: bool) -> AdminConsolidateResult;

    // ── Embedding migration ──────────────────────────────────────────

    /// Get the embedding dimension stored in the engine's vector indexes.
    fn embedding_dim(&self) -> usize;

    /// Count nodes that have an embedding vector.
    fn count_nodes_with_embedding(&self) -> u64;

    /// Re-embed all nodes with a new embedding function and dimension.
    ///
    /// `embed_fn` is a synchronous closure that takes a text string and
    /// returns an embedding vector of length `new_dim`. Callers are
    /// responsible for bridging async providers.
    fn migrate_embedding_dimension(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
    ) -> Result<RebuildStats>;
}
