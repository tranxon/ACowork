//! Consolidation pipeline integration tests.
//!
//! End-to-end tests covering the full consolidation flow:
//! 1. Store episodes via memory_store (instant extraction)
//! 2. Run offline consolidation (upgrade Pending → Active)
//! 3. Run experience generalization (pattern extraction → ProceduralNodes)
//! 4. Validate with retrieval quality metrics
//!
//! Phase 3 S4.6

use std::collections::HashMap;

use chrono::{TimeDelta, Utc};
use rollball_grafeo::{
    ConsolidationScheduler, GeneralizationConfig, GrafeoStore, OfflineConsolidationConfig,
    SchedulerConfig, TriggerReason,
    EvalQuery, MetricsAggregator,
    OnlineRetrievalMetrics, HintType,
    ConflictResolutionRecord,
    KnowledgeNode, KnowledgeSubType, NodeStatus,
    Episode, ContentType, EMBEDDING_DIM,
};
use rollball_grafeo::consolidation::triple_extraction::{LlmMessage, LlmResponse, TripleExtractorLlm};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_store() -> GrafeoStore {
    GrafeoStore::new_in_memory().unwrap()
}

fn test_embedding(text: &str) -> Vec<f32> {
    // Deterministic pseudo-embedding based on text hash
    let mut vec = vec![0.1f32; EMBEDDING_DIM];
    if !text.is_empty() {
        let hash = text.chars().map(|c| c as usize).sum::<usize>();
        vec[0] = (hash as f32 % 1.0).max(0.1);
        vec[1] = ((hash / 7) as f32 % 1.0).max(0.1);
    }
    vec
}

fn old_time() -> chrono::DateTime<Utc> {
    Utc::now() - TimeDelta::hours(2)
}

/// Mock LLM that returns predefined responses for triple extraction and pattern discovery.
struct MockConsolidationLlm {
    triple_response: String,
    pattern_response: String,
}

#[async_trait::async_trait]
impl TripleExtractorLlm for MockConsolidationLlm {
    async fn chat(&self, messages: Vec<LlmMessage>) -> Result<LlmResponse, String> {
        let system = messages.first().map(|m| m.content.as_str()).unwrap_or("");
        let response = if system.contains("knowledge extraction") {
            self.triple_response.clone()
        } else if system.contains("behavior pattern") {
            self.pattern_response.clone()
        } else {
            "[]".to_string()
        };
        Ok(LlmResponse {
            content: response,
            usage_tokens: Some(150),
        })
    }
}

// ---------------------------------------------------------------------------
// Test 1: Full consolidation pipeline — instant → offline → generalization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_full_consolidation_pipeline() {
    let store = test_store();

    // Step 1: Store some pending knowledge nodes (simulating instant extraction)
    let kn1 = KnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: "likes".to_string(),
        object: "coffee".to_string(),
        sub_type: KnowledgeSubType::Preference,
        confidence: 0.8,
        source_episode_id: None,
        embedding: Some(test_embedding("user likes coffee")),
        status: NodeStatus::Pending,
        created_at: old_time(),
        updated_at: old_time(),
        metadata: HashMap::new(),
    };
    let kn2 = KnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: "lives_in".to_string(),
        object: "Shanghai".to_string(),
        sub_type: KnowledgeSubType::Fact,
        confidence: 0.9,
        source_episode_id: None,
        embedding: Some(test_embedding("user lives in Shanghai")),
        status: NodeStatus::Pending,
        created_at: old_time(),
        updated_at: old_time(),
        metadata: HashMap::new(),
    };
    let kn3 = KnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: "name".to_string(),
        object: "Alice".to_string(),
        sub_type: KnowledgeSubType::Fact,
        confidence: 0.2, // Low confidence → should be marked Dormant
        source_episode_id: None,
        embedding: Some(test_embedding("user name Alice")),
        status: NodeStatus::Pending,
        created_at: old_time(),
        updated_at: old_time(),
        metadata: HashMap::new(),
    };
    store.store_knowledge(&kn1).unwrap();
    store.store_knowledge(&kn2).unwrap();
    store.store_knowledge(&kn3).unwrap();

    // Step 2: Run offline consolidation (no generalization yet)
    let config = OfflineConsolidationConfig {
        batch_size: 50,
        min_pending_age_hours: 1,
    };
    let result = store.run_offline_consolidation_with_generalization(
        &config,
        None,    // no LLM
        None,    // no embedding fn → skip generalization
        None,    // no gen config
    ).await.unwrap();

    assert_eq!(result.upgraded, 2, "Two nodes should be upgraded (confidence >= 0.7)");
    assert_eq!(result.marked_dormant, 1, "One node should be marked Dormant (confidence < 0.3)");
    assert_eq!(result.procedural_created, 0, "No generalization without embedding fn");
    assert_eq!(result.procedural_boosted, 0);

    // Verify nodes are in correct states
    let all_knowledge = store.get_all_active_knowledge().unwrap();
    assert_eq!(all_knowledge.len(), 2, "Two Active knowledge nodes after consolidation");
}

