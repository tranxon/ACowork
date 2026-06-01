//! LPG node types for the RollBall memory system.
//!
//! Defines the five memory node structures (Episode, KnowledgeNode, ProceduralNode,
//! AutobiographicalNode) together with their enums, label/edge constants,
//! and conversions to/from Grafeo property values.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use grafeo_common::types::{NodeId, Timestamp, Value};
use serde::{Deserialize, Serialize};
use serde_json;

use crate::error::{GrafeoError, Result};

// ---------------------------------------------------------------------------
// Embedding dimension
// ---------------------------------------------------------------------------

/// Default embedding vector dimension (384d).
/// When an embedding provider is available, the actual dimension is configured
/// via [`GrafeoConfig::embedding_dim`] at [`GrafeoStore::open`] time.
pub const DEFAULT_EMBEDDING_DIM: usize = 384;

/// Configuration for opening a Grafeo persistence store.
pub struct GrafeoConfig {
    /// Filesystem path to the `.grafeo` database directory.
    pub db_path: std::path::PathBuf,
    /// Embedding vector dimension, obtained from the active [`EmbeddingProvider`].
    /// Falls back to [`DEFAULT_EMBEDDING_DIM`] when no provider is available.
    pub embedding_dim: usize,
}

impl Default for GrafeoConfig {
    fn default() -> Self {
        Self {
            db_path: std::path::PathBuf::from(".grafeo"),
            embedding_dim: DEFAULT_EMBEDDING_DIM,
        }
    }
}

// ---------------------------------------------------------------------------
// Node labels
// ---------------------------------------------------------------------------

/// LPG node labels used in the RollBall memory system.
pub mod labels {
    /// Episodic memory label (experiential layer).
    pub const EPISODIC: &str = "Episodic";
    /// Knowledge memory label (semantic layer).
    pub const KNOWLEDGE: &str = "Knowledge";
    /// Procedural memory label (behavior patterns).
    pub const PROCEDURAL: &str = "Procedural";
    /// Autobiographical memory label (self-knowledge).
    pub const AUTOBIOGRAPHICAL: &str = "Autobiographical";
    /// System configuration label.
    pub const SYSTEM_CONFIG: &str = "SystemConfig";
    /// Tool invocation record label.
    pub const TOOL_INVOCATION: &str = "ToolInvocation";
    /// Session label.
    pub const SESSION: &str = "Session";

    /// All memory labels in a static slice (for iteration).
    pub const ALL: &[&str] = &[
        EPISODIC,
        KNOWLEDGE,
        PROCEDURAL,
        AUTOBIOGRAPHICAL,
        SYSTEM_CONFIG,
        TOOL_INVOCATION,
        SESSION,
    ];
}

// ---------------------------------------------------------------------------
// Edge types
// ---------------------------------------------------------------------------

/// LPG edge types used in the RollBall memory system.
pub mod edge_types {
    /// Session owns a memory node.
    pub const HAS_MEMORY: &str = "HAS_MEMORY";
    /// Knowledge node references another knowledge node.
    pub const REFERENCES: &str = "REFERENCES";
    /// Autobiographical node self-references.
    pub const SELF_REFERENCES: &str = "SELF_REFERENCES";
    /// Tool invocation produced a memory node.
    pub const PRODUCED: &str = "PRODUCED";
    /// Knowledge node derived from an episodic node.
    pub const DERIVED_FROM: &str = "DERIVED_FROM";
    /// New knowledge node evolved from an older version.
    pub const EVOLUTION_FROM: &str = "EVOLUTION_FROM";
    /// New knowledge node corrects an older incorrect version.
    pub const CORRECTS: &str = "CORRECTS";

