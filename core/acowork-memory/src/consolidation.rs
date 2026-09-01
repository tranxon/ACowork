//! Consolidation types for the memory system.
//!
//! These types define the data structures used by the consolidation pipeline:
//! - Instant extraction (memory_store tool calls)
//! - Offline consolidation (background upgrade of Pending nodes)
//! - Experience generalization (pattern extraction from repeated episodes)
//! - Triple extraction (LLM-driven knowledge extraction)
//! - Scheduling configuration
//!
//! All types use `u64` for node IDs (not grafeo_common::NodeId) to keep
//! this crate independent of the storage engine.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{AutobioCategory, ConflictSignal, KnowledgeSubType, NodeStatus, PrivacyLevel};

// ============================================================================
// Embedding function type alias
// ============================================================================

/// Shared embedding function type used across consolidation pipelines.
pub type EmbeddingFn = Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>;

// ============================================================================
// LLM abstraction (migrated from grafeo::consolidation::triple_extraction)
// ============================================================================

/// A single message in the LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Response from the LLM abstraction.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// The text content of the assistant's reply.
    pub content: String,
    /// Token usage (if available).
    pub usage_tokens: Option<u64>,
}

/// Trait for making LLM calls during triple extraction.
///
/// Implemented by the runtime layer using the active Provider.
/// This trait keeps the consolidation pipeline independent of
/// the provider ecosystem while still supporting LLM-driven consolidation.
#[async_trait::async_trait]
pub trait TripleExtractorLlm: Send + Sync {
    /// Send a chat request and return the response text.
    async fn chat(&self, messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String>;
}

// ============================================================================
// Instant extraction types (migrated from grafeo::consolidation::instant)
// ============================================================================

/// Input from LLM's `memory_store` tool call.
#[derive(Debug, Clone)]
pub struct MemoryStoreInput {
    /// Natural language content from LLM.
    pub content: String,
    /// Knowledge sub-type: Fact | Preference | Relation.
    ///
    /// Ignored when `autobiographical` is `Some` — autobiographical writes
    /// route to `AutobiographicalNode` regardless of `sub_type`.
    pub sub_type: KnowledgeSubType,
    /// Optional subject hint (defaults to "user").
    pub subject: Option<String>,
    /// Optional predicate hint.
    pub predicate: Option<String>,
    /// Optional object hint.
    pub object: Option<String>,
    /// LLM's confidence in this knowledge (default 0.7).
    pub confidence: Option<f32>,
    /// Source episode ID for traceability.
    pub source_episode_id: Option<u64>,
    /// Pre-computed embedding vector.
    pub embedding: Option<Vec<f32>>,
    /// Optional privacy level (design §7.1). Defaults to `Personal`.
    ///
    /// When `Some`, the pipeline stamps it on the created KnowledgeNode;
    /// when `None`, the conservative default `Personal` applies.
    pub privacy: Option<PrivacyLevel>,
    /// Optional importance score [0.0, 1.0] (design §3.1). Defaults to 0.5.
    pub importance: Option<f32>,
    /// Optional keywords provided by the LLM to aid retrieval (design §4.1).
    /// Persisted into node `metadata["keywords"]`.
    pub keywords: Option<Vec<String>>,
    /// Optional autobiographical path.
    ///
    /// When `Some`, the pipeline writes to `AutobiographicalNode` instead of
    /// `KnowledgeNode` / `ProceduralNode`. `sub_type`, `subject`, `predicate`,
    /// `object` are ignored in this case. Idempotent on `(aspect, key)` —
    /// re-emitting the same key updates the existing node in place.
    pub autobiographical: Option<AutobiographicalStoreInput>,
}

/// Subset of `MemoryStoreInput` that targets the autobiographical (self-
/// knowledge) layer.
///
/// The LLM populates this when the content is about *the Agent itself* —
///
/// identity, capabilities, limitations, self-preferences, milestones, or
/// long-term relationships — rather than about the user or the world. This
/// distinguishes "I tend to give conclusions first" (autobiographical
/// preference) from "user prefers concise replies" (knowledge preference).
#[derive(Debug, Clone)]
pub struct AutobiographicalStoreInput {
    /// Self-knowledge aspect (identity / capability / limitation /
    /// preference / history / relationship).
    pub aspect: AutobioCategory,
    /// Optional key for idempotent updates (e.g. "style", "name").
    ///
    /// When `None`, a stable key is derived from `aspect` + the first 8
    /// words of `content` (lower-cased, snake_cased).
    pub key: Option<String>,
    /// Provenance of this knowledge. Defaults to `"user_statement"`.
    ///
    /// Conventional values:
    /// - `"user_statement"` — user directly told the agent
    /// - `"important_event"` — significant interaction worth recording
    /// - `"self_evaluation"` — internal reflection (rare for instant path)
    pub source: Option<String>,
}

/// Action recommended by the conflict resolver.
///
/// Uses `u64` for node IDs to stay storage-engine-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    /// Auto-resolve: new replaces old.
    AutoReplace {
        /// The existing node to be superseded.
        old_node_id: u64,
        /// New status for the old node (typically Dormant).
        new_status: NodeStatus,
    },
    /// Both kept, marked for user confirmation.
    MarkAmbiguous {
        /// Shared conflict group identifier.
        conflict_group_id: String,
    },
    /// Defer to LLM offline arbitration.
    DeferToLLM,
}

