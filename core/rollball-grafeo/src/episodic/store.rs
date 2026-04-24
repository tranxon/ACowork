//! Episode write operations with automatic content classification and artifact compression.
#![allow(clippy::collapsible_if)]

use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{labels, edge_types, ArtifactRef, ContentType, Episode};

// ---------------------------------------------------------------------------
// Content auto-classification
// ---------------------------------------------------------------------------

/// Classify episode content using deterministic heuristics (no LLM).
///
/// - **Artifact**: code blocks, file paths, diff format.
/// - **Structural**: markdown lists, tables.
/// - **Informational**: default fallback.
pub fn classify_content(content: &str) -> ContentType {
    let has_code_block = content.contains("```");
    let has_diff_format = content.contains("diff\n")
        || content.contains("@@ ")
        || (content.lines().any(|l| l.starts_with("+ "))
            && content.lines().any(|l| l.starts_with("- ")));
    let has_file_path = content.lines().any(|l| {
        let t = l.trim();
        (t.starts_with("src/")
            || t.starts_with("lib/")
            || t.starts_with("tests/")
            || t.starts_with("docs/")
            || t.starts_with("examples/")
            || t.starts_with("core/")
            || t.starts_with("apps/")
            || t.ends_with(".rs")
            || t.ends_with(".py")
            || t.ends_with(".js")
            || t.ends_with(".toml")
            || t.ends_with(".md")
            || t.ends_with(".json"))
            && !t.contains(' ')
            && t.len() < 200
    });

    if has_code_block || has_diff_format || has_file_path {
        return ContentType::Artifact;
    }

    let has_markdown_list = content
        .lines()
        .any(|l| l.starts_with("- ") || l.starts_with("* ") || l.starts_with("1. "));
    let has_table = content.contains("| ") && content.contains(" |");

    if has_markdown_list || has_table {
        return ContentType::Structural;
    }

    ContentType::Informational
}

// ---------------------------------------------------------------------------
// Artifact compression
// ---------------------------------------------------------------------------

/// Extract artifact references from content heuristically.
///
/// Detects file paths and generates [`ArtifactRef`] entries with
/// line ranges when available.
pub fn extract_artifact_refs(content: &str) -> Vec<ArtifactRef> {
    let mut refs = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Scan tokens inside the line to tolerate surrounding text.
        for token in trimmed.split_whitespace() {
            let t = token.trim_matches(|c: char| {
                c == '`' || c == '*' || c == '_' || c == '(' || c == ')' || c == '[' || c == ']' || c == '<' || c == '>' || c == ':' || c == ',' || c == '.' || c == ';'
            });
            let is_path = t.starts_with("src/")
                || t.starts_with("lib/")
                || t.starts_with("tests/")
                || t.starts_with("docs/")
                || t.starts_with("examples/")
                || t.starts_with("core/")
                || t.starts_with("apps/")
                || t.ends_with(".rs")
                || t.ends_with(".py")
                || t.ends_with(".js")
                || t.ends_with(".toml")
                || t.ends_with(".md")
                || t.ends_with(".json");
            if !is_path || t.contains(' ') || t.len() >= 200 {
                continue;
            }

            // Try to infer a line range from surrounding context.
            let line_range = if content.contains("```") {
                // Look for code block boundaries near this line.
                let start = lines[..idx]
                    .iter()
                    .rposition(|l| l.trim().starts_with("```"))
                    .map(|s| (s + 2) as u32);
                let end = lines[idx..]
                    .iter()
                    .position(|l| l.trim().starts_with("```"))
                    .map(|e| (idx + e) as u32);
                match (start, end) {
                    (Some(s), Some(e)) if s <= e => Some((s, e)),
                    _ => None,
                }
            } else {
                None
            };

            refs.push(ArtifactRef {
                path: t.to_string(),
                hash: None,
                description: "Artifact referenced in episode".to_string(),
                line_range,
            });
        }
    }

    // Deduplicate by path.
    refs.sort_by(|a, b| a.path.cmp(&b.path));
    refs.dedup_by(|a, b| a.path == b.path);

    refs
}

/// Compress artifact-type content to a summary + artifact references.
///
/// The summary is the first 200 characters of the original content,
/// truncated with "..." if longer. Uses UTF-8-safe truncation to avoid
/// panicking on multi-byte character boundaries.
pub fn compress_artifact_content(content: &str) -> (String, Vec<ArtifactRef>) {
    let refs = extract_artifact_refs(content);

    let summary = if content.chars().count() > 200 {
        // UTF-8 safe truncation: take first 200 chars, then find byte boundary
        let truncated: String = content.chars().take(200).collect();
        format!("{}...", truncated)
    } else {
        content.to_string()
    };

    (summary, refs)
}

// ---------------------------------------------------------------------------
// GrafeoStore episode writes
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Store an Episode node, automatically classifying its content type.
    ///
    /// Artifact content is compressed to a summary + [`ArtifactRef`] list.
    /// Returns the newly created [`NodeId`].
    pub fn store_episode(&self, episode: &Episode) -> Result<NodeId> {
        let mut episode = episode.clone();

        // Auto-classify content.
        episode.content_type = classify_content(&episode.content);

        // Compress artifact content.
        if episode.content_type == ContentType::Artifact {
            let (compressed, refs) = compress_artifact_content(&episode.content);
            episode.content = compressed;
            episode.artifact_refs = refs;
        }

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
    fn find_or_create_session(&self, session_id: &str) -> Result<NodeId> {
        let session = self.db.session();
        let gql = format!(
            "MATCH (s:Session) WHERE s.session_id = '{}' RETURN id(s)",
            crate::episodic::escape_gql_string(session_id)
        );
        let result = session.execute(&gql)?;

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
    use crate::types::{ContentType, EMBEDDING_DIM, Episode};
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
            content_type: ContentType::Informational,
            embedding: Some(vec![0.1f32; EMBEDDING_DIM]),
            timestamp: test_dt(),
            consolidated: false,
            metadata: HashMap::new(),
            artifact_refs: vec![],
            importance: 0.5,
        }
    }

    #[test]
    fn test_classify_content_informational() {
        let text = "Hello, how are you today?";
        assert_eq!(classify_content(text), ContentType::Informational);
    }

    #[test]
    fn test_classify_content_artifact() {
        let text = "```rust\nfn main() {}\n```";
        assert_eq!(classify_content(text), ContentType::Artifact);
    }

    #[test]
    fn test_classify_content_structural() {
        let text = "- item one\n- item two";
        assert_eq!(classify_content(text), ContentType::Structural);
    }

    #[test]
    fn test_extract_artifact_refs() {
        let text = "Check src/main.rs for the entry point.";
        let refs = extract_artifact_refs(text);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "src/main.rs");
    }

    #[test]
    fn test_compress_artifact_content() {
        let text = "a".repeat(250);
        let (summary, refs) = compress_artifact_content(&text);
        assert!(summary.ends_with("..."));
        assert_eq!(summary.len(), 203); // 200 + "..."
        assert!(refs.is_empty());
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

    #[test]
    fn test_store_episode_auto_classifies_artifact() {
        let store = test_store();
        let mut ep = make_episode("```rust\nsrc/main.rs\nfn main() {}\n```");
        ep.content_type = ContentType::Informational; // will be overridden
        let id = store.store_episode(&ep).unwrap();

        let node = store.db.get_node(id).unwrap();
        let props: Vec<(String, Value)> = node
            .properties_as_btree()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        let restored = Episode::from_properties(id, &props).unwrap();
        assert_eq!(restored.content_type, ContentType::Artifact);
        assert!(!restored.artifact_refs.is_empty());
    }
}
