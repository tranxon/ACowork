use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use acowork_core::tools::traits::Tool;
use acowork_core::EmbeddingProvider;

use acowork_grafeo::consolidation::{DefaultEpisodicDistiller, EpisodicDistiller};
use acowork_grafeo::grafeo::GrafeoStore;

use acowork_memory::consolidation::{
    DistillerConfig, LlmMessage, LlmResponse, PromotionDecision, PromotionKind,
    TripleExtractorLlm,
};
use acowork_memory::types::{AutobioCategory, Episode, KnowledgeSubType};
use acowork_memory::{
    MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery, labels,
};

use acowork_runtime::memory::MemorySessionHandle;
use acowork_runtime::tools::builtin::memory_store::MemoryStoreTool;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

// ============================================================================
// Deterministic embedding provider (mirrors the production fallback chain)
// ============================================================================

struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-adr068-e2e"
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
// Scripted server-side LLM for distiller Step 2a (extract) + Step 4 (judge)
// ============================================================================

/// A fake `TripleExtractorLlm` returning a fixed response queue. The distiller
/// pops one response per LLM call: first the batch extraction JSON, then one
/// judge JSON per candidate cluster.
struct ScriptedDistillerLlm {
    responses: Mutex<VecDeque<String>>,
}

impl ScriptedDistillerLlm {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait::async_trait]
impl TripleExtractorLlm for ScriptedDistillerLlm {
    async fn chat(
        &self,
        _messages: Vec<LlmMessage>,
    ) -> std::result::Result<LlmResponse, String> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "ScriptedDistillerLlm: response queue exhausted".to_string())?;
        Ok(LlmResponse {
            content: resp,
            usage_tokens: None,
        })
    }
}

/// Build the Step 2a extraction JSON for one episode.
fn raw_extract(episode_id: u64, kind: &str, a: &str, b: &str) -> String {
    match kind {
        "triple" => format!(
            "{{\"episode_id\": {id}, \"structure\": {{\"kind\": \"triple\", \"subject\": \"user\", \"predicate\": \"{a}\", \"object\": \"{b}\"}}, \"autobio_candidate\": null}}",
            id = episode_id
        ),
        "autobio" => format!(
            "{{\"episode_id\": {id}, \"structure\": null, \"autobio_candidate\": {{\"aspect\": \"{a}\", \"key_hint\": \"{b}\"}}}}",
            id = episode_id
        ),
        _ => String::new(),
    }
}

fn extraction_response(items: &[(u64, &str, &str, &str)]) -> String {
    let arr: Vec<String> = items
        .iter()
        .map(|(id, k, a, b)| raw_extract(*id, k, a, b))
        .collect();
    format!("[{}]", arr.join(","))
}

/// Build the Step 4 judge JSON.
fn judge_response(decision: &str, confidence: f32, content: &str) -> String {
    format!(
        "{{\"decision\": \"{decision}\", \"confidence\": {confidence}, \"reasoning\": \"r\", \"merged_content\": \"{content}\"}}"
    )
}

// ============================================================================
// Harness
// ============================================================================

struct Adr068E2e {
    store: Arc<GrafeoStore>,
    handle: Arc<MemorySessionHandle>,
}

impl Adr068E2e {
    fn new() -> Self {
        let store = Arc::new(GrafeoStore::new_in_memory().expect("in-memory store"));
        let handle = Arc::new(MemorySessionHandle::new(Some(Arc::new(
            DeterministicEmbedding,
        ))));
        let provider: Arc<dyn MemoryProvider> = store.clone();
        handle.set_provider(provider);
        Self { store, handle }
    }

    fn provider(&self) -> Arc<dyn MemoryProvider> {
        self.handle.provider().expect("provider set")
    }

    fn store_tool(&self) -> MemoryStoreTool {
        MemoryStoreTool::new("com.test.adr068", Some(self.handle.clone()))
    }