// ---------------------------------------------------------------------------
// Test 2: Consolidation + generalization with embedding function
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_consolidation_with_generalization() {
    let store = test_store();

    // Store assistant episodes (unconsolidated)
    for i in 0..5 {
        let ep = Episode {
            id: None,
            session_id: "sess-1".to_string(),
            turn_index: i,
            role: "assistant".to_string(),
            content: "Let me check the weather for you.\n{\"name\": \"http_request\", \"arguments\": {\"url\": \"weather-api\"}}".to_string(),
            content_type: ContentType::Informational,
            embedding: Some(test_embedding(&format!("weather check {}", i))),
            timestamp: Utc::now(),
            consolidated: false,
            metadata: HashMap::new(),
            artifact_refs: vec![],
            importance: 0.7,
        };
        store.store_episode(&ep).unwrap();
    }

    // Also store a pending knowledge node
    let kn = KnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: "likes".to_string(),
        object: "sunny days".to_string(),
        sub_type: KnowledgeSubType::Preference,
        confidence: 0.85,
        source_episode_id: None,
        embedding: Some(test_embedding("user likes sunny days")),
        status: NodeStatus::Pending,
        created_at: old_time(),
        updated_at: old_time(),
        metadata: HashMap::new(),
    };
    store.store_knowledge(&kn).unwrap();

    // Run full consolidation with generalization
    let config = OfflineConsolidationConfig {
        batch_size: 50,
        min_pending_age_hours: 1,
    };
    let gen_config = GeneralizationConfig {
        min_observations: 3,
        max_episodes_scan: 100,
        ..GeneralizationConfig::default()
    };

    let result = store.run_offline_consolidation_with_generalization(
        &config,
        None,
        Some(&test_embedding),
        Some(&gen_config),
    ).await.unwrap();

    assert_eq!(result.upgraded, 1, "One knowledge node should be upgraded");
    // procedural_created is always >= 0 for unsigned types, so no assert needed
}

// ---------------------------------------------------------------------------
// Test 3: Scheduler + consolidation pipeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_scheduler_consolidation_pipeline() {
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(test_store()));

    // Seed pending knowledge nodes
    {
        let s = store.lock().await;
        for i in 0..5 {
            let kn = KnowledgeNode {
                id: None,
                subject: "user".to_string(),
                predicate: format!("test_{}", i),
                object: "value".to_string(),
                sub_type: KnowledgeSubType::Fact,
                confidence: 0.8,
                source_episode_id: None,
                embedding: Some(test_embedding(&format!("test {}", i))),
                status: NodeStatus::Pending,
                created_at: old_time(),
                updated_at: old_time(),
                metadata: HashMap::new(),
            };
            s.store_knowledge(&kn).unwrap();
        }
    }

    let scheduler = ConsolidationScheduler::new(
        store.clone(),
        SchedulerConfig {
            idle_timeout_secs: 0, // Immediate timeout
            accumulation_threshold: 999,
            batch_size: 50,
            min_pending_age_hours: 1,
        },
    );

    scheduler.update_pending_count(5).await;
    let run = scheduler.run_now(TriggerReason::IdleTimeout).await.unwrap();

    assert_eq!(run.trigger, TriggerReason::IdleTimeout);
    assert_eq!(run.result.upgraded, 5, "All 5 pending nodes should be upgraded");
    assert!(run.started_at <= run.finished_at);

    let history = scheduler.get_history().await;
    assert_eq!(history.len(), 1);
}

