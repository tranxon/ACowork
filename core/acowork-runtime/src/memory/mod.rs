//! Memory module (MemoryProvider client)
//!
//! ADR-051 P2: MemoryManager + associated types moved to acowork-memory.
//! This module re-exports them for backward compatibility.

pub mod consolidation_bg;
pub mod judge_llm;
pub mod llm_adapter;
pub mod manager;
pub mod metrics;
pub mod session_handle;

// Re-export MemoryManager and types from acowork-memory.
pub use acowork_memory::{
    ConversationRecord, InjectedMemory, MemoryManager, MemoryManagerConfig, RetrievalResult,
    RetrievedMemory,
};

pub use consolidation_bg::{
    ConsolidationBgTask, ConsolidationParams, start_consolidation_pipeline,
};
pub use judge_llm::evaluate_retrieval_llm;
pub use llm_adapter::ProviderLlmAdapter;
pub use metrics::{
    AlertThresholds, ConflictAccuracyStats, ConflictResolutionRecord, MetricsAlert,
    MetricsAlertType, RetrievalMetricsAggregator,
};
pub use session_handle::MemorySessionHandle;
