//! Episode write operations.

use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{labels, edge_types, Episode};

// ---------------------------------------------------------------------------
// GrafeoStore episode writes
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Store an Episode node in the graph database.
    /// Returns the newly created [`NodeId`].
    pub fn store_episode(&self, episode: &Episode) -> Result<NodeId> {
        let props = episode.to_properties();
        let embedding = props.iter().find(|(k, _)| k == "embedding").map(|(_, v)| v.clone());
        let non_emb_props: Vec<_> = props.into_iter().filter(|(k, _)| k != "embedding").collect();

        let id = self
            .db
            .create_node_with_props(&[labels::EPISODIC], non_emb_props.iter().map(|(k, v)| (k.as_str(), v.clone())));

        if let Some(emb) = embedding {
            self.db.set_node_property(id, "embedding", emb);
        }

        Ok(id)
    }

    /// Store an Episode and link it to a Session node via `HAS_MEMORY` edge.
    ///
    /// If a Session node with the given `session_id` does not exist, one is
    /// created automatically.
    pub fn store_episode_with_session(
        &self,
        episode: &Episode,
        session_id: &str,
    ) -> Result<NodeId> {
        let mut episode = episode.clone();
        episode.session_id = session_id.to_string();

        let episode_id = self.store_episode(&episode)?;

        // Find or create the Session node.
        let session_node_id = self.find_or_create_session(session_id)?;

        // Create HAS_MEMORY edge.
        self.db.create_edge_with_props(
            session_node_id,
            episode_id,
            edge_types::HAS_MEMORY,
            std::iter::empty::<(&str, Value)>(),
        );

        Ok(episode_id)
    }

    /// Find an existing Session node by `session_id`, or create one.
    ///
    /// S5.3: Uses parameterized query (`$sid`) instead of string
    /// interpolation to prevent GQL injection.
    fn find_or_create_session(&self, session_id: &str) -> Result<NodeId> {
        let session = self.db.session();
        let gql = "MATCH (s:Session) WHERE s.session_id = $sid RETURN id(s)";
        let mut params = std::collections::HashMap::new();
        params.insert("sid".to_string(), Value::from(session_id));
        let result = session.execute_with_params(gql, params)?;

        if let Some(row) = result.rows().first() {
            if let Some(Value::Int64(id)) = row.first() {
                return Ok(NodeId::new(*id as u64));
            }
        }

        // Session does not exist — create it.
        let id = self.db.create_node_with_props(
            &[labels::SESSION],
            [("session_id", Value::from(session_id))],
        );
        Ok(id)
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

    fn make_episode(content: &str) -> Episode {
        Episode {
            id: None,
            session_id: "s1".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: content.to_string(),
            embedding: Some(vec![0.1f32; DEFAULT_EMBEDDING_DIM]),
            timestamp: test_dt(),
            consolidated: false,
            metadata: HashMap::new(),
            importance: 0.5,
        }
    }

    #[test]
    fn test_store_episode_roundtrip() {
        let store = test_store();
        let ep = make_episode("hello world");
        let id = store.store_episode(&ep).unwrap();

        let node = store.db.get_node(id).unwrap();
        let props: Vec<(String, Value)> = node
            .properties_as_btree()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        let restored = Episode::from_properties(id, &props).unwrap();
        assert_eq!(restored.content, "hello world");
        assert_eq!(restored.session_id, "s1");
    }

    #[test]
    fn test_store_episode_with_session() {
        let store = test_store();
        let ep = make_episode("session msg");
        let id = store.store_episode_with_session(&ep, "sess-42").unwrap();

        let node = store.db.get_node(id).unwrap();
        let props: Vec<(String, Value)> = node
            .properties_as_btree()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        let restored = Episode::from_properties(id, &props).unwrap();
        assert_eq!(restored.session_id, "sess-42");

        // Verify session node exists and HAS_MEMORY edge was created.
        let session = store.db.session();
        let gql = "MATCH (s:Session)-[:HAS_MEMORY]->(e:Episodic) WHERE s.session_id = 'sess-42' RETURN e";
        let result = session.execute(gql).unwrap();
        assert_eq!(result.rows().len(), 1);
    }
}
