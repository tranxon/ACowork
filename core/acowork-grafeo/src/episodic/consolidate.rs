//! Consolidation markers and cleanup for episodic memory.
#![allow(clippy::collapsible_if)]

use chrono::TimeDelta;
use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
// labels not needed in this module

impl GrafeoStore {
    /// Mark an episode as consolidated (transferred to semantic layer).
    pub fn mark_episode_consolidated(&self, episode_id: NodeId) -> Result<()> {
        self.db
            .set_node_property(episode_id, "consolidated", Value::from(true));
        Ok(())
    }

    /// Record a sticky "skip" tombstone on episodes whose cluster the LLM
    /// judge declined to promote (ADR-068 Step 4 `skip`).
    ///
    /// The marker is merged into the episode's `metadata` property under the
    /// `distiller_skip` key as `{cluster_key, reason, at}` (same serialization
    /// as [`crate::types::Episode::metadata`]). The episode stays
    /// unconsolidated and retrievable, but
    /// [`get_unconsolidated_episodes_by_subtype`](Self::get_unconsolidated_episodes_by_subtype)
    /// filters marked episodes out so the distiller never re-extracts or
    /// re-judges the cluster (no infinite retry, no repeated LLM cost).
    pub fn mark_episodes_skipped(
        &self,
        ids: &[NodeId],
        cluster_key: &str,
        reason: &str,
    ) -> Result<()> {
        let marker = serde_json::json!({
            "cluster_key": cluster_key,
            "reason": reason,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        for id in ids {
            let mut metadata: std::collections::HashMap<String, serde_json::Value> =
                match self.db.get_node(*id) {
                    Some(node) => node
                        .get_property("metadata")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_default(),
                    None => std::collections::HashMap::new(),
                };
            metadata.insert(
                "distiller_skip".to_string(),
                marker.clone(),
            );
            let serialized =
                serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
            self.db
                .set_node_property(*id, "metadata", Value::from(serialized));
        }
        Ok(())
    }

    /// Retrieve unconsolidated episodes, ordered by timestamp ascending.
    ///
    /// These are candidates for the offline consolidation pipeline.
    pub fn get_unconsolidated_episodes(&self, limit: usize) -> Result<Vec<crate::types::Episode>> {
        // Clamp to the GQL int64 range — a raw usize::MAX literal is rejected
        // by the engine with a syntax error.
        let limit = limit.min(i64::MAX as usize);
        let session = self.db.session();
        // Note: GQL ORDER BY / WHERE returns bare Int64 IDs instead of full node maps
        // in the current grafeo-engine version. Fetch all and filter/sort in Rust.
        let gql = format!("MATCH (e:Episodic) RETURN e LIMIT {}", limit);
        let result = session.execute(&gql)?;

        let mut episodes: Vec<crate::types::Episode> = Vec::new();
        for row in result.rows() {
            if let Some(Value::Map(map)) = row.first() {
                if let Ok(ep) = crate::episodic::value_to_episode(&Value::Map(map.clone())) {
                    episodes.push(ep);
                }
            }
        }
        // Filter and sort in Rust (GQL ORDER BY + WHERE changes return format)
        episodes.retain(|ep| !ep.consolidated);
        episodes.sort_by_key(|ep| ep.timestamp);
        episodes.truncate(limit);
        Ok(episodes)
    }

    /// Retrieve unconsolidated episodes, optionally filtered by knowledge
    /// subtype, ordered by timestamp ascending (ADR-068 Step 1).
    ///
    /// When `subtype` is `Some`, only episodes carrying that `knowledge_subtype`
    /// are returned; `None` returns all unconsolidated episodes regardless of
    /// classification. The episode node id is preserved so the distillation
    /// pipeline can mark episodes consolidated after promotion.
    pub fn get_unconsolidated_episodes_by_subtype(
        &self,
        subtype: Option<crate::types::KnowledgeSubType>,
        limit: usize,
    ) -> Result<Vec<crate::types::Episode>> {
        // Clamp to the GQL int64 range — a raw usize::MAX literal is rejected
        // by the engine with a syntax error.
        let limit = limit.min(i64::MAX as usize);
        let session = self.db.session();
        // Note: GQL ORDER BY / WHERE returns bare Int64 IDs instead of full
        // node maps in the current grafeo-engine version. Fetch all and
        // filter/sort in Rust.
        let gql = format!("MATCH (e:Episodic) RETURN e LIMIT {}", limit);
        let result = session.execute(&gql)?;

        let mut episodes: Vec<crate::types::Episode> = Vec::new();
        for row in result.rows() {
            if let Some(Value::Map(map)) = row.first() {
                if let Ok(ep) = crate::episodic::value_to_episode(&Value::Map(map.clone())) {
                    episodes.push(ep);
                }
            }
        }
        // Filter unconsolidated + subtype, then sort by timestamp ascending.
        episodes.retain(|ep| {
            !ep.consolidated
                && !ep.metadata.contains_key("distiller_skip")
                && subtype
                    .as_ref()
                    .is_none_or(|st| ep.knowledge_subtype == Some(st.clone()))
        });
        episodes.sort_by_key(|ep| ep.timestamp);
        episodes.truncate(limit);
        Ok(episodes)
    }

    /// Remove old consolidated episodes beyond the retention period.
    ///
    /// Returns the number of deleted episodes.
    pub fn cleanup_old_episodes(&self, retention_days: u32) -> Result<usize> {
        let cutoff = chrono::Utc::now() - TimeDelta::days(i64::from(retention_days));
        let cutoff_us = cutoff.timestamp_micros();

        // Find candidate episodes and filter by timestamp in Rust.
        let session = self.db.session();
        let gql = "MATCH (e:Episodic) WHERE e.consolidated = true RETURN e";
        let result = session.execute(gql)?;

        let mut ids = Vec::new();
        for row in result.rows() {
            if let Some(Value::Map(map)) = row.first() {
                if let Ok(ep) = crate::episodic::value_to_episode(&Value::Map(map.clone())) {
                    if ep.timestamp.timestamp_micros() < cutoff_us {
                        if let Some(id) = ep.id {
                            ids.push(id);
                        }
                    }
                }
            }
        }

        let count = ids.len();
        for id in ids {
            self.db.delete_node(id);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DEFAULT_EMBEDDING_DIM, Episode};
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn test_dt() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn make_episode(session_id: &str, content: &str, ts: DateTime<Utc>) -> Episode {
        Episode {
            id: None,
            session_id: session_id.to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: content.to_string(),
            embedding: Some(vec![0.1f32; DEFAULT_EMBEDDING_DIM]),
            timestamp: ts,
            consolidated: false,
            metadata: HashMap::new(),
            importance: 0.5,
            knowledge_subtype: None,
        }
    }

    #[test]
    fn test_mark_consolidated() {
        let store = test_store();
        let ep = make_episode("s1", "hello", test_dt());
        let id = store.store_episode(&ep).unwrap();

        store.mark_episode_consolidated(id).unwrap();

        let unconsolidated = store.get_unconsolidated_episodes(10).unwrap();
        assert!(unconsolidated.is_empty());
    }

    #[test]
    fn test_mark_episodes_skipped_persists_and_excludes_from_scan() {
        // ADR-068 A3 / review P2-1 (storage layer): the skip tombstone is
        // merged into the episode metadata property and the distiller scan
        // (get_unconsolidated_episodes_by_subtype) excludes marked episodes.
        let store = test_store();
        let base = test_dt();

        let mut ep1 = make_episode("s1", "skip me", base);
        ep1.knowledge_subtype = Some(crate::types::KnowledgeSubType::Fact);
        let id1 = store.store_episode(&ep1).unwrap();

        let mut ep2 = make_episode("s1", "defer me", base + TimeDelta::minutes(1));
        ep2.knowledge_subtype = Some(crate::types::KnowledgeSubType::Fact);
        store.store_episode(&ep2).unwrap();

        store
            .mark_episodes_skipped(&[id1], "user lives in Shanghai", "contradictory")
            .unwrap();

        // The tombstoned episode is excluded; the untouched one still appears.
        let remaining = store
            .get_unconsolidated_episodes_by_subtype(None, 10)
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "defer me");
        assert!(!remaining[0].metadata.contains_key("distiller_skip"));

        // The marker survives the property round-trip on the marked episode.
        let node = store.db.get_node(id1).unwrap();
        let props = node.properties_as_btree();
        let meta_str = props
            .get("metadata")
            .expect("metadata property present")
            .as_str()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(meta_str).unwrap();
        assert_eq!(parsed["distiller_skip"]["cluster_key"], "user lives in Shanghai");
        assert_eq!(parsed["distiller_skip"]["reason"], "contradictory");
        assert!(parsed["distiller_skip"]["at"].is_string());
    }

    #[test]
    fn test_get_unconsolidated_episodes() {
        let store = test_store();
        let base = test_dt();

        let ep1 = make_episode("s1", "first", base);
        store.store_episode(&ep1).unwrap();

        let mut ep2 = make_episode("s1", "second", base + TimeDelta::minutes(1));
        ep2.consolidated = true;
        store.store_episode(&ep2).unwrap();

        // Verify both episodes were stored by retrieving directly
        let node1 = store
            .db
            .get_node(grafeo_common::types::NodeId::new(0))
            .unwrap();
        let node2 = store
            .db
            .get_node(grafeo_common::types::NodeId::new(1))
            .unwrap();
        let props1: Vec<(String, Value)> = node1
            .properties_as_btree()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        let props2: Vec<(String, Value)> = node2
            .properties_as_btree()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        let ep1_restored =
            Episode::from_properties(grafeo_common::types::NodeId::new(0), &props1).unwrap();
        let ep2_restored =
            Episode::from_properties(grafeo_common::types::NodeId::new(1), &props2).unwrap();
        assert_eq!(ep1_restored.content, "first");
        assert!(!ep1_restored.consolidated);
        assert_eq!(ep2_restored.content, "second");
        assert!(ep2_restored.consolidated);

        let results = store.get_unconsolidated_episodes(10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "first");
    }

    #[test]
    fn test_cleanup_old_episodes() {
        let store = test_store();
        let now = Utc::now();

        let mut ep1 = make_episode("s1", "old", now - TimeDelta::days(30));
        ep1.consolidated = true;
        let mut ep2 = make_episode("s1", "recent", now - TimeDelta::days(5));
        ep2.consolidated = true;
        let mut ep3 = make_episode("s1", "unconsolidated old", now - TimeDelta::days(30));
        ep3.consolidated = false;
        store.store_episode(&ep1).unwrap();
        store.store_episode(&ep2).unwrap();
        store.store_episode(&ep3).unwrap();

        let deleted = store.cleanup_old_episodes(14).unwrap();
        assert_eq!(deleted, 1); // only ep1 is old AND consolidated

        let remaining = store.search_episodes_by_session("s1", 10).unwrap();
        assert_eq!(remaining.len(), 2);
    }
}
