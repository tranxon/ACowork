//! Grafeo node export filtering for Agent package data isolation.
//!
//! Provides `FilteredNode` and `export_nodes_filtered` to selectively export
//! Grafeo nodes based on `PackageOptions`. This ensures private data
//! (Episodes, private KnowledgeNodes) is excluded by default when building
//! an `.agent` package.

use serde::{Deserialize, Serialize};

use rollball_core::packaging::PackageOptions;

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::labels;
use crate::types::{
    AutobiographicalNode, Episode, KnowledgeNode, ProceduralNode,
};

// ---------------------------------------------------------------------------
// FilteredNode
// ---------------------------------------------------------------------------

/// A Grafeo node that passed the export filter.
///
/// Carries the node's label and its serialized JSON representation,
/// suitable for embedding inside a `.agent` package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredNode {
    /// Grafeo node label (e.g. "Episodic", "Knowledge", "Procedural", "Autobiographical").
    pub label: String,
    /// Serialized JSON representation of the node.
    pub data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Privacy classification for KnowledgeNode
// ---------------------------------------------------------------------------

/// Determine whether a KnowledgeNode is considered "Public" or "Private".
///
/// The privacy level is stored in `node.metadata["privacy"]` as a string:
/// - `"Public"` → public knowledge
/// - `"Personal"` or `"Sensitive"` → private knowledge
///
/// If the privacy field is absent, the node defaults to **Private** for
/// safety (conservative default: exclude when in doubt).
fn knowledge_privacy_is_public(node: &KnowledgeNode) -> bool {
    node.metadata
        .get("privacy")
        .and_then(|v| v.as_str())
        .map(|s| s == "Public")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Export filtering
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Export Grafeo nodes filtered by `PackageOptions`.
    ///
    /// Iterates all memory nodes in the store and returns only those that
    /// should be included in the package according to the options:
    ///
    /// | Node type                      | Included when                        |
    /// |--------------------------------|--------------------------------------|
    /// | Episode                        | `options.include_episodes`           |
    /// | KnowledgeNode (Public)         | `options.include_public_knowledge`   |
    /// | KnowledgeNode (Private)        | `options.include_private_knowledge`  |
    /// | ProceduralNode                 | `options.include_procedural`         |
    /// | AutobiographicalNode           | `options.include_autobiographical`   |
    ///
    /// Nodes with other labels (SystemConfig, ToolInvocation, Session) are
    /// always excluded from export since they are runtime-internal.
    pub fn export_nodes_filtered(&self, options: &PackageOptions) -> Result<Vec<FilteredNode>> {
        let graph = self.db.graph_store();
        let mut filtered = Vec::new();

        // --- Episodes ---
        if options.include_episodes {
            let node_ids = graph.nodes_by_label(labels::EPISODIC);
            for id in node_ids {
                if let Some(n) = self.db.get_node(id) {
                    let props: Vec<(String, grafeo_common::types::Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    if let Ok(ep) = Episode::from_properties(id, &props) {
                        filtered.push(FilteredNode {
                            label: labels::EPISODIC.to_string(),
                            data: serde_json::to_value(&ep).unwrap_or(serde_json::Value::Null),
                        });
                    }
                }
            }
        }

        // --- KnowledgeNodes ---
        let include_public = options.include_public_knowledge;
        let include_private = options.include_private_knowledge;
        if include_public || include_private {
            let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);
            for id in node_ids {
                if let Some(n) = self.db.get_node(id) {
                    let props: Vec<(String, grafeo_common::types::Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    if let Ok(kn) = KnowledgeNode::from_properties(id, &props) {
                        let is_public = knowledge_privacy_is_public(&kn);
                        let should_include = (is_public && include_public)
                            || (!is_public && include_private);
                        if should_include {
                            filtered.push(FilteredNode {
                                label: labels::KNOWLEDGE.to_string(),
                                data: serde_json::to_value(&kn)
                                    .unwrap_or(serde_json::Value::Null),
                            });
                        }
                    }
                }
            }
        }

        // --- ProceduralNodes ---
        if options.include_procedural {
            let node_ids = graph.nodes_by_label(labels::PROCEDURAL);
            for id in node_ids {
                if let Some(n) = self.db.get_node(id) {
                    let props: Vec<(String, grafeo_common::types::Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    if let Ok(pn) = ProceduralNode::from_properties(id, &props) {
                        filtered.push(FilteredNode {
                            label: labels::PROCEDURAL.to_string(),
                            data: serde_json::to_value(&pn)
                                .unwrap_or(serde_json::Value::Null),
                        });
                    }
                }
            }
        }

        // --- AutobiographicalNodes ---
        if options.include_autobiographical {
            let node_ids = graph.nodes_by_label(labels::AUTOBIOGRAPHICAL);
            for id in node_ids {
                if let Some(n) = self.db.get_node(id) {
                    let props: Vec<(String, grafeo_common::types::Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    if let Ok(an) = AutobiographicalNode::from_properties(id, &props) {
                        filtered.push(FilteredNode {
                            label: labels::AUTOBIOGRAPHICAL.to_string(),
                            data: serde_json::to_value(&an)
                                .unwrap_or(serde_json::Value::Null),
                        });
                    }
                }
            }
        }

        Ok(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AutobioCategory, ContentType, KnowledgeSubType, NodeStatus,
    };
    use std::collections::HashMap;

    fn test_dt() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn make_public_knowledge(subject: &str, predicate: &str, object: &str) -> KnowledgeNode {
        let mut metadata = HashMap::new();
        metadata.insert(
            "privacy".to_string(),
            serde_json::Value::String("Public".to_string()),
        );
        KnowledgeNode {
            id: None,
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata,
        }
    }

    fn make_private_knowledge(subject: &str, predicate: &str, object: &str) -> KnowledgeNode {
        let mut metadata = HashMap::new();
        metadata.insert(
            "privacy".to_string(),
            serde_json::Value::String("Personal".to_string()),
        );
        KnowledgeNode {
            id: None,
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata,
        }
    }

    fn make_sensitive_knowledge(subject: &str, predicate: &str, object: &str) -> KnowledgeNode {
        let mut metadata = HashMap::new();
        metadata.insert(
            "privacy".to_string(),
            serde_json::Value::String("Sensitive".to_string()),
        );
        KnowledgeNode {
            id: None,
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata,
        }
    }

    fn make_episode(session_id: &str, content: &str) -> Episode {
        Episode {
            id: None,
            session_id: session_id.to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: content.to_string(),
            content_type: ContentType::Informational,
            embedding: None,
            timestamp: test_dt(),
            consolidated: false,
            metadata: HashMap::new(),
            artifact_refs: vec![],
            importance: 0.5,
        }
    }

    fn make_procedural(name: &str) -> ProceduralNode {
        ProceduralNode {
            id: None,
            name: name.to_string(),
            trigger_condition: "always".to_string(),
            action_pattern: "do stuff".to_string(),
            success_count: 5,
            fail_count: 0,
            confidence: 0.9,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        }
    }

    fn make_autobiographical(key: &str, value: &str) -> AutobiographicalNode {
        AutobiographicalNode {
            id: None,
            category: AutobioCategory::Identity,
            key: key.to_string(),
            value: value.to_string(),
            confidence: 1.0,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        }
    }

    /// Populate a test store with all node types.
    fn populate_test_store(store: &GrafeoStore) {
        store.store_episode(&make_episode("sess-1", "Hello")).unwrap();
        store
            .store_knowledge(&make_public_knowledge("agent", "framework", "RollBall"))
            .unwrap();
        store
            .store_knowledge(&make_private_knowledge("user", "name", "Alice"))
            .unwrap();
        store
            .store_knowledge(&make_sensitive_knowledge("user", "email", "alice@example.com"))
            .unwrap();
        store.store_procedural(&make_procedural("deploy_flow")).unwrap();
        store
            .store_autobiographical(&make_autobiographical("name", "WeatherBot"))
            .unwrap();
    }

    #[test]
    fn test_export_nodes_filtered_default() {
        let store = GrafeoStore::new_in_memory().unwrap();
        populate_test_store(&store);

        let opts = PackageOptions::default();
        let filtered = store.export_nodes_filtered(&opts).unwrap();

        // Default: exclude Episodes, private KnowledgeNodes, config
        // Include: public KnowledgeNodes, ProceduralNodes, AutobiographicalNodes
        let labels: Vec<&str> = filtered.iter().map(|n| n.label.as_str()).collect();

        assert!(
            !labels.contains(&"Episodic"),
            "Episodes should be excluded by default"
        );
        assert!(
            labels.contains(&"Knowledge"),
            "Public KnowledgeNodes should be included by default"
        );
        assert_eq!(
            filtered.iter().filter(|n| n.label == "Knowledge").count(),
            1,
            "Only public KnowledgeNode should be included"
        );
        assert!(
            labels.contains(&"Procedural"),
            "ProceduralNodes should be included by default"
        );
        assert!(
            labels.contains(&"Autobiographical"),
            "AutobiographicalNodes should be included by default"
        );
    }

    #[test]
    fn test_export_nodes_filtered_include_all() {
        let store = GrafeoStore::new_in_memory().unwrap();
        populate_test_store(&store);

        let opts = PackageOptions {
            include_conversations: true,
            include_episodes: true,
            include_private_knowledge: true,
            include_procedural: true,
            include_autobiographical: true,
            include_public_knowledge: true,
            include_config: true,
        };
        let filtered = store.export_nodes_filtered(&opts).unwrap();

        let labels: Vec<&str> = filtered.iter().map(|n| n.label.as_str()).collect();

        assert!(
            labels.contains(&"Episodic"),
            "Episodes should be included when opted in"
        );
        assert!(
            labels.contains(&"Knowledge"),
            "KnowledgeNodes should be included when opted in"
        );
        // Should have 3 knowledge nodes: 1 public + 1 personal + 1 sensitive
        assert_eq!(
            filtered.iter().filter(|n| n.label == "Knowledge").count(),
            3,
            "All knowledge nodes should be included when both public and private are opted in"
        );
        assert!(
            labels.contains(&"Procedural"),
            "ProceduralNodes should be included"
        );
        assert!(
            labels.contains(&"Autobiographical"),
            "AutobiographicalNodes should be included"
        );
    }

    #[test]
    fn test_export_nodes_filtered_exclude_all() {
        let store = GrafeoStore::new_in_memory().unwrap();
        populate_test_store(&store);

        let opts = PackageOptions {
            include_conversations: false,
            include_episodes: false,
            include_private_knowledge: false,
            include_procedural: false,
            include_autobiographical: false,
            include_public_knowledge: false,
            include_config: false,
        };
        let filtered = store.export_nodes_filtered(&opts).unwrap();

        assert!(
            filtered.is_empty(),
            "No nodes should be included when all options are disabled"
        );
    }

    #[test]
    fn test_export_nodes_filtered_only_public_knowledge() {
        let store = GrafeoStore::new_in_memory().unwrap();
        populate_test_store(&store);

        let opts = PackageOptions {
            include_public_knowledge: true,
            include_private_knowledge: false,
            include_episodes: false,
            include_procedural: false,
            include_autobiographical: false,
            include_conversations: false,
            include_config: false,
        };
        let filtered = store.export_nodes_filtered(&opts).unwrap();

        assert_eq!(filtered.len(), 1, "Only public knowledge should be included");
        assert_eq!(filtered[0].label, "Knowledge");
    }

    #[test]
    fn test_knowledge_privacy_is_public() {
        let public = make_public_knowledge("a", "b", "c");
        let private = make_private_knowledge("a", "b", "c");
        let sensitive = make_sensitive_knowledge("a", "b", "c");

        assert!(knowledge_privacy_is_public(&public));
        assert!(!knowledge_privacy_is_public(&private));
        assert!(!knowledge_privacy_is_public(&sensitive));

        // No privacy metadata → defaults to private (safe default)
        let no_privacy = KnowledgeNode {
            id: None,
            subject: "a".to_string(),
            predicate: "b".to_string(),
            object: "c".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };
        assert!(
            !knowledge_privacy_is_public(&no_privacy),
            "Nodes without privacy metadata should default to private"
        );
    }

    #[test]
    fn test_filtered_node_serialization() {
        let store = GrafeoStore::new_in_memory().unwrap();
        populate_test_store(&store);

        let opts = PackageOptions::default();
        let filtered = store.export_nodes_filtered(&opts).unwrap();

        for node in &filtered {
            // Each FilteredNode should be serializable
            let json = serde_json::to_string(node).unwrap();
            assert!(!json.is_empty());

            // And deserializable
            let restored: FilteredNode = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.label, node.label);
        }
    }
}
