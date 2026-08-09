//! acowork-memory - MemoryProvider trait and shared memory types
//!
//! This crate defines the MemoryProvider trait abstraction and shared types.
//! Grafeo (acowork-grafeo) is the primary implementation.
//!
//! The `MemoryProvider` trait (ADR-051) extends the original `MemoryStore`
//! trait with additional methods for consolidation, CRUD, and lifecycle
//! control. During the migration period, both traits coexist:
//! - `MemoryStore` (16 methods) - existing impls remain valid
//! - `MemoryProvider` (35+ methods) - full trait for Runtime decoupling
//!
//! Design ref: ADR-051, docs/05-memory.md §10

pub mod admin;
pub mod consolidation;
pub mod judge;
pub mod manager;
pub mod provider;
pub mod store;
pub mod types;

// Re-exports: MemoryProvider trait (new, for ADR-051 migration)
pub use provider::MemoryProvider;

// Re-exports: MemoryAdminService trait (ADR-051 P4, admin/management operations)
pub use admin::{
    AdminConsolidateResult, AdminListNodesOutput, AdminListNodesParams, AdminNodeDetail,
    AdminNodeRecord, AdminStats, MemoryAdminService, RebuildStats,
};

// Re-exports: MemoryStore trait (original, for backward compatibility)
pub use store::MemoryStore;

// Re-exports: MemoryManager + associated types (ADR-051 P2, moved from runtime)
pub use manager::{
    ConversationRecord, InjectedMemory, MemoryManager, MemoryManagerConfig,
    RetrieveAndInjectResult, RetrievalResult, RetrievedMemory,
};

// Re-exports: consolidation types
pub use consolidation::{
    BehaviorPattern, ConflictAction, ConflictResolutionDetail, EmbeddingFn, GeneralizationConfig,
    GeneralizationResult, LlmMessage, LlmResponse, MemoryStoreInput, MemoryStoreResult,
    OfflineConsolidationConfig, OfflineConsolidationResult, PatternCategory, SchedulerConfig,
    TripleExtractorLlm,
};

// Backward-compatible alias: grafeo used `ProcessResult` for this type.
pub use consolidation::MemoryStoreResult as ProcessResult;

// Re-exports: judge types (ADR-051 P4, moved from acowork_grafeo)
pub use judge::{JudgeConfig, JudgeResult, should_sample};

// Re-exports: core memory types
pub use types::{
    AutobioCategory, AutobiographicalNode, ConflictSignal, ConflictType, ContextSource,
    DEFAULT_EMBEDDING_DIM, DecayConfig, DecayScanResult, DistilledEpisode, Episode, KnowledgeNode,
    KnowledgeSubType, MemoryContext, MemoryNode, MemoryQuery, NodeStatus, PrivacyLevel,
    ProceduralNode, PurgeResult, ResultSource, RetrievalMetrics, SearchResult, StoreHealth,
    StoreStats, Triple,
};

// Label and edge type constants
pub use types::edge_types;
pub use types::labels;
pub use types::{HintType, MemoryFilters, NodeTypeFilter};
