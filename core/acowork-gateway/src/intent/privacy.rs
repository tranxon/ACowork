//! Intent response privacy filtering
//!
//! Filters sensitive memory content from Intent responses before
//! cross-agent forwarding, enforcing the PrivacyLevel policy.

use acowork_core::memory::traits::{MemoryNode, PrivacyLevel};
use serde_json::Value;

/// Strip memory nodes marked as `Sensitive` from an intent response.
///
/// Inspects the `memories` field of the response. If present and an array,
/// each element is deserialized as a `MemoryNode`; nodes with
/// `privacy_level == PrivacyLevel::Sensitive` are removed. Non-object
/// elements and elements that fail deserialization are kept as-is.
///
/// # Example
///
/// ```
/// use serde_json::json;
/// use acowork_core::memory::traits::{MemoryNode, PrivacyLevel};
/// use acowork_gateway::intent::privacy::filter_sensitive_content;
///
/// let response = json!({
///     "action": "memory_search",
///     "memories": [
///         { "id": "1", "content": "public info", "metadata": null, "zone": "semantic", "privacy_level": "Public" },
///         { "id": "2", "content": "secret", "metadata": null, "zone": "semantic", "privacy_level": "Sensitive" }
///     ]
/// });
///
/// let filtered = filter_sensitive_content(response);
/// let memories = filtered.get("memories").unwrap().as_array().unwrap();
/// assert_eq!(memories.len(), 1);
/// ```
pub fn filter_sensitive_content(mut response: Value) -> Value {
    if let Some(memories) = response.get_mut("memories")
        && let Some(arr) = memories.as_array_mut()
    {
        let filtered: Vec<Value> = arr
            .iter()
            .filter(|v| {
                if let Ok(node) = serde_json::from_value::<MemoryNode>((*v).clone()) {
                    node.privacy_level != PrivacyLevel::Sensitive
                } else {
                    // Non-MemoryNode values are kept as-is
                    true
                }
            })
            .cloned()
            .collect();
        *memories = Value::Array(filtered);
    }
    response
}

