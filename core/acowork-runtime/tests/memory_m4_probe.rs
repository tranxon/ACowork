//! Probe: verify the actual score domain and min_score filtering behavior on
//! the auto_inject path (fact-check for the ADR-062 `min_score = 0.3`
//! assumption on the RRF scale). Runs against the Knowledge (sediment) layer.
//!
//! ADR-068 note: `MemoryStoreTool` only writes Episodes now, so this probe
//! seeds Knowledge nodes directly through `GrafeoStore`'s native path — the
//! same path the EpisodicDistiller uses on promotion — to keep measuring the
//! sediment-layer retrieval score domain.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;

use acowork_core::EmbeddingProvider;

use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::types::KnowledgeNode as GrafeoKnowledgeNode;

use acowork_memory::{
    KnowledgeSubType, MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery, NodeStatus,
    PrivacyLevel, labels,
};

use acowork_runtime::memory::MemorySessionHandle;

struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-probe"
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
        Ok(acowork_memory::manager::procedural_embedding_fallback(text))
    }
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
    fn dimension(&self) -> usize {
        384
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// Seed a Knowledge node directly (ADR-068 — see file header).
async fn seed_knowledge_fact(
    store: &GrafeoStore,
    content: &str,
    confidence: f32,
    importance: f32,
) -> u64 {
    let embedding = DeterministicEmbedding
        .embed(content)
        .await
        .expect("embed ok");
    let node = GrafeoKnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: String::new(),
        object: content.to_string(),
        sub_type: KnowledgeSubType::Fact,
        confidence,
        source_episode_id: None,
        source_episode_ids: Vec::new(),
        promotion_metadata: None,
        embedding: Some(embedding),
        status: NodeStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        metadata: HashMap::new(),
        privacy: PrivacyLevel::Personal,
        importance,
    };
    store
        .store_node(
            labels::KNOWLEDGE,
            node.to_properties()
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone())),
        )
        .expect("store_node ok")
        .0
}

#[tokio::test]
async fn probe_min_score_domain() {
    let store = Arc::new(GrafeoStore::new_in_memory().expect("store"));
    let handle = Arc::new(MemorySessionHandle::new(Some(Arc::new(
        DeterministicEmbedding,
    ))));
    let provider: Arc<dyn MemoryProvider> = store.clone();
    handle.set_provider(provider);

    let id = seed_knowledge_fact(
        &store,
        "User prefers dark mode for the code editor",
        0.9,
        0.8,
    )
    .await;
    println!("stored node id = {id}");

    let manager = MemoryManager::new(MemoryManagerConfig::default());

    // Probe 1: raw text search score domain (no min_score).
    let raw = store
        .text_search_with_filter("Knowledge", "content", "dark mode editor", 10, None)
        .unwrap();
    println!("raw text search scores: {:?}", raw);

    // Probe 2: hybrid search via provider (with embedding, no min_score).
    let emb = DeterministicEmbedding.embed("dark mode editor").await.unwrap();
    let hybrid = store
        .hybrid_search_full("Knowledge", "dark mode editor", &emb, 10, 0.8, 0.2, None)
        .unwrap();
    println!("hybrid scores (no min_score): {:?}", hybrid);

    // Probe 3: hybrid with min_score = 0.3 (the ADR-062 §6.4 assumption).
    let hybrid30 = store
        .hybrid_search_full("Knowledge", "dark mode editor", &emb, 10, 0.8, 0.2, Some(0.3))
        .unwrap();
    println!("hybrid scores (min_score=0.3): {:?}", hybrid30);

    // Probe 4: full retrieve with auto_inject, min_score=Some(0.3) and None.
    for ms in [Some(0.3f32), None] {
        let mut q = MemoryQuery::auto_inject("dark mode editor".to_string(), None);
        q.min_score = ms;
        let res = manager
            .retrieve(&*store, &mut q, Some(&DeterministicEmbedding))
            .await
            .unwrap();
        println!(
            "auto_inject min_score={ms:?} → {} results, scores: {:?}",
            res.memories.len(),
            res.memories.iter().map(|m| (m.node_id, m.score)).collect::<Vec<_>>()
        );
    }
}