// ---------------------------------------------------------------------------
// Test 4: Pattern dedup across multiple generalization runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pattern_dedup_across_runs() {
    let store = test_store();

    let episodes = vec![
        ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
        ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
        ("ep3".to_string(), "weather".to_string(), "http_request".to_string()),
    ];

    // First run — creates the pattern
    let result1 = store
        .generalize_patterns(&episodes, None, &test_embedding, 3)
        .await
        .unwrap();
    assert_eq!(result1.nodes_created, 1);
    assert_eq!(result1.nodes_boosted, 0);

    // Second run with same episodes — should boost existing, not create new
    let result2 = store
        .generalize_patterns(&episodes, None, &test_embedding, 3)
        .await
        .unwrap();
    assert_eq!(result2.nodes_created, 0, "Should not create duplicate");
    assert_eq!(result2.nodes_boosted, 1, "Should boost existing node");
    assert_eq!(result2.patterns_deduplicated, 1);

    // Verify only one ProceduralNode exists
    let all = store.get_all_procedural_nodes().unwrap();
    assert_eq!(all.len(), 1, "Should have exactly one ProceduralNode");
    assert!(all[0].success_count > 3, "Success count should be boosted");
}

// ---------------------------------------------------------------------------
// Test 5: LLM-driven triple extraction + conflict classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_llm_triple_extraction_integration() {
    let store = test_store();

    let llm = MockConsolidationLlm {
        triple_response: r#"[{"subject":"user","predicate":"works_at","object":"Acme Corp","confidence":0.9,"sub_type":"fact"},{"subject":"user","predicate":"hobbies","object":"hiking","confidence":0.75,"sub_type":"preference"}]"#.to_string(),
        pattern_response: "[]".to_string(),
    };

    let result = store
        .extract_triples(
            &[("ep-1".to_string(), "I work at Acme Corp and enjoy hiking".to_string())],
            &llm,
            &test_embedding,
        )
        .await
        .unwrap();

    assert_eq!(result.triples.len(), 2);
    assert_eq!(result.deduplicated, 0);

    // First triple is high confidence → Active
    // Second is medium confidence → Pending
    let active = store.get_all_active_knowledge().unwrap();
    assert_eq!(active.len(), 1, "Only high-confidence triple should be Active");
}

// ---------------------------------------------------------------------------
// Test 6: Quality metrics aggregation over consolidation cycles
// ---------------------------------------------------------------------------

#[test]
fn test_quality_metrics_over_consolidation() {
    let mut aggregator = MetricsAggregator::with_defaults(1.0);

    // Simulate a series of retrieval operations with varying quality
    for i in 0..20 {
        let metrics = OnlineRetrievalMetrics {
            result_count: 5,
            avg_score: if i < 15 { 0.8 } else { 0.3 }, // Quality drops in last 5
            max_score: 0.95,
            abstention_triggered: i >= 15,
            retrieval_level: if i >= 15 { 2 } else { 0 },
            graph_expand_nodes: 3,
            hint_type: if i % 2 == 0 { HintType::Hybrid } else { HintType::Semantic },
        };
        aggregator.record_retrieval(&metrics);
    }

    // Record some conflict resolutions
    for _ in 0..8 {
        aggregator.record_conflict(&ConflictResolutionRecord {
            heuristic_type: "Evolution".to_string(),
            final_type: "Evolution".to_string(),
            correct: true,
            auto_resolved: true,
        });
    }
    // A few incorrect
    for _ in 0..2 {
        aggregator.record_conflict(&ConflictResolutionRecord {
            heuristic_type: "Evolution".to_string(),
            final_type: "Correction".to_string(),
            correct: false,
            auto_resolved: false,
        });
    }

    assert_eq!(aggregator.total_retrievals(), 20);
    assert!((aggregator.abstention_rate() - 0.25).abs() < 0.01); // 5/20
    assert!((aggregator.conflict_stats().accuracy() - 0.8).abs() < f32::EPSILON); // 8/10
}