    /// Store a classified episode directly (as if written earlier by the LLM
    /// tool at `timestamp`).
    fn seed_episode(&self, content: &str, subtype: KnowledgeSubType, ts: DateTime<Utc>) {
        let ep = Episode {
            session_id: "com.test.adr068".to_string(),
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
        self.provider()
            .store_episode(&ep)
            .expect("store_episode ok");
    }
}

// ============================================================================
// W1 / E2 — LLM tool writes Episodes only
// ============================================================================

/// W1 (e2e): each of the four accepted `category` values routes into an
/// Episode carrying the matching `knowledge_subtype`.
#[tokio::test]
async fn tool_routes_all_four_subtypes() {
    let e2e = Adr068E2e::new();
    let tool = e2e.store_tool();

    let cases = [
        ("fact", "User lives in Shanghai", "Fact"),
        ("preference", "User prefers dark mode", "Preference"),
        ("relation", "Alice works with Bob at Acme", "Relation"),
        (
            "procedure",
            "When user asks for a summary, reply in 3 sentences",
            "Procedure",
        ),
    ];

    for (category, content, expect_subtype) in cases {
        let result = tool
            .execute(
                serde_json::json!({
                    "category": category,
                    "content": content,
                }),
                None,
            )
            .await
            .expect("tool execute");
        assert!(result.ok, "tool failed for {category}: {:?}", result.error);
        assert!(
            result.content.contains(expect_subtype),
            "result must echo subtype {expect_subtype}: {}",
            result.content
        );
    }

    let provider = e2e.provider();
    for (_, _, expect_subtype) in cases {
        let subtype = match expect_subtype {
            "Fact" => KnowledgeSubType::Fact,
            "Preference" => KnowledgeSubType::Preference,
            "Relation" => KnowledgeSubType::Relation,
            _ => KnowledgeSubType::Procedure,
        };
        let eps = provider
            .get_episodes_by_subtype(Some(subtype.clone()), 10)
            .expect("get_episodes_by_subtype ok");
        assert_eq!(eps.len(), 1, "one {expect_subtype} episode stored");
        let (_, ep) = &eps[0];
        assert!(!ep.consolidated, "fresh episode unconsolidated");
        assert_eq!(ep.knowledge_subtype, Some(subtype));
    }
}

/// E2: an agent-feedback `preference` write through the real tool lands as an
/// unconsolidated Episode with `knowledge_subtype = Preference` and no legacy
/// autobiographical routing fields (W2).
#[tokio::test]
async fn tool_write_creates_preference_episode() {
    let e2e = Adr068E2e::new();
    let tool = e2e.store_tool();

    let result = tool
        .execute(
            serde_json::json!({
                "category": "preference",
                "content": "You are too verbose — give shorter answers",
            }),
            None,
        )
        .await
        .expect("tool execute");
    assert!(result.ok, "tool failed: {:?}", result.error);
    assert!(
        result.content.starts_with("Stored episode:"),
        "{}",
        result.content
    );

    let eps = e2e
        .provider()
        .get_episodes_by_subtype(Some(KnowledgeSubType::Preference), 10)
        .expect("get_episodes_by_subtype ok");
    assert_eq!(eps.len(), 1, "exactly one Preference episode");
    let (_, ep) = &eps[0];
    assert_eq!(ep.knowledge_subtype, Some(KnowledgeSubType::Preference));
    assert!(!ep.consolidated, "unconsolidated, awaiting the distiller");
    // W2: no aspect/key/source on the episode.
    for legacy in ["aspect", "key", "source"] {
        assert!(
            !ep.metadata.contains_key(legacy),
            "no legacy `{legacy}` field"
        );
    }
}

// ============================================================================
// E3/E4 — EpisodicDistiller promotes evidence-backed clusters
// ============================================================================

/// E3 (+ D10): three limitation-feedback Preference episodes spread across
/// 14+ days promote to a single `AutobiographicalNode{category=Limitation,
/// key="verbose_response"}`, episodes are marked consolidated, and the
/// `promotion_evaluations` audit contains exactly one matching entry.
#[tokio::test]
async fn distiller_promotes_autobio_limitation_and_audits() {
    let e2e = Adr068E2e::new();
    let now = Utc::now();
    for i in 0..3 {
        e2e.seed_episode(
            "You are too verbose, give shorter answers",
            KnowledgeSubType::Preference,
            now - ChronoDuration::days(i * 7),
        );
    }

    let eps = e2e
        .provider()
        .get_episodes_by_subtype(Some(KnowledgeSubType::Preference), 10)
        .expect("scan ok");
    assert_eq!(eps.len(), 3);
    let ids: Vec<u64> = eps.iter().map(|(id, _)| *id).collect();

    let llm = ScriptedDistillerLlm::new(vec![
        extraction_response(
            &ids
                .iter()
                .map(|id| (*id, "autobio", "limitation", "verbose_response"))
                .collect::<Vec<_>>(),
        ),
        judge_response("promote", 0.95, "agent should be concise"),
    ]);

    let distiller = DefaultEpisodicDistiller;
    let result = distiller
        .run(
            e2e.provider().as_ref(),
            Some(&llm),
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("distiller run ok");

    // D10: audit ↔ outcome one-to-one for the promoted cluster.
    assert_eq!(result.promotion_evaluations.len(), 1);
    let eval = &result.promotion_evaluations[0];
    assert_eq!(eval.promoted_kind, PromotionKind::AutobioLimitation);
    assert!(matches!(eval.decision, PromotionDecision::Promoted));
    assert_eq!(eval.source_episode_ids.len(), 3);
    assert!(eval.llm_confidence > 0.0);
    assert!(!eval.llm_reasoning.is_empty());

    assert_eq!(result.autobio_promoted, 1);
    assert_eq!(result.episodes_marked_consolidated, 3);

    // Sediment node present with full provenance.
    let node = e2e
        .store
        .find_autobiographical_by_key("verbose_response")
        .expect("lookup ok")
        .expect("AutobiographicalNode exists");
    assert_eq!(node.category, AutobioCategory::Limitation);
    assert_eq!(node.source_episode_ids.len(), 3);
    let meta = node.promotion_metadata.as_ref().expect("promotion metadata");
    assert_eq!(meta.promoted_by, "episodic_distiller");
    assert_eq!(meta.evidence_episode_ids.len(), 3);
    assert!(meta.llm_judge_confidence > 0.0);

    // A4: the audit entry must map to the REAL storage id of the promoted
    // node (rollback requires the mapping to exist in the data).
    assert_eq!(
        eval.promoted_node_id,
        node.id.map(|n| n.0),
        "promoted_node_id must equal the stored node id"
    );
    assert!(eval.promoted_node_id.is_some());

    // Episodes are consolidated — a second run must not re-promote them.
    let remaining = e2e
        .provider()
        .get_episodes_by_subtype(None, 10)
        .expect("scan ok");
    assert!(remaining.is_empty(), "all evidence episodes consolidated");
}

/// E4: two Fact episodes with the same predicate promote to a single
/// `KnowledgeNode` through the same audit path (evidence + judge + write).
#[tokio::test]
async fn distiller_promotes_fact_with_two_evidence_episodes() {
    let e2e = Adr068E2e::new();
    let now = Utc::now();
    for i in 0..2 {
        e2e.seed_episode(
            "User lives in Shanghai",
            KnowledgeSubType::Fact,
            now - ChronoDuration::days(i as i64),
        );
    }

    let eps = e2e
        .provider()
        .get_episodes_by_subtype(Some(KnowledgeSubType::Fact), 10)
        .expect("scan ok");
    assert_eq!(eps.len(), 2);
    let ids: Vec<u64> = eps.iter().map(|(id, _)| *id).collect();

    let llm = ScriptedDistillerLlm::new(vec![
        extraction_response(
            &ids
                .iter()
                .map(|id| (*id, "triple", "lives_in", "Shanghai"))
                .collect::<Vec<_>>(),
        ),
        judge_response("promote", 0.95, "user lives in Shanghai"),
    ]);

    let result = DefaultEpisodicDistiller
        .run(
            e2e.provider().as_ref(),
            Some(&llm),
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("distiller run ok");

    assert_eq!(result.facts_promoted, 1);
    assert_eq!(result.episodes_marked_consolidated, 2);
    assert_eq!(result.promotion_evaluations.len(), 1);
    let eval = &result.promotion_evaluations[0];
    assert_eq!(eval.promoted_kind, PromotionKind::Fact);
    assert!(matches!(eval.decision, PromotionDecision::Promoted));

    let node = e2e
        .store
        .find_knowledge_by_subject("user", "lives_in")
        .expect("lookup ok")
        .expect("KnowledgeNode exists");
    assert_eq!(node.sub_type, KnowledgeSubType::Fact);
    assert_eq!(node.source_episode_ids.len(), 2);
    assert!(node.promotion_metadata.is_some());

    // A4: the audit entry maps to the REAL storage id of the promoted node.
    assert_eq!(
        eval.promoted_node_id,
        node.id.map(|n| n.0),
        "promoted_node_id must equal the stored node id"
    );
    assert!(eval.promoted_node_id.is_some());
}

/// Step-2 failure handling: with no server-side LLM the run is a no-op —
/// episodes stay unconsolidated and can be retried later (ADR-068 §3.4.2).
#[tokio::test]
async fn distiller_noop_without_llm_keeps_episodes_pending() {
    let e2e = Adr068E2e::new();
    let now = Utc::now();
    for i in 0..3 {
        e2e.seed_episode(
            "User lives in Shanghai",
            KnowledgeSubType::Fact,
            now - ChronoDuration::days(i as i64),
        );
    }

    let result = DefaultEpisodicDistiller
        .run(
            e2e.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("distiller run ok (no-op)");

    assert_eq!(result.facts_promoted, 0);
    assert_eq!(result.promotion_evaluations.len(), 0);
    assert_eq!(result.episodes_marked_consolidated, 0);

    let remaining = e2e
        .provider()
        .get_episodes_by_subtype(Some(KnowledgeSubType::Fact), 10)
        .expect("scan ok");
    assert_eq!(remaining.len(), 3, "episodes untouched, awaiting retry");
}

// ============================================================================
// E5 — promoted sediment nodes reach MemoryManager::retrieve
// ============================================================================

/// E5: after the distiller promotes the cluster, the resulting
/// AutobiographicalNode is retrievable through the real `MemoryManager`
/// chain (the node appears as an `Autobiographical` memory).
#[tokio::test]
async fn retrieve_surfaces_promoted_autobiographical_node() {
    let e2e = Adr068E2e::new();
    let now = Utc::now();
    for i in 0..3 {
        e2e.seed_episode(
            "You are too verbose, give shorter answers",
            KnowledgeSubType::Preference,
            now - ChronoDuration::days(i * 7),
        );
    }
    let eps = e2e
        .provider()
        .get_episodes_by_subtype(Some(KnowledgeSubType::Preference), 10)
        .expect("scan ok");
    let ids: Vec<u64> = eps.iter().map(|(id, _)| *id).collect();

    let llm = ScriptedDistillerLlm::new(vec![
        extraction_response(
            &ids
                .iter()
                .map(|id| (*id, "autobio", "limitation", "verbose_response"))
                .collect::<Vec<_>>(),
        ),
        judge_response("promote", 0.95, "agent should be concise"),
    ]);
    let result = DefaultEpisodicDistiller
        .run(
            e2e.provider().as_ref(),
            Some(&llm),
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("distiller run ok");
    assert_eq!(result.autobio_promoted, 1);

    // The promoted node must be retrievable through the real manager chain.
    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let mut query = MemoryQuery::new("concise shorter answers".to_string());
    query.abstention_enabled = false;
    let retrieved = manager
        .retrieve(
            &*e2e.store,
            &mut query,
            Some(&DeterministicEmbedding),
        )
        .await
        .expect("retrieve ok");

    assert!(
        retrieved
            .memories
            .iter()
            .any(|m| m.label == labels::AUTOBIOGRAPHICAL),
        "promoted AutobiographicalNode must be retrievable, got: {:?}",
        retrieved
            .memories
            .iter()
            .map(|m| (m.label.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );
}

// ============================================================================
// D16 — the Episode schema is two-axis clean
// ============================================================================

/// D16: serialized Episodes expose NO structured-knowledge fields
/// (subject/predicate/object/trigger/action) and NO autobiographical routing
/// fields (aspect/key/source/category) — the two axes (knowledge_subtype for
/// routing, content+metadata for evidence) are the only channels.
#[test]
fn episode_schema_is_two_axis_clean() {
    let ep = Episode {
        session_id: "s1".to_string(),
        turn_index: 0,
        role: "assistant".to_string(),
        content: "User lives in Shanghai".to_string(),
        embedding: None,
        timestamp: Utc::now(),
        consolidated: false,
        metadata: Default::default(),
        importance: 0.5,
        knowledge_subtype: Some(KnowledgeSubType::Fact),
    };

    let value = serde_json::to_value(&ep).expect("episode serializes");
    let obj = value.as_object().expect("episode is an object");
    let keys: Vec<&String> = obj.keys().collect();

    let allowed = [
        "session_id",
        "turn_index",
        "role",
        "content",
        "embedding",
        "timestamp",
        "consolidated",
        "metadata",
        "importance",
        "knowledge_subtype",
    ];
    for key in &keys {
        assert!(
            allowed.contains(&key.as_str()),
            "unexpected Episode field `{key}` — schema must stay two-axis clean"
        );
    }
    // Negative assertions mirroring ADR-068 §3.3 (W2): no structured triples,
    // no autobio routing.
    for forbidden in [
        "subject",
        "predicate",
        "object",
        "trigger",
        "action",
        "aspect",
        "key_hint",
        "key",
        "source",
        "category",
        "autobio",
    ] {
        assert!(
            !obj.contains_key(forbidden),
            "Episode must not carry field `{forbidden}`"
        );
    }
    // The routing field is a single enum value.
    assert_eq!(
        obj.get("knowledge_subtype").and_then(|v| v.as_str()),
        Some("Fact")
    );
}

// ============================================================================
// E1 — manifest bootstrap of Identity + Capability nodes
// ============================================================================

/// E1: after agent initialization the manifest-declared Identity and
/// Capability nodes exist (ADR-068 M8 bootstrap scope). The bootstrap free
/// function is the testable exit point for the startup path.
#[tokio::test]
async fn bootstrap_creates_identity_and_capability_nodes() {
    let toml_str = r#"
        agent_id = "com.example.bootstrap"
        version = "1.0.0"
        name = "Bootstrap Agent"
        description = "An agent used to test manifest bootstrap"
        author = "acowork"
        runtime_version = "0.1.0"
        display_name = "Bootstrap"
        role = "tester"

        [memory]
        enabled = true

        [capabilities.weather]
        description = "Query weather forecasts"

        [capabilities.search]
        description = "Search the web"
    "#;
    let manifest = acowork_core::manifest::AgentManifest::from_toml(toml_str)
        .expect("manifest parses");
    let e2e = Adr068E2e::new();
    let provider = e2e.provider();

    let outcome =
        acowork_runtime::agent::bootstrap_autobio::bootstrap_autobiographical_from_manifest(
            &manifest,
            provider.as_ref(),
        );
    assert_eq!(outcome.skipped_existing, false);
    // agent_id + name + description + display_name + role
    assert_eq!(outcome.identity_written, 5);
    assert_eq!(outcome.capability_written, 2);

    let identities = provider
        .find_autobiographical_by_category(AutobioCategory::Identity)
        .expect("identity lookup ok");
    assert_eq!(identities.len(), 5);
    let keys: Vec<&str> = identities.iter().map(|n| n.key.as_str()).collect();
    for expected in ["agent_id", "name", "description", "display_name", "role"] {
        assert!(keys.contains(&expected), "missing Identity key {expected}");
    }

    let capabilities = provider
        .find_autobiographical_by_category(AutobioCategory::Capability)
        .expect("capability lookup ok");
    assert_eq!(capabilities.len(), 2);
    let cap_keys: Vec<&str> = capabilities.iter().map(|n| n.key.as_str()).collect();
    assert!(cap_keys.contains(&"weather"));
    assert!(cap_keys.contains(&"search"));
    assert_eq!(capabilities[0].source, "manifest");
}

/// E1 (idempotency): a second bootstrap over an already-bootstrapped store
/// is a no-op and does not duplicate nodes.
#[tokio::test]
async fn bootstrap_is_idempotent() {
    let toml_str = r#"
        agent_id = "com.example.bootstrap2"
        version = "1.0.0"
        name = "Bootstrap Agent 2"
        description = "Idempotency test"
        author = "acowork"
        runtime_version = "0.1.0"

        [capabilities.weather]
        description = "Query weather"
    "#;
    let manifest = acowork_core::manifest::AgentManifest::from_toml(toml_str)
        .expect("manifest parses");
    let e2e = Adr068E2e::new();
    let provider = e2e.provider();

    let first = acowork_runtime::agent::bootstrap_autobio::bootstrap_autobiographical_from_manifest(
        &manifest,
        provider.as_ref(),
    );
    assert_eq!(first.identity_written, 3); // agent_id, name, description
    assert_eq!(first.capability_written, 1);

    let second = acowork_runtime::agent::bootstrap_autobio::bootstrap_autobiographical_from_manifest(
        &manifest,
        provider.as_ref(),
    );
    assert!(second.skipped_existing, "second bootstrap must be skipped");
    assert_eq!(second.identity_written, 0);
    assert_eq!(second.capability_written, 0);

    let identities = provider
        .find_autobiographical_by_category(AutobioCategory::Identity)
        .expect("lookup ok");
    assert_eq!(identities.len(), 3, "no duplicate Identity nodes");
    let capabilities = provider
        .find_autobiographical_by_category(AutobioCategory::Capability)
        .expect("lookup ok");
    assert_eq!(capabilities.len(), 1, "no duplicate Capability nodes");
}
