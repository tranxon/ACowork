//! ADR-062 M4 (P2): before/after retrieval-quality benchmark.
//!
//! Quantifies the retrieval-quality delta of the D1 (Dormant exclusion) and
//! D2 (auto_inject min_score fix) changes against a FIXED corpus + FIXED query
//! set with ground truth, so the numbers can be archived as the M5 gate
//! evidence (ADR-062 §5.2/§5.4).
//!
//! Metrics (per §5.2):
//!   1. Retrieval Precision@5 / Recall@5 / MRR  — via `evaluate_retrieval_quality`
//!   2. Dormant-garbage-in-context ratio        — fraction of retrieved results
//!      whose node status is Dormant (must be 0 after D1)
//!   3. auto_inject injection hit rate          — fraction of auto_inject queries
//!      that return non-empty (D2 min_score 0.3 → 0.0)
//!   4. confidence/importance distribution       — methodology shown; real data is
//!      collected via `memory_write_scores` telemetry in M3.6 (out of scope here)
//!
//! The corpus and query set are deterministic:
//!   - Corpus: 5 relevant Knowledge nodes (Active), 3 garbage nodes marked
//!     Dormant via `transition_to_dormant`, 2 distractor Active nodes.
//!   - Query set: 5 fixed queries, each with a ground-truth relevant node id.
//!
//! IMPORTANT: self-contained — uses an in-memory `GrafeoStore`, never touches
//! the running Gateway / Runtime / Desktop processes or their ports.

use std::sync::Arc;

use acowork_core::tools::traits::Tool;
use acowork_core::EmbeddingProvider;

use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::retrieval_metrics::{EvalQuery, evaluate_retrieval_quality};

use acowork_memory::{MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery, NodeStatus};

use acowork_runtime::memory::MemorySessionHandle;
use acowork_runtime::tools::builtin::memory_store::MemoryStoreTool;

use grafeo_common::types::NodeId;

/// Deterministic embedding provider (same text → same 384-dim vector).
/// Mirrors the production fallback chain's deterministic behavior so retrieval
/// semantics are stable across runs.
struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-m4-bench"
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

/// Shared harness: a real in-memory `GrafeoStore` wired into a real
/// `MemorySessionHandle` (provider + embedding), ready for `MemoryStoreTool`
/// and `MemoryManager::retrieve`.
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
        let provider: Arc<dyn MemoryProvider> = store.clone();
        handle.set_provider(provider);
        Self { store, handle }
    }

    fn store_tool(&self) -> MemoryStoreTool {
        MemoryStoreTool::new("com.test.m4-bench", Some(self.handle.clone()))
    }

    /// Parse the node id back out of `MemoryStoreTool`'s result content
    /// (`"Stored fact: \"...\" (confidence: 0.80, id: 5)"`).
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

    /// Store a knowledge node via the real tool chain and return its node id.
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
}

// ============================================================================
// Fixed corpus + query set
// ============================================================================

/// Fixed corpus: (key, content, confidence, importance).
/// - `A*` = ground-truth relevant nodes, kept Active.
/// - `D*` = garbage nodes that lexically overlap the queries, then marked Dormant.
/// - `N*` = distractor Active nodes (share partial words, not ground truth).
const CORPUS: &[(&str, &str, f32, f32)] = &[
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

/// Fixed query set: (query text, ground-truth relevant corpus key).
const QUERIES: &[(&str, &str)] = &[
    ("dark mode editor", "A1"),
    ("Shanghai river home", "A2"),
    ("Acme backend engineer", "A3"),
    ("Japanese language code", "A4"),
    ("cats pets at home", "A5"),
];

/// Which corpus keys are garbage (to be marked Dormant).
const DORMANT_KEYS: &[&str] = &["D1", "D2", "D3"];
/// Which corpus keys must remain Active (relevant + distractor).
const ACTIVE_KEYS: &[&str] = &["A1", "A2", "A3", "A4", "A5", "N1", "N2"];

// ============================================================================
// Benchmark runner
// ============================================================================

/// Per-state retrieval summary.
#[derive(Default)]
struct RunSummary {
    p5: f32,
    r5: f32,
    mrr: f32,
    dormant: f32,
    ai_hit: f32,
}

/// Run the full query set under one D1 state, one auto_inject min_score,
/// and one graph-expand (pagerank) toggle.
///
/// `enable_graph_expand=false` disables PageRank boost too (it is gated on
/// `enable_graph_expand` in `manager.rs`), producing a deterministic retrieval
/// pipeline. This is REQUIRED for reproducible MRR: the default pipeline's
/// PageRank computation iterates HashMaps/HashSets (RandomState), so scores
/// for near-tied nodes flip across processes → MRR varies run-to-run while
/// P@5 / Dormant-garbage / auto_inject (membership metrics) stay stable.
async fn run_state(
    e2e: &BenchE2e,
    relevant: &[EvalQuery],
    exclude_dormant: bool,
    auto_inject_min_score: Option<f32>,
    enable_graph_expand: bool,
) -> RunSummary {
    let mut cfg = MemoryManagerConfig::default();
    cfg.quality.exclude_dormant = exclude_dormant;
    cfg.enable_graph_expand = enable_graph_expand;
    let manager = MemoryManager::new(cfg);

    let mut per_query: Vec<Vec<u64>> = Vec::with_capacity(QUERIES.len());
    let mut ai_hits = 0usize;

    for (query_text, _rel_key) in QUERIES {
        let mut q = MemoryQuery::new(query_text.to_string());
        q.abstention_enabled = false;
        q.limit = 10;
        let result = manager
            .retrieve(&*e2e.store, &mut q, Some(&DeterministicEmbedding))
            .await
            .expect("retrieve ok");
        let ids: Vec<u64> = result.memories.iter().map(|m| m.node_id).collect();

        let mut ai = MemoryQuery::auto_inject(query_text.to_string(), None);
        ai.min_score = auto_inject_min_score;
        let ai_result = manager
            .retrieve(&*e2e.store, &mut ai, Some(&DeterministicEmbedding))
            .await
            .expect("auto_inject ok");
        if !ai_result.memories.is_empty() {
            ai_hits += 1;
        }
        per_query.push(ids);
    }

    let bench = evaluate_retrieval_quality(relevant, &per_query, &[5]);
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
        dormant: compute_dormant_ratio(e2e, &per_query),
        ai_hit: ai_hits as f32 / QUERIES.len() as f32,
    }
}