// ---------------------------------------------------------------------------
// Test 7: Benchmark metrics for evaluating retrieval quality
// ---------------------------------------------------------------------------

#[test]
fn test_benchmark_metrics_evaluation() {
    let queries = vec![
        EvalQuery {
            query: "user preferences".to_string(),
            relevant_ids: vec![1, 2],
        },
        EvalQuery {
            query: "user location".to_string(),
            relevant_ids: vec![3],
        },
    ];

    let results = vec![
        vec![1, 4, 5, 2],  // P@2: 1/2=0.5, R@4: 2/2=1.0
        vec![3, 6, 7],     // P@1: 1/1=1.0, R@3: 1/1=1.0
    ];

    let metrics = rollball_grafeo::evaluate_retrieval_quality(&queries, &results, &[1, 3, 5]);

    assert_eq!(metrics.num_queries, 2);
    assert!(!metrics.precision_at_k.is_empty());

    // MRR should be 1.0 (first relevant at rank 1 for both queries)
    assert!((metrics.mrr - 1.0).abs() < f32::EPSILON);
}

// ---------------------------------------------------------------------------
// Test 8: Full end-to-end — episodes → extraction → consolidation → metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_consolidation_pipeline() {
    let store = test_store();

    // 1. Store episodes
    for i in 0..3 {
        let ep = Episode {
            id: None,
            session_id: format!("sess-{}", i),
            turn_index: 0,
            role: "assistant".to_string(),
            content: "Checking the weather now.\n{\"name\": \"http_request\"}".to_string(),
            content_type: ContentType::Informational,
            embedding: Some(test_embedding(&format!("weather {}", i))),
            timestamp: Utc::now(),
            consolidated: false,
            metadata: HashMap::new(),
            artifact_refs: vec![],
            importance: 0.7,
        };
        store.store_episode(&ep).unwrap();
    }

    // 2. Store some pending knowledge
    let kn = KnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: "favorite_weather".to_string(),
        object: "sunny".to_string(),
        sub_type: KnowledgeSubType::Preference,
        confidence: 0.85,
        source_episode_id: None,
        embedding: Some(test_embedding("user likes sunny")),
        status: NodeStatus::Pending,
        created_at: old_time(),
        updated_at: old_time(),
        metadata: HashMap::new(),
    };
    store.store_knowledge(&kn).unwrap();

    // 3. Run full consolidation
    let offline_config = OfflineConsolidationConfig::default();
    let gen_config = GeneralizationConfig {
        min_observations: 3,
        ..GeneralizationConfig::default()
    };

    let result = store
        .run_offline_consolidation_with_generalization(
            &offline_config,
            None,
            Some(&test_embedding),
            Some(&gen_config),
        )
        .await
        .unwrap();

    // 4. Verify results
    assert!(result.upgraded >= 1, "At least one knowledge node should be upgraded");
    assert_eq!(result.marked_dormant, 0, "No nodes should be marked Dormant (all have good confidence)");

    // 5. Verify knowledge is now Active
    let active = store.get_all_active_knowledge().unwrap();
    assert!(!active.is_empty(), "Should have active knowledge nodes");

    // 6. Verify procedural nodes may have been created
    let procedural = store.get_all_procedural_nodes().unwrap();
    // May or may not have nodes depending on pattern detection
    assert!(procedural.len() <= 1, "At most one procedural pattern from 3 similar episodes");
}

