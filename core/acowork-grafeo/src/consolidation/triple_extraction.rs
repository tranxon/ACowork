//! Triple extraction — LLM-driven knowledge extraction from Episode text.
//!
//! ADR-057: the synchronous distillation landing pipeline
//! ([`crate::consolidation::distill`]) replaces this module's role for
//! runtime compaction. The legacy `extract_triples` implementation was
//! DELETED (ADR-057 C7 + triples-removed): it had no production caller —
//! compaction lands triples synchronously through `ingest_distilled_triples`
//! and offline Step-2 re-extraction was removed. This module now only
//! re-exports the shared LLM abstraction types that downstream modules
//! (conflict_llm, generalization, offline, eval) still reference through
//! the `crate::consolidation::triple_extraction::*` path.
//!
//! Design: `docs/05-memory.md` §4.3

// ---------------------------------------------------------------------------
// LLM abstraction (re-exported from acowork-memory)
// ---------------------------------------------------------------------------

pub use acowork_memory::consolidation::{LlmMessage, LlmResponse, TripleExtractorLlm};
