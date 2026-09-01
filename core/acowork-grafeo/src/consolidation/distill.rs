//! Distillation embedding helper (ADR-057 P0, triples-removed).
//!
//! Compact-model output is now summary-only (the `<triples>` block was dropped
//! because compact-model output quality was too low). The DistilledEpisode
//! landing path (`MemoryManager::record_distilled` → `MemoryProvider::store_episode`)
//! needs a summary embedding best-effort, so this module exposes the shared
//! helper used by the runtime. The knowledge-layer landing pipeline that
//! previously lived here (per-triple dedup, conflict detection,
//! `Episodic -[SOURCED_FROM]-> Knowledge` edges) has been removed entirely;
//! knowledge updates now flow through the `memory_store` tool / procedural
//! creation paths (see `docs/memory-write-entrypoints.md`).
//!
//! Embedding degradation policy (D1): when no `EmbeddingProvider` is
//! available — or the provider errors — the helper returns `None` and the
//! node is stored WITHOUT a vector. No fake/hash vectors are written to the
//! semantic layer: a meaningless vector would pollute both semantic search
//! and future conflict detection. The deterministic hash embedding remains
//! available for ProceduralNode creation paths only (see
//! `acowork_memory::procedural_embedding_fallback`), where the write
//! contract requires a non-empty vector.

use acowork_core::EmbeddingProvider;

use crate::grafeo::GrafeoStore;

impl GrafeoStore {
    /// Compute a text embedding via the supplied provider.
    ///
    /// Returns `None` when no provider is configured or the provider errors
    /// (degradation is logged). `None` means "store without a vector" — it
    /// never means "store a fake vector".
    pub async fn compute_text_embedding(
        text: &str,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Option<Vec<f32>> {
        match embedding_provider {
            Some(prov) => match prov.embed(text).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        provider = prov.name(),
                        "embedding generation failed during distillation landing; storing without vector"
                    );
                    None
                }
            },
            None => None,
        }
    }
}

