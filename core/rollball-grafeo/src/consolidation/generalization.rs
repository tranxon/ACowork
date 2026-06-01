//! Experience generalization — pattern extraction from repeated episodes.
//!
//! Phase 3 S4.4: Identifies repeated behavior patterns across multiple
//! Episodes and abstracts them into ProceduralNodes. Also supports
//! LLM-driven pattern discovery for more complex generalizations.
//!
//! Key capabilities:
//! - Rule-based pattern detection from (action, tool_calls) episode tuples
//! - LLM-driven pattern discovery for complex cross-episode generalization
//! - Episode scanning from GrafeoStore for unconsolidated episodes
//! - Pattern deduplication against existing ProceduralNodes
//! - Confidence boosting when patterns reinforce existing nodes
//! - Integration with the offline consolidation pipeline
//!
//! Design: `docs/05-memory.md` §4.2 (step ④), §4.3

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use grafeo_common::types::Value;
use serde::{Deserialize, Serialize};

use crate::consolidation::triple_extraction::{LlmMessage, TripleExtractorLlm};
use crate::error::{GrafeoError, Result};
use crate::grafeo::GrafeoStore;
use crate::types::{labels, NodeStatus, ProceduralNode};

// ---------------------------------------------------------------------------
// Generalization types
// ---------------------------------------------------------------------------

/// Category of a behavior pattern — used for grouping and dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PatternCategory {
    /// Tool usage pattern (e.g., "use http_request for weather lookups").
    ToolUsage,
    /// User preference pattern (e.g., "user prefers concise output").
    UserPreference,
    /// Workflow pattern (e.g., "when asked for a report, first gather data, then format").
    Workflow,
    /// Error recovery pattern (e.g., "on API timeout, retry once").
    ErrorRecovery,
}

impl PatternCategory {
    /// Returns the string representation used in ProceduralNode metadata.
    pub fn as_str(&self) -> &'static str {
        match self {
            PatternCategory::ToolUsage => "ToolUsage",
            PatternCategory::UserPreference => "UserPreference",
            PatternCategory::Workflow => "Workflow",
            PatternCategory::ErrorRecovery => "ErrorRecovery",
        }
    }
}

impl std::str::FromStr for PatternCategory {
    type Err = GrafeoError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "ToolUsage" => Ok(PatternCategory::ToolUsage),
            "UserPreference" => Ok(PatternCategory::UserPreference),
            "Workflow" => Ok(PatternCategory::Workflow),
            "ErrorRecovery" => Ok(PatternCategory::ErrorRecovery),
            _ => Err(GrafeoError::Memory(format!(
                "unknown PatternCategory: {s}"
            ))),
        }
    }
}

/// A detected behavior pattern from episodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorPattern {
    /// Human-readable name for the pattern.
    pub name: String,
    /// Trigger condition description.
    pub trigger_condition: String,
    /// Action pattern description.
    pub action_pattern: String,
    /// Number of episodes this pattern was observed in.
    pub observation_count: usize,
    /// Confidence in the pattern [0.0, 1.0].
    pub confidence: f32,
    /// Pattern category (for grouping and dedup).
    #[serde(default = "default_pattern_category")]
    pub category: PatternCategory,
}

fn default_pattern_category() -> PatternCategory {
    PatternCategory::ToolUsage
}

/// Result of the generalization process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizationResult {
    /// Detected patterns.
    pub patterns: Vec<BehaviorPattern>,
    /// Number of new ProceduralNodes created.
    pub nodes_created: usize,
    /// Number of existing ProceduralNodes boosted (confidence incremented).
    pub nodes_boosted: usize,
    /// Number of patterns deduplicated against existing nodes.
    pub patterns_deduplicated: usize,
    /// Timestamp of the generalization.
    pub generalized_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Generalization configuration
// ---------------------------------------------------------------------------

/// Configuration for the experience generalization process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizationConfig {
    /// Minimum number of observations before a pattern is considered valid.
    /// Default: 3.
    pub min_observations: usize,
    /// Maximum number of unconsolidated episodes to scan per run.
    /// Default: 100.
    pub max_episodes_scan: usize,
    /// Confidence boost applied when a pattern reinforces an existing node.
    /// Default: 0.05.
    pub confidence_boost: f32,
    /// Maximum confidence for a ProceduralNode (cap after boosting).
    /// Default: 0.98.
    pub max_confidence: f32,
    /// Whether to use LLM for pattern discovery when available.
    /// Default: true.
    pub use_llm: bool,
}