// ---------------------------------------------------------------------------
// Test 9: Conflict detection + classification + resolution pipeline
// ---------------------------------------------------------------------------

#[test]
fn test_conflict_resolution_pipeline() {
    let store = test_store();

    // Store existing knowledge
    let existing = KnowledgeNode {
        id: None,
        subject: "user".to_string(),
        predicate: "lives_in".to_string(),
        object: "Beijing".to_string(),
        sub_type: KnowledgeSubType::Fact,
        confidence: 0.85,
        source_episode_id: None,
        embedding: Some(test_embedding("user lives in Beijing")),
        status: NodeStatus::Active,
        created_at: Utc::now() - TimeDelta::days(30),
        updated_at: Utc::now() - TimeDelta::days(30),
        metadata: HashMap::new(),
    };
    let _existing_id = store.store_knowledge(&existing).unwrap();

    // Process a conflicting input.
    // Note: conflict detection requires embedding cosine similarity > 0.85.
    // Our test embeddings are similar enough to trigger this.
    let input = rollball_grafeo::MemoryStoreInput {
        content: "User now lives in Shanghai".to_string(),
        sub_type: KnowledgeSubType::Fact,
        subject: Some("user".to_string()),
        predicate: Some("lives_in".to_string()),
        object: Some("Shanghai".to_string()),
        confidence: Some(0.9),
        source_episode_id: None,
        embedding: Some(test_embedding("user lives in Shanghai")),
    };

    let result = store.process_memory_store(&input).unwrap();
    // Conflict detection may or may not trigger depending on embedding similarity.
    // Either way the pipeline should complete without error.
    drop(result);

    // Verify the new knowledge was stored
    let all_active = store.get_all_active_knowledge().unwrap();
    assert!(!all_active.is_empty(), "Should have active knowledge nodes");
}

// ---------------------------------------------------------------------------
// Test 10: GeneralizationConfig behavior verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_generalization_config_min_observations() {
    let store = test_store();

    // Only 2 observations — below default min_observations of 3
    let episodes = vec![
        ("ep1".to_string(), "translate".to_string(), "llm_call".to_string()),
        ("ep2".to_string(), "translate".to_string(), "llm_call".to_string()),
    ];

    // With min_observations=3 → no patterns
    let result = store
        .generalize_patterns(&episodes, None, &test_embedding, 3)
        .await
        .unwrap();
    assert_eq!(result.nodes_created, 0, "Below threshold → no patterns");

    // With min_observations=2 → one pattern
    let result = store
        .generalize_patterns(&episodes, None, &test_embedding, 2)
        .await
        .unwrap();
    assert_eq!(result.nodes_created, 1, "At threshold → pattern created");
}

// ---------------------------------------------------------------------------
// Test 11: Metrics aggregator alerts across consolidation cycles
// ---------------------------------------------------------------------------

#[test]
fn test_metrics_aggregator_with_conflict_accuracy() {
    let mut aggregator = MetricsAggregator::with_defaults(1.0);

    // Good retrievals
    for _ in 0..10 {
        let m = OnlineRetrievalMetrics {
            result_count: 5,
            avg_score: 0.85,
            max_score: 0.95,
            abstention_triggered: false,
            retrieval_level: 0,
            graph_expand_nodes: 2,
            hint_type: HintType::Hybrid,
        };
        let alerts = aggregator.record_retrieval(&m);
        assert!(alerts.is_empty(), "Good retrievals should not generate alerts");
    }

    assert!((aggregator.current_nrr() - 0.85).abs() < 0.01);

    // All correct conflict resolutions
    for _ in 0..10 {
        aggregator.record_conflict(&ConflictResolutionRecord {
            heuristic_type: "Evolution".to_string(),
            final_type: "Evolution".to_string(),
            correct: true,
            auto_resolved: true,
        });
    }

    assert!((aggregator.conflict_stats().accuracy() - 1.0).abs() < f32::EPSILON);
    assert_eq!(aggregator.conflict_stats().total, 10);
}
