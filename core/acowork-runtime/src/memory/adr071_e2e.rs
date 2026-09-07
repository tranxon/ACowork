//! ADR-071 e2e — manual-distiller full chain + opt-in gate.
//!
//! Lives in-crate (not `tests/`) because `AgentCore::new` is `pub(crate)`
//! and this suite must inject `memory_provider` / `embedding_provider` /
//! `consolidation_timer` (all `pub(crate)` fields) to wire a REAL
//! `GrafeoStore` behind the HTTP endpoint. `prompts_reload_e2e` documents
//! why an integration test cannot construct an `AgentCore`.
//!
//! Covers the ADR-071 W2 claim that was previously only stated in a commit
//! message: "A full end-to-end run (episodes -> promoted nodes) wires a real
//! AgentCore." Scenarios:
//!
//! 1. **E1 — HTTP manual distill promotes episodes**: seed two classified
//!    episodes (Fact + Preference) into a real in-memory GrafeoStore, run
//!    `POST /memory/distill`, and assert the HTTP `DistillResponse`, the
//!    promoted `KnowledgeNode`s (with `promotion_metadata`), the
//!    consolidated-episode cleanup, and the `GET /memory/consolidation/status`
//!    `last_run` summary.
//! 2. **E2 — disabled distiller refuses the manual trigger**: manifest has no
//!    `[memory.distiller]` section (opt-in off); `POST /memory/distill`
//!    returns 409 and produces zero semantic-layer nodes.
//!
//! The LLM is a scripted `MockProvider` (response queue) behind the SAME
//! `ProviderLlmAdapter` the production path uses — no distiller internals are
//! bypassed. The multi-thread runtime also exercises the W2 `block_in_place`
//! embedding bridge (current-thread runtimes degrade to exact-key clustering).

#![cfg(test)]
#![cfg(feature = "grafeo-backend")]

use std::sync::Arc;

use acowork_core::providers::mock::{MockProvider, MockResponse};
use acowork_core::EmbeddingProvider;
use acowork_grafeo::grafeo::GrafeoStore;
use acowork_memory::types::{Episode, KnowledgeSubType};
use acowork_memory::MemoryProvider;
use chrono::{Duration as ChronoDuration, Utc};

use crate::agent::agent_core::{AgentCore, BuiltinToolEntry};
use crate::memory::consolidation_bg::ConsolidationTimer;

const AGENT_ID: &str = "com.test.adr071-e2e";

// ============================================================================
// Deterministic embedding (same fallback as the production embedding chain)
// ============================================================================

struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-adr071-e2e"
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
// Scripted LLM responses (Step 2a extract JSON, then Step 4 judge JSON)
// ============================================================================

fn extract_entry(episode_id: u64, subject: &str, predicate: &str, object: &str) -> String {
    format!(
        "{{\"episode_id\": {id}, \"structure\": {{\"kind\": \"triple\", \"subject\": \"{s}\", \"predicate\": \"{p}\", \"object\": \"{o}\"}}, \"autobio_candidate\": null}}",
        id = episode_id,
        s = subject,
        p = predicate,
        o = object,
    )
}

fn extract_response(entries: &[String]) -> String {
    format!("[{}]", entries.join(","))
}

fn judge_response(decision: &str, confidence: f32, content: &str) -> String {
    format!(
        "{{\"decision\": \"{decision}\", \"confidence\": {confidence}, \"reasoning\": \"r\", \"merged_content\": \"{content}\"}}"
    )
}

// ============================================================================
// Harness
// ============================================================================

struct Adr071E2e {
    core: Arc<AgentCore>,
    store: Arc<GrafeoStore>,
    timer: Arc<ConsolidationTimer>,
}