/// Detailed record of a single conflict resolution action.
#[derive(Debug, Clone)]
pub struct ConflictResolutionDetail {
    /// The existing node involved in the conflict.
    pub existing_node_id: u64,
    /// The resolution action taken.
    pub action: ConflictAction,
    /// The conflict signal that triggered the resolution.
    pub signal: ConflictSignal,
}

/// Result of processing a `memory_store` tool call.
///
/// Also re-exported as `ProcessResult` for backward compatibility.
#[derive(Debug, Clone)]
pub struct MemoryStoreResult {
    /// The ID of the newly created (or updated) knowledge node.
    pub node_id: u64,
    /// Detailed conflict resolution records.
    pub conflict_resolutions: Vec<ConflictResolutionDetail>,
}

// ============================================================================
// Generalization types (migrated from grafeo::consolidation::generalization)
// ============================================================================

/// Category of a behavior pattern - used for grouping and dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PatternCategory {
    /// Tool usage pattern (e.g., "use http_request for weather lookups").
    ToolUsage,
    /// User preference pattern (e.g., "user prefers concise output").
    UserPreference,
    /// Workflow pattern (e.g., "when asked for a report, first gather data, then format").
    Workflow,
    /// Error recovery pattern (e.g., "on API timeout, retry once").
    ErrorRecovery,
}

impl PatternCategory {
    /// Returns the string representation used in ProceduralNode metadata.
    pub fn as_str(&self) -> &'static str {
        match self {
            PatternCategory::ToolUsage => "ToolUsage",
            PatternCategory::UserPreference => "UserPreference",
            PatternCategory::Workflow => "Workflow",
            PatternCategory::ErrorRecovery => "ErrorRecovery",
        }
    }
}

impl std::str::FromStr for PatternCategory {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "ToolUsage" => Ok(PatternCategory::ToolUsage),
            "UserPreference" => Ok(PatternCategory::UserPreference),
            "Workflow" => Ok(PatternCategory::Workflow),
            "ErrorRecovery" => Ok(PatternCategory::ErrorRecovery),
            _ => Err(format!("unknown PatternCategory: {s}")),
        }
    }
}

/// A detected behavior pattern from episodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorPattern {
    /// Human-readable name for the pattern.
    pub name: String,
    /// Trigger condition description.
    pub trigger_condition: String,
    /// Action pattern description.
    pub action_pattern: String,
    /// Number of episodes this pattern was observed in.
    pub observation_count: usize,
    /// Confidence in the pattern [0.0, 1.0].
    pub confidence: f32,
    /// Pattern category (for grouping and dedup).
    #[serde(default = "default_pattern_category")]
    pub category: PatternCategory,
}

fn default_pattern_category() -> PatternCategory {
    PatternCategory::ToolUsage
}

