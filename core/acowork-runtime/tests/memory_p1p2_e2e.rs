//! P1/P2 memory data-quality end-to-end tests.
//!
//! Covers every functional branch affected by the P1 (data-correctness) and
//! P2 (behavior-alignment + data-completeness) memory workstreams, driven
//! through the REAL component chain — no mocks:
//!
//! - Write side: `MemoryStoreTool` → `MemoryProvider::process_memory_store`
//!   → in-memory `GrafeoStore` (typed privacy/importance/keywords/source).
//! - Read side: `export_nodes_filtered` (privacy filtering), `get_knowledge`
//!   (typed field round-trip), `MemoryManager::retrieve` (abstention prompt,
//!   HintType::Identity reaches all labels).
//! - Forgetting: `run_decay_scan` / `get_dormant_candidates` (FLOOR, day
//!   unit, BOOST_CAP), `run_offline_consolidation_with_generalization`
//!   → `run_episodic_cleanup` (three-rule policy).
//! - Graph: `GraphExpandConfig` / `get_expand_thresholds` (G11), edge weight
//!   auto-computation via `create_memory_edge` + `compute_edge_weight` (G12).
//!
//! Matrix (one test per affected branch):
//!   A1  store_knowledge_persists_privacy_importance_keywords   (P1-2/P1-3)
//!   A2  store_knowledge_default_privacy_personal               (P1-2 default)
//!   A3  store_autobio_source_persisted                         (P2 G7)
//!   A4  store_autobio_default_source                           (P2 G7 default)
//!   B1  export_filters_private_knowledge                       (P1-2 export)
//!   B2  export_includes_private_when_requested                 (P1-2 export)
//!   C1  retrieve_empty_injects_abstention_prompt               (P2 G9)
//!   C2  retrieve_identity_hint_reaches_knowledge               (P2 G10)
//!   C3  retrieve_excludes_dormant_keeps_pending                (ADR-062 D1)
//!   D1  decay_formula_floor_dayunit_cap                        (P1-1 formula)
//!   D2  decay_scan_high_importance_survives                    (P1-1 scan)
//!   D3  decay_scan_low_importance_dormant                      (P1-1 scan)
//!   D4  episodic_cleanup_three_rules                           (P2 G13)
//!   E1  graph_expand_thresholds_aligned                        (P2 G11)
//!   E2  edge_weight_auto_computed                              (P2 G12)
//!   E3  edge_weight_explicit_not_overridden                    (P2 G12)
//!   E4  edge_weight_no_confidence_skips                        (P2 G12)
//!
//! IMPORTANT: these tests are fully self-contained — they use an in-memory
//! `GrafeoStore::new_in_memory()` and never touch the running Gateway /
//! Runtime / Desktop processes, their data dirs, or the :19875/:19876 ports.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use acowork_core::packaging::PackageOptions;
use acowork_core::tools::traits::Tool;
use acowork_core::EmbeddingProvider;

use acowork_grafeo::forgetting::compute_decay_score;
use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::spreading::{GraphExpandConfig, get_expand_thresholds};

use acowork_memory::{
    DecayConfig, HintType, MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery,
    OfflineConsolidationConfig, PrivacyLevel, labels,
};

use acowork_runtime::memory::MemorySessionHandle;
use acowork_runtime::tools::builtin::memory_store::MemoryStoreTool;

use grafeo_common::types::{NodeId, Timestamp, Value};

/// Deterministic embedding provider (same text → same 384-dim vector).
/// Mirrors the production fallback chain's deterministic behavior so recall
/// and dedup semantics are stable across runs.
struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-p1p2-e2e"
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

/// Shared e2e harness: a real in-memory `GrafeoStore` wired into a real
/// `MemorySessionHandle` (provider + embedding), ready for `MemoryStoreTool`
/// and `MemoryManager::retrieve`.
struct MemoryE2e {
    store: Arc<GrafeoStore>,
    handle: Arc<MemorySessionHandle>,
}