impl Default for GeneralizationConfig {
    fn default() -> Self {
        Self {
            min_observations: 3,
            max_episodes_scan: 100,
            confidence_boost: 0.05,
            max_confidence: 0.98,
            use_llm: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Simple pattern detection (rule-based, no LLM)
// ---------------------------------------------------------------------------

/// Detect patterns from a list of (action, tool_calls) pairs.
///
/// Simple heuristic: if the same action+tool combination appears
/// \>= `min_observations` times, it's a pattern. Patterns are
/// automatically categorized by their content.
pub fn detect_simple_patterns(
    episodes: &[(String, String, String)], // (episode_id, action, tool_calls)
    min_observations: usize,
) -> Vec<BehaviorPattern> {
    let mut action_counts: HashMap<String, (usize, Vec<String>)> = HashMap::new();

    for (_ep_id, action, tool_calls) in episodes {
        let key = format!("{}|{}", action, tool_calls);
        let entry = action_counts.entry(key.clone()).or_insert((0, Vec::new()));
        entry.0 += 1;
        if !entry.1.contains(&action.to_string()) {
            entry.1.push(action.to_string());
        }
    }

    let mut patterns = Vec::new();
    for (key, (count, _actions)) in action_counts {
        if count >= min_observations {
            let parts: Vec<&str> = key.splitn(2, '|').collect();
            let action = parts.first().unwrap_or(&"unknown");
            let tools = parts.get(1).unwrap_or(&"");

            let confidence = (0.5 + 0.1 * (count as f32).min(5.0)).min(0.95);
            let category = categorize_pattern(action, tools);

            patterns.push(BehaviorPattern {
                name: format!("pattern_{}", action),
                trigger_condition: format!("When user asks about {}", action),
                action_pattern: format!("Use {} to fulfill request", tools),
                observation_count: count,
                confidence,
                category,
            });
        }
    }

    // Sort by observation count descending
    patterns.sort_by_key(|p| std::cmp::Reverse(p.observation_count));
    patterns
}

/// Categorize a pattern based on its action and tool_calls content.
fn categorize_pattern(action: &str, tool_calls: &str) -> PatternCategory {
    let action_lower = action.to_lowercase();
    let tools_lower = tool_calls.to_lowercase();

    // Error recovery: keywords suggest retries, fallbacks, error handling
    if action_lower.contains("retry")
        || action_lower.contains("fallback")
        || action_lower.contains("error")
        || action_lower.contains("timeout")
        || tools_lower.contains("retry")
    {
        return PatternCategory::ErrorRecovery;
    }

    // User preference: keywords suggest user's personal style
    if action_lower.contains("prefer")
        || action_lower.contains("style")
        || action_lower.contains("concise")
        || action_lower.contains("short")
        || action_lower.contains("detailed")
        || action_lower.contains("format")
    {
        return PatternCategory::UserPreference;
    }

    // Workflow: multi-step actions or sequential tool calls
    if action_lower.contains("report")
        || action_lower.contains("workflow")
        || action_lower.contains("pipeline")
        || tools_lower.contains(',')
        || tools_lower.contains("then")
    {
        return PatternCategory::Workflow;
    }

    // Default: tool usage
    PatternCategory::ToolUsage
}

// ---------------------------------------------------------------------------
// LLM-driven pattern discovery
// ---------------------------------------------------------------------------

const GENERALIZATION_PROMPT: &str = r#"You are a behavior pattern discovery assistant. Given a list of observed actions and their tool calls, identify recurring patterns.

Rules:
1. Look for actions that share similar trigger conditions.
2. Identify common tool call sequences.
3. Abstract into general patterns.
4. Assign a confidence score (0.0-1.0) based on how consistently the pattern appears.
5. Classify each pattern into one of these categories:
   - "ToolUsage": Simple tool usage pattern (e.g., "use http_request for weather")
   - "UserPreference": User preference pattern (e.g., "user prefers concise output")
   - "Workflow": Multi-step workflow pattern (e.g., "gather data then format report")
   - "ErrorRecovery": Error handling or retry pattern (e.g., "on timeout, retry once")

Output format (JSON array):
[
  {
    "name": "weather_lookup_pattern",
    "trigger_condition": "When user asks about weather",
    "action_pattern": "Use http_request to fetch weather data",
    "observation_count": 3,
    "confidence": 0.9,
    "category": "ToolUsage"
  }
]

If no patterns can be identified, return an empty array: []"#;

/// Discover patterns using an LLM.
pub async fn discover_patterns_llm(
    episodes: &[(String, String, String)],
    llm: &dyn TripleExtractorLlm,
) -> Result<Vec<BehaviorPattern>> {
    if episodes.is_empty() {
        return Ok(Vec::new());
    }

    let episode_list: String = episodes
        .iter()
        .enumerate()
        .map(|(i, (_id, action, tools))| format!("{}. Action: {}, Tools: {}", i + 1, action, tools))
        .collect::<Vec<_>>()
        .join("\n");

    let messages = vec![
        LlmMessage {
            role: "system".to_string(),
            content: GENERALIZATION_PROMPT.to_string(),
        },
        LlmMessage {
            role: "user".to_string(),
            content: episode_list,
        },
    ];

    let response = llm.chat(messages).await.map_err(|e| {
        GrafeoError::Memory(format!("LLM call failed during pattern discovery: {}", e))
    })?;

    parse_patterns(&response.content)
}

// ---------------------------------------------------------------------------
// Name similarity (for deduplication)
// ---------------------------------------------------------------------------

/// Compute a simple trigram-based similarity between two strings.
/// Returns a value in [0.0, 1.0] where 1.0 means identical.
fn name_similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    if a_lower == b_lower {
        return 0.99;
    }

    // Trigram similarity
    let trigrams_a = trigrams(&a_lower);
    let trigrams_b = trigrams(&b_lower);

    if trigrams_a.is_empty() || trigrams_b.is_empty() {
        return 0.0;
    }

    let common = trigrams_a.intersection(&trigrams_b).count() as f32;
    let total = trigrams_a.union(&trigrams_b).count() as f32;

    if total == 0.0 {
        0.0
    } else {
        common / total
    }
}

/// Extract character trigrams from a string.
fn trigrams(s: &str) -> std::collections::HashSet<String> {
    let chars: Vec<char> = format!("  {s}  ").chars().collect();
    let mut set = std::collections::HashSet::new();
    for i in 0..chars.len().saturating_sub(2) {
        let tri: String = chars[i..i + 3].iter().collect();
        set.insert(tri);
    }
    set
}

// ---------------------------------------------------------------------------
// GrafeoStore: Episode scanning for patterns
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Scan unconsolidated episodes from the store and extract
    /// (episode_id, action_summary, tool_calls) tuples suitable for
    /// pattern detection.
    ///
    /// Only episodes with `consolidated == false` and `role == "assistant"`
    /// are included, since assistant turns contain the action/tool info.
    pub fn scan_episodes_for_pattern_extraction(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut episodes = Vec::new();

        for id in node_ids {
            if episodes.len() >= limit {
                break;
            }

            if let Some(n) = self.db.get_node(id) {
                // Only unconsolidated assistant turns
                let is_consolidated = n
                    .get_property("consolidated")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let role = n
                    .get_property("role")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                if is_consolidated || role != "assistant" {
                    continue;
                }

                let content = n
                    .get_property("content")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                let ep_id = id.0.to_string();

                // Extract a simplified action summary from content.
                // Tool calls in assistant messages often appear as JSON-like
                // structures. We extract the tool names as the "tools" and
                // use the first line of content as the "action".
                let (action, tools) = extract_action_and_tools(content);

                episodes.push((ep_id, action, tools));
            }
        }

        Ok(episodes)
    }

    /// Get all ProceduralNodes (for dedup checking).
    pub fn get_all_procedural_nodes(&self) -> Result<Vec<ProceduralNode>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::PROCEDURAL);

