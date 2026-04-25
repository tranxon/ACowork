//! Consolidation pipeline — instant extraction and offline consolidation.
//!
//! The consolidation pipeline processes knowledge extracted by the LLM
//! during conversation (instant) and in background batches (offline).
//!
//! - **Instant extraction** (`instant`): processes `memory_store` tool calls
//!   from the LLM in real-time, performing dedup, conflict detection, and
//!   status assignment (Active/Pending).
//! - **Offline consolidation** (`offline`): upgrades Pending nodes to Active
//!   based on age and evidence. Full LLM-based re-evaluation is planned for
//!   Phase 3.

pub mod ambiguous;
pub mod conflict_llm;
pub mod generalization;
pub mod instant;
pub mod offline;
pub mod scheduler;
pub mod triple_extraction;

pub use ambiguous::AmbiguousConflict;
pub use conflict_llm::{LlmConflictType, ConflictClassification, classify_conflict};
pub use generalization::{BehaviorPattern, GeneralizationConfig, GeneralizationResult, PatternCategory, detect_simple_patterns, discover_patterns_llm};
pub use instant::{ConflictCandidate, MemoryStoreInput, ConflictResolutionDetail, ProcessResult};
pub use offline::{OfflineConsolidationConfig, OfflineConsolidationResult};
pub use scheduler::{ConsolidationScheduler, SchedulerConfig, ConsolidationRun, TriggerReason};
pub use triple_extraction::{ExtractedTriple, ExtractionResult, LlmMessage, LlmResponse, TripleExtractorLlm};