impl MemoryE2e {
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
        MemoryStoreTool::new("com.test.agent", Some(self.handle.clone()))
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
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("cannot parse node id from: {content}"))
    }
}

/// Microseconds timestamp for `days` days ago (for decay / episodic-cleanup
/// age control).
fn micros_days_ago(days: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_micros() as i64;
    now - days * 86_400 * 1_000_000
}

// ============================================================================
// A series — write-path persistence (P1-2 typed fields, P1-3 keywords,
// P2 G7 autobiographical source)
// ============================================================================

/// A1 (P1-2/P1-3): explicit `privacy`, `importance`, and `keywords` passed
/// through `MemoryStoreTool` are persisted on the grafeo `KnowledgeNode`.
#[tokio::test]
async fn store_knowledge_persists_privacy_importance_keywords() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    let result = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "User lives in Shanghai",
                "privacy": "public",
                "importance": 0.9,
                "keywords": ["shanghai", "location"],
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(result.ok, "tool failed: {:?}", result.error);

    let id = MemoryE2e::node_id_from_tool_result(&result.content);
    let node = e2e
        .store
        .get_knowledge(NodeId::new(id))
        .expect("get_knowledge ok")
        .expect("knowledge exists");

    assert_eq!(node.privacy, PrivacyLevel::Public);
    assert!((node.importance - 0.9).abs() < 1e-6, "importance = {}", node.importance);

    let keywords = node
        .metadata
        .get("keywords")
        .and_then(|v| v.as_array())
        .expect("keywords array persisted");
    let strings: Vec<&str> = keywords.iter().filter_map(|v| v.as_str()).collect();
    assert!(strings.contains(&"shanghai") && strings.contains(&"location"));
}

/// A2 (P1-2 default): without explicit privacy/importance, the conservative
/// defaults `Personal` and `0.5` are applied.
#[tokio::test]
async fn store_knowledge_default_privacy_personal() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    let result = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "User prefers dark mode",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(result.ok, "tool failed: {:?}", result.error);

    let id = MemoryE2e::node_id_from_tool_result(&result.content);
    let node = e2e
        .store
        .get_knowledge(NodeId::new(id))
        .expect("get_knowledge ok")
        .expect("knowledge exists");

    assert_eq!(node.privacy, PrivacyLevel::Personal);
    assert!((node.importance - 0.5).abs() < 1e-6, "importance = {}", node.importance);
}