/// Build a real AgentCore wired to the given real in-memory GrafeoStore,
/// with the given scripted LLM response queue. `enabled=false` omits the
/// `[memory.distiller]` manifest section (opt-in off).
///
/// The store is passed in so callers can seed episodes and read their REAL
/// node ids FIRST, then build the extract JSON with those ids (GrafeoStore
/// node ids are not 0-based sequential).
fn build_core(enabled: bool, store: Arc<GrafeoStore>, llm_responses: Vec<MockResponse>) -> Adr071E2e {
    let config = crate::config::RuntimeConfig::default();
    let distiller_toml = if enabled {
        "[memory.distiller]\nenabled = true\nbatch_size = 20\n"
    } else {
        ""
    };
    let manifest = acowork_core::AgentManifest::from_toml(&format!(
        r#"
        agent_id = "{AGENT_ID}"
        version = "1.0.0"
        name = "Test ADR-071 distiller e2e"
        description = "Manual distiller full-chain e2e"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "mock"
        model = "test-model"

        {distiller_toml}
        "#
    ))
    .expect("manifest parse ok");

    let provider = Arc::new(MockProvider::new(llm_responses));
    let mut core = AgentCore::new(
        config,
        manifest,
        provider,
        Vec::<BuiltinToolEntry>::new(),
    );

    // Inject the providers + timer the production session_init wires in
    // Phase B (see `startup::session_init`). The timer's scheduler policy
    // comes from `distiller_scheduler_config()` — the SAME source the
    // production `start_consolidation_pipeline` uses — so the status
    // endpoint reports the effective switch/interval.
    core.memory_provider = Some(store.clone());
    core.embedding_provider = Some(Arc::new(DeterministicEmbedding));
    let timer = Arc::new(ConsolidationTimer::new(core.distiller_scheduler_config()));
    core.consolidation_timer = Some(timer.clone());

    let core = Arc::new(core);
    Adr071E2e { core, store, timer }
}

impl Adr071E2e {
    /// Seed a classified episode directly (as if written earlier by the
    /// memory_store tool at `ts`), returning its real node id.
    fn seed_episode(
        &self,
        content: &str,
        subtype: KnowledgeSubType,
        ts: chrono::DateTime<Utc>,
    ) -> u64 {
        let ep = Episode {
            session_id: AGENT_ID.to_string(),
            turn_index: 0,
            role: "assistant".to_string(),
            content: content.to_string(),
            embedding: None,
            timestamp: ts,
            consolidated: false,
            metadata: Default::default(),
            importance: 0.5,
            knowledge_subtype: Some(subtype),
        };
        // Go through the trait (`dyn MemoryProvider`) so the acowork-memory
        // `Episode` type is stored — GrafeoStore has an inherent
        // `store_episode` for its own `grafeo::Episode` that would shadow it.
        let provider: Arc<dyn MemoryProvider> = self.store.clone();
        provider.store_episode(&ep).expect("store_episode ok")
    }

    /// Unconsolidated episodes in timestamp order (the distiller's Step 1
    /// candidate set), so tests can map real ids onto the scripted extract
    /// JSON.
    fn unconsolidated_ids(&self) -> Vec<u64> {
        self.store
            .get_episodes_by_subtype(None, 20)
            .expect("scan ok")
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }
}

/// Spawn a `RuntimeHttpServer` with the AgentCore + consolidation-timer
/// slots populated, so the manual-distill + status endpoints read the real
/// objects the background loop would use. Every other slot is a minimal
/// stub (`None` / empty) — same pattern as `prompts_reload_e2e::spawn_server`,
/// which documents why each unused slot can be empty.
async fn spawn_server(e2e: &Adr071E2e) -> u16 {
    let temp_dir = std::env::temp_dir().join(format!(
        "acowork-test-adr071-e2e-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    let snapshots = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let latest = Arc::new(std::sync::RwLock::new(None));
    let dispatch_tx = Arc::new(tokio::sync::Mutex::new(None));
    let embed_dim = Arc::new(std::sync::RwLock::new(0));
    let degraded_reasons = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mqtt_client = Arc::new(tokio::sync::Mutex::new(None));
    let session_metadata = Arc::new(tokio::sync::Mutex::new(None));
    let memory_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation = Arc::new(tokio::sync::Mutex::new(None));
    let agent_tools = Arc::new(tokio::sync::Mutex::new(None));
    let agent_config = Arc::new(tokio::sync::Mutex::new(None));
    let attachment = Arc::new(tokio::sync::Mutex::new(None));
    let session_config = Arc::new(tokio::sync::Mutex::new(None));
    let consolidation_timer: Arc<
        std::sync::RwLock<Option<Arc<crate::memory::ConsolidationTimer>>>,
    > = Arc::new(std::sync::RwLock::new(Some(e2e.timer.clone())));
    let rag_provider: Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>> =
        Arc::new(std::sync::RwLock::new(None));
    let debug_service = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_resolver = Arc::new(std::sync::RwLock::new(
        crate::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
    ));
    let session_manager_slot: Arc<
        tokio::sync::RwLock<
            Option<Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));
    let agent_core_slot: Arc<
        std::sync::RwLock<Option<Arc<crate::agent::agent_core::AgentCore>>>,
    > = Arc::new(std::sync::RwLock::new(Some(e2e.core.clone())));

    let server = crate::http::RuntimeHttpServer::start(
        temp_dir.clone(),
        temp_dir.clone(),
        AGENT_ID.to_string(),
        snapshots,
        latest,
        dispatch_tx,
        embed_dim,
        degraded_reasons,
        mqtt_client,
        session_metadata,
        memory_query,
        workspace_query,
        workspace_mutation,
        agent_tools,
        agent_config,
        attachment,
        session_config,
        consolidation_timer,
        rag_provider,
        debug_service,
        workspace_resolver,
        session_manager_slot,
        agent_core_slot,
    )
    .await
    .expect("runtime http server should start");

    server.port
}