    /// All edge types in a static slice (for iteration).
    pub const ALL: &[&str] = &[
        HAS_MEMORY,
        REFERENCES,
        SELF_REFERENCES,
        PRODUCED,
        DERIVED_FROM,
        EVOLUTION_FROM,
        CORRECTS,
    ];
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Sub-type of a KnowledgeNode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeSubType {
    /// Objective fact.
    Fact,
    /// User preference or style.
    Preference,
    /// Relationship between entities.
    Relation,
}

impl KnowledgeSubType {
    /// Returns the string representation used in Grafeo properties.
    pub fn as_str(&self) -> &'static str {
        match self {
            KnowledgeSubType::Fact => "Fact",
            KnowledgeSubType::Preference => "Preference",
            KnowledgeSubType::Relation => "Relation",
        }
    }
}

impl std::str::FromStr for KnowledgeSubType {
    type Err = GrafeoError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "Fact" => Ok(KnowledgeSubType::Fact),
            "Preference" => Ok(KnowledgeSubType::Preference),
            "Relation" => Ok(KnowledgeSubType::Relation),
            _ => Err(GrafeoError::Memory(format!(
                "unknown KnowledgeSubType: {s}"
            ))),
        }
    }
}

/// Lifecycle status of a memory node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    /// Normal participation in retrieval.
    Active,
    /// Decayed below threshold — retained but excluded from search.
    Dormant,
    /// Recently created, pending offline confirmation.
    Pending,
}

impl NodeStatus {
    /// Returns the string representation used in Grafeo properties.
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeStatus::Active => "Active",
            NodeStatus::Dormant => "Dormant",
            NodeStatus::Pending => "Pending",
        }
    }
}

impl std::str::FromStr for NodeStatus {
    type Err = GrafeoError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "Active" => Ok(NodeStatus::Active),
            "Dormant" => Ok(NodeStatus::Dormant),
            "Pending" => Ok(NodeStatus::Pending),
            _ => Err(GrafeoError::Memory(format!("unknown NodeStatus: {s}"))),
        }
    }
}

/// Category of autobiographical memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutobioCategory {
    /// Name, age, location.
    Identity,
    /// Skills, tools.
    Capability,
    /// Constraints, weaknesses.
    Limitation,
    /// Likes, dislikes, style.
    Preference,
    /// Past events, milestones.
    History,
    /// Connections with others.
    Relationship,
}

impl AutobioCategory {
    /// Returns the string representation used in Grafeo properties.
    pub fn as_str(&self) -> &'static str {
        match self {
            AutobioCategory::Identity => "Identity",
            AutobioCategory::Capability => "Capability",
            AutobioCategory::Limitation => "Limitation",
            AutobioCategory::Preference => "Preference",
            AutobioCategory::History => "History",
            AutobioCategory::Relationship => "Relationship",
        }
    }
}

