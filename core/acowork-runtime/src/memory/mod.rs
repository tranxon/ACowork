//! Memory module (MemoryProvider client)
pub mod consolidation_bg;
pub mod judge_llm;
pub mod llm_adapter;
pub mod manager;
pub mod metrics;
pub mod session_handle;

pub use consolidation_bg::{
    ConsolidationBgTask, ConsolidationParams, start_consolidation_pipeline,
};
pub use judge_llm::evaluate_retrieval_llm;
pub use llm_adapter::ProviderLlmAdapter;
pub use manager::{
    ConversationRecord, InjectedMemory, MemoryManager, MemoryManagerConfig, RetrievalResult,
    RetrievedMemory,
};
pub use metrics::{
    AlertThresholds, ConflictAccuracyStats, ConflictResolutionRecord, MetricsAlert,
    MetricsAlertType, RetrievalMetricsAggregator,
};
pub use session_handle::MemorySessionHandle;
