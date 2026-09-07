//! Memory module (MemoryProvider client)
//!
//! ADR-051 P2: MemoryManager + associated types moved to acowork-memory.
//! This module re-exports them for backward compatibility.

// ADR-071 e2e: manual-distill full chain (HTTP → AgentCore → distiller →
// real GrafeoStore). Sits in-crate because `AgentCore::new` is `pub(crate)`
// and the test needs to inject `memory_provider` / `embedding_provider` /
// `consolidation_timer` (all `pub(crate)` fields).
#[cfg(all(test, feature = "grafeo-backend"))]
mod adr071_e2e;

pub mod consolidation_bg;
pub mod judge_llm;
pub mod llm_adapter;
pub mod manager;
pub mod metrics;
pub mod session_handle;

// Re-export MemoryManager and types from acowork-memory.
pub use acowork_memory::{
    InjectedMemory, MemoryManager, MemoryManagerConfig,
    RetrieveAndInjectResult, RetrievalResult, RetrievedMemory,
};

pub use consolidation_bg::{
    ConsolidationBgTask, ConsolidationParams, ConsolidationTimer, TriggerReason,
    start_consolidation_pipeline,
};
pub use judge_llm::evaluate_retrieval_llm;
pub use llm_adapter::ProviderLlmAdapter;
pub use metrics::{
    AlertThresholds, ConflictAccuracyStats, ConflictResolutionRecord, MetricsAlert,
    MetricsAlertType, RetrievalMetricsAggregator,
};
pub use session_handle::MemorySessionHandle;
