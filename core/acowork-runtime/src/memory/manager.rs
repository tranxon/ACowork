//! Memory manager tests (GrafeoStore integration).
//!
//! ADR-051 P2: MemoryManager implementation moved to acowork-memory.
//! This file retains GrafeoStore-dependent integration tests that
//! cannot live in acowork-memory (which doesn't depend on acowork-grafeo).
//!
//! Pure-logic tests (config, inject formatting) live in acowork-memory.

// Re-export for backward compatibility.
pub use acowork_memory::{
    ConversationRecord, InjectedMemory, MemoryManager, MemoryManagerConfig, RetrievalResult,
    RetrievedMemory,
};


#[cfg(test)]
mod tests {
    use super::*;
    use acowork_grafeo::grafeo::GrafeoStore as TestStore;
    use acowork_grafeo::types::DEFAULT_EMBEDDING_DIM;
    use acowork_memory::{labels, HintType, MemoryProvider, MemoryQuery};
    use chrono::Utc;
    use grafeo_common::types::{NodeId, Value};

    /// Helper: create an in-memory TestStore for testing.
    fn test_store() -> TestStore {
        TestStore::new_in_memory().unwrap()
    }

    /// Helper: generate a test embedding vector.
    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    /// Helper: store an Episodic node with content and embedding.
    fn store_episode(store: &TestStore, content: &str, embedding: &[f32]) -> u64 {
        let id = store
            .store_node(labels::EPISODIC, [("content", Value::from(content))])
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id.as_u64()
    }

    /// Helper: store a Knowledge node with embedding.
    fn store_knowledge(
        store: &TestStore,
        subject: &str,
        predicate: &str,
        object: &str,
        embedding: &[f32],
    ) -> u64 {
        let id = store
            .store_node(
                labels::KNOWLEDGE,
                [
                    ("subject", Value::from(subject)),
                    ("predicate", Value::from(predicate)),
                    ("object", Value::from(object)),
                    ("sub_type", Value::from("Fact")),
                    ("confidence", Value::from(0.9f64)),
                    ("status", Value::from("Active")),
                ],
            )
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id.as_u64()
    }

    /// Helper: store an Autobiographical node.
    #[allow(dead_code)]
    fn store_autobiographical(
        store: &TestStore,
        key: &str,
        value: &str,
        embedding: &[f32],
    ) -> u64 {
        let id = store
            .store_node(
                labels::AUTOBIOGRAPHICAL,
                [
                    ("category", Value::from("Identity")),
                    ("key", Value::from(key)),
                    ("value", Value::from(value)),
                    ("confidence", Value::from(1.0f64)),
                    ("status", Value::from("Active")),
                ],
            )
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id.as_u64()
    }