/// Fraction of retrieved results whose node status is Dormant, across queries.
fn compute_dormant_ratio(e2e: &BenchE2e, per_query: &[Vec<u64>]) -> f32 {
    let mut total = 0usize;
    let mut dormant = 0usize;
    for ids in per_query {
        for id in ids {
            total += 1;
            let status = e2e
                .store
                .get_node_status(*id)
                .ok()
                .flatten()
                .unwrap_or(NodeStatus::Active);
            if status == NodeStatus::Dormant {
                dormant += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        dormant as f32 / total as f32
    }
}

// ============================================================================
// The M4 benchmark test
// ============================================================================

#[tokio::test]
async fn m4_benchmark_before_after() {
    let e2e = BenchE2e::new();

    // ── 1. Build the fixed corpus via the real write chain ──
    let mut ids_by_key: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for (key, content, confidence, importance) in CORPUS {
        let id = e2e.store_knowledge(content, *confidence, *importance).await;
        ids_by_key.insert(*key, id);
    }

    // ── 2. Mark garbage nodes Dormant (deterministic, bypasses decay timing) ──
    for key in DORMANT_KEYS {
        let id = ids_by_key[key];
        e2e.store
            .transition_to_dormant(NodeId::new(id))
            .expect("transition ok");
        let status = e2e.store.get_node_status(id).ok().flatten().unwrap();
        assert_eq!(status, NodeStatus::Dormant, "{key} must be Dormant");
    }
    for key in ACTIVE_KEYS {
        let id = ids_by_key[key];
        let status = e2e.store.get_node_status(id).ok().flatten().unwrap();
        assert_eq!(status, NodeStatus::Active, "{key} must stay Active");
    }

    // Ground truth: one relevant node per query.
    let relevant: Vec<EvalQuery> = QUERIES
        .iter()
        .map(|(text, rel_key)| EvalQuery {
            query: text.to_string(),
            relevant_ids: vec![ids_by_key[*rel_key]],
        })
        .collect();

    // ── 3. BEFORE: D1 off (old behaviour) + auto_inject min_score 0.3 ──
    // Deterministic pipeline (graph_expand=false) for reproducible MRR.
    let before = run_state(&e2e, &relevant, false, Some(0.3), false).await;

    // ── 4. AFTER: D1 on (default) + auto_inject min_score → quality.min_score ──
    let after = run_state(&e2e, &relevant, true, None, false).await;

    // ── 5. Report (before/after comparison table) ──
    println!("\n===== ADR-062 M4: before/after retrieval benchmark =====");
    println!("{:<34}{:>12}{:>12}", "metric", "before", "after");
    println!("{:-<58}", "");
    println!("{:<34}{:>12.4}{:>12.4}", "Precision@5", before.p5, after.p5);
    println!("{:<34}{:>12.4}{:>12.4}", "Recall@5", before.r5, after.r5);
    println!("{:<34}{:>12.4}{:>12.4}", "MRR", before.mrr, after.mrr);
    println!(
        "{:<34}{:>12.4}{:>12.4}",
        "Dormant garbage ratio", before.dormant, after.dormant
    );
    println!(
        "{:<34}{:>12.2}{:>12.2}",
        "auto_inject hit rate %",
        before.ai_hit * 100.0,
        after.ai_hit * 100.0
    );
    println!("{:-<58}", "");

    // ── 6. Gate assertions (M5 evidence, ADR-062 §5.2) ──
    // D1 guarantee: after must have zero Dormant garbage in context.
    assert!(
        after.dormant.abs() < 1e-6,
        "after D1: Dormant garbage ratio must be 0, got {}",
        after.dormant
    );
    // The benchmark must be non-vacuous: before D1, garbage is actually present.
    assert!(
        before.dormant > 0.0,
        "before D1: garbage ratio must be > 0 (corpus must contain retrievable Dormant nodes), got {}",
        before.dormant
    );
    // Precision must not regress after D1.
    assert!(
        after.p5 >= before.p5 - 1e-6,
        "after D1 Precision@5 must be >= before, got {after} vs {before}",
        after = after.p5,
        before = before.p5
    );
    // D2: auto_inject hit rate must improve (min_score 0.3 was silently
    // filtering everything on the RRF score scale).
    assert!(
        after.ai_hit >= before.ai_hit,
        "auto_inject hit rate must not regress, got {:.2}% vs {:.2}%",
        after.ai_hit * 100.0,
        before.ai_hit * 100.0
    );
}
