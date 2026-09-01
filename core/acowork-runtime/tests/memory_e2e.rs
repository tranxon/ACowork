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
//!     → `parse_compact_output` (quality gate + `<summary>` block,
//!       ADR-057 triples-removed)
//!     → `MemoryManager::record_distilled`
//!     → `GrafeoStore::store_episode`
//! — against a real in-memory GrafeoStore wired into a real
//! `RuntimeHttpServer` listening on 127.0.0.1 (random port). The only
//! simulated part is the LLM: its compact-model output is a fixture
//! string (LLM prompt/parse contracts are unit-tested in
//! `episode_distill.rs`).
//!
//! ADR-057 (triples-removed): compaction no longer lands Knowledge nodes
//! or `SOURCED_FROM` edges — only an Episodic node. Knowledge persistence
//! is exercised separately via `memory_store` / procedural paths.

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
/// pipeline realistic: identical Episodic summaries deduplicate,
/// distinct ones do not.
struct DeterministicEmbedding;

/// Embedding provider that always fails at runtime — used to verify the
/// D1 best-effort degradation in `record_distilled`: a failing embedding
/// provider must NOT fail the episode write (the vector is dropped, the
/// episode still lands).
struct FailingEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for FailingEmbedding {
    fn name(&self) -> &str {
        "failing-test"
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
        Err(acowork_core::EmbeddingError::Unavailable(
            "test-only failure".to_string(),
        ))
    }

    async fn embed_batch(
        &self,
        _texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
        Err(acowork_core::EmbeddingError::Unavailable(
            "test-only failure".to_string(),
        ))
    }

    fn dimension(&self) -> usize {
        384
    }

    async fn is_available(&self) -> bool {
        true
    }
}

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

/// Compact-model output fixture (ADR-057 triples-removed): a single
/// `<summary>` block that lands as one Episodic memory node.
const COMPACT_OUTPUT: &str = "<summary>\
User worked on the ADR-057 memory distillation landing pipeline and verified it end to end.\
</summary>";

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
    // ADR-057 triples-removed): parse <summary> → record_distilled →
    // GrafeoStore::store_episode (summary-only, no triple landing).
    let provider: Arc<dyn MemoryProvider> = e2e.store.clone();
    EpisodeDistiller::write_summary_to_provider(
        COMPACT_OUTPUT,
        "sess-e2e",
        &Some(provider),
        Some(&DeterministicEmbedding),
    )
    .await
    .expect("fixture must pass the summary quality gate and land");

    // ── 3. Panel list: 1 Episodic node, no Knowledge nodes ──────────────
    let list = get_json(&e2e, "/memory/nodes?page=1&size=50").await;
    assert_eq!(list["total"].as_u64(), Some(1), "one distilled episode");
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
    assert_eq!(knowledge.len(), 0, "triples-removed: no Knowledge nodes from distillation");
    let episode_id = episodes[0]["node_id"].as_u64().expect("episode node id");

    // ── 4. Episodic type filter — only Episodic nodes ────────────────────
    let ep_only = get_json(&e2e, "/memory/nodes?type=Episodic").await;
    assert_eq!(ep_only["total"].as_u64(), Some(1));
    assert!(
        ep_only["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["node_type"] == "Episodic"),
        "type filter must hold"
    );

    // ── 5. Node detail: the episode content matches the fixture summary ─
    let detail = get_json(&e2e, &format!("/memory/nodes/{episode_id}")).await;
    assert_eq!(detail["found"].as_bool(), Some(true));
    assert_eq!(detail["node_type"].as_str(), Some("Episodic"));
    let content = detail["content"].as_str().expect("episode content");
    assert!(
        content.contains("ADR-057 memory distillation landing pipeline"),
        "stored episode must preserve the compact-model summary text, got: {content}"
    );

    // ── 6. Stats card reflects the landing ──────────────────────────────
    let stats = get_json(&e2e, "/memory/stats").await;
    assert_eq!(stats["total_nodes"].as_u64(), Some(1));

    // ── 7. Graph view: the single episode is visible ─────────────────────
    let graph = get_json(&e2e, "/memory/graph").await;
    assert_eq!(graph["node_count"].as_u64(), Some(1));

    // ── 8. "Consolidate now" button (panel action) ───────────────────────
    let report = post_json(
        &e2e,
        "/memory/consolidate",
        serde_json::json!({"force": false, "retention_days": 30}),
    )
    .await;
    assert_eq!(report["started"].as_bool(), Some(true));
    // The fresh distilled episode must survive consolidation, not disappear.
    let after = get_json(&e2e, "/memory/nodes?type=Episodic").await;
    assert_eq!(after["total"].as_u64(), Some(1));

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}