impl std::str::FromStr for AutobioCategory {
    type Err = GrafeoError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "Identity" => Ok(AutobioCategory::Identity),
            "Capability" => Ok(AutobioCategory::Capability),
            "Limitation" => Ok(AutobioCategory::Limitation),
            "Preference" => Ok(AutobioCategory::Preference),
            "History" => Ok(AutobioCategory::History),
            "Relationship" => Ok(AutobioCategory::Relationship),
            _ => Err(GrafeoError::Memory(format!(
                "unknown AutobioCategory: {s}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// A single conversation episode (user message + assistant response + context).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    /// Grafeo node ID (`None` before persistence).
    pub id: Option<NodeId>,
    /// Session identifier.
    pub session_id: String,
    /// Turn index within the session.
    pub turn_index: u32,
    /// Role: "user" | "assistant" | "tool".
    pub role: String,
    /// Content.
    pub content: String,
    /// Semantic embedding vector (generated by Runtime).
    pub embedding: Option<Vec<f32>>,
    /// Timestamp of the interaction.
    pub timestamp: DateTime<Utc>,
    /// Whether this episode has been consolidated to the semantic layer.
    pub consolidated: bool,
    /// Optional contextual metadata (topic, sentiment, etc.).
    pub metadata: HashMap<String, serde_json::Value>,
    /// Importance score assigned by LLM at write time [0.0, 1.0].
    #[serde(default)]
    pub importance: f32,
}

/// Semantic memory node — fact, preference, or relation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeNode {
    /// Grafeo node ID (`None` before persistence).
    pub id: Option<NodeId>,
    /// Subject of the knowledge (usually "user").
    pub subject: String,
    /// Predicate / relation.
    pub predicate: String,
    /// Object / value.
    pub object: String,
    /// Sub-type classification.
    pub sub_type: KnowledgeSubType,
    /// Confidence [0.0, 1.0].
    pub confidence: f32,
    /// Source episode ID (traceability).
    pub source_episode_id: Option<NodeId>,
    /// Semantic embedding.
    pub embedding: Option<Vec<f32>>,
    /// Lifecycle status.
    pub status: NodeStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Optional metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Procedural memory node — behavior pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralNode {
    /// Grafeo node ID (`None` before persistence).
    pub id: Option<NodeId>,
    /// Human-readable name.
    pub name: String,
    /// Trigger condition description.
    pub trigger_condition: String,
    /// Action pattern description.
    pub action_pattern: String,
    /// Number of successful activations.
    pub success_count: u32,
    /// Number of failed activations.
    pub fail_count: u32,
    /// Confidence [0.0, 1.0].
    pub confidence: f32,
    /// Semantic embedding.
    pub embedding: Option<Vec<f32>>,
    /// Lifecycle status.
    pub status: NodeStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Optional metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Autobiographical memory node — self-knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutobiographicalNode {
    /// Grafeo node ID (`None` before persistence).
    pub id: Option<NodeId>,
    /// Category of self-knowledge.
    pub category: AutobioCategory,
    /// Key (e.g. "name", "language", "location").
    pub key: String,
    /// Value.
    pub value: String,
    /// Confidence [0.0, 1.0].
    pub confidence: f32,
    /// Source episode ID.
    pub source_episode_id: Option<NodeId>,
    /// Semantic embedding.
    pub embedding: Option<Vec<f32>>,
    /// Lifecycle status (schema enforces Active).
    pub status: NodeStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Optional metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Helper functions for property conversion
// ---------------------------------------------------------------------------

/// Convert `chrono::DateTime<Utc>` to `grafeo_common::Timestamp`.
fn dt_to_timestamp(dt: DateTime<Utc>) -> Timestamp {
    Timestamp::from_micros(dt.timestamp_micros())
}

/// Convert `grafeo_common::Timestamp` to `chrono::DateTime<Utc>`.
fn timestamp_to_dt(ts: Timestamp) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(ts.as_micros())
        .ok_or_else(|| GrafeoError::Memory("invalid timestamp".to_string()))
}

/// Convert an optional embedding to a Grafeo Value.
fn embedding_to_value(embedding: Option<&Vec<f32>>) -> Value {
    match embedding {
        Some(v) => Value::Vector(Arc::from(v.as_slice())),
        None => Value::Null,
    }
}

/// Convert an optional Grafeo Vector value back to `Vec<f32>`.
fn value_to_embedding(value: Option<&Value>) -> Option<Vec<f32>> {
    value.and_then(|v| v.as_vector().map(|s| s.to_vec()))
}

/// Serialize metadata HashMap to a JSON string Value.
fn metadata_to_value(metadata: &HashMap<String, serde_json::Value>) -> Value {
    if metadata.is_empty() {
        Value::Null
    } else {
        match serde_json::to_string(metadata) {
            Ok(s) => Value::String(s.into()),
            Err(_) => Value::Null,
        }
    }
}

/// Deserialize metadata from a JSON string Value.
fn value_to_metadata(value: Option<&Value>) -> Result<HashMap<String, serde_json::Value>> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => {
            serde_json::from_str(s).map_err(GrafeoError::Serialization)
        }
        _ => Ok(HashMap::new()),
    }
}

/// Build a property map from a slice of (key, value) pairs for lookup.
fn prop_map(props: &[(String, Value)]) -> HashMap<&str, &Value> {
    props
        .iter()
        .map(|(k, v)| (k.as_str(), v))
        .collect()
}

/// Get a required string property.
fn get_string<'a>(map: &HashMap<&str, &'a Value>, key: &str) -> Result<&'a str> {
    map.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| GrafeoError::Memory(format!("missing required property: {key}")))
}