/// A3 (P2 G7): explicit autobiographical `source` survives the write path.
#[tokio::test]
async fn store_autobio_source_persisted() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    let result = tool
        .execute(
            serde_json::json!({
                "category": "autobiographical",
                "content": "I tend to give conclusions first",
                "aspect": "preference",
                "key": "style",
                "source": "important_event",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(result.ok, "tool failed: {:?}", result.error);

    let node = e2e
        .store
        .find_autobiographical_by_key("style")
        .expect("find ok")
        .expect("autobio exists");
    assert_eq!(node.source.as_str(), "important_event");
}

/// A4 (P2 G7 default): without an explicit `source`, `"user_statement"` is used.
#[tokio::test]
async fn store_autobio_default_source() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    let result = tool
        .execute(
            serde_json::json!({
                "category": "autobiographical",
                "content": "I am an AI assistant",
                "aspect": "identity",
                "key": "agent_name",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(result.ok, "tool failed: {:?}", result.error);

    let node = e2e
        .store
        .find_autobiographical_by_key("agent_name")
        .expect("find ok")
        .expect("autobio exists");
    assert_eq!(node.source.as_str(), "user_statement");
}

// ============================================================================
// B series — export privacy filtering (P1-2 export)
// ============================================================================

/// B1 (P1-2 export): `export_nodes_filtered` with default `PackageOptions`
/// excludes `Personal`/`Sensitive` knowledge, keeping only `Public`.
#[tokio::test]
async fn export_filters_private_knowledge() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    // public knowledge
    let r1 = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "Company is called ACowork",
                "privacy": "public",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(r1.ok);

    // personal (default) knowledge — should be excluded by default export
    let r2 = tool
        .execute(
            serde_json::json!({
                "category": "preference",
                "content": "User likes green tea",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(r2.ok);

    let filtered = e2e
        .store
        .export_nodes_filtered(&PackageOptions::default())
        .expect("export ok");

    let knowledge: Vec<_> = filtered
        .iter()
        .filter(|n| n.label == labels::KNOWLEDGE)
        .collect();
    assert_eq!(knowledge.len(), 1, "only public knowledge exported");

    let data = knowledge[0].data.as_object().expect("data is object");
    let privacy = data.get("privacy").and_then(|v| v.as_str()).unwrap_or_default();
    assert_eq!(privacy, "Public");
}

/// B2 (P1-2 export): with `include_private_knowledge = true`, private
/// knowledge is included.
#[tokio::test]
async fn export_includes_private_when_requested() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    let r1 = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "Company is called ACowork",
                "privacy": "public",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(r1.ok);

    let r2 = tool
        .execute(
            serde_json::json!({
                "category": "preference",
                "content": "User likes green tea",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(r2.ok);

    let options = PackageOptions {
        include_private_knowledge: true,
        ..PackageOptions::default()
    };
    let filtered = e2e
        .store
        .export_nodes_filtered(&options)
        .expect("export ok");

    let knowledge: Vec<_> = filtered
        .iter()
        .filter(|n| n.label == labels::KNOWLEDGE)
        .collect();
    assert_eq!(knowledge.len(), 2, "both public and private knowledge exported");
}

// ============================================================================
// C series — retrieval behavior (P2 G9 abstention, P2 G10 Identity labels)
// ============================================================================

/// C1 (P2 G9): an empty result set with `abstention_enabled` injects the
/// built-in abstention prompt and reports `abstention_triggered`.
#[tokio::test]
async fn retrieve_empty_injects_abstention_prompt() {
    let e2e = MemoryE2e::new();
    let manager = MemoryManager::new(MemoryManagerConfig::default());

    let mut query = MemoryQuery::new("quantum entanglement of hedgehogs");
    query.abstention_enabled = true;

    let result = manager
        .retrieve(&*e2e.store, &mut query, Some(&DeterministicEmbedding))
        .await
        .expect("retrieve ok");

    assert!(result.memories.is_empty(), "expected no memories on empty store");
    assert!(result.metrics.abstention_triggered, "abstention triggered");
    let prompt = result.abstention_prompt.expect("abstention prompt injected");
    assert!(!prompt.is_empty(), "abstention prompt is non-empty");
    assert!(
        prompt.contains("not sure")
            || prompt.contains("I don't know")
            || prompt.contains("do not know"),
        "prompt signals abstention: {prompt}"
    );
}

/// C2 (P2 G10): `HintType::Identity` searches all labels, so a `Knowledge`
/// node is reachable through an Identity hint.
#[tokio::test]
async fn retrieve_identity_hint_reaches_knowledge() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    let result = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "User lives in Shanghai",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(result.ok, "tool failed: {:?}", result.error);

    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let mut query = MemoryQuery::new("User lives in Shanghai");
    query.hint_type = HintType::Identity;
    query.abstention_enabled = false;

    let retrieved = manager
        .retrieve(&*e2e.store, &mut query, Some(&DeterministicEmbedding))
        .await
        .expect("retrieve ok");

    assert!(
        retrieved.memories.iter().any(|m| m.label == labels::KNOWLEDGE),
        "Identity hint must reach Knowledge nodes, got: {:?}",
        retrieved
            .memories
            .iter()
            .map(|m| (m.label.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );
}

/// C3 (ADR-062 D1): retrieval excludes Dormant nodes but keeps Active and
/// Pending nodes.
///
/// A node is stored (Active), verified retrievable, aged + decayed to
/// Dormant, and must then disappear from the same query's results. A
/// separate low-confidence node kept as Pending must remain retrievable
/// (Pending nodes participate in retrieval and are naturally down-ranked
/// by confidence — ADR-062 §3.3).
#[tokio::test]
async fn retrieve_excludes_dormant_keeps_pending() {
    let e2e = MemoryE2e::new();
    let tool = e2e.store_tool();

    // ── Node A: high-confidence Active, low importance → decays to Dormant ──
    let a = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "User keeps a travel journal about Tokyo",
                "confidence": 0.95,
                "importance": 0.1,
            }),
            None,
        )
        .await
        .expect("tool execute A");
    assert!(a.ok, "tool A failed: {:?}", a.error);
    let a_id = MemoryE2e::node_id_from_tool_result(&a.content);

    // ── Node B: low-confidence → Pending, stays retrievable ──
    let b = tool
        .execute(
            serde_json::json!({
                "category": "fact",
                "content": "User may prefer cycling to work",
                "confidence": 0.6,
                "importance": 0.5,
            }),
            None,
        )
        .await
        .expect("tool execute B");
    assert!(b.ok, "tool B failed: {:?}", b.error);
    let b_id = MemoryE2e::node_id_from_tool_result(&b.content);

    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let query = |text: &str| {
        let mut q = MemoryQuery::new(text.to_string());
        q.abstention_enabled = false;
        q
    };

    // Sanity: both retrievable before decay (A is Active, B is Pending).
    let before_a = manager
        .retrieve(
            &*e2e.store,
            &mut query("User keeps a travel journal about Tokyo"),
            Some(&DeterministicEmbedding),
        )
        .await
        .expect("retrieve A before");
    assert!(
        before_a.memories.iter().any(|m| m.node_id == a_id),
        "node A must be retrievable while Active"
    );

    let before_b = manager
        .retrieve(
            &*e2e.store,
            &mut query("User may prefer cycling to work"),
            Some(&DeterministicEmbedding),
        )
        .await
        .expect("retrieve B before");
    assert!(
        before_b.memories.iter().any(|m| m.node_id == b_id),
        "node B must be retrievable while Pending"
    );

    // Age A 30 days + decay scan → Dormant.
    e2e.store.db().set_node_property(
        NodeId::new(a_id),
        "created_at",
        Value::from(Timestamp::from_micros(micros_days_ago(30))),
    );
    let transitioned = e2e
        .store
        .run_decay_scan(&DecayConfig::default())
        .expect("decay scan ok");
    assert_eq!(transitioned, 1, "node A must transition to Dormant");

    // After: A excluded, B (Pending) still returned.
    let after_a = manager
        .retrieve(
            &*e2e.store,
            &mut query("User keeps a travel journal about Tokyo"),
            Some(&DeterministicEmbedding),
        )
        .await
        .expect("retrieve A after");
    assert!(
        !after_a.memories.iter().any(|m| m.node_id == a_id),
        "Dormant node A must be excluded from retrieval, got: {:?}",
        after_a
            .memories
            .iter()
            .map(|m| (m.node_id, m.content.clone()))
            .collect::<Vec<_>>()
    );

    let after_b = manager
        .retrieve(
            &*e2e.store,
            &mut query("User may prefer cycling to work"),
            Some(&DeterministicEmbedding),
        )
        .await
        .expect("retrieve B after");
    assert!(
        after_b.memories.iter().any(|m| m.node_id == b_id),
        "Pending node B must remain retrievable, got: {:?}",
        after_b
            .memories
            .iter()
            .map(|m| (m.node_id, m.content.clone()))
            .collect::<Vec<_>>()
    );
}

// ============================================================================
// D series — forgetting (P1-1 decay formula/scan, P2 G13 episodic cleanup)
// ============================================================================

/// D1 (P1-1 formula): verifies the three decay-formula corrections:
/// FLOOR lower bound, recency in *days* (not hours), and BOOST_CAP.
#[test]
fn decay_formula_floor_dayunit_cap() {
    let cfg = DecayConfig::default();

    // (a) FLOOR: even an extremely old, never-accessed node scores
    //     `importance * floor`, never 0 (lower bound 0.05).
    let very_old = compute_decay_score(&cfg, 0.5, 100_000.0, 0);
    let floor_expected = 0.5 * cfg.floor;
    assert!(
        (very_old - floor_expected).abs() < 1e-6,
        "FLOOR must bind: got {very_old}, expected ~{floor_expected}"
    );

    // (b) DAY unit: 1 day of decay → exp(-lambda * 1) ≈ 0.9704 (lambda 0.03).
    //     If recency were in hours this would be exp(-0.72) ≈ 0.4868.
    let one_day = compute_decay_score(&cfg, 1.0, 1.0, 0);
    let day_expected = 1.0 * (-0.03_f64).exp();
    assert!(
        (one_day - day_expected as f32).abs() < 1e-4,
        "recency must be in days: got {one_day}, expected ~{day_expected}"
    );
    assert!(
        one_day > 0.8,
        "day-unit recency keeps recent memory hot (got {one_day})"
    );

    // (c) BOOST_CAP: many recent accesses are capped at `boost_cap`.
    let many_hits = compute_decay_score(&cfg, 1.0, 10.0, 10_000);
    let capped_access = cfg.boost_cap; // min(access_per_hit * hits, boost_cap)
    let capped_expected = 1.0 * (((-0.03_f64 * 10.0).exp() + capped_access as f64) as f32).min(1.0);
    assert!(
        (many_hits - capped_expected).abs() < 1e-4,
        "BOOST_CAP must cap access: got {many_hits}, expected ~{capped_expected}"
    );
    assert!(many_hits <= 1.0, "score never exceeds 1.0");
}

/// D2 (P1-1 scan): high-importance knowledge survives `run_decay_scan` even
/// after 30 days without access.
#[tokio::test]
async fn decay_scan_high_importance_survives() {
    let e2e = MemoryE2e::new();

    let id = e2e
        .store
        .store_node(
            labels::KNOWLEDGE,
            [
                ("content", Value::from("User's critical preference")),
                ("importance", Value::from(0.9f64)),
            ],
        )
        .expect("store_node ok");
    e2e.store
        .db()
        .set_node_property(id, "created_at", Value::from(Timestamp::from_micros(micros_days_ago(30))));

    let transitioned = e2e
        .store
        .run_decay_scan(&DecayConfig::default())
        .expect("decay scan ok");
    assert_eq!(transitioned, 0, "high importance must survive decay");

    let node = e2e.store.db().get_node(id).expect("node exists");
    let status = node
        .get_property("status")
        .and_then(Value::as_str)
        .unwrap_or("Active");
    assert_eq!(status, "Active", "high-importance node stays Active");
}

/// D3 (P1-1 scan): low-importance knowledge goes Dormant after 30 days.
#[tokio::test]
async fn decay_scan_low_importance_dormant() {
    let e2e = MemoryE2e::new();

    let id = e2e
        .store
        .store_node(
            labels::KNOWLEDGE,
            [
                ("content", Value::from("A trivial detail")),
                ("importance", Value::from(0.1f64)),
            ],
        )
        .expect("store_node ok");
    e2e.store
        .db()
        .set_node_property(id, "created_at", Value::from(Timestamp::from_micros(micros_days_ago(30))));

    let transitioned = e2e
        .store
        .run_decay_scan(&DecayConfig::default())
        .expect("decay scan ok");
    assert_eq!(transitioned, 1, "low importance must be transitioned");

    let node = e2e.store.db().get_node(id).expect("node exists");
    let status = node
        .get_property("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(status, "Dormant", "low-importance node goes Dormant");
}

/// D4 (P2 G13): `run_episodic_cleanup` applies all three rules —
/// consolidated>7d → Dormant; unconsolidated>14d + low importance → Dormant;
/// unconsolidated>14d + high importance → kept Active + `needs_consolidation`.
#[tokio::test]
async fn episodic_cleanup_three_rules() {
    let e2e = MemoryE2e::new();

    // Rule 1: consolidated episode, 30 days old → Dormant.
    let r1 = e2e
        .store
        .store_node(
            labels::EPISODIC,
            [("content", Value::from("consolidated old episode"))],
        )
        .expect("store_node ok");
    e2e.store
        .db()
        .set_node_property(r1, "consolidated", Value::from(true));
    e2e.store
        .db()
        .set_node_property(r1, "created_at", Value::from(Timestamp::from_micros(micros_days_ago(30))));

    // Rule 2: unconsolidated, 30 days old, low importance → Dormant.
    let r2 = e2e
        .store
        .store_node(
            labels::EPISODIC,
            [("content", Value::from("unconsolidated low value episode"))],
        )
        .expect("store_node ok");
    e2e.store
        .db()
        .set_node_property(r2, "consolidated", Value::from(false));
    e2e.store
        .db()
        .set_node_property(r2, "importance", Value::from(0.1f64));
    e2e.store
        .db()
        .set_node_property(r2, "created_at", Value::from(Timestamp::from_micros(micros_days_ago(30))));

    // Rule 3: unconsolidated, 30 days old, high importance → kept + flagged.
    let r3 = e2e
        .store
        .store_node(
            labels::EPISODIC,
            [("content", Value::from("unconsolidated important episode"))],
        )
        .expect("store_node ok");
    e2e.store
        .db()
        .set_node_property(r3, "consolidated", Value::from(false));
    e2e.store
        .db()
        .set_node_property(r3, "importance", Value::from(0.9f64));
    e2e.store
        .db()
        .set_node_property(r3, "created_at", Value::from(Timestamp::from_micros(micros_days_ago(30))));

    let result = e2e
        .store
        .run_offline_consolidation_with_generalization(
            &OfflineConsolidationConfig::default(),
            None,
            None,
            None,
        )
        .await
        .expect("consolidation ok");

    assert_eq!(result.episodic_cleaned, 2, "rules 1+2 dormancy count");

    let status = |id: NodeId| -> String {
        e2e.store
            .db()
            .get_node(id)
            .expect("node exists")
            .get_property("status")
            .and_then(Value::as_str)
            .unwrap_or("Active")
            .to_string()
    };

    assert_eq!(status(r1), "Dormant", "rule 1: consolidated old → Dormant");
    assert_eq!(status(r2), "Dormant", "rule 2: unconsolidated low value → Dormant");
    assert_eq!(status(r3), "Active", "rule 3: unconsolidated high value → kept Active");

    let r3_node = e2e.store.db().get_node(r3).expect("node exists");
    let needs_consolidation = r3_node
        .get_property("needs_consolidation")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(needs_consolidation, "rule 3: flagged for priority consolidation");
}

// ============================================================================
// E series — graph behavior (P2 G11 expand thresholds, P2 G12 edge weight)
// ============================================================================

/// E1 (P2 G11): default `GraphExpandConfig` and `"s"` branch use
/// `[0.1, 0.15, 0.2]`; the `"r"` branch stays at `[0.1, 0.12, 0.15]`.
#[test]
fn graph_expand_thresholds_aligned() {
    let default_thresholds = GraphExpandConfig::default().early_stop_thresholds;
    assert_eq!(
        default_thresholds,
        vec![0.1, 0.15, 0.2],
        "default expand thresholds must be [0.1, 0.15, 0.2]"
    );

    let s_thresholds = get_expand_thresholds("s");
    assert_eq!(
        s_thresholds,
        vec![0.1, 0.15, 0.2],
        "'s' branch must match default [0.1, 0.15, 0.2]"
    );

    let r_thresholds = get_expand_thresholds("r");
    assert_eq!(
        r_thresholds,
        vec![0.1, 0.12, 0.15],
        "'r' branch keeps [0.1, 0.12, 0.15]"
    );
}

/// E2 (P2 G12): `create_memory_edge` without an explicit weight computes it
/// from the endpoints' confidence via `compute_edge_weight(avg, days=0)`.
#[tokio::test]
async fn edge_weight_auto_computed() {
    let e2e = MemoryE2e::new();

    let a = e2e
        .store
        .store_node(
            labels::KNOWLEDGE,
            [
                ("content", Value::from("node A")),
                ("confidence", Value::from(0.8f64)),
            ],
        )
        .expect("store_node ok");
    let b = e2e
        .store
        .store_node(
            labels::KNOWLEDGE,
            [
                ("content", Value::from("node B")),
                ("confidence", Value::from(0.6f64)),
            ],
        )
        .expect("store_node ok");

    e2e.store
        .create_memory_edge(a, b, "REFERENCES", Vec::new())
        .expect("create edge ok");

    let edges = e2e
        .store
        .get_edges_by_type(a, "REFERENCES")
        .expect("get edges ok");
    assert_eq!(edges.len(), 1, "one edge created");

    let (_, _, props) = &edges[0];
    let weight = props
        .iter()
        .find(|(k, _)| k == "weight")
        .map(|(_, v)| v.as_float64().expect("weight is float"))
        .expect("auto-computed weight present");

    // Hardcoded expectation (NOT derived from the function under test):
    // compute_edge_weight(0.7, 0.0) = min(0.8, 0.7 * exp(0)) = 0.7.
    // Deriving the expected value from `compute_edge_weight` itself would let
    // a broken implementation pass (mutation-tested smell).
    let expected = 0.7f64;
    assert!(
        (weight - expected).abs() < 1e-6,
        "auto weight {weight} != expected {expected}"
    );
}

/// E3 (P2 G12): an explicit `weight` property is honored (not overridden by
/// auto-computation).
#[tokio::test]
async fn edge_weight_explicit_not_overridden() {
    let e2e = MemoryE2e::new();

    let a = e2e
        .store
        .store_node(
            labels::KNOWLEDGE,
            [
                ("content", Value::from("node A")),
                ("confidence", Value::from(0.9f64)),
            ],
        )
        .expect("store_node ok");
    let b = e2e
        .store
        .store_node(
            labels::KNOWLEDGE,
            [
                ("content", Value::from("node B")),
                ("confidence", Value::from(0.9f64)),
            ],
        )
        .expect("store_node ok");

    e2e.store
        .create_memory_edge(
            a,
            b,
            "REFERENCES",
            vec![("weight".to_string(), Value::from(0.2f64))],
        )
        .expect("create edge ok");

    let edges = e2e
        .store
        .get_edges_by_type(a, "REFERENCES")
        .expect("get edges ok");
    let (_, _, props) = &edges[0];
    let weight = props
        .iter()
        .find(|(k, _)| k == "weight")
        .map(|(_, v)| v.as_float64().expect("weight is float"))
        .expect("explicit weight present");
    assert!(
        (weight - 0.2).abs() < 1e-6,
        "explicit weight 0.2 must not be overridden, got {weight}"
    );
}

/// E4 (P2 G12): when neither endpoint carries a `confidence` property, weight
/// auto-computation is skipped (no `weight` property is written).
#[tokio::test]
async fn edge_weight_no_confidence_skips() {
    let e2e = MemoryE2e::new();

    let a = e2e
        .store
        .store_node(labels::KNOWLEDGE, [("content", Value::from("node A"))])
        .expect("store_node ok");
    let b = e2e
        .store
        .store_node(labels::KNOWLEDGE, [("content", Value::from("node B"))])
        .expect("store_node ok");

    e2e.store
        .create_memory_edge(a, b, "REFERENCES", Vec::new())
        .expect("create edge ok");

    let edges = e2e
        .store
        .get_edges_by_type(a, "REFERENCES")
        .expect("get edges ok");
    let (_, _, props) = &edges[0];
    assert!(
        !props.iter().any(|(k, _)| k == "weight"),
        "no confidence → weight auto-computation skipped"
    );
}