        let mut nodes = Vec::new();
        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let props: Vec<(String, Value)> = n
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();

                if let Ok(pn) = ProceduralNode::from_properties(id, &props) {
                    nodes.push(pn);
                }
            }
        }
        Ok(nodes)
    }
}

/// Extract an action summary and tool call string from assistant content.
///
/// The action is the first meaningful line of the content (up to 100 chars).
/// The tools are extracted from any JSON-like tool_call structures found,
/// or an empty string if none are found.
fn extract_action_and_tools(content: &str) -> (String, String) {
    // Find tool calls — look for patterns like "tool_name" or "name": "xxx"
    let mut tool_names = Vec::new();

    // Simple extraction: find "name": "xxx" patterns in the content
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("\"name\"") && trimmed.contains(":") {
            // Try to extract the value after "name":
            if let Some(idx) = trimmed.find("\"name\"") {
                let rest = &trimmed[idx + 6..];
                let rest = rest.trim_start_matches([':', ' ', '"']);
                if let Some(end) = rest.find('"') {
                    let name = &rest[..end];
                    if !name.is_empty() && !tool_names.contains(&name.to_string()) {
                        tool_names.push(name.to_string());
                    }
                }
            }
        }
    }

    let action = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(100)
        .collect::<String>();

    let tools = tool_names.join(", ");

    (action, tools)
}

