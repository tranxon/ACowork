//! ADR-062 M5 (P2): keyword_index before/after retrieval benchmark.
//!
//! M4 (see `memory_m4_bench.rs`) verified D1 (Dormant exclusion) + D2
//! (auto_inject min_score fix). M5 verifies the keyword-index path
//! (ADR-062 §6.2 — Plan Y): write-time folding of `metadata["keywords"]`
//! into the BM25-indexed `object` field, gated by `quality.keyword_index`.
//!
//! Design:
//!   - Corpus = M4's 10 fixed nodes (kept stable for cross-milestone
//!     regression) + 3 keyword-distinctive nodes K1/K2/K3. Each K* node has
//!     lexically-disjoint content (e.g. "A custom note") and distinctive
//!     keywords (e.g. ["santorini", "vacation", "summer-2024"]) that DO NOT
//!     appear in any other corpus node.
//!   - Query set = M4's 5 content queries (baseline) + 3 keyword-only queries
//!     where ONLY the K* node can match (BM25 alone, content-only, cannot hit
//!     K* nodes because their content is lexically generic).
//!   - Two states:
//!     * before = `quality.keyword_index = false` (M4 behaviour, keywords are
//!       metadata-only and BM25 cannot see them).
//!     * after  = `quality.keyword_index = true`  (M5 behaviour, keywords are
//!       folded into `object` so BM25 matches).
//!   - Metrics: Precision@5 / Recall@5 / MRR (full query set) + keyword
//!     hit rate (fraction of K* queries that returned their ground-truth K*
//!     in top-5).
//!
//! Determinism:
//!   - `enable_graph_expand = false` to disable PageRank boost (random
//!     HashMap iteration), keeping MRR reproducible across processes.
//!   - `DeterministicEmbedding` (same-text-same-vector) keeps vector scores
//!     stable.
//!
//! IMPORTANT: uses in-memory `GrafeoStore`, never touches the running
//! Gateway / Runtime / Desktop processes or their ports.

use std::sync::Arc;

use acowork_core::tools::traits::Tool;
use acowork_core::EmbeddingProvider;

use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::retrieval_metrics::{EvalQuery, evaluate_retrieval_quality};

use acowork_memory::{MemoryManager, MemoryManagerConfig, MemoryQuery};

use acowork_runtime::memory::MemorySessionHandle;
use acowork_runtime::tools::builtin::memory_store::MemoryStoreTool;

use grafeo_common::types::NodeId;

// ============================================================================
// Deterministic embedding (mirrors M4 harness)
// ============================================================================

struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-m5-bench"
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

// ============================================================================
// Harness scaffold
// ============================================================================

struct BenchE2e {
    store: Arc<GrafeoStore>,
    handle: Arc<MemorySessionHandle>,
}

impl BenchE2e {
    fn new() -> Self {
        let store = Arc::new(GrafeoStore::new_in_memory().expect("in-memory store"));
        let handle = Arc::new(MemorySessionHandle::new(Some(Arc::new(
            DeterministicEmbedding,
        ))));
        let provider: Arc<dyn acowork_memory::MemoryProvider> = store.clone();
        handle.set_provider(provider);
        Self { store, handle }
    }

    fn store_tool(&self) -> MemoryStoreTool {
        MemoryStoreTool::new("com.test.m5-bench", Some(self.handle.clone()))
    }

    fn node_id_from_tool_result(content: &str) -> u64 {
        let marker = "id: ";
        let idx = content
            .find(marker)
            .unwrap_or_else(|| panic!("no `id:` marker in tool result: {content}"));
        content[idx + marker.len()..]
            .trim_end_matches(')')
            .trim()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("cannot parse node id from: {content}"))
    }

    /// Store a content-only knowledge node (no keywords).
    async fn store_knowledge(&self, content: &str, confidence: f32, importance: f32) -> u64 {
        let tool = self.store_tool();
        let result = tool
            .execute(
                serde_json::json!({
                    "category": "fact",
                    "content": content,
                    "confidence": confidence,
                    "importance": importance,
                }),
                None,
            )
            .await
            .expect("tool execute ok");
        assert!(result.ok, "store failed: {:?}", result.error);
        Self::node_id_from_tool_result(&result.content)
    }

    /// Store a knowledge node WITH LLM-provided keywords. When the
    /// `quality.keyword_index` gate is on at write-time (Plan Y), the
    /// keywords are folded into the BM25-indexed `object` field so text
    /// search naturally matches them.
    async fn store_knowledge_with_keywords(
        &self,
        content: &str,
        confidence: f32,
        importance: f32,
        keywords: &[&str],
    ) -> u64 {
        let tool = self.store_tool();
        let result = tool
            .execute(
                serde_json::json!({
                    "category": "fact",
                    "content": content,
                    "confidence": confidence,
                    "importance": importance,
                    "keywords": keywords,
                }),
                None,
            )
            .await
            .expect("tool execute ok");
        assert!(result.ok, "store failed: {:?}", result.error);
        Self::node_id_from_tool_result(&result.content)
    }
}

