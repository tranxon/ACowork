//! acowork-grafeo — Grafeo graph database engine
//!
//! Phase 2: Full graph database implementation with:
//! - Three-layer five-type biomimetic memory
//! - Forgetting mechanism (decay)
//! - Associative diffusion retrieval
//! - Privacy level filtering

pub mod abstention;
pub mod backup;
pub mod conflict;
pub mod consolidation;
pub mod engineering;
pub mod episodic;
pub mod error;
pub mod eval;
pub mod export;
pub mod forgetting;
pub mod grafeo;
pub mod graph;
pub mod index_config;
pub mod judge;
pub mod retrieval;
pub mod retrieval_metrics;
pub mod semantic;
pub mod spreading;
pub mod stats;
pub mod types;

pub use abstention::{
    AbstentionConfig, AbstentionResult, check_abstention, get_min_score_for_agent,
};
pub use acowork_memory::{ConflictSignal, ConflictType};
pub use backup::{BackupConfig, BackupMetadata, BackupType};
pub use conflict::{
    FACT_THRESHOLD, PREFERENCE_THRESHOLD, RELATION_THRESHOLD, TEMPORAL_WINDOW_HOURS,
    detect_conflict,
};
pub use consolidation::{
    BehaviorPattern, ConflictCandidate, ConsolidationRun, ConsolidationScheduler,
    EmbeddingFn, GeneralizationConfig, GeneralizationResult, MemoryStoreInput,
    OfflineConsolidationConfig, OfflineConsolidationResult, PatternCategory, SchedulerConfig,
    TriggerReason,
};
pub use engineering::{
    CapacityConfig, CapacityStatus, ConcurrencyConfig, EmbeddingLevel, HealthCheckResult,
};
pub use error::{GrafeoError, Result};
pub use eval::{EvalConfig, EvalDimension, EvalResult, run_eval};
pub use export::FilteredNode;
pub use forgetting::DecayConfig;
pub use grafeo::GrafeoStore;
pub use grafeo::RebuildStats;
pub use index_config::{
    EPISODIC_TEXT_FIELDS, HNSW_DEFAULT_EF_CONSTRUCTION, HNSW_DEFAULT_EF_SEARCH, HNSW_DEFAULT_M,
    HnswConfig, KNOWLEDGE_TEXT_FIELDS, VECTOR_METRIC, validate_embedding_dim,
};
pub use judge::{JudgeConfig, JudgeResult, evaluate_retrieval, should_sample};
pub use retrieval_metrics::{
    AlertThresholds, BenchmarkMetrics, ConflictAccuracyStats, ConflictResolutionRecord, EvalQuery,
    HintType, MetricsAggregator, MetricsAlert, MetricsAlertType, OnlineRetrievalMetrics,
    evaluate_retrieval_quality, mean_reciprocal_rank, precision_at_k, recall_at_k,
};
pub use spreading::{
    ExpandedNode, GraphExpandConfig, compute_edge_counts, config_from_hint, get_expand_thresholds,
    get_hint_weights, topology_boost, validate_expand_config,
};
pub use stats::{MemoryStats, SlaConfig, SlaStatus, check_sla, collect_stats};
pub use types::{
    AutobioCategory, AutobiographicalNode, DEFAULT_EMBEDDING_DIM, Episode, GrafeoConfig,
    KnowledgeNode, KnowledgeSubType, NodeStatus, ProceduralNode, edge_types, labels,
};
