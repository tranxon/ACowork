//! Memory end-to-end tests (ADR-057 P0).
//!
//! Simulates the Desktop App's memory-panel flow over the exact HTTP
//! interface the Gateway reverse-proxies to the Runtime:
//!   GET  /memory/nodes          — panel list (pagination / type filter)
//!   GET  /memory/nodes/{nid}    — node detail view
//!   GET  /memory/stats          — panel statistics card
//!   GET  /memory/graph          — memory graph view
//!   POST /memory/consolidate    — panel "consolidate now" button
//!
//! The write path is NOT a test stub: it exercises the real production
//! compaction landing chain —
//!   `EpisodeDistiller::write_summary_to_provider`
//!     → `parse_compact_output` (5-field triples per ADR-057 D7/D8)
//!     → `MemoryManager::record_distilled`
//!     → `GrafeoStore::ingest_distilled_triples`
//!     → instant pipeline (`process_memory_store`) + `SOURCED_FROM` edges
//! — against a real in-memory GrafeoStore wired into a real
//! `RuntimeHttpServer` listening on 127.0.0.1 (random port). The only
//! simulated part is the LLM: its compact-model output is a fixture
//! string (LLM prompt/parse contracts are unit-tested in
//! `episode_distill.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::EmbeddingProvider;
use acowork_grafeo::grafeo::GrafeoStore;
use acowork_memory::MemoryProvider;

use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::episode_distill::EpisodeDistiller;
use acowork_runtime::http::server::{
    RuntimeHttpServer, SharedConsolidationTimer, SharedMemoryStore, SharedSessionManagerSlot,
};
use acowork_runtime::http::{
    SharedDegradation, SharedDispatchSender, SharedEmbedDimension, SharedMqttClientSlot,
};
use acowork_runtime::usecases::GrafeoMemoryAdapter;

/// Deterministic embedding provider standing in for the production
/// `FallbackEmbeddingProvider` (ONNX → Ollama → Remote chain).
///
/// Same text → same 384-dim vector (cosine 1.0), different text →
/// unrelated vector. This keeps the dedup semantics of the landing
/// pipeline realistic: identical triples deduplicate, distinct triples
/// do not. (The "no provider" degradation path — nodes land without
/// vectors and dedup is skipped — is covered in `acowork-grafeo`
/// `consolidation::distill` unit tests.)
struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-test"
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

/// Compact-model output fixture: one high-confidence Fact (Active path)
/// and one low-confidence Preference (Pending path).
const COMPACT_OUTPUT: &str = "<summary>\
User worked on the ADR-057 memory distillation landing pipeline and verified it end to end.\
</summary>\
<triples>\
User | requested | context compaction fix | 0.95 | Fact\n\
User | might prefer | tea | 0.6 | Preference\n\
</triples>";

struct MemoryE2e {
    port: u16,
    store: Arc<GrafeoStore>,
    _temp_dir: std::path::PathBuf,
}

async fn spawn_memory_e2e_server(tag: &str) -> MemoryE2e {
    let temp_dir = std::env::temp_dir().join(format!(
        "acowork-test-memory-e2e-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Real in-memory GrafeoStore, shared between the write path
    // (MemoryProvider) and the HTTP read path (MemoryAdminService).
    let store = Arc::new(GrafeoStore::new_in_memory().expect("in-memory store"));
    let memory_store: SharedMemoryStore =
        Arc::new(std::sync::RwLock::new(Some(store.clone())));

    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
    let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
    let embed_dim: SharedEmbedDimension = Arc::new(std::sync::RwLock::new(0));
    let degraded: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
    let consolidation_timer: SharedConsolidationTimer =
        Arc::new(std::sync::RwLock::new(None));
    let session_manager_slot: SharedSessionManagerSlot =
        Arc::new(tokio::sync::RwLock::new(None));

    let server = RuntimeHttpServer::start(
        temp_dir.clone(),
        "com.test.agent".to_string(),
        snapshots,
        latest,
        dispatch_tx,
        embed_dim.clone(),
        degraded,
        mqtt_client,
        Arc::new(tokio::sync::Mutex::new(None)), // session metadata (unused)
        Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            GrafeoMemoryAdapter::new(memory_store, embed_dim),
        )))),
        Arc::new(tokio::sync::Mutex::new(None)), // workspace query
        Arc::new(tokio::sync::Mutex::new(None)), // workspace mutation
        Arc::new(tokio::sync::Mutex::new(None)), // agent tools
        Arc::new(tokio::sync::Mutex::new(None)), // agent config
        Arc::new(tokio::sync::Mutex::new(None)), // attachment
        Arc::new(tokio::sync::Mutex::new(None)), // session config
        consolidation_timer,
        Arc::new(std::sync::RwLock::new(None)), // rag provider
        Arc::new(tokio::sync::Mutex::new(None)), // debug service
        Arc::new(std::sync::RwLock::new(
            acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        )),
        session_manager_slot,
    )
    .await
    .expect("runtime http server should start");

    MemoryE2e {
        port: server.port,
        store,
        _temp_dir: temp_dir,
    }
}