// ============================================================================
// Fixed corpus + query set
// ============================================================================

/// M4 base corpus, kept verbatim for cross-milestone regression.
/// - `A*` = ground-truth relevant (content-matchable).
/// - `D*` = garbage nodes (marked Dormant).
/// - `N*` = Active distractors.
const M4_CORPUS: &[(&str, &str, f32, f32)] = &[
    ("A1", "User prefers dark mode for the code editor", 0.9, 0.8),
    ("A2", "User lives in Shanghai near the Huangpu river", 0.9, 0.8),
    ("A3", "User works at Acme Corp as a backend engineer", 0.9, 0.8),
    ("A4", "User speaks Japanese and writes fluent code", 0.9, 0.8),
    ("A5", "User keeps two cats named Mochi and Tofu", 0.9, 0.8),
    ("D1", "User used to prefer dark mode in the terminal", 0.9, 0.1),
    ("D2", "Old note about the Shanghai office address", 0.9, 0.1),
    ("D3", "User previously worked at Acme Corp in sales", 0.9, 0.1),
    ("N1", "User prefers a light theme during winter", 0.9, 0.5),
    ("N2", "User reads books about backend architecture", 0.9, 0.5),
];

/// Keyword-distinctive nodes added in M5. Each K* has lexically-disjoint
/// content (BM25 alone cannot match) and unique keywords (the only path to
/// relevance).
const K_CORPUS: &[(&str, &str, f32, f32, &[&str])] = &[
    (
        "K1",
        "A custom note",
        0.9,
        0.8,
        &["santorini", "vacation", "summer-2024"],
    ),
    (
        "K2",
        "Another personal entry",
        0.9,
        0.8,
        &["graphql", "schema-stitching", "federation"],
    ),
    (
        "K3",
        "A third note",
        0.9,
        0.8,
        &["beekeeping", "apiary", "honey-extraction"],
    ),
];

/// M4 baseline queries — kept verbatim for regression check.
const M4_QUERIES: &[(&str, &str)] = &[
    ("dark mode editor", "A1"),
    ("Shanghai river home", "A2"),
    ("Acme backend engineer", "A3"),
    ("Japanese language code", "A4"),
    ("cats pets at home", "A5"),
];

/// M5 keyword-only queries. Each query's tokens appear ONLY in the
/// matching K* node's `keywords` array — never in any other corpus node's
/// content. So `keyword_index=false` must return 0 relevant hits; only
/// `keyword_index=true` (keywords folded into BM25-indexed `object`) hits.
const K_QUERIES: &[(&str, &str)] = &[
    ("santorini vacation summer-2024", "K1"),
    ("graphql schema-stitching federation", "K2"),
    ("beekeeping apiary honey-extraction", "K3"),
];

// ============================================================================
// Benchmark runner
// ============================================================================

#[derive(Default)]
struct RunSummary {
    p5: f32,
    r5: f32,
    mrr: f32,
    /// Fraction of K* queries whose ground-truth K* node appears in top-5.
    keyword_hit_rate: f32,
    /// Fraction of K* queries whose ground-truth K* node appears anywhere
    /// in the result set (top-10).
    keyword_any_rank: f32,
}