/// Result of the generalization process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizationResult {
    /// Detected patterns.
    pub patterns: Vec<BehaviorPattern>,
    /// Number of new ProceduralNodes created.
    pub nodes_created: usize,
    /// Number of existing ProceduralNodes boosted (confidence incremented).
    pub nodes_boosted: usize,
    /// Number of patterns deduplicated against existing nodes.
    pub patterns_deduplicated: usize,
    /// Timestamp of the generalization.
    pub generalized_at: DateTime<Utc>,
}

/// Configuration for the experience generalization process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizationConfig {
    /// Minimum number of observations before a pattern is considered valid.
    /// Default: 3.
    pub min_observations: usize,
    /// Maximum number of unconsolidated episodes to scan per run.
    /// Default: 100.
    pub max_episodes_scan: usize,
    /// Confidence boost applied when a pattern reinforces an existing node.
    /// Default: 0.05.
    pub confidence_boost: f32,
    /// Maximum confidence for a ProceduralNode (cap after boosting).
    /// Default: 0.98.
    pub max_confidence: f32,
    /// Whether to use LLM for pattern discovery when available.
    /// Default: true.
    pub use_llm: bool,
}

impl Default for GeneralizationConfig {
    fn default() -> Self {
        Self {
            min_observations: 3,
            max_episodes_scan: 100,
            confidence_boost: 0.05,
            max_confidence: 0.98,
            use_llm: true,
        }
    }
}

// ============================================================================
// Offline consolidation types (migrated from grafeo::consolidation::offline)
// ============================================================================

/// Offline consolidation configuration.
#[derive(Debug, Clone)]
pub struct OfflineConsolidationConfig {
    /// Maximum number of pending nodes to process per batch.
    /// Default: 50.
    pub batch_size: usize,
    /// Minimum age (in hours) before a Pending node is eligible for
    /// offline processing. Default: 1.
    pub min_pending_age_hours: u64,
}

impl Default for OfflineConsolidationConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            min_pending_age_hours: 1,
        }
    }
}

/// Result of an offline consolidation run.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OfflineConsolidationResult {
    /// Number of nodes upgraded from Pending -> Active.
    pub upgraded: usize,
    /// Number of nodes kept as Pending (not old enough or not enough evidence).
    pub kept_pending: usize,
    /// Number of nodes marked Dormant (low confidence after re-evaluation).
    pub marked_dormant: usize,
    /// Number of new ProceduralNodes created by generalization.
    pub procedural_created: usize,
    /// Number of existing ProceduralNodes boosted by generalization.
    pub procedural_boosted: usize,
    /// Number of History nodes compressed into summaries.
    pub history_compressed: usize,
    /// Number of triples extracted from unconsolidated episodes.
    pub triples_extracted: usize,
    /// Number of conflicts resolved by LLM arbitration.
    pub conflicts_resolved: usize,
    /// Number of conflicts classified as Evolution (old -> Dormant, new -> Active).
    pub conflicts_evolution: usize,
    /// Number of conflicts classified as Correction (old -> Dormant, new -> Active).
    pub conflicts_correction: usize,
    /// Number of conflicts classified as Ambiguous (both kept, user confirmation needed).
    pub conflicts_ambiguous: usize,
    /// Number of episodic nodes cleaned up (transitioned to Dormant by §2 rules).
    pub episodic_cleaned: usize,
}

// ============================================================================
// Scheduler configuration (migrated from grafeo::consolidation::scheduler)
// ============================================================================

/// Configuration for the consolidation scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Idle timeout in seconds before automatic consolidation.
    /// Default: 1800 (30 minutes).
    pub idle_timeout_secs: u64,
    /// Minimum number of pending nodes before triggering consolidation.
    /// Default: 50.
    pub accumulation_threshold: usize,
    /// Batch size per consolidation run.
    /// Default: 50 (inherited from OfflineConsolidationConfig).
    pub batch_size: usize,
    /// Minimum age (in hours) before a Pending node is eligible.
    /// Default: 1 (inherited from OfflineConsolidationConfig).
    pub min_pending_age_hours: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 1800,
            accumulation_threshold: 50,
            batch_size: 50,
            min_pending_age_hours: 1,
        }
    }
}