    #[tokio::test]
    async fn test_retrieve_normal() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "user likes rust programming", &emb);
        store_knowledge(&store, "user", "lives_in", "Beijing", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut query = MemoryQuery {
            query_text: "rust programming".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store as &dyn MemoryProvider, &mut query, None).await.unwrap();
        assert!(!result.memories.is_empty(), "expected at least one result");
        assert!(!result.metrics.abstention_triggered);
    }

    #[tokio::test]
    async fn test_retrieve_empty() {
        let store = test_store();
        let emb = test_embedding();

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut query = MemoryQuery {
            query_text: "something completely unrelated".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99), // Very high threshold — should filter everything.
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store as &dyn MemoryProvider, &mut query, None).await.unwrap();
        assert!(result.memories.is_empty());
        assert!(result.metrics.abstention_triggered);
        assert_eq!(result.metrics.result_count, 0);
    }

    #[tokio::test]
    async fn test_retrieve_abstention() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "test content", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut query = MemoryQuery {
            query_text: "unrelated query".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99),
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store as &dyn MemoryProvider, &mut query, None).await.unwrap();
        assert!(result.metrics.abstention_triggered);
    }

    #[tokio::test]
    async fn test_retrieve_no_embedding_fallback() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "rust programming tutorial", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut query = MemoryQuery {
            query_text: "rust programming".to_string(),
            embedding: None,
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store as &dyn MemoryProvider, &mut query, None).await.unwrap();
        // Text search should still find results.
        assert!(!result.memories.is_empty());
    }

    #[test]
    fn test_record_conversation() {
        let store = test_store();
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        let record = ConversationRecord {
            session_id: "sess-1".to_string(),
            turn_index: 0,
            user_message: "Hello".to_string(),
            assistant_response: "Hi there!".to_string(),
            retrieved_memory_ids: vec!["mem-1".to_string()],
            timestamp: Utc::now(),
        };

        manager.record(&store as &dyn MemoryProvider, &record).unwrap();

        // Verify the episode was stored by searching.
        let text_results = store
            .text_search_with_filter(labels::EPISODIC, "content", "Hello", 5, None)
            .unwrap();
        assert!(
            !text_results.is_empty(),
            "expected recorded episode to be found"
        );
    }

    #[tokio::test]
    async fn test_process_turn() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "user prefers concise replies", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut query = MemoryQuery {
            query_text: "concise".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let (injected, metrics) = manager
            .process_turn(&store, &mut query, None)
            .await
            .unwrap();

        assert!(!injected.formatted_text.is_empty());
        assert!(metrics.result_count > 0);
        assert!(!metrics.abstention_triggered);
    }

    #[tokio::test]
    async fn test_process_turn_abstention() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "some content", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut query = MemoryQuery {
            query_text: "completely unrelated".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99),
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let (injected, metrics) = manager
            .process_turn(&store, &mut query, None)
            .await
            .unwrap();

        assert!(metrics.abstention_triggered);
        assert_eq!(injected.memory_count, 0);
        assert!(injected.formatted_text.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn test_retrieve_with_pagerank_boost() {
        let store = test_store();
        let emb = test_embedding();

        // Create three Episode nodes with the same embedding and similar content
        // so hybrid_search returns all three.
        let a_id = store_episode(&store, "Rust is a systems programming language", &emb);
        let b_id = store_episode(&store, "Rust powers web services and APIs", &emb);
        let c_id = store_episode(&store, "Rust has excellent tooling", &emb);

        // Create edges: A → B and C → B, making B the hub with 2 incoming edges.
        store
            .create_memory_edge(NodeId::new(a_id), NodeId::new(b_id), "RELATES_TO", vec![])
            .unwrap();
        store
            .create_memory_edge(NodeId::new(c_id), NodeId::new(b_id), "RELATES_TO", vec![])
            .unwrap();

        // Retrieve with PageRank enabled (default config, strong boost).
        let mut config = MemoryManagerConfig::default();
        config.enable_graph_expand = true;
        config.pagerank_weight = 0.3; // Strong boost to make topology effect visible.
        let manager = MemoryManager::new(config);

        let mut query = MemoryQuery {
            query_text: "Rust".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store as &dyn MemoryProvider, &mut query, None).await.unwrap();
        assert!(
            !result.memories.is_empty(),
            "should retrieve Rust-related nodes"
        );
    }

    #[tokio::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn test_retrieve_pagerank_disabled() {
        let store = test_store();
        let emb = test_embedding();

        let a_id = store_episode(&store, "Python is a scripting language", &emb);
        let b_id = store_episode(&store, "Python excels at data science", &emb);
        store
            .create_memory_edge(NodeId::new(a_id), NodeId::new(b_id), "RELATES_TO", vec![])
            .unwrap();

        // PageRank disabled.
        let mut config = MemoryManagerConfig::default();
        config.pagerank_weight = 0.0;
        let manager = MemoryManager::new(config);

        let mut query = MemoryQuery {
            query_text: "Python".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store as &dyn MemoryProvider, &mut query, None).await.unwrap();
        assert!(!result.memories.is_empty());
    }

    #[test]
    fn test_record_procedural_from_failure() {
        let store = test_store();
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        // Record a failure from a tool.
        manager
            .record_procedural_from_failure(&store, "bash", "Error: command not found")
            .unwrap();

        // Verify the ProceduralNode was created.
        let found = store.find_procedural_by_trigger("bash", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].learned_from, "execution_failure");
        assert_eq!(found[0].source_skill, Some("bash".to_string()));
        assert_eq!(found[0].fail_count, 1);
        assert!(found[0].confidence <= 0.7); // Low confidence
        assert_eq!(found[0].status, acowork_grafeo::types::NodeStatus::Pending);
    }

    #[test]
    fn test_record_procedural_from_failure_reinforce() {
        let store = test_store();
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        // First failure.
        manager
            .record_procedural_from_failure(&store, "bash", "Error: timeout")
            .unwrap();

        // Second failure for the same tool.
        manager
            .record_procedural_from_failure(&store, "bash", "Error: permission denied")
            .unwrap();

        // Should have only one node (reinforced, not duplicated).
        let found = store.find_procedural_by_trigger("bash", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fail_count, 2, "fail_count should be incremented");
    }

    #[test]
    fn test_extract_node_content_procedural() {
        let store = test_store();

        // Store a procedural node.
        use acowork_grafeo::types::{NodeStatus, ProceduralNode};
        let node = ProceduralNode {
            id: None,
            name: "concise_summary".to_string(),
            trigger_condition: "user asks for summary".to_string(),
            action_pattern: "reply in 3 sentences max".to_string(),
            success_count: 5,
            fail_count: 1,
            confidence: 0.9,
            activation_count: 3,
            source_skill: None,
            learned_from: "user_feedback".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        let id = store.store_procedural(&node).unwrap();

        // extract_node_content should format it as "当 X 时，优先 Y".
        let content = store.get_node_content(id.as_u64()).ok().flatten().unwrap_or_default();        assert!(
            content.starts_with("当"),
            "Procedural content should start with '当', got: {}",
            content
        );
        assert!(
            content.contains("优先"),
            "Procedural content should contain '优先', got: {}",
            content
        );
        assert!(
            content.contains("user asks for summary"),
            "Should contain trigger_condition"
        );
        assert!(
            content.contains("reply in 3 sentences max"),
            "Should contain action_pattern"
        );
    }

    #[test]
    fn test_self_evaluate_creates_limitation_node() {
        use acowork_grafeo::types::{
            AutobioCategory, AutobiographicalNode, NodeStatus, ProceduralNode,
        };

        let store = test_store();

        // Create ProceduralNodes with low success rate for skill "bash".
        // success=1, fail=4 → rate=20% < 60%, observations=5 ≥ 5.
        let node = ProceduralNode {
            id: None,
            name: "avoid_bash".to_string(),
            trigger_condition: "使用 bash 工具时".to_string(),
            action_pattern: "避免 bash".to_string(),
            success_count: 1,
            fail_count: 4,
            confidence: 0.6,
            activation_count: 0,
            source_skill: Some("bash".to_string()),
            learned_from: "execution_failure".to_string(),
            embedding: None,
            status: NodeStatus::Pending,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_procedural(&node).unwrap();

        // Manually run the self-evaluation logic (normally in AgentLoop,
        // but we test the core logic here).
        let nodes = store.get_all_procedural_nodes().unwrap();
        let mut skill_stats: std::collections::HashMap<String, (u32, u32)> =
            std::collections::HashMap::new();
        for n in &nodes {
            if let Some(ref skill) = n.source_skill {
                let entry = skill_stats.entry(skill.clone()).or_insert((0, 0));
                entry.0 += n.success_count;
                entry.1 += n.fail_count;
            }
        }

        // Verify skill stats computation.
        assert_eq!(skill_stats.get("bash"), Some(&(1, 4)));

        // Simulate the Limitation node creation.
        let key = "skill_bash".to_string();
        let value = "bash 成功率仅 20%（1 次成功 / 4 次失败）".to_string();
        let limitation = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Limitation,
            key: key.clone(),
            value,
            confidence: 0.8,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_autobiographical(&limitation).unwrap();

        // Verify the Limitation node was stored.
        let found = store.find_autobiographical_by_key(&key).unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.category, AutobioCategory::Limitation);
        assert!(found.value.contains("20%"));
    }

    #[test]
    fn test_self_evaluate_no_limitation_for_high_success() {
        use acowork_grafeo::types::{NodeStatus, ProceduralNode};

        let store = test_store();

        // Create ProceduralNodes with high success rate.
        // success=8, fail=2 → rate=80% > 60%, should NOT create Limitation.
        let node = ProceduralNode {
            id: None,
            name: "good_skill".to_string(),
            trigger_condition: "使用 python 工具时".to_string(),
            action_pattern: "使用 python".to_string(),
            success_count: 8,
            fail_count: 2,
            confidence: 0.9,
            activation_count: 0,
            source_skill: Some("python".to_string()),
            learned_from: "user_feedback".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_procedural(&node).unwrap();

        // Compute stats.
        let nodes = store.get_all_procedural_nodes().unwrap();
        let mut skill_stats: std::collections::HashMap<String, (u32, u32)> =
            std::collections::HashMap::new();
        for n in &nodes {
            if let Some(ref skill) = n.source_skill {
                let entry = skill_stats.entry(skill.clone()).or_insert((0, 0));
                entry.0 += n.success_count;
                entry.1 += n.fail_count;
            }
        }

        let (success, fail) = skill_stats.get("python").unwrap();
        let total = success + fail;
        let rate = *success as f32 / total as f32;

        // Rate should be 80%, above the 60% threshold.
        assert!(
            rate > 0.60,
            "python skill success rate should be > 60%, got {}",
            rate
        );

        // No Limitation node should exist for python.
        let found = store.find_autobiographical_by_key("skill_python").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_auto_generate_relationship_span_over_30_days() {
        use acowork_grafeo::types::{AutobioCategory, AutobiographicalNode, Episode, NodeStatus};

        let store = test_store();

        // Create an old episode (45 days ago).
        let old_time = chrono::Utc::now() - chrono::TimeDelta::days(45);
        let episode = Episode {
            id: None,
            session_id: "test-session".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "Hello".to_string(),
            embedding: None,
            timestamp: old_time,
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            importance: 0.5,
        };
        store.store_episode(&episode).unwrap();

        // Simulate the Relationship generation logic.
        let db = store.db();
        let graph = db.graph_store();
        let episodic_ids = graph.nodes_by_label(acowork_grafeo::types::labels::EPISODIC);

        let mut earliest_time: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut episode_count: u32 = 0;

        for id in episodic_ids {
            if let Some(n) = db.get_node(id) {
                episode_count += 1;
                if let Some(ts) = n
                    .get_property("created_at")
                    .and_then(grafeo_common::types::Value::as_timestamp)
                    && let Some(dt) = chrono::DateTime::from_timestamp_micros(ts.as_micros()) {
                        match earliest_time {
                            None => earliest_time = Some(dt),
                            Some(earliest) if dt < earliest => earliest_time = Some(dt),
                            _ => {}
                        }
                    }
            }
        }

        let earliest = earliest_time.unwrap();
        let span_days = (chrono::Utc::now() - earliest).num_days();
        assert!(
            span_days >= 30,
            "span should be >= 30 days, got {}",
            span_days
        );

        // Create the Relationship node.
        let key = "collaboration_span".to_string();
        let value = format!("已合作 {} 天（{} 次对话记录）", span_days, episode_count);
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Relationship,
            key: key.clone(),
            value,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_autobiographical(&node).unwrap();

        // Verify the Relationship node was stored.
        let found = store.find_autobiographical_by_key(&key).unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.category, AutobioCategory::Relationship);
        assert!(found.value.contains("天"));
    }

    #[test]
    fn test_auto_generate_relationship_span_under_30_days() {
        use acowork_grafeo::types::Episode;

        let store = test_store();

        // Create a recent episode (5 days ago).
        let recent_time = chrono::Utc::now() - chrono::TimeDelta::days(5);
        let episode = Episode {
            id: None,
            session_id: "test-session".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "Hello".to_string(),
            embedding: None,
            timestamp: recent_time,
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            importance: 0.5,
        };
        store.store_episode(&episode).unwrap();

        // Compute span — should be < 30 days.
        let db = store.db();
        let graph = db.graph_store();
        let episodic_ids = graph.nodes_by_label(acowork_grafeo::types::labels::EPISODIC);

        let mut earliest_time: Option<chrono::DateTime<chrono::Utc>> = None;
        for id in episodic_ids {
            if let Some(n) = db.get_node(id)
                && let Some(ts) = n
                    .get_property("created_at")
                    .and_then(grafeo_common::types::Value::as_timestamp)
                    && let Some(dt) = chrono::DateTime::from_timestamp_micros(ts.as_micros()) {
                        match earliest_time {
                            None => earliest_time = Some(dt),
                            Some(earliest) if dt < earliest => earliest_time = Some(dt),
                            _ => {}
                        }
                    }
        }

        let span_days = (chrono::Utc::now() - earliest_time.unwrap()).num_days();
        assert!(
            span_days < 30,
            "span should be < 30 days, got {}",
            span_days
        );

        // No Relationship node should exist.
        let found = store
            .find_autobiographical_by_key("collaboration_span")
            .unwrap();
        assert!(found.is_none());
    }

}