/// Run the full query set under one `keyword_index` toggle.
async fn run_state(
    e2e: &BenchE2e,
    relevant: &[EvalQuery],
    keyword_ground_truth_ids: &std::collections::HashMap<String, u64>,
    keyword_index: bool,
) -> RunSummary {
    let mut cfg = MemoryManagerConfig::default();
    cfg.quality.exclude_dormant = true; // M5 ships D1 on by default
    cfg.quality.keyword_index = keyword_index;
    cfg.enable_graph_expand = false; // determinism — see M4 report §5.2
    let manager = MemoryManager::new(cfg);

    let mut per_query: Vec<Vec<u64>> = Vec::with_capacity(relevant.len());
    let mut kw_top5_hits = 0usize;
    let mut kw_any_rank_hits = 0usize;
    let mut kw_query_count = 0usize;

    for q in relevant {
        let mut mq = MemoryQuery::new(q.query.clone());
        mq.abstention_enabled = false;
        mq.limit = 10;
        let result = manager
            .retrieve(&*e2e.store, &mut mq, Some(&DeterministicEmbedding))
            .await
            .expect("retrieve ok");
        let ids: Vec<u64> = result.memories.iter().map(|m| m.node_id).collect();

        // Track K* query outcomes separately (K* ground truth is keyed by
        // the query text itself in `keyword_ground_truth_ids`).
        if let Some(&target_id) = keyword_ground_truth_ids.get(&q.query) {
            kw_query_count += 1;
            if ids.iter().take(5).any(|id| *id == target_id) {
                kw_top5_hits += 1;
            }
            if ids.iter().any(|id| *id == target_id) {
                kw_any_rank_hits += 1;
            }
        }

        per_query.push(ids);
    }

    let bench = evaluate_retrieval_quality(relevant, &per_query, &[5]);
    let denom = if kw_query_count > 0 {
        kw_query_count as f32
    } else {
        1.0
    };
    RunSummary {
        p5: bench
            .precision_at_k
            .iter()
            .find(|(k, _)| *k == 5)
            .map(|(_, v)| *v)
            .unwrap_or(0.0),
        r5: bench
            .recall_at_k
            .iter()
            .find(|(k, _)| *k == 5)
            .map(|(_, v)| *v)
            .unwrap_or(0.0),
        mrr: bench.mrr,
        keyword_hit_rate: kw_top5_hits as f32 / denom,
        keyword_any_rank: kw_any_rank_hits as f32 / denom,
    }
}

// ============================================================================
// The M5 benchmark test
// ============================================================================