/// Get a required i64 property.
fn get_i64(map: &HashMap<&str, &Value>, key: &str) -> Result<i64> {
    map.get(key)
        .and_then(|v| v.as_int64())
        .ok_or_else(|| GrafeoError::Memory(format!("missing required property: {key}")))
}

/// Get a required f64 property and cast to f32.
fn get_f32(map: &HashMap<&str, &Value>, key: &str) -> Result<f32> {
    map.get(key)
        .and_then(|v| v.as_float64())
        .map(|f| f as f32)
        .ok_or_else(|| GrafeoError::Memory(format!("missing required property: {key}")))
}

/// Get a required bool property.
fn get_bool(map: &HashMap<&str, &Value>, key: &str) -> Result<bool> {
    map.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| GrafeoError::Memory(format!("missing required property: {key}")))
}

/// Get an optional NodeId from an Int64 value.
fn get_optional_node_id(map: &HashMap<&str, &Value>, key: &str) -> Option<NodeId> {
    map.get(key).and_then(|v| v.as_int64()).map(|id| NodeId::new(id as u64))
}

// ---------------------------------------------------------------------------
// Episode conversions
// ---------------------------------------------------------------------------

impl Episode {
    /// Convert to Grafeo node properties for storage.
    pub fn to_properties(&self) -> Vec<(String, Value)> {
        let mut props = vec![
            ("session_id".to_string(), Value::from(self.session_id.as_str())),
            ("turn_index".to_string(), Value::from(i64::from(self.turn_index))),
            ("role".to_string(), Value::from(self.role.as_str())),
            ("content".to_string(), Value::from(self.content.as_str())),
            ("timestamp".to_string(), Value::from(dt_to_timestamp(self.timestamp))),
            ("consolidated".to_string(), Value::from(self.consolidated)),
            ("metadata".to_string(), metadata_to_value(&self.metadata)),
            ("importance".to_string(), Value::from(f64::from(self.importance))),
        ];
        if let Some(ref emb) = self.embedding {
            props.push(("embedding".to_string(), embedding_to_value(Some(emb))));
        }
        props
    }

    /// Reconstruct from Grafeo node properties.
    pub fn from_properties(id: NodeId, props: &[(String, Value)]) -> Result<Self> {
        let map = prop_map(props);
        Ok(Episode {
            id: Some(id),
            session_id: get_string(&map, "session_id")?.to_string(),
            turn_index: get_i64(&map, "turn_index")? as u32,
            role: get_string(&map, "role")?.to_string(),
            content: get_string(&map, "content")?.to_string(),
            embedding: value_to_embedding(map.get("embedding").copied()),
            timestamp: timestamp_to_dt(
                map.get("timestamp")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing timestamp".to_string()))?,
            )?,
            consolidated: get_bool(&map, "consolidated")?,
            metadata: value_to_metadata(map.get("metadata").copied())?,
            importance: map
                .get("importance")
                .and_then(|v| v.as_float64())
                .map(|f| f as f32)
                .unwrap_or(0.0),
        })
    }
}

// ---------------------------------------------------------------------------
// KnowledgeNode conversions
// ---------------------------------------------------------------------------

