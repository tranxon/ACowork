//! Temporary probe: verify the actual score domain and min_score filtering
//! behavior on the auto_inject path. This exists to fact-check the ADR-062
//! assumption that `min_score = 0.3` filters out everything on the RRF scale
//! (scores ~1/(k+rank), k=60 → ~0.016). If the before/after auto_inject hit
//! rate is identical, we must explain WHY before writing the M4 report.

use std::sync::Arc;

use acowork_core::tools::traits::Tool;
use acowork_core::EmbeddingProvider;

use acowork_grafeo::grafeo::GrafeoStore;

use acowork_memory::{MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery};

use acowork_runtime::memory::MemorySessionHandle;
use acowork_runtime::tools::builtin::memory_store::MemoryStoreTool;

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

fn node_id_from_tool_result(content: &str) -> u64 {
    let marker = "id: ";
    let idx = content
        .find(marker)
        .unwrap_or_else(|| panic!("no `id:` marker: {content}"));
    content[idx + marker.len()..]
        .trim_end_matches(')')
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("cannot parse node id from: {content}"))
}

#[tokio::test]
async fn probe_min_score_domain() {
    let store = Arc::new(GrafeoStore::new_in_memory().expect("store"));
    let handle = Arc::new(MemorySessionHandle::new(Some(Arc::new(
        DeterministicEmbedding,
    ))));
    let provider: Arc<dyn MemoryProvider> = store.clone();
    handle.set_provider(provider);

    let tool = MemoryStoreTool::new("com.test.probe", Some(handle.clone()));
    let r = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "User prefers dark mode for the code editor",
                "confidence": 0.9,
                "importance": 0.8,
            }),
            None,
        )
        .await
        .unwrap();
    assert!(r.ok, "{:?}", r.error);
    let id = node_id_from_tool_result(&r.content);
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