#[tokio::test]
async fn m5_keyword_index_before_after() {
    let e2e = BenchE2e::new();

    // ── 1. Build the fixed corpus via the real write chain ──────────────
    let mut ids_by_key: std::collections::HashMap<&str, u64> =
        std::collections::HashMap::new();

    // M4 corpus (content-only).
    for (key, content, confidence, importance) in M4_CORPUS {
        let id = e2e
            .store_knowledge(content, *confidence, *importance)
            .await;
        ids_by_key.insert(*key, id);
    }

    // K* corpus (content + keywords). Keywords are persisted into
    // metadata["keywords"] unconditionally; whether they ALSO fold into the
    // BM25-indexed `object` is controlled by `quality.keyword_index` at
    // write-time (Plan Y — applied during retrieval here via per-state cfg).
    //
    // Note: the corpus is built ONCE with `keyword_index=false` so that
    // before/after states only differ in the retrieval cfg. The Plan Y
    // write-time fold is exercised by `after` in the search path: keywords
    // already live in metadata["keywords"], so the before state has them
    // there but NOT in object; the after state — when we additionally
    // re-store with keyword_index=true — would put them in object.
    //
    // To keep the corpus fixed across both states, we instead exercise the
    // M5 fold path via a SECOND store pass with `keyword_index=true` (see
    // helper below): the K* node ids in the BEFORE state have keywords in
    // metadata only; in the AFTER state the same K* nodes have keywords
    // folded into object. Both ground-truth ids refer to the K* node.
    for (key, content, confidence, importance, keywords) in K_CORPUS {
        let id = e2e
            .store_knowledge_with_keywords(content, *confidence, *importance, keywords)
            .await;
        ids_by_key.insert(*key, id);
    }

    // Mark garbage nodes Dormant (M4 fixtures).
    for key in ["D1", "D2", "D3"] {
        let id = ids_by_key[key];
        e2e.store
            .transition_to_dormant(NodeId::new(id))
            .expect("transition ok");
    }

    // ── 2. Build ground truth for the full query set ─────────────���─────
    let mut relevant: Vec<EvalQuery> = Vec::new();
    let mut keyword_ground_truth_ids: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    for (text, rel_key) in M4_QUERIES {
        relevant.push(EvalQuery {
            query: text.to_string(),
            relevant_ids: vec![ids_by_key[*rel_key]],
        });
    }
    for (text, rel_key) in K_QUERIES {
        let k_id = ids_by_key[*rel_key];
        relevant.push(EvalQuery {
            query: text.to_string(),
            relevant_ids: vec![k_id],
        });
        keyword_ground_truth_ids.insert(text.to_string(), k_id);
    }

    // ── 3. BEFORE: keyword_index = false (M4 baseline behaviour) ────────
    // Plan Y: keywords remain in metadata["keywords"] only, BM25 cannot
    // see them, K* queries return 0 relevant hits.
    let before =
        run_state(&e2e, &relevant, &keyword_ground_truth_ids, false).await;

    // ── 4. AFTER: keyword_index = true (M5 behaviour) ───────────────────
    // Plan Y: keywords are folded into the BM25-indexed `object` field at
    // write-time, K* queries hit their ground truth via BM25.
    //
    // To simulate the write-time fold under the after-state cfg, we re-store
    // the K* nodes with `keyword_index=true` so that the fold is applied to
    // `object` for THIS state. The M4 nodes are unchanged (their content
    // matches M4 queries regardless of keywords).
    let m5_store = BenchE2e::new();
    let mut m5_ids: std::collections::HashMap<&str, u64> =
        std::collections::HashMap::new();
    for (key, content, confidence, importance) in M4_CORPUS {
        let id = m5_store
            .store_knowledge(content, *confidence, *importance)
            .await;
        m5_ids.insert(*key, id);
    }
    for (key, content, confidence, importance, keywords) in K_CORPUS {
        // The fold happens inside process_memory_store based on
        // self.quality().keyword_index. To exercise it under the after-state
        // cfg, we set the gate on the in-memory provider before re-storing.
        m5_store
            .store
            .set_quality(
                acowork_memory::quality::MemoryQualityConfig {
                    keyword_index: true,
                    ..Default::default()
                },
            )
            .ok();
        let id = m5_store
            .store_knowledge_with_keywords(content, *confidence, *importance, keywords)
            .await;
        m5_ids.insert(*key, id);
    }
    for key in ["D1", "D2", "D3"] {
        let id = m5_ids[key];
        m5_store
            .store
            .transition_to_dormant(NodeId::new(id))
            .expect("transition ok");
    }
    let mut m5_relevant: Vec<EvalQuery> = Vec::new();
    let mut m5_keyword_gt: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for (text, rel_key) in M4_QUERIES {
        m5_relevant.push(EvalQuery {
            query: text.to_string(),
            relevant_ids: vec![m5_ids[*rel_key]],
        });
    }
    for (text, rel_key) in K_QUERIES {
        let k_id = m5_ids[*rel_key];
        m5_relevant.push(EvalQuery {
            query: text.to_string(),
            relevant_ids: vec![k_id],
        });
        m5_keyword_gt.insert(text.to_string(), k_id);
    }
    let after =
        run_state(&m5_store, &m5_relevant, &m5_keyword_gt, true).await;

    // ── 5. Report (before/after comparison table) ──────────────────────
    println!("\n===== ADR-062 M5: keyword_index before/after =====");
    println!(
        "{:<34}{:>12}{:>12}",
        "metric", "before", "after"
    );
    println!("{:-<58}", "");
    println!("{:<34}{:>12.4}{:>12.4}", "Precision@5 (full)", before.p5, after.p5);
    println!("{:<34}{:>12.4}{:>12.4}", "Recall@5 (full)", before.r5, after.r5);
    println!("{:<34}{:>12.4}{:>12.4}", "MRR (full)", before.mrr, after.mrr);
    println!(
        "{:<34}{:>12.4}{:>12.4}",
        "keyword hit@5 rate (K* only)", before.keyword_hit_rate, after.keyword_hit_rate
    );
    println!(
        "{:<34}{:>12.4}{:>12.4}",
        "keyword any-rank rate (K* only)", before.keyword_any_rank, after.keyword_any_rank
    );
    println!("{:-<58}", "");

    // ── 6. Gate assertions (M5 evidence, ADR-062 §6.2) ─────────────────
    // Plan Y: BEFORE (keyword_index=false) must NOT retrieve K* nodes —
    // content is generic ("A custom note"), BM25 alone cannot match.
    assert!(
        before.keyword_hit_rate.abs() < 1e-6,
        "before keyword_index=false: K* hit@5 rate must be 0, got {}",
        before.keyword_hit_rate
    );
    // Plan Y: AFTER (keyword_index=true) MUST retrieve K* nodes —
    // keywords folded into object, BM25 matches.
    assert!(
        after.keyword_hit_rate > 0.0,
        "after keyword_index=true: K* hit@5 rate must be > 0, got {}",
        after.keyword_hit_rate
    );
    // Regression guard: M4 baselines must not regress.
    assert!(
        after.p5 >= before.p5 - 1e-6,
        "M5 after Precision@5 must be >= before (no regression), got after={} vs before={}",
        after.p5,
        before.p5
    );
    assert!(
        after.r5 >= before.r5 - 1e-6,
        "M5 after Recall@5 must be >= before (no regression), got after={} vs before={}",
        after.r5,
        before.r5
    );
}