//! Episodic (experiential) memory layer.
//!
//! Handles write, search, consolidation, and cleanup of Episode nodes
//! stored in Grafeo with the `Episodic` label.

pub mod consolidate;
pub mod search;
pub mod store;

use grafeo_common::types::Value;

use crate::error::{GrafeoError, Result};
use crate::types::Episode;

/// Convert a Grafeo node query result (`Value::Map`) into an [`Episode`].
///
/// The map is expected to contain `_id` (int), `_labels` (list), and all
/// episode properties at the top level, as produced by `RETURN n` in GQL.
fn value_to_episode(value: &Value) -> Result<Episode> {
    match value {
        Value::Map(map) => {
            let id = map
                .get(&grafeo_common::types::PropertyKey::new("_id"))
                .and_then(|v| v.as_int64())
                .map(|id| grafeo_common::types::NodeId::new(id as u64))
                .ok_or_else(|| GrafeoError::Memory("missing _id in node map".to_string()))?;

            let props: Vec<(String, Value)> = map
                .iter()
                .filter(|(k, _)| k.as_str() != "_id" && k.as_str() != "_labels")
                .map(|(k, v)| (k.as_str().to_string(), v.clone()))
                .collect();

            Episode::from_properties(id, &props)
        }
        _ => Err(GrafeoError::Memory(
            "expected Value::Map for node result".to_string(),
        )),
    }
}

/// Escape a string for safe use in GQL literal expressions.
///
/// Escapes backslashes, single quotes, double quotes, newlines, tabs,
/// and other control characters to prevent GQL injection attacks.
#[allow(dead_code)]
fn escape_gql_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