impl KnowledgeNode {
    /// Convert to Grafeo node properties for storage.
    pub fn to_properties(&self) -> Vec<(String, Value)> {
        let content = format!("{} {} {}", self.subject, self.predicate, self.object);
        let mut props = vec![
            ("content".to_string(), Value::from(content.as_str())),
            ("subject".to_string(), Value::from(self.subject.as_str())),
            ("predicate".to_string(), Value::from(self.predicate.as_str())),
            ("object".to_string(), Value::from(self.object.as_str())),
            ("sub_type".to_string(), Value::from(self.sub_type.as_str())),
            ("confidence".to_string(), Value::from(f64::from(self.confidence))),
            ("status".to_string(), Value::from(self.status.as_str())),
            ("created_at".to_string(), Value::from(dt_to_timestamp(self.created_at))),
            ("updated_at".to_string(), Value::from(dt_to_timestamp(self.updated_at))),
            ("metadata".to_string(), metadata_to_value(&self.metadata)),
        ];
        if let Some(id) = self.source_episode_id {
            props.push(("source_episode_id".to_string(), Value::from(id.as_u64() as i64)));
        }
        if let Some(ref emb) = self.embedding {
            props.push(("embedding".to_string(), embedding_to_value(Some(emb))));
        }
        props
    }

    /// Reconstruct from Grafeo node properties.
    pub fn from_properties(id: NodeId, props: &[(String, Value)]) -> Result<Self> {
        let map = prop_map(props);
        Ok(KnowledgeNode {
            id: Some(id),
            subject: get_string(&map, "subject")?.to_string(),
            predicate: get_string(&map, "predicate")?.to_string(),
            object: get_string(&map, "object")?.to_string(),
            sub_type: get_string(&map, "sub_type")?.parse()?,
            confidence: get_f32(&map, "confidence")?,
            source_episode_id: get_optional_node_id(&map, "source_episode_id"),
            embedding: value_to_embedding(map.get("embedding").copied()),
            status: get_string(&map, "status")?.parse()?,
            created_at: timestamp_to_dt(
                map.get("created_at")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing created_at".to_string()))?,
            )?,
            updated_at: timestamp_to_dt(
                map.get("updated_at")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing updated_at".to_string()))?,
            )?,
            metadata: value_to_metadata(map.get("metadata").copied())?,
        })
    }
}

// ---------------------------------------------------------------------------
// ProceduralNode conversions
// ---------------------------------------------------------------------------

impl ProceduralNode {
    /// Convert to Grafeo node properties for storage.
    pub fn to_properties(&self) -> Vec<(String, Value)> {
        let content = format!("{} {} {}", self.name, self.trigger_condition, self.action_pattern);
        let mut props = vec![
            ("content".to_string(), Value::from(content.as_str())),
            ("name".to_string(), Value::from(self.name.as_str())),
            ("trigger_condition".to_string(), Value::from(self.trigger_condition.as_str())),
            ("action_pattern".to_string(), Value::from(self.action_pattern.as_str())),
            ("success_count".to_string(), Value::from(i64::from(self.success_count))),
            ("fail_count".to_string(), Value::from(i64::from(self.fail_count))),
            ("confidence".to_string(), Value::from(f64::from(self.confidence))),
            ("status".to_string(), Value::from(self.status.as_str())),
            ("created_at".to_string(), Value::from(dt_to_timestamp(self.created_at))),
            ("updated_at".to_string(), Value::from(dt_to_timestamp(self.updated_at))),
            ("metadata".to_string(), metadata_to_value(&self.metadata)),
        ];
        if let Some(ref emb) = self.embedding {
            props.push(("embedding".to_string(), embedding_to_value(Some(emb))));
        }
        props
    }

    /// Reconstruct from Grafeo node properties.
    pub fn from_properties(id: NodeId, props: &[(String, Value)]) -> Result<Self> {
        let map = prop_map(props);
        Ok(ProceduralNode {
            id: Some(id),
            name: get_string(&map, "name")?.to_string(),
            trigger_condition: get_string(&map, "trigger_condition")?.to_string(),
            action_pattern: get_string(&map, "action_pattern")?.to_string(),
            success_count: get_i64(&map, "success_count")? as u32,
            fail_count: get_i64(&map, "fail_count")? as u32,
            confidence: get_f32(&map, "confidence")?,
            embedding: value_to_embedding(map.get("embedding").copied()),
            status: get_string(&map, "status")?.parse()?,
            created_at: timestamp_to_dt(
                map.get("created_at")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing created_at".to_string()))?,
            )?,
            updated_at: timestamp_to_dt(
                map.get("updated_at")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing updated_at".to_string()))?,
            )?,
            metadata: value_to_metadata(map.get("metadata").copied())?,
        })
    }
}