async fn get_json(e2e: &MemoryE2e, path: &str) -> serde_json::Value {
    let url = format!("http://127.0.0.1:{}{}", e2e.port, path);
    let resp = reqwest::get(&url).await.expect("request succeeds");
    assert!(
        resp.status().is_success(),
        "GET {path} failed with {}",
        resp.status()
    );
    resp.json().await.expect("valid json body")
}

async fn post_json(e2e: &MemoryE2e, path: &str, body: serde_json::Value) -> serde_json::Value {
    let url = format!("http://127.0.0.1:{}{}", e2e.port, path);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("request succeeds");
    assert!(
        resp.status().is_success(),
        "POST {path} failed with {}",
        resp.status()
    );
    resp.json().await.expect("valid json body")
}

/// Full desktop-panel flow: empty state → compaction landing → list /
/// detail / stats / graph → consolidate.
#[tokio::test]
async fn desktop_memory_panel_flow_after_distillation_landing() {
    let e2e = spawn_memory_e2e_server("panel").await;

    // ── 1. Empty state (panel opens on a fresh agent) ────────────────────
    let stats = get_json(&e2e, "/memory/stats").await;
    assert_eq!(stats["total_nodes"].as_u64(), Some(0));

    // ── 2. Simulate a compaction distillation landing ────────────────────
    // This is the production write path for compacted summaries (ADR-011 /
    // ADR-057): parse 5-field triples → record_distilled →
    // GrafeoStore::ingest_distilled_triples → instant pipeline.
    let provider: Arc<dyn MemoryProvider> = e2e.store.clone();
    EpisodeDistiller::write_summary_to_provider(
        COMPACT_OUTPUT,
        "sess-e2e",
        &Some(provider),
        Some(&DeterministicEmbedding),
    )
    .await;

    // ── 3. Panel list: 1 Episodic + 2 Knowledge nodes ───────────────────
    let list = get_json(&e2e, "/memory/nodes?page=1&size=50").await;
    assert_eq!(list["total"].as_u64(), Some(3), "episode + 2 knowledge nodes");
    let nodes = list["nodes"].as_array().expect("nodes array");

    let episodes: Vec<&serde_json::Value> = nodes
        .iter()
        .filter(|n| n["node_type"] == "Episodic")
        .collect();
    let knowledge: Vec<&serde_json::Value> = nodes
        .iter()
        .filter(|n| n["node_type"] == "Knowledge")
        .collect();
    assert_eq!(episodes.len(), 1, "exactly one distilled episode");
    assert_eq!(knowledge.len(), 2, "one knowledge node per triple");
    let episode_id = episodes[0]["node_id"].as_u64().expect("episode node id");

    // ── 4. Type filter (panel's "Knowledge" tab) ─────────────────────────
    let kn_only = get_json(&e2e, "/memory/nodes?type=Knowledge").await;
    assert_eq!(kn_only["total"].as_u64(), Some(2));
    assert!(
        kn_only["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["node_type"] == "Knowledge"),
        "type filter must hold"
    );

    // ── 5. Sub-type filter (Fact tab): only the Fact triple ──────────────
    let facts = get_json(&e2e, "/memory/nodes?type=Knowledge&sub_type=Fact").await;
    assert_eq!(facts["total"].as_u64(), Some(1));

    // ── 6. Node detail: Active dispatch + source_episode_id traceability ─
    let mut active_kn: Option<u64> = None;
    let mut pending_kn: Option<u64> = None;
    for n in kn_only["nodes"].as_array().unwrap() {
        match n["status"].as_str() {
            Some("Active") => active_kn = n["node_id"].as_u64(),
            Some("Pending") => pending_kn = n["node_id"].as_u64(),
            other => panic!("unexpected knowledge status: {other:?}"),
        }
    }
    let active_kn = active_kn.expect("0.95-confidence Fact must dispatch Active");
    let pending_kn = pending_kn.expect("0.6-confidence Preference must dispatch Pending");

    let detail = get_json(&e2e, &format!("/memory/nodes/{active_kn}")).await;
    assert_eq!(detail["found"].as_bool(), Some(true));
    assert_eq!(detail["status"].as_str(), Some("Active"));
    assert_eq!(detail["sub_type"].as_str(), Some("Fact"));
    // D4 reverse link: the knowledge node must trace back to its episode.
    // Property values are serialized as tagged grafeo Values
    // (e.g. {"Int64": 7}) — that is the existing admin contract the
    // desktop panel consumes.
    let source_prop = &detail["properties"]["source_episode_id"];
    let source = source_prop
        .as_u64()
        .or_else(|| source_prop.as_i64().map(|v| v as u64))
        .or_else(|| source_prop["Int64"].as_u64())
        .or_else(|| source_prop["Int64"].as_i64().map(|v| v as u64))
        .expect("source_episode_id present in node properties");
    assert_eq!(source, episode_id, "knowledge must link back to its episode");

    let pending_detail = get_json(&e2e, &format!("/memory/nodes/{pending_kn}")).await;
    assert_eq!(pending_detail["status"].as_str(), Some("Pending"));

    // ── 7. Stats card reflects the landing ──────────────────────────────
    let stats = get_json(&e2e, "/memory/stats").await;
    assert_eq!(stats["total_nodes"].as_u64(), Some(3));

    // ── 8. Graph view: all three nodes visible ───────────────────────────
    let graph = get_json(&e2e, "/memory/graph").await;
    assert_eq!(graph["node_count"].as_u64(), Some(3));

    // ── 9. "Consolidate now" button (panel action) ───────────────────────
    let report = post_json(
        &e2e,
        "/memory/consolidate",
        serde_json::json!({"force": false, "retention_days": 30}),
    )
    .await;
    assert_eq!(report["started"].as_bool(), Some(true));
    // The fresh Pending node is too young to upgrade (min_pending_age_hours)
    // — it must survive consolidation, not disappear.
    let after = get_json(&e2e, "/memory/nodes?type=Knowledge").await;
    assert_eq!(after["total"].as_u64(), Some(2));

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}

/// Cross-layer traceability through the panel interfaces: the SOURCED_FROM
/// edge written at landing time (D9) must be observable from the store side
/// so the desktop graph view can follow episode → knowledge once edges are
/// surfaced (graph endpoint currently returns nodes only).
#[tokio::test]
async fn desktop_memory_panel_sourced_from_edges_survive_landing() {
    let e2e = spawn_memory_e2e_server("edges").await;

    let provider: Arc<dyn MemoryProvider> = e2e.store.clone();
    EpisodeDistiller::write_summary_to_provider(
        COMPACT_OUTPUT,
        "sess-e2e-edges",
        &Some(provider),
        Some(&DeterministicEmbedding),
    )
    .await;

    // Resolve the episode node id through the same HTTP list the panel uses.
    let episodes = get_json(&e2e, "/memory/nodes?type=Episodic").await;
    let episode_id = episodes["nodes"][0]["node_id"]
        .as_u64()
        .expect("episode id");

    // Store-side verification (what the graph view will consume once the
    // edges field of /memory/graph is populated): the episode must have one
    // outgoing SOURCED_FROM edge per landed knowledge node (D9).
    let edges = e2e.store.get_edges(
        grafeo_common::types::NodeId(episode_id),
        grafeo_core::graph::Direction::Outgoing,
    );
    assert_eq!(
        edges.len(),
        2,
        "each landed triple must produce one SOURCED_FROM edge"
    );
    assert!(
        edges
            .iter()
            .all(|e| e.edge_type.as_str() == acowork_grafeo::types::edge_types::SOURCED_FROM)
    );

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}

/// Idempotency from the panel's perspective: re-landing the SAME compact
/// output (e.g. duplicate compaction event) must not duplicate knowledge.
#[tokio::test]
async fn desktop_memory_panel_duplicate_distillation_is_idempotent() {
    let e2e = spawn_memory_e2e_server("dup").await;

    let provider: Arc<dyn MemoryProvider> = e2e.store.clone();
    for _ in 0..2 {
        EpisodeDistiller::write_summary_to_provider(
            COMPACT_OUTPUT,
            "sess-e2e-dup",
            &Some(provider.clone()),
            Some(&DeterministicEmbedding),
        )
        .await;
    }

    // Two episodes (one per write) but still only 2 knowledge nodes —
    // the triples are deduplicated by the instant pipeline.
    let episodes = get_json(&e2e, "/memory/nodes?type=Episodic").await;
    assert_eq!(episodes["total"].as_u64(), Some(2));
    let knowledge = get_json(&e2e, "/memory/nodes?type=Knowledge").await;
    assert_eq!(
        knowledge["total"].as_u64(),
        Some(2),
        "duplicate compaction output must not duplicate knowledge nodes"
    );

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}
