//! Episode retrieval and search operations.
#![allow(clippy::collapsible_if)]

use chrono::{DateTime, Utc};
use grafeo_common::types::Value;

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::retrieval::cosine_distance_to_similarity;
use crate::types::{labels, Episode};

impl GrafeoStore {
    /// Search episodes within a time range.
    ///
    /// Returns up to `limit` episodes whose timestamp falls between
    /// `start` (inclusive) and `end` (inclusive), ordered by timestamp descending.
    pub fn search_episodes_by_time(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let start_us = start.timestamp_micros();
        let end_us = end.timestamp_micros();

        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut episodes = Vec::new();
        for node_id in node_ids {
            if let Some(node) = self.db.get_node(node_id) {
                let props: Vec<(String, Value)> = node
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                if let Ok(ep) = Episode::from_properties(node_id, &props) {
                    let ts_us = ep.timestamp.timestamp_micros();
                    if ts_us >= start_us && ts_us <= end_us {
                        episodes.push(ep);
                    }
                }
            }
        }
        // Already sorted by the order nodes_by_label returns, but ensure
        // timestamp ordering anyway
        episodes.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        episodes.truncate(limit);
        Ok(episodes)
    }

    /// Search episodes belonging to a specific session.
    ///
    /// Returns up to `limit` episodes with the given `session_id`,
    /// ordered by timestamp descending.
    ///
    /// S5.3: Uses `graph_store().nodes_by_label()` + in-memory filtering
    /// instead of raw GQL string interpolation to prevent GQL injection.
    /// The underlying GrafeoDB engine's GQL query for labels does not
    /// reliably work in in-memory mode, so we use the graph store API.
    pub fn search_episodes_by_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut episodes = Vec::new();
        for node_id in node_ids {
            if let Some(node) = self.db.get_node(node_id) {
                let props: Vec<(String, Value)> = node
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                if let Ok(ep) = Episode::from_properties(node_id, &props) {
                    if ep.session_id == session_id {
                        episodes.push(ep);
                    }
                }
            }
        }

        // Sort by timestamp descending and apply limit
        episodes.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        episodes.truncate(limit);
        Ok(episodes)
    }

    /// Search episodes by keyword using BM25 full-text search.
    ///
    /// Returns up to `limit` episodes with their BM25 relevance scores,
    /// sorted by score descending.
    pub fn search_episodes_by_keyword(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(Episode, f64)>> {
        let results = self.db.text_search(labels::EPISODIC, "content", query, limit)?;

        let mut episodes = Vec::new();
        for (node_id, score) in results {
            if let Some(node) = self.db.get_node(node_id) {
                let props: Vec<(String, Value)> = node
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                if let Ok(ep) = Episode::from_properties(node_id, &props) {
                    episodes.push((ep, score));
                }
            }
        }
        Ok(episodes)
    }

    /// Search episodes by embedding similarity using HNSW vector search.
    ///
    /// Returns up to `limit` episodes with their similarity scores,
    /// sorted by similarity descending (higher = more similar).
    pub fn search_episodes_by_embedding(
        &self,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Episode, f64)>> {
        let raw = self
            .db
            .vector_search(labels::EPISODIC, "embedding", embedding, limit, None, None)?;

        let mut episodes = Vec::new();
        for (node_id, distance) in raw {
            // Convert cosine distance to similarity score using shared function.
            let score = cosine_distance_to_similarity(distance);
            if let Some(node) = self.db.get_node(node_id) {
                let props: Vec<(String, Value)> = node
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                if let Ok(ep) = Episode::from_properties(node_id, &props) {
                    episodes.push((ep, score));
                }
            }
        }
        Ok(episodes)
    }

    /// List all episodes ordered by timestamp descending.
    ///
    /// Returns up to `limit` episodes across all sessions.
    pub fn list_all_episodes(&self, limit: usize) -> Result<Vec<Episode>> {
        let session = self.db.session();
        let gql = format!(
            "MATCH (e:Episodic) RETURN e ORDER BY e.timestamp DESC LIMIT {}",
            limit
        );
        let result = session.execute(&gql)?;

        let mut episodes = Vec::new();
        for row in result.rows() {
            if let Some(Value::Map(map)) = row.first() {
                if let Ok(ep) = crate::episodic::value_to_episode(&Value::Map(map.clone())) {
                    episodes.push(ep);
                }
            }
        }
        Ok(episodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_EMBEDDING_DIM;
    use chrono::TimeDelta;
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
        }
    }

    #[test]
    fn test_search_by_time() {
        let store = test_store();
        let base = test_dt();

        let ep1 = make_episode("s1", "hello", base - TimeDelta::hours(2));
        let ep2 = make_episode("s1", "world", base - TimeDelta::hours(1));
        let ep3 = make_episode("s1", "today", base);
        store.store_episode(&ep1).unwrap();
        store.store_episode(&ep2).unwrap();
        store.store_episode(&ep3).unwrap();

        let results = store
            .search_episodes_by_time(base - TimeDelta::hours(1), base, 10)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "today");
        assert_eq!(results[1].content, "world");
    }

    #[test]
    fn test_search_by_session() {
        let store = test_store();
        let base = test_dt();

        let ep1 = make_episode("alpha", "msg a", base);
        let ep2 = make_episode("beta", "msg b", base);
        let ep3 = make_episode("alpha", "msg c", base + TimeDelta::minutes(1));
        store.store_episode(&ep1).unwrap();
        store.store_episode(&ep2).unwrap();
        store.store_episode(&ep3).unwrap();

        let results = store.search_episodes_by_session("alpha", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "msg c");
        assert_eq!(results[1].content, "msg a");
    }

    #[test]
    fn test_search_by_keyword() {
        let store = test_store();
        let base = test_dt();

        let ep1 = make_episode("s1", "rust programming language", base);
        let ep2 = make_episode("s1", "python scripting", base);
        store.store_episode(&ep1).unwrap();
        store.store_episode(&ep2).unwrap();

        let results = store.search_episodes_by_keyword("rust", 10).unwrap();
        assert!(!results.is_empty());
        let contents: Vec<String> = results.iter().map(|(ep, _)| ep.content.clone()).collect();
        assert!(contents.iter().any(|c| c.contains("rust")));
    }

    #[test]
    fn test_search_by_embedding() {
        let store = test_store();
        let base = test_dt();

        let mut ep1 = make_episode("s1", "vector one", base);
        ep1.embedding = Some(vec![1.0f32; DEFAULT_EMBEDDING_DIM]);
        let mut ep2 = make_episode("s1", "vector two", base);
        ep2.embedding = Some(vec![0.0f32; DEFAULT_EMBEDDING_DIM]);
        store.store_episode(&ep1).unwrap();
        store.store_episode(&ep2).unwrap();

        let query = vec![0.9f32; DEFAULT_EMBEDDING_DIM];
        let results = store.search_episodes_by_embedding(&query, 10).unwrap();
        assert!(!results.is_empty());
        // The episode with embedding closer to the query should rank higher.
        assert_eq!(results[0].0.content, "vector one");
    }
}