// ============================================================================
// E1 — HTTP manual distill: episodes -> promoted nodes (real store)
// ============================================================================

/// Seed two classified episodes, run `POST /memory/distill` through the real
/// HTTP stack, and assert the full chain: DistillResponse counts, promoted
/// `KnowledgeNode`s with `promotion_metadata.promoted_by = "episodic_distiller"`,
/// episode cleanup (`consolidated = true`), and the status endpoint's
/// `last_run` summary.
#[tokio::test(flavor = "multi_thread")]
async fn e1_http_manual_distill_promotes_episodes() {
    let now = Utc::now();
    let store = Arc::new(GrafeoStore::new_in_memory().expect("in-memory store"));
    let mut e2e = build_core(true, store.clone(), Vec::new());

    // Seed evidence episodes per subtype so the distiller's evidence gate
    // passes: Fact needs >= 2, Preference >= 3 (ADR-068 Step 3). All members
    // of a subtype share the same predicate so they cluster together. ts is
    // spread so `get_unconsolidated_episodes_by_subtype` (timestamp-ascending)
    // returns them in a deterministic order.
    let fact_ids: Vec<u64> = (0..2)
        .map(|i| {
            e2e.seed_episode(
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - ChronoDuration::days(2) + ChronoDuration::hours(i),
            )
        })
        .collect();
    let pref_ids: Vec<u64> = (0..3)
        .map(|i| {
            e2e.seed_episode(
                "User prefers dark mode",
                KnowledgeSubType::Preference,
                now - ChronoDuration::days(1) + ChronoDuration::hours(i),
            )
        })
        .collect();
    let ids = e2e.unconsolidated_ids();
    assert_eq!(ids.len(), 5, "five seeded episodes");
    assert_eq!(&ids[0..2], &fact_ids[..], "facts come first (older ts)");
    assert_eq!(&ids[2..5], &pref_ids[..], "preferences follow");

    // Now that the REAL node ids are known, build the scripted LLM queue:
    // one Step 2a extract call (one entry per episode), then one Step 4
    // judge per cluster (Fact first, then Preference) + safety margin.
    let mut extract_entries: Vec<String> = fact_ids
        .iter()
        .map(|id| extract_entry(*id, "user", "lives_in", "Shanghai"))
        .collect();
    extract_entries.extend(
        pref_ids
            .iter()
            .map(|id| extract_entry(*id, "user", "prefers", "dark_mode")),
    );
    Arc::get_mut(&mut e2e.core).expect("unique core ref").provider = Arc::new(
        MockProvider::new(vec![
            MockResponse::Text {
                content: extract_response(&extract_entries),
            },
            MockResponse::Text {
                content: judge_response("promote", 0.92, "user lives in Shanghai"),
            },
            MockResponse::Text {
                content: judge_response("promote", 0.90, "user prefers dark mode"),
            },
            // Safety margin if the cluster order differs from the seed order.
            MockResponse::Text {
                content: judge_response("promote", 0.80, "fallback"),
            },
        ]),
    );

    let port = spawn_server(&e2e).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // ── POST /memory/distill ────────────────────────────────────────────
    let resp = client
        .post(format!("{base}/memory/distill"))
        .send()
        .await
        .expect("POST /memory/distill");
    assert_eq!(resp.status(), 200, "manual distill must succeed");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["started"], true);
    assert_eq!(body["episodes_scanned"], 5, "five seeded episodes scanned");
    assert!(
        body["facts_promoted"].as_u64().unwrap() >= 1,
        "fact promoted: {body}"
    );
    assert!(
        body["preferences_promoted"].as_u64().unwrap() >= 1,
        "preference promoted: {body}"
    );
    assert_eq!(
        body["episodes_marked_consolidated"].as_u64().unwrap(),
        5,
        "all five evidence episodes consolidated"
    );

    // ── Sediment layer: promoted nodes carry full provenance ────────────
    let fact = e2e
        .store
        .find_knowledge_by_subject("user", "lives_in")
        .expect("lookup ok")
        .expect("Fact KnowledgeNode exists");
    let meta = fact.promotion_metadata.as_ref().expect("promotion metadata");
    assert_eq!(meta.promoted_by, "episodic_distiller");
    assert_eq!(
        meta.evidence_episode_ids.len(),
        2,
        "both fact evidence episodes cited"
    );
    assert!(meta.llm_judge_confidence > 0.0);

    let pref = e2e
        .store
        .find_knowledge_by_subject("user", "prefers")
        .expect("lookup ok")
        .expect("Preference KnowledgeNode exists");
    let meta = pref.promotion_metadata.as_ref().expect("promotion metadata");
    assert_eq!(meta.promoted_by, "episodic_distiller");
    assert_eq!(
        meta.evidence_episode_ids.len(),
        3,
        "all three preference evidence episodes cited"
    );

    // ── Episodes consumed: a second run must find nothing ───────────────
    let remaining = e2e
        .store
        .get_episodes_by_subtype(None, 10)
        .expect("scan ok");
    assert!(
        remaining.is_empty(),
        "all evidence episodes consolidated, got {}",
        remaining.len()
    );

    // ── Status endpoint surfaces the run summary ────────────────────────
    let status = client
        .get(format!("{base}/memory/consolidation/status"))
        .send()
        .await
        .expect("GET status");
    assert_eq!(status.status(), 200);
    let status: serde_json::Value = status.json().await.unwrap();
    let d = &status["distiller"];
    assert_eq!(d["enabled"], true);
    let last_run = d["last_run"].as_object().expect("last_run present");
    assert_eq!(
        last_run["episodes_scanned"].as_u64().unwrap(),
        5,
        "last_run: {last_run:?}"
    );
    assert!(
        last_run["total_promoted"].as_u64().unwrap() >= 2,
        "last_run total_promoted: {last_run:?}"
    );
    assert_eq!(
        last_run["episodes_marked_consolidated"].as_u64().unwrap(),
        5
    );
    assert!(
        last_run["at"].as_str().is_some(),
        "last_run carries a timestamp"
    );
}