// ---------------------------------------------------------------------------
// AutobiographicalNode conversions
// ---------------------------------------------------------------------------

impl AutobiographicalNode {
    /// Convert to Grafeo node properties for storage.
    pub fn to_properties(&self) -> Vec<(String, Value)> {
        let content = format!("{}: {}", self.key, self.value);
        let mut props = vec![
            ("content".to_string(), Value::from(content.as_str())),
            ("category".to_string(), Value::from(self.category.as_str())),
            ("key".to_string(), Value::from(self.key.as_str())),
            ("value".to_string(), Value::from(self.value.as_str())),
            ("confidence".to_string(), Value::from(f64::from(self.confidence))),
            ("status".to_string(), Value::from(self.status.as_str())),
            ("created_at".to_string(), Value::from(dt_to_timestamp(self.created_at))),
            ("updated_at".to_string(), Value::from(dt_to_timestamp(self.updated_at))),
            ("metadata".to_string(), metadata_to_value(&self.metadata)),
        ];
        if let Some(id) = self.source_episode_id {
            props.push(("source_episode_id".to_string(), Value::from(id.as_u64() as i64)));
        }
        if let Some(ref emb) = self.embedding {
            props.push(("embedding".to_string(), embedding_to_value(Some(emb))));
        }
        props
    }

