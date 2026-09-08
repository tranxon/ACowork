//! Consolidation pipeline — instant extraction and episodic distillation.
//!
//! - **Instant extraction** (`instant`): processes `memory_store` tool calls
//!   from the LLM in real-time (dedup, conflict detection and arbitration).
//! - **EpisodicDistiller** (`distiller` / `distill`): the single producer of
//!   the semantic layer (ADR-068) — promotes Episodic evidence to
//!   Knowledge / Procedural / Autobiographical nodes.
//!
//! The legacy offline consolidation pipeline (Pending-node upgrades,
//! experience generalization) has been removed: Pending has had no producer
//! since ADR-068 and the admin-driven manual pass had no consumer.

pub mod ambiguous;
pub mod conflict_llm;
pub mod distill;
pub mod distiller;
pub mod instant;
pub mod scheduler;
pub mod triple_extraction;

pub use ambiguous::AmbiguousConflict;
pub use conflict_llm::{ConflictClassification, LlmConflictType, classify_conflict};
pub use distiller::{DefaultEpisodicDistiller, EpisodicDistiller};

// Types migrated to acowork-memory are re-exported from the submodules.
// The submodules themselves re-export from acowork_memory::consolidation.
pub use instant::{
    ConflictCandidate, ConflictResolutionDetail, MemoryStoreInput, MemoryStoreResult,
    ProcessResult,
};
pub use scheduler::SchedulerConfig;
pub use triple_extraction::{LlmMessage, LlmResponse, TripleExtractorLlm};