/// Idempotency from the panel's perspective: re-landing the SAME compact
/// output (e.g. duplicate compaction event) must store one episode per call —
/// the natural-language summary has no uniqueness contract, so duplicates
/// surface as multiple Episodic rows the panel can inspect.
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
        .await
        .expect("fixture must pass the summary quality gate and land");
    }

    // Two episodes (one per write) — triples-removed, so no Knowledge nodes.
    let episodes = get_json(&e2e, "/memory/nodes?type=Episodic").await;
    assert_eq!(episodes["total"].as_u64(), Some(2));
    let knowledge = get_json(&e2e, "/memory/nodes?type=Knowledge").await;
    assert_eq!(
        knowledge["total"].as_u64(),
        Some(0),
        "triples-removed: no Knowledge nodes are ever created from distillation"
    );

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}

/// The quality gate must reject polluted LLM output BEFORE it reaches the
/// store: verbatim role-label echoes (the model copied the raw dialog into
/// the summary) must not land any node (P1: quality-over-nothing).
#[tokio::test]
async fn desktop_memory_panel_rejects_polluted_summary() {
    let e2e = spawn_memory_e2e_server("reject").await;

    let provider: Arc<dyn MemoryProvider> = e2e.store.clone();
    let polluted = "<summary>用户：你好\n[User]: 你好\n[Assistant]: 我来看一下\n[Tool(bash)]: ls\n对话结束</summary>";
    let err = EpisodeDistiller::write_summary_to_provider(
        polluted,
        "sess-e2e-reject",
        &Some(provider),
        Some(&DeterministicEmbedding),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            acowork_runtime::error::RuntimeError::Summary(
                acowork_runtime::episode_distill::SummaryError::LowQuality(_)
            )
        ),
        "verbatim role labels must fail the quality gate, got: {err:?}"
    );

    // Nothing landed — the panel shows an empty memory.
    let stats = get_json(&e2e, "/memory/stats").await;
    assert_eq!(stats["total_nodes"].as_u64(), Some(0));

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}

/// D1 best-effort embedding: when the embedding provider is absent (None)
/// or fails at runtime, `record_distilled` must still land the episode —
/// only the vector is dropped (summary-only landing, ADR-057). This is
/// the "no provider / provider error" degradation path promised in
/// `MemoryManager::record_distilled`'s contract.
#[tokio::test]
async fn desktop_memory_distillation_embedding_degrades_gracefully() {
    let e2e = spawn_memory_e2e_server("embed-degrade").await;

    let provider: Arc<dyn MemoryProvider> = e2e.store.clone();

    // ── 1. No embedding provider at all (None) ───────────────────────────
    EpisodeDistiller::write_summary_to_provider(
        COMPACT_OUTPUT,
        "sess-e2e-embed-none",
        &Some(provider.clone()),
        None,
    )
    .await
    .expect("episode must land without an embedding provider");

    let eps_none = e2e
        .store
        .get_episodes(Some("sess-e2e-embed-none"), 10)
        .expect("episodes readable");
    assert_eq!(eps_none.len(), 1, "episode landed with no provider");
    assert!(
        eps_none[0].embedding.is_none(),
        "D1: no provider → embedding must be None"
    );

    // ── 2. Embedding provider errors at runtime (Some + failing) ─────────
    EpisodeDistiller::write_summary_to_provider(
        COMPACT_OUTPUT,
        "sess-e2e-embed-error",
        &Some(provider.clone()),
        Some(&FailingEmbedding),
    )
    .await
    .expect("episode must land even when embedding fails (D1)");

    let eps_err = e2e
        .store
        .get_episodes(Some("sess-e2e-embed-error"), 10)
        .expect("episodes readable");
    assert_eq!(eps_err.len(), 1, "episode landed despite embedding failure");
    assert!(
        eps_err[0].embedding.is_none(),
        "D1: provider error → embedding degrades to None, episode still stored"
    );

    // Both episodes visible through the HTTP panel.
    let episodes = get_json(&e2e, "/memory/nodes?type=Episodic").await;
    assert_eq!(episodes["total"].as_u64(), Some(2));

    std::fs::remove_dir_all(&e2e._temp_dir).ok();
}
