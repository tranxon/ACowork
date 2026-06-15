//! acowork-memory — MemoryStore trait and shared memory types
//!
//! This crate defines the MemoryStore trait abstraction and shared types.
//! Grafeo (acowork-grafeo) is the primary implementation (Phase 2).
//!
//! Design ref: docs/05-memory.md §10

pub mod store;
pub mod types;

// Re-exports for convenience
pub use store::MemoryStore;
pub use types::{
    AutobioCategory, AutobiographicalNode, ConflictSignal, ConflictType, ContextSource,
    DecayConfig, DecayScanResult, Episode, KnowledgeNode, KnowledgeSubType, MemoryContext,
    MemoryNode, MemoryQuery, NodeStatus, PrivacyLevel, ProceduralNode, PurgeResult, ResultSource,
    RetrievalMetrics, SearchResult, StoreHealth, StoreStats,
};

// Label and edge type constants
pub use types::edge_types;
pub use types::labels;
pub use types::{HintType, MemoryFilters, NodeTypeFilter};