/// Filter a list of memory nodes, removing Sensitive ones.
pub fn filter_memory_nodes(nodes: Vec<MemoryNode>) -> Vec<MemoryNode> {
    nodes
        .into_iter()
        .filter(|n| n.privacy_level != PrivacyLevel::Sensitive)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_filter_sensitive_content_removes_sensitive() {
        let response = json!({
            "action": "memory_search",
            "memories": [
                {
                    "id": "1",
                    "content": "public info",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Public"
                },
                {
                    "id": "2",
                    "content": "personal info",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Personal"
                },
                {
                    "id": "3",
                    "content": "secret key",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Sensitive"
                }
            ]
        });

        let filtered = filter_sensitive_content(response);
        let memories = filtered.get("memories").unwrap().as_array().unwrap();
        assert_eq!(memories.len(), 2);

        let ids: Vec<String> = memories
            .iter()
            .map(|m| m.get("id").unwrap().as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains(&"1".to_string()));
        assert!(ids.contains(&"2".to_string()));
        assert!(!ids.contains(&"3".to_string()));
    }

    #[test]
    fn test_filter_sensitive_content_no_memories_field() {
        let response = json!({
            "action": "ping",
            "data": "hello"
        });

        let filtered = filter_sensitive_content(response.clone());
        assert_eq!(filtered, response);
    }

    #[test]
    fn test_filter_sensitive_content_empty_memories() {
        let response = json!({
            "memories": []
        });

        let filtered = filter_sensitive_content(response);
        let memories = filtered.get("memories").unwrap().as_array().unwrap();
        assert!(memories.is_empty());
    }

    #[test]
    fn test_filter_memory_nodes() {
        let nodes = vec![
            MemoryNode {
                id: "1".to_string(),
                content: "public".to_string(),
                metadata: Value::Null,
                zone: "semantic".to_string(),
                privacy_level: PrivacyLevel::Public,
            },
            MemoryNode {
                id: "2".to_string(),
                content: "secret".to_string(),
                metadata: Value::Null,
                zone: "semantic".to_string(),
                privacy_level: PrivacyLevel::Sensitive,
            },
        ];

        let filtered = filter_memory_nodes(nodes);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "1");
    }

    #[test]
    fn test_filter_sensitive_content_keeps_non_object_items() {
        let response = json!({
            "memories": [
                "not a memory node",
                42,
                {
                    "id": "1",
                    "content": "normal",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Public"
                }
            ]
        });

        let filtered = filter_sensitive_content(response);
        let memories = filtered.get("memories").unwrap().as_array().unwrap();
        assert_eq!(memories.len(), 3);
    }

    #[test]
    fn test_filter_sensitive_content_memories_not_array() {
        let response = json!({
            "memories": "this is not an array"
        });

        let filtered = filter_sensitive_content(response.clone());
        assert_eq!(filtered, response);
    }

    // =====================================================================
    // S2.11.2 + S2.11.3 Isolation verification tests
    // =====================================================================

    #[test]
    fn test_storage_isolation_two_grafeo_stores() {
        let store_a = acowork_grafeo::GrafeoStore::new_in_memory().unwrap();
        let store_b = acowork_grafeo::GrafeoStore::new_in_memory().unwrap();

        // Store data in A
        let node_id_a = store_a
            .store_node(
                acowork_grafeo::types::labels::EPISODIC,
                [("content", grafeo_common::types::Value::from("data in A"))],
            )
            .unwrap();

        // Verify B cannot see A's node by ID
        assert!(
            store_b.get_node(node_id_a).is_none(),
            "Store B should not see Store A's data"
        );

        // Verify A can see its own data
        assert!(
            store_a.get_node(node_id_a).is_some(),
            "Store A should see its own data"
        );

        // Store data in B
        let node_id_b = store_b
            .store_node(
                acowork_grafeo::types::labels::EPISODIC,
                [("content", grafeo_common::types::Value::from("data in B"))],
            )
            .unwrap();

        // Verify A cannot see B's data (even if NodeId overlaps, content must differ)
        if let Some(node) = store_a.get_node(node_id_b) {
            let content = node.get_property("content").and_then(|v| v.as_str());
            assert_ne!(
                content,
                Some("data in B"),
                "Store A should not see Store B's data"
            );
        }

        // Verify node counts are isolated via capacity status
        let config = acowork_grafeo::CapacityConfig::default();
        let status_a = store_a.get_capacity_status(&config).unwrap();
        let status_b = store_b.get_capacity_status(&config).unwrap();
        assert_eq!(status_a.total_nodes, 1, "Store A should have 1 node");
        assert_eq!(status_b.total_nodes, 1, "Store B should have 1 node");
    }

    #[test]
    fn test_intent_filtering_removes_sensitive_nodes() {
        let response = json!({
            "action": "memory_recall",
            "memories": [
                {
                    "id": "1",
                    "content": "User likes sunny weather",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Public"
                },
                {
                    "id": "2",
                    "content": "User password is abc123",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Sensitive"
                },
                {
                    "id": "3",
                    "content": "User lives in Beijing",
                    "metadata": null,
                    "zone": "semantic",
                    "privacy_level": "Personal"
                }
            ]
        });

        let filtered = filter_sensitive_content(response);
        let memories = filtered.get("memories").unwrap().as_array().unwrap();

        assert_eq!(memories.len(), 2, "Sensitive node should be stripped");

        let ids: Vec<String> = memories
            .iter()
            .map(|m| m.get("id").unwrap().as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains(&"1".to_string()));
        assert!(ids.contains(&"3".to_string()));
        assert!(!ids.contains(&"2".to_string()));
    }

    #[test]
    fn test_cross_agent_isolation() {
        let grafeo_a = acowork_grafeo::GrafeoStore::new_in_memory().unwrap();
        let grafeo_b = acowork_grafeo::GrafeoStore::new_in_memory().unwrap();

        // Agent A stores sensitive data
        let node_id_a = grafeo_a
            .store_node(
                acowork_grafeo::types::labels::KNOWLEDGE,
                [
                    ("subject", grafeo_common::types::Value::from("user")),
                    ("predicate", grafeo_common::types::Value::from("has")),
                    (
                        "object",
                        grafeo_common::types::Value::from("API key: sk-xxx"),
                    ),
                ],
            )
            .unwrap();

        // Agent B tries to access A's data through direct retrieve by ID
        // NodeId may overlap across independent in-memory stores, so verify content isolation
        if let Some(node) = grafeo_b.get_node(node_id_a) {
            let object = node.get_property("object").and_then(|v| v.as_str());
            assert_ne!(
                object,
                Some("API key: sk-xxx"),
                "B should not retrieve A's node by ID"
            );
        }

        // Agent B stores its own data
        let node_id_b = grafeo_b
            .store_node(
                acowork_grafeo::types::labels::KNOWLEDGE,
                [
                    ("subject", grafeo_common::types::Value::from("user")),
                    ("predicate", grafeo_common::types::Value::from("likes")),
                    ("object", grafeo_common::types::Value::from("rust")),
                ],
            )
            .unwrap();

        // Verify A cannot see B's data
        if let Some(node) = grafeo_a.get_node(node_id_b) {
            let object = node.get_property("object").and_then(|v| v.as_str());
            assert_ne!(object, Some("rust"), "A should not retrieve B's node by ID");
        }

        // Verify each agent only sees its own nodes via capacity status
        let config = acowork_grafeo::CapacityConfig::default();
        let status_a = grafeo_a.get_capacity_status(&config).unwrap();
        let status_b = grafeo_b.get_capacity_status(&config).unwrap();
        assert_eq!(status_a.total_nodes, 1, "A should only have its own node");
        assert_eq!(status_b.total_nodes, 1, "B should only have its own node");
    }
}
