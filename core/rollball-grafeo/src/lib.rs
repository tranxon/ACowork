//! rollball-grafeo — Grafeo graph database engine
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
pub mod export;
pub mod eval;
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

pub use abstention::{AbstentionConfig, AbstentionResult, check_abstention, get_min_score_for_agent};
pub use backup::{BackupConfig, BackupMetadata, BackupType};
pub use consolidation::{ConflictCandidate, MemoryStoreInput, OfflineConsolidationConfig, OfflineConsolidationResult, ConsolidationScheduler, SchedulerConfig, ConsolidationRun, TriggerReason, BehaviorPattern, GeneralizationConfig, GeneralizationResult, PatternCategory};
pub use conflict::{detect_conflict, FACT_THRESHOLD, PREFERENCE_THRESHOLD, RELATION_THRESHOLD, TEMPORAL_WINDOW_HOURS};
pub use engineering::{CapacityConfig, CapacityStatus, ConcurrencyConfig, EmbeddingLevel, HealthCheckResult};
pub use eval::{EvalConfig, EvalDimension, EvalResult, run_eval};
pub use rollball_memory::{ConflictSignal, ConflictType};
pub use forgetting::DecayConfig;
pub use judge::{JudgeConfig, JudgeResult, evaluate_retrieval, should_sample};
pub use retrieval_metrics::{BenchmarkMetrics, EvalQuery, OnlineRetrievalMetrics, HintType, ConflictResolutionRecord, ConflictAccuracyStats, MetricsAggregator, AlertThresholds, MetricsAlert, MetricsAlertType, precision_at_k, recall_at_k, mean_reciprocal_rank, evaluate_retrieval_quality};
pub use stats::{MemoryStats, SlaConfig, SlaStatus, check_sla, collect_stats};
pub use error::{GrafeoError, Result};
pub use grafeo::GrafeoStore;
pub use index_config::{HnswConfig, validate_embedding_dim, HNSW_DEFAULT_EF_CONSTRUCTION, HNSW_DEFAULT_EF_SEARCH, HNSW_DEFAULT_M, EPISODIC_TEXT_FIELDS, KNOWLEDGE_TEXT_FIELDS, VECTOR_METRIC};
pub use spreading::{
    GraphExpandConfig, ExpandedNode,
    topology_boost, compute_edge_counts,
    get_hint_weights, get_expand_thresholds, config_from_hint,
    validate_expand_config,
};
pub use export::FilteredNode;
pub use types::{
    labels, edge_types, DEFAULT_EMBEDDING_DIM, GrafeoConfig,
    AutobioCategory, AutobiographicalNode,
    Episode, KnowledgeNode, KnowledgeSubType,
    NodeStatus, ProceduralNode,
};