// ============================================================================
// E2 — opt-in gate: disabled distiller refuses the manual trigger
// ============================================================================

/// Manifest without `[memory.distiller]` → `POST /memory/distill` returns
/// 409 and must not produce ANY semantic-layer node, even with a non-empty
/// episode backlog (ADR-068/071 opt-in invariant).
#[tokio::test(flavor = "multi_thread")]
async fn e2_disabled_distiller_refuses_manual_trigger() {
    let now = Utc::now();
    let store = Arc::new(GrafeoStore::new_in_memory().expect("in-memory store"));
    let e2e = build_core(false, store.clone(), Vec::new());
    e2e.seed_episode(
        "User lives in Beijing",
        KnowledgeSubType::Fact,
        now - ChronoDuration::days(1),
    );

    let port = spawn_server(&e2e).await;
    let base = format!("http://127.0.0.1:{port}");

    let resp = reqwest::Client::new()
        .post(format!("{base}/memory/distill"))
        .send()
        .await
        .expect("POST /memory/distill");
    assert_eq!(
        resp.status(),
        409,
        "disabled distiller must refuse the manual trigger"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap_or("").contains("distiller is disabled"),
        "409 body explains the opt-in gate: {body}"
    );

    // No semantic-layer node and the episode stays unconsolidated.
    let fact = e2e
        .store
        .find_knowledge_by_subject("user", "lives_in")
        .expect("lookup ok");
    assert!(fact.is_none(), "disabled distiller must not promote");
    let remaining = e2e
        .store
        .get_episodes_by_subtype(None, 10)
        .expect("scan ok");
    assert_eq!(remaining.len(), 1, "episode must remain unconsolidated");
    assert!(!remaining[0].1.consolidated);
}