// ---------------------------------------------------------------------------
// GrafeoStore: Generalize patterns with dedup and boosting
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Generalize patterns from episodes and store them as ProceduralNodes.
    ///
    /// Uses simple rule-based detection by default. Optionally uses LLM
    /// for more complex pattern discovery.
    ///
    /// New in S4.4:
    /// - Pattern deduplication against existing ProceduralNodes
    /// - Confidence boosting for existing nodes when patterns match
    /// - GeneralizationConfig for fine-grained control
    pub async fn generalize_patterns(
        &self,
        episodes: &[(String, String, String)],
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: &dyn Fn(&str) -> Vec<f32>,
        min_observations: usize,
    ) -> Result<GeneralizationResult> {
        let config = GeneralizationConfig {
            min_observations,
            ..GeneralizationConfig::default()
        };
        self.generalize_patterns_with_config(episodes, llm, embedding_fn, &config)
            .await
    }

    /// Generalize patterns with full configuration support.
    pub async fn generalize_patterns_with_config(
        &self,
        episodes: &[(String, String, String)],
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: &dyn Fn(&str) -> Vec<f32>,
        config: &GeneralizationConfig,
    ) -> Result<GeneralizationResult> {
        let patterns = if let Some(llm) = llm {
            if config.use_llm {
                discover_patterns_llm(episodes, llm).await?
            } else {
                detect_simple_patterns(episodes, config.min_observations)
            }
        } else {
            detect_simple_patterns(episodes, config.min_observations)
        };

        // Get existing ProceduralNodes for dedup
        let existing = self.get_all_procedural_nodes()?;

        let mut nodes_created = 0;
        let mut nodes_boosted = 0;
        let mut patterns_deduplicated = 0;

        for pattern in &patterns {
            // Check if a similar ProceduralNode already exists
            let similar = find_similar_procedural(&pattern.name, &existing);

            match similar {
                Some((idx, _similarity)) => {
                    // Boost the existing node's confidence and success_count
                    let existing_node = &existing[idx];
                    let new_confidence = (existing_node.confidence + config.confidence_boost)
                        .min(config.max_confidence);
                    let new_success = existing_node.success_count
                        + pattern.observation_count as u32;

                    let mut updated = existing_node.clone();
                    updated.confidence = new_confidence;
                    updated.success_count = new_success;
                    updated.updated_at = Utc::now();

                    // Upgrade Pending → Active if confidence reaches threshold
                    if updated.status == NodeStatus::Pending && updated.confidence >= 0.8 {
                        updated.status = NodeStatus::Active;
                    }

                    self.update_procedural(&updated)?;
                    nodes_boosted += 1;
                    patterns_deduplicated += 1;
                }
                None => {
                    // Create a new ProceduralNode
                    let embedding = embedding_fn(&format!(
                        "{} {}",
                        pattern.trigger_condition, pattern.action_pattern
                    ));

                    let mut metadata = HashMap::new();
                    metadata.insert(
                        "category".to_string(),
                        serde_json::Value::String(pattern.category.as_str().to_string()),
                    );

                    let node = ProceduralNode {
                        id: None,
                        name: pattern.name.clone(),
                        trigger_condition: pattern.trigger_condition.clone(),
                        action_pattern: pattern.action_pattern.clone(),
                        success_count: pattern.observation_count as u32,
                        fail_count: 0,
                        confidence: pattern.confidence,
                        embedding: Some(embedding),
                        status: if pattern.confidence >= 0.8 {
                            NodeStatus::Active
                        } else {
                            NodeStatus::Pending
                        },
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                        metadata,
                    };

                    self.store_procedural(&node)?;
                    nodes_created += 1;
                }
            }
        }

        Ok(GeneralizationResult {
            patterns,
            nodes_created,
            nodes_boosted,
            patterns_deduplicated,
            generalized_at: Utc::now(),
        })
    }

    /// Run generalization from unconsolidated episodes in the store.
    ///
    /// This is the main entry point for the offline consolidation pipeline
    /// (step ④ in the design doc). It:
    /// 1. Scans unconsolidated episodes
    /// 2. Extracts action/tool patterns
    /// 3. Detects recurring patterns (rule-based or LLM)
    /// 4. Stores new ProceduralNodes or boosts existing ones
    pub async fn run_generalization(
        &self,
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: &dyn Fn(&str) -> Vec<f32>,
        config: &GeneralizationConfig,
    ) -> Result<GeneralizationResult> {
        let episodes = self.scan_episodes_for_pattern_extraction(config.max_episodes_scan)?;

        if episodes.is_empty() {
            return Ok(GeneralizationResult {
                patterns: Vec::new(),
                nodes_created: 0,
                nodes_boosted: 0,
                patterns_deduplicated: 0,
                generalized_at: Utc::now(),
            });
        }

        self.generalize_patterns_with_config(&episodes, llm, embedding_fn, config)
            .await
    }
}