    /// Reconstruct from Grafeo node properties.
    pub fn from_properties(id: NodeId, props: &[(String, Value)]) -> Result<Self> {
        let map = prop_map(props);
        Ok(AutobiographicalNode {
            id: Some(id),
            category: get_string(&map, "category")?.parse()?,
            key: get_string(&map, "key")?.to_string(),
            value: get_string(&map, "value")?.to_string(),
            confidence: get_f32(&map, "confidence")?,
            source_episode_id: get_optional_node_id(&map, "source_episode_id"),
            embedding: value_to_embedding(map.get("embedding").copied()),
            status: get_string(&map, "status")?.parse()?,
            created_at: timestamp_to_dt(
                map.get("created_at")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing created_at".to_string()))?,
            )?,
            updated_at: timestamp_to_dt(
                map.get("updated_at")
                    .and_then(|v| v.as_timestamp())
                    .ok_or_else(|| GrafeoError::Memory("missing updated_at".to_string()))?,
            )?,
            metadata: value_to_metadata(map.get("metadata").copied())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helper to create a test DateTime<Utc>
    // -----------------------------------------------------------------------
    fn test_dt() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    // =====================================================================
    // Episode round-trip tests
    // =====================================================================

    #[test]
    fn test_episode_roundtrip_full() {
        let original = Episode {
            id: None,
            session_id: "sess-42".to_string(),
            turn_index: 7,
            role: "user".to_string(),
            content: "Hello, world!".to_string(),
            embedding: Some(test_embedding()),
            timestamp: test_dt(),
            consolidated: false,
            metadata: {
                let mut m = HashMap::new();
                m.insert("topic".to_string(), serde_json::Value::String("greeting".to_string()));
                m
            },
            importance: 0.5,
        };

        let props = original.to_properties();
        let restored = Episode::from_properties(NodeId::new(1), &props).unwrap();

        assert_eq!(restored.session_id, original.session_id);
        assert_eq!(restored.turn_index, original.turn_index);
        assert_eq!(restored.role, original.role);
        assert_eq!(restored.content, original.content);
        assert_eq!(restored.consolidated, original.consolidated);
        assert_eq!(restored.importance, original.importance);
        assert_eq!(restored.metadata, original.metadata);
        assert!(restored.embedding.is_some());
        assert_eq!(restored.embedding.unwrap().len(), DEFAULT_EMBEDDING_DIM);
    }

    #[test]
    fn test_episode_roundtrip_minimal() {
        let original = Episode {
            id: None,
            session_id: "sess-1".to_string(),
            turn_index: 0,
            role: "assistant".to_string(),
            content: "Sure.".to_string(),
            embedding: None,
            timestamp: test_dt(),
            consolidated: true,
            metadata: HashMap::new(),
            importance: 0.8,
        };

        let props = original.to_properties();
        let restored = Episode::from_properties(NodeId::new(2), &props).unwrap();

        assert!(restored.embedding.is_none());
    }

    // =====================================================================
    // KnowledgeNode round-trip tests
    // =====================================================================

    #[test]
    fn test_knowledge_node_roundtrip_fact() {
        let original = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(),
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.95,
            source_episode_id: Some(NodeId::new(10)),
            embedding: Some(test_embedding()),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };

        let props = original.to_properties();
        let restored = KnowledgeNode::from_properties(NodeId::new(3), &props).unwrap();

        assert_eq!(restored.subject, original.subject);
        assert_eq!(restored.predicate, original.predicate);
        assert_eq!(restored.object, original.object);
        assert_eq!(restored.sub_type, KnowledgeSubType::Fact);
        assert_eq!(restored.confidence, original.confidence);
        assert_eq!(restored.source_episode_id, original.source_episode_id);
        assert_eq!(restored.status, NodeStatus::Active);

    }

    #[test]
    fn test_knowledge_node_roundtrip_preference() {
        let original = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "prefers".to_string(),
            object: "concise replies".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Pending,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };

        let props = original.to_properties();
        let restored = KnowledgeNode::from_properties(NodeId::new(4), &props).unwrap();

        assert_eq!(restored.sub_type, KnowledgeSubType::Preference);
        assert_eq!(restored.status, NodeStatus::Pending);
        assert!(restored.source_episode_id.is_none());
        assert!(restored.embedding.is_none());
    }

    // =====================================================================
    // ProceduralNode round-trip tests
    // =====================================================================

    #[test]
    fn test_procedural_node_roundtrip() {
        let original = ProceduralNode {
            id: None,
            name: "concise_output".to_string(),
            trigger_condition: "user asks for summary".to_string(),
            action_pattern: "reply in 3 sentences max".to_string(),
            success_count: 12,
            fail_count: 1,
            confidence: 0.85,
            embedding: Some(test_embedding()),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };

        let props = original.to_properties();
        let restored = ProceduralNode::from_properties(NodeId::new(5), &props).unwrap();

        assert_eq!(restored.name, original.name);
        assert_eq!(restored.trigger_condition, original.trigger_condition);
        assert_eq!(restored.action_pattern, original.action_pattern);
        assert_eq!(restored.success_count, 12);
        assert_eq!(restored.fail_count, 1);
        assert_eq!(restored.confidence, original.confidence);
        assert_eq!(restored.status, NodeStatus::Active);

    }

    #[test]
    fn test_procedural_node_roundtrip_minimal() {
        let original = ProceduralNode {
            id: None,
            name: "fallback".to_string(),
            trigger_condition: "always".to_string(),
            action_pattern: "be polite".to_string(),
            success_count: 0,
            fail_count: 0,
            confidence: 0.5,
            embedding: None,
            status: NodeStatus::Dormant,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };

        let props = original.to_properties();
        let restored = ProceduralNode::from_properties(NodeId::new(6), &props).unwrap();

        assert_eq!(restored.status, NodeStatus::Dormant);
        assert!(restored.embedding.is_none());
    }

    // =====================================================================
    // AutobiographicalNode round-trip tests
    // =====================================================================

    #[test]
    fn test_autobiographical_node_roundtrip_identity() {
        let original = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Identity,
            key: "name".to_string(),
            value: "WeatherBot".to_string(),
            confidence: 1.0,
            source_episode_id: None,
            embedding: Some(test_embedding()),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };

        let props = original.to_properties();
        let restored = AutobiographicalNode::from_properties(NodeId::new(7), &props).unwrap();

        assert_eq!(restored.category, AutobioCategory::Identity);
        assert_eq!(restored.key, "name");
        assert_eq!(restored.value, "WeatherBot");
        assert_eq!(restored.confidence, 1.0);
        assert_eq!(restored.status, NodeStatus::Active);
        assert!(restored.embedding.is_some());
    }

