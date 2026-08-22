//! Memory query use case.
//!
//! ADR-040: wraps Grafeo memory operations behind a trait so the HTTP
//! server does not directly depend on the memory store type.
//!
//! The types exposed here are the **single source of truth** for the
//! memory-query wire format (HTTP JSON shape documented in
//! `docs/zh/protocols/http.md` §7.7 + the canonical `MemoryStatsResponse`
//! shape mirrored in `acowork-gateway::http::memory_api`). They are
//! `Serialize`/`Deserialize` so the HTTP handlers can ferry them
//! through `serde_json::to_value` directly — no separate JSON-shaping
//! layer that could drift from the public contract.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Parameters for listing memory nodes (paginated, filtered).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNodeQuery {
    pub page: u32,
    pub size: u32,
    pub node_type: String,
    /// Sub-classification filter (Knowledge / Autobiographical only).
    /// See [`crate::http::memory_query::ListNodesParams::sub_type`].
    #[serde(default)]
    pub sub_type: String,
    pub keyword: String,
    pub time_range: String,
}

/// Lightweight memory node for list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub node_id: u64,
    pub node_type: String,
    /// Secondary classification (see [`AdminNodeRecord::sub_type`]).
    /// `None` for Episodic/Procedural nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<String>,
    pub content: String,
    pub confidence: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
}

/// Response for listing memory nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNodeListResponse {
    pub nodes: Vec<MemoryNode>,
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub model_dim: u64,
}

/// Memory store statistics.
///
/// This struct is the single source of truth for the `GET /memory/stats`
/// wire format. The HTTP handler serializes it directly with
/// `serde_json::to_value`; the service-layer trait returns it directly;
/// the internal `http::memory_query::get_stats` business logic also
/// returns it (so the pre-ADR-040 fallback path produces byte-identical
/// JSON to the ADR-040 service path). Field names + types mirror the
/// canonical `MemoryStatsResponse` defined in
/// `acowork-gateway::http::memory_api`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    /// Total number of memory nodes across all labels.
    pub total_nodes: u64,
    /// On-disk storage size in bytes. Reserved for future use; currently 0.
    pub storage_bytes: u64,
    /// Node count grouped by memory label (`Knowledge` / `Episodic` / etc.).
    pub by_type: HashMap<String, u64>,
    /// Node count grouped by status (`Active` / `Dormant` / `purged`).
    /// The desktop Memory panel reads `by_status["Active"]` /
    /// `by_status["Dormant"]` to drive its status cards.
    pub by_status: HashMap<String, u64>,
    /// Mean decay score across all sampled nodes (0.0..=1.0).
    pub avg_decay_score: f64,
    /// Vector-index health string (`healthy` / `no_store` / `error: …`).
    pub index_health: String,
    /// Embedding dimension of the persisted HNSW index. 0 if no index.
    /// Compared against `model_dim` by the desktop to detect dimension
    /// mismatches and surface the "Rebuild Index" banner.
    pub stored_dim: u64,
    /// Number of nodes with a non-NULL `embedding` field. Compared against
    /// `total_nodes` to detect missing embeddings.
    pub nodes_with_embedding: u64,
    /// Embedding dimension of the active embedding model (0 if none).
    pub model_dim: u64,
}

/// Result of a consolidation run.
///
/// This is the wire-format struct serialized by the HTTP handler and
/// consumed by the Desktop Memory panel (`ConsolidateResponse` in
/// `apps/acowork-desktop/src/lib/types.ts`). Field names MUST stay
/// in sync with the frontend TypeScript type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    /// Whether consolidation actually started (false if store unavailable).
    pub started: bool,
    /// Wall-clock duration of the consolidation run in milliseconds.
    pub duration_ms: u64,
    /// Number of pending knowledge nodes processed (upgraded + kept + dormant).
    pub episodes_consolidated: u64,
    /// Number of new knowledge nodes created (triples extracted + procedural).
    pub knowledge_nodes_generated: u64,
    /// Human-readable summary message for the UI.
    pub message: String,
}

/// Memory query methods.
#[async_trait]
pub trait MemoryQueryService: Send + Sync {
    /// List nodes with pagination, type/keyword/time-range filters.
    async fn list_nodes(&self, query: &MemoryNodeQuery) -> Result<MemoryNodeListResponse>;

    /// Get a single node by id.
    async fn get_node(&self, node_id: u64) -> Result<serde_json::Value>;

    /// Get memory store statistics.
    async fn get_stats(&self) -> Result<MemoryStats>;

    /// Trigger a consolidation run.
    async fn consolidate(&self, force: bool, retention_days: u32) -> Result<ConsolidationReport>;

    /// Delete a node by id.
    async fn delete_node(&self, node_id: u64) -> Result<()>;

    /// Create a new memory node.
    ///
    /// `input.label` is the node label (e.g. `"Knowledge"`, `"Episodic"`).
    /// `input.properties` is a flat key→JSON map; the implementation is
    /// responsible for serialising values into the underlying store. Returns
    /// the newly-assigned `node_id`.
    async fn create_node(&self, input: &CreateMemoryNodeInput) -> Result<u64>;

    /// Update (merge) properties on an existing node.
    ///
    /// Existing properties not listed in `properties` are left untouched.
    /// Returns 404 (mapped from a dedicated error) if the node is absent.
    async fn update_node(&self, node_id: u64, properties: &HashMap<String, serde_json::Value>) -> Result<()>;
}

/// Input for [`MemoryQueryService::create_node`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMemoryNodeInput {
    /// Node label (e.g. `"Knowledge"`, `"Episodic"`).
    pub label: String,
    /// Flat key→JSON property map. Empty map is allowed.
    pub properties: HashMap<String, serde_json::Value>,
}