/// Find an existing ProceduralNode with a name similar to the given pattern name.
///
/// Returns the index in the `existing` slice and the similarity score,
/// or `None` if no match exceeds the threshold.
fn find_similar_procedural(
    pattern_name: &str,
    existing: &[ProceduralNode],
) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;

    for (idx, node) in existing.iter().enumerate() {
        let sim = name_similarity(pattern_name, &node.name);
        match best {
            Some((_best_idx, best_sim)) if sim <= best_sim => {}
            _ => best = Some((idx, sim)),
        }
    }

    // Only consider it a match if similarity is high enough
    // Use a threshold of 0.6 for procedural node name matching
    best.and_then(|(idx, sim)| {
        if sim >= 0.6 {
            Some((idx, sim))
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_patterns(content: &str) -> Result<Vec<BehaviorPattern>> {
    let json_str = extract_json_array(content);

    if json_str.trim() == "[]" {
        return Ok(Vec::new());
    }

    let raw: Vec<RawPattern> = serde_json::from_str(&json_str).map_err(|e| {
        GrafeoError::Memory(format!("Failed to parse pattern discovery response: {}", e))
    })?;

    Ok(raw
        .into_iter()
        .map(|r| BehaviorPattern {
            name: r.name,
            trigger_condition: r.trigger_condition,
            action_pattern: r.action_pattern,
            observation_count: r.observation_count,
            confidence: r.confidence.clamp(0.0, 1.0),
            category: r
                .category
                .and_then(|c| c.parse().ok())
                .unwrap_or(PatternCategory::ToolUsage),
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct RawPattern {
    name: String,
    trigger_condition: String,
    action_pattern: String,
    observation_count: usize,
    confidence: f32,
    /// Category is optional for backward compatibility with older LLM responses.
    #[serde(default)]
    category: Option<String>,
}

fn extract_json_array(content: &str) -> String {
    let trimmed = content.trim();
    if let Some(start) = trimmed.find("```json") {
        let json_start = start + 7;
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim().to_string();
        }
    }
    if let Some(start) = trimmed.find("```") {
        let json_start = start + 3;
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim().to_string();
        }
    }
    trimmed.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_EMBEDDING_DIM;

    // =====================================================================
    // Test: Simple pattern detection
    // =====================================================================

    #[test]
    fn test_detect_simple_patterns_basic() {
        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep3".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let patterns = detect_simple_patterns(&episodes, 3);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].observation_count, 3);
        assert!(patterns[0].confidence >= 0.8);
    }

    // =====================================================================
    // Test: Below threshold — no patterns
    // =====================================================================

    #[test]
    fn test_detect_simple_patterns_below_threshold() {
        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let patterns = detect_simple_patterns(&episodes, 3);
        assert!(patterns.is_empty());
    }

    // =====================================================================
    // Test: Multiple different patterns
    // =====================================================================

    #[test]
    fn test_detect_multiple_patterns() {
        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep3".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep4".to_string(), "calendar".to_string(), "intent_send".to_string()),
            ("ep5".to_string(), "calendar".to_string(), "intent_send".to_string()),
            ("ep6".to_string(), "calendar".to_string(), "intent_send".to_string()),
        ];

        let patterns = detect_simple_patterns(&episodes, 3);
        assert_eq!(patterns.len(), 2);
    }

    // =====================================================================
    // Test: Pattern categorization
    // =====================================================================

    #[test]
    fn test_categorize_pattern_tool_usage() {
        assert_eq!(
            categorize_pattern("weather", "http_request"),
            PatternCategory::ToolUsage
        );
    }

    #[test]
    fn test_categorize_pattern_error_recovery() {
        assert_eq!(
            categorize_pattern("retry_on_error", "http_request"),
            PatternCategory::ErrorRecovery
        );
    }

    #[test]
    fn test_categorize_pattern_user_preference() {
        assert_eq!(
            categorize_pattern("concise_format", "text_formatter"),
            PatternCategory::UserPreference
        );
    }

    #[test]
    fn test_categorize_pattern_workflow() {
        assert_eq!(
            categorize_pattern("report", "gather_data,format_output"),
            PatternCategory::Workflow
        );
    }

    // =====================================================================
    // Test: PatternCategory roundtrip
    // =====================================================================

    #[test]
    fn test_pattern_category_roundtrip() {
        for cat in [
            PatternCategory::ToolUsage,
            PatternCategory::UserPreference,
            PatternCategory::Workflow,
            PatternCategory::ErrorRecovery,
        ] {
            let s = cat.as_str();
            let parsed: PatternCategory = s.parse().unwrap();
            assert_eq!(parsed, cat);
        }
    }

    // =====================================================================
    // Test: Name similarity
    // =====================================================================

    #[test]
    fn test_name_similarity_identical() {
        assert!((name_similarity("weather_lookup", "weather_lookup") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_name_similarity_case_insensitive() {
        let sim = name_similarity("Weather_Lookup", "weather_lookup");
        assert!(sim > 0.9, "case-insensitive names should be very similar, got {sim}");
    }

    #[test]
    fn test_name_similarity_similar() {
        let sim = name_similarity("weather_lookup", "weather_lookup_pattern");
        assert!(sim > 0.5, "similar names should have moderate similarity, got {sim}");
    }

    #[test]
    fn test_name_similarity_different() {
        let sim = name_similarity("weather_lookup", "calendar_reminder");
        assert!(sim < 0.5, "very different names should have low similarity, got {sim}");
    }

    // =====================================================================
    // Test: Trigram extraction
    // =====================================================================

    #[test]
    fn test_trigrams_basic() {
        let t = trigrams("abc");
        assert!(t.contains(" ab"));
        assert!(t.contains("abc"));
        assert!(t.contains("bc "));
    }

    // =====================================================================
    // Test: extract_action_and_tools
    // =====================================================================

    #[test]
    fn test_extract_action_and_tools_plain_text() {
        let (action, tools) = extract_action_and_tools("Hello, how can I help?");
        assert_eq!(action, "Hello, how can I help?");
        assert!(tools.is_empty());
    }

    #[test]
    fn test_extract_action_and_tools_with_tool_call() {
        let content = r#"Let me check the weather.
{"name": "http_request", "arguments": {"url": "..."}}"#;
        let (action, tools) = extract_action_and_tools(content);
        assert!(action.contains("weather"));
        assert!(tools.contains("http_request"));
    }

    // =====================================================================
    // Test: GeneralizationConfig defaults
    // =====================================================================

    #[test]
    fn test_generalization_config_defaults() {
        let config = GeneralizationConfig::default();
        assert_eq!(config.min_observations, 3);
        assert_eq!(config.max_episodes_scan, 100);
        assert!((config.confidence_boost - 0.05).abs() < f32::EPSILON);
        assert!((config.max_confidence - 0.98).abs() < f32::EPSILON);
        assert!(config.use_llm);
    }

    // =====================================================================
    // Test: LLM pattern discovery with mock
    // =====================================================================

    use crate::consolidation::triple_extraction::LlmResponse;

    struct MockPatternLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl TripleExtractorLlm for MockPatternLlm {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String> {
            Ok(LlmResponse {
                content: self.response.clone(),
                usage_tokens: Some(200),
            })
        }
    }

    fn test_embedding_fn(_text: &str) -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    #[tokio::test]
    async fn test_discover_patterns_llm() {
        let llm = MockPatternLlm {
            response: r#"[{"name":"weather_lookup","trigger_condition":"User asks about weather","action_pattern":"Use http_request to fetch weather data","observation_count":5,"confidence":0.9,"category":"ToolUsage"}]"#.to_string(),
        };

        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let patterns = discover_patterns_llm(&episodes, &llm).await.unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].name, "weather_lookup");
        assert_eq!(patterns[0].category, PatternCategory::ToolUsage);
    }

    // =====================================================================
    // Test: LLM pattern discovery with category
    // =====================================================================

    #[tokio::test]
    async fn test_discover_patterns_llm_with_categories() {
        let llm = MockPatternLlm {
            response: r#"[{"name":"concise_output","trigger_condition":"User asks for summary","action_pattern":"Reply concisely","observation_count":4,"confidence":0.85,"category":"UserPreference"},{"name":"retry_on_timeout","trigger_condition":"API call times out","action_pattern":"Retry once","observation_count":3,"confidence":0.8,"category":"ErrorRecovery"}]"#.to_string(),
        };

        let episodes = vec![
            ("ep1".to_string(), "summary".to_string(), "text_gen".to_string()),
        ];

        let patterns = discover_patterns_llm(&episodes, &llm).await.unwrap();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].category, PatternCategory::UserPreference);
        assert_eq!(patterns[1].category, PatternCategory::ErrorRecovery);
    }

    // =====================================================================
    // Test: LLM response without category (backward compat)
    // =====================================================================

    #[tokio::test]
    async fn test_discover_patterns_llm_no_category() {
        let llm = MockPatternLlm {
            response: r#"[{"name":"weather_lookup","trigger_condition":"User asks about weather","action_pattern":"Use http_request","observation_count":3,"confidence":0.9}]"#.to_string(),
        };

        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let patterns = discover_patterns_llm(&episodes, &llm).await.unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].category, PatternCategory::ToolUsage); // default
    }

    // =====================================================================
    // Test: Full generalization with GrafeoStore (new nodes)
    // =====================================================================

    #[tokio::test]
    async fn test_generalize_patterns_store() {
        let store = GrafeoStore::new_in_memory().unwrap();

        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep3".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let result = store
            .generalize_patterns(&episodes, None, &test_embedding_fn, 3)
            .await
            .unwrap();

        assert_eq!(result.patterns.len(), 1);
        assert_eq!(result.nodes_created, 1);
        assert_eq!(result.nodes_boosted, 0);
        assert_eq!(result.patterns_deduplicated, 0);
    }

    // =====================================================================
    // Test: Empty episodes → no patterns
    // =====================================================================

    #[tokio::test]
    async fn test_generalize_patterns_empty() {
        let store = GrafeoStore::new_in_memory().unwrap();

        let result = store
            .generalize_patterns(&[], None, &test_embedding_fn, 3)
            .await
            .unwrap();

        assert!(result.patterns.is_empty());
        assert_eq!(result.nodes_created, 0);
    }

    // =====================================================================
    // Test: Deduplication — existing node gets boosted
    // =====================================================================

    #[tokio::test]
    async fn test_generalize_patterns_dedup_boost() {
        let store = GrafeoStore::new_in_memory().unwrap();

        // Pre-create a ProceduralNode with a similar name
        let existing = ProceduralNode {
            id: None,
            name: "pattern_weather".to_string(),
            trigger_condition: "When user asks about weather".to_string(),
            action_pattern: "Use http_request to fulfill request".to_string(),
            success_count: 3,
            fail_count: 0,
            confidence: 0.8,
            embedding: Some(test_embedding_fn("weather")),
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        };
        store.store_procedural(&existing).unwrap();

        // Run generalization with the same pattern
        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep3".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let result = store
            .generalize_patterns(&episodes, None, &test_embedding_fn, 3)
            .await
            .unwrap();

        // Should boost the existing node, not create a new one
        assert_eq!(result.nodes_created, 0, "should not create a new node");
        assert_eq!(result.nodes_boosted, 1, "should boost the existing node");
        assert_eq!(result.patterns_deduplicated, 1);

        // Verify the existing node was updated
        let all_procedural = store.get_all_procedural_nodes().unwrap();
        assert_eq!(all_procedural.len(), 1, "should still have only one node");
        assert!(all_procedural[0].confidence > 0.8, "confidence should be boosted");
        assert_eq!(all_procedural[0].success_count, 6, "3 + 3 = 6");
    }

    // =====================================================================
    // Test: Pending node upgraded to Active on boost
    // =====================================================================

    #[tokio::test]
    async fn test_generalize_patterns_pending_to_active() {
        let store = GrafeoStore::new_in_memory().unwrap();

        // Pre-create a Pending ProceduralNode
        let existing = ProceduralNode {
            id: None,
            name: "pattern_weather".to_string(),
            trigger_condition: "When user asks about weather".to_string(),
            action_pattern: "Use http_request to fulfill request".to_string(),
            success_count: 3,
            fail_count: 0,
            confidence: 0.75,
            embedding: Some(test_embedding_fn("weather")),
            status: NodeStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        };
        store.store_procedural(&existing).unwrap();

        let episodes = vec![
            ("ep1".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "weather".to_string(), "http_request".to_string()),
            ("ep3".to_string(), "weather".to_string(), "http_request".to_string()),
        ];

        let result = store
            .generalize_patterns(&episodes, None, &test_embedding_fn, 3)
            .await
            .unwrap();

        assert_eq!(result.nodes_boosted, 1);

        // Verify the node was upgraded to Active
        let all_procedural = store.get_all_procedural_nodes().unwrap();
        assert_eq!(all_procedural[0].status, NodeStatus::Active);
    }

    // =====================================================================
    // Test: ProceduralNode metadata includes category
    // =====================================================================

    #[tokio::test]
    async fn test_generalize_patterns_category_metadata() {
        let store = GrafeoStore::new_in_memory().unwrap();

        let episodes = vec![
            ("ep1".to_string(), "retry_on_error".to_string(), "http_request".to_string()),
            ("ep2".to_string(), "retry_on_error".to_string(), "http_request".to_string()),
            ("ep3".to_string(), "retry_on_error".to_string(), "http_request".to_string()),
        ];

        let result = store
            .generalize_patterns(&episodes, None, &test_embedding_fn, 3)
            .await
            .unwrap();

        assert_eq!(result.nodes_created, 1);

        let all_procedural = store.get_all_procedural_nodes().unwrap();
        assert_eq!(all_procedural.len(), 1);
        let category = all_procedural[0].metadata.get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(category, "ErrorRecovery");
    }

    // =====================================================================
    // Test: BehaviorPattern serialization (with category)
    // =====================================================================

    #[test]
    fn test_behavior_pattern_serde() {
        let pattern = BehaviorPattern {
            name: "test".to_string(),
            trigger_condition: "when X".to_string(),
            action_pattern: "do Y".to_string(),
            observation_count: 3,
            confidence: 0.9,
            category: PatternCategory::UserPreference,
        };
        let json = serde_json::to_string(&pattern).unwrap();
        let decoded: BehaviorPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(pattern.name, decoded.name);
        assert_eq!(pattern.observation_count, decoded.observation_count);
        assert_eq!(pattern.category, decoded.category);
    }

    // =====================================================================
    // Test: GeneralizationResult serialization
    // =====================================================================

    #[test]
    fn test_generalization_result_serde() {
        let result = GeneralizationResult {
            patterns: vec![BehaviorPattern {
                name: "test".to_string(),
                trigger_condition: "when X".to_string(),
                action_pattern: "do Y".to_string(),
                observation_count: 3,
                confidence: 0.9,
                category: PatternCategory::ToolUsage,
            }],
            nodes_created: 1,
            nodes_boosted: 0,
            patterns_deduplicated: 0,
            generalized_at: Utc::now(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: GeneralizationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.nodes_created, decoded.nodes_created);
        assert_eq!(result.nodes_boosted, decoded.nodes_boosted);
    }

    // =====================================================================
    // Test: find_similar_procedural
    // =====================================================================

    #[test]
    fn test_find_similar_procedural_exact_match() {
        let existing = vec![ProceduralNode {
            id: None,
            name: "pattern_weather".to_string(),
            trigger_condition: "when weather".to_string(),
            action_pattern: "use http_request".to_string(),
            success_count: 5,
            fail_count: 0,
            confidence: 0.8,
            embedding: None,
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        }];

        let result = find_similar_procedural("pattern_weather", &existing);
        assert!(result.is_some());
        let (idx, sim) = result.unwrap();
        assert_eq!(idx, 0);
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_find_similar_procedural_no_match() {
        let existing = vec![ProceduralNode {
            id: None,
            name: "pattern_calendar".to_string(),
            trigger_condition: "when calendar".to_string(),
            action_pattern: "use intent_send".to_string(),
            success_count: 5,
            fail_count: 0,
            confidence: 0.8,
            embedding: None,
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
        }];

        let result = find_similar_procedural("pattern_weather", &existing);
        assert!(result.is_none(), "very different names should not match");
    }

    // =====================================================================
    // Test: run_generalization from store episodes
    // =====================================================================

    #[tokio::test]
    async fn test_run_generalization_no_episodes() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let config = GeneralizationConfig::default();

        let result = store
            .run_generalization(None, &test_embedding_fn, &config)
            .await
            .unwrap();

        assert_eq!(result.nodes_created, 0);
        assert_eq!(result.patterns.len(), 0);
    }

    // =====================================================================
    // Test: generalize_patterns_with_config
    // =====================================================================

    #[tokio::test]
    async fn test_generalize_with_config_no_llm() {
        let store = GrafeoStore::new_in_memory().unwrap();
        let config = GeneralizationConfig {
            min_observations: 2,
            use_llm: false,
            ..GeneralizationConfig::default()
        };

        let episodes = vec![
            ("ep1".to_string(), "translate".to_string(), "llm_call".to_string()),
            ("ep2".to_string(), "translate".to_string(), "llm_call".to_string()),
        ];

        let result = store
            .generalize_patterns_with_config(&episodes, None, &test_embedding_fn, &config)
            .await
            .unwrap();

        assert_eq!(result.nodes_created, 1);
        assert_eq!(result.patterns.len(), 1);
    }
}