    #[test]
    fn test_autobiographical_node_roundtrip_history() {
        let original = AutobiographicalNode {
            id: None,
            category: AutobioCategory::History,
            key: "first_skill".to_string(),
            value: "learned weekly-report".to_string(),
            confidence: 0.9,
            source_episode_id: Some(NodeId::new(20)),
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: HashMap::new(),
        };

        let props = original.to_properties();
        let restored = AutobiographicalNode::from_properties(NodeId::new(8), &props).unwrap();

        assert_eq!(restored.category, AutobioCategory::History);
        assert_eq!(restored.source_episode_id, Some(NodeId::new(20)));
        assert!(restored.embedding.is_none());
    }

    // =====================================================================
    // Enum string round-trip tests
    // =====================================================================

    #[test]
    fn test_knowledge_sub_type_roundtrip() {
        for variant in [KnowledgeSubType::Fact, KnowledgeSubType::Preference, KnowledgeSubType::Relation] {
            let s = variant.as_str();
            let parsed: KnowledgeSubType = s.parse().unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn test_node_status_roundtrip() {
        for variant in [NodeStatus::Active, NodeStatus::Dormant, NodeStatus::Pending] {
            let s = variant.as_str();
            let parsed: NodeStatus = s.parse().unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn test_autobio_category_roundtrip() {
        for variant in [
            AutobioCategory::Identity,
            AutobioCategory::Capability,
            AutobioCategory::Limitation,
            AutobioCategory::Preference,
            AutobioCategory::History,
            AutobioCategory::Relationship,
        ] {
            let s = variant.as_str();
            let parsed: AutobioCategory = s.parse().unwrap();
            assert_eq!(parsed, variant);
        }
    }

    // =====================================================================
    // Label / Edge constants tests
    // =====================================================================

    #[test]
    fn test_all_labels_present() {
        assert!(labels::ALL.contains(&labels::EPISODIC));
        assert!(labels::ALL.contains(&labels::KNOWLEDGE));
        assert!(labels::ALL.contains(&labels::PROCEDURAL));
        assert!(labels::ALL.contains(&labels::AUTOBIOGRAPHICAL));
        assert!(labels::ALL.contains(&labels::SYSTEM_CONFIG));
        assert!(labels::ALL.contains(&labels::TOOL_INVOCATION));
        assert!(labels::ALL.contains(&labels::SESSION));
        assert_eq!(labels::ALL.len(), 7);
    }

    #[test]
    fn test_all_edge_types_present() {
        assert!(edge_types::ALL.contains(&edge_types::HAS_MEMORY));
        assert!(edge_types::ALL.contains(&edge_types::REFERENCES));
        assert!(edge_types::ALL.contains(&edge_types::SELF_REFERENCES));
        assert!(edge_types::ALL.contains(&edge_types::PRODUCED));
        assert!(edge_types::ALL.contains(&edge_types::DERIVED_FROM));
        assert_eq!(edge_types::ALL.len(), 7);
    }

    // =====================================================================
    // Error handling tests
    // =====================================================================

    #[test]
    fn test_from_properties_missing_field() {
        let props: Vec<(String, Value)> = vec![
            ("session_id".to_string(), Value::from("s1")),
            // missing turn_index, role, content, ...
        ];
        let result = Episode::from_properties(NodeId::new(99), &props);
        assert!(result.is_err());
    }

    #[test]
    fn test_enum_parse_invalid() {
        assert!("Unknown".parse::<KnowledgeSubType>().is_err());
        assert!("Unknown".parse::<NodeStatus>().is_err());
        assert!("Unknown".parse::<AutobioCategory>().is_err());
    }
}
