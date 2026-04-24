//! MemoryStore trait for storage backend abstraction
//!
//! This trait defines the interface for memory storage backends.
//! Grafeo is the primary implementation (Phase 2).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

/// Memory node with metadata
///
/// ⚠️ NOTE: The `zone` field is defined but NOT currently used in Phase 1-3.
/// Zone functionality is deferred to Phase 4+. Currently all nodes belong to
/// the `default` zone. See docs/05-memory.md §8.2 for details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub id: String,
    pub content: String,
    pub metadata: Value,
    /// Business scenario zone (e.g., "work", "personal", "system").
    /// ⚠️ UNUSED in Phase 1-3. Reserved for Phase 4+.
    pub zone: String,
    pub privacy_level: PrivacyLevel,
}

/// Privacy level for memory nodes
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PrivacyLevel {
    Public,
    Personal,
    Sensitive,
}

/// MemoryStore trait for abstracting storage backends
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Store a memory node
    async fn store(&self, node: MemoryNode) -> Result<()>;

    /// Retrieve a memory node by ID
    async fn retrieve(&self, id: &str) -> Result<Option<MemoryNode>>;

    /// Search memories by query (keyword search for Phase 1)
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryNode>>;

    /// Delete a memory node
    async fn delete(&self, id: &str) -> Result<()>;

    /// List all memory nodes in a zone
    /// ⚠️ NOT IMPLEMENTED in GrafeoStore (Phase 1-3). Reserved for Phase 4+.
    async fn list_by_zone(&self, zone: &str) -> Result<Vec<MemoryNode>>;
}
