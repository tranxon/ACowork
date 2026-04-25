//! LLM-driven conflict classification and evidence verification.
//!
//! Phase 3 S4.3: When the heuristic conflict detector returns `Ambiguous`
//! or `DeferToLLM`, this module uses an LLM to classify the conflict as
//! Evolution, Correction, or Ambiguous, with evidence verification.
//!
//! Design: `docs/05-memory.md` §4.3

use serde::{Deserialize, Serialize};

use crate::consolidation::triple_extraction::{TripleExtractorLlm, LlmMessage};
use crate::error::{GrafeoError, Result};

// ---------------------------------------------------------------------------
// Conflict classification
// ---------------------------------------------------------------------------

/// LLM-classified conflict type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LlmConflictType {
    /// Knowledge evolved — the old value is outdated but was once correct.
    /// Action: replace old with new, mark old as Dormant.
    Evolution,
    /// Knowledge was wrong — the old value was incorrect from the start.
    /// Action: replace old with new, mark old as Dormant.
    Correction,
    /// Ambiguous — both values could be true simultaneously.
    /// Action: keep both, mark for user confirmation.
    Ambiguous,
}

/// Result of LLM conflict classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictClassification {
    /// The classified conflict type.
    pub conflict_type: LlmConflictType,
    /// LLM's reasoning for the classification.
    pub reasoning: String,
    /// Confidence in the classification [0.0, 1.0].
    pub confidence: f32,
    /// Suggested action.
    pub suggested_action: String,
    /// Whether evidence verification passed.
    pub evidence_verified: bool,
}

// ---------------------------------------------------------------------------
// Prompt template
// ---------------------------------------------------------------------------

const CONFLICT_CLASSIFICATION_PROMPT: &str = r#"You are a knowledge conflict resolver. Given two conflicting memory entries, classify the conflict and suggest an action.

Rules:
1. **Evolution**: The old value was correct at the time but is now outdated (e.g., "user lives in Beijing" → "user lives in Shanghai" because they moved).
2. **Correction**: The old value was wrong from the start (e.g., "user birthday is March" → "user birthday is May" because they corrected it).
3. **Ambiguous**: Both values could be true simultaneously (e.g., "user likes Chinese food" vs "user likes Western food" — they could like both).

Consider the evidence context provided. Return valid JSON only.

Output format:
{
  "conflict_type": "evolution" | "correction" | "ambiguous",
  "reasoning": "One sentence explanation",
  "confidence": 0.9,
  "suggested_action": "replace" | "keep_both" | "ask_user",
  "evidence_verified": true
}"#;

// ---------------------------------------------------------------------------
// Classification function
// ---------------------------------------------------------------------------

/// Classify a conflict between two knowledge entries using an LLM.
///
/// # Arguments
/// * `old_subject`, `old_predicate`, `old_object` — The existing knowledge triple.
/// * `new_subject`, `new_predicate`, `new_object` — The incoming knowledge triple.
/// * `evidence_context` — Optional surrounding context from source episodes.
/// * `llm` — The LLM abstraction for making the call.
pub async fn classify_conflict(
    old_subject: &str,
    old_predicate: &str,
    old_object: &str,
    new_subject: &str,
    new_predicate: &str,
    new_object: &str,
    evidence_context: Option<&str>,
    llm: &dyn TripleExtractorLlm,
) -> Result<ConflictClassification> {
    let evidence = evidence_context.unwrap_or("(no context available)");

    let user_message = format!(
        "Old knowledge: ({}, {}, {})\n\
         New knowledge: ({}, {}, {})\n\
         Evidence context: {}",
        old_subject, old_predicate, old_object,
        new_subject, new_predicate, new_object,
        evidence,
    );

    let messages = vec![
        LlmMessage {
            role: "system".to_string(),
            content: CONFLICT_CLASSIFICATION_PROMPT.to_string(),
        },
        LlmMessage {
            role: "user".to_string(),
            content: user_message,
        },
    ];

    let response = llm.chat(messages).await.map_err(|e| {
        GrafeoError::Memory(format!("LLM call failed during conflict classification: {}", e))
    })?;

    parse_classification(&response.content)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_classification(content: &str) -> Result<ConflictClassification> {
    let json_str = extract_json_object(content);

    let raw: RawClassification = serde_json::from_str(&json_str).map_err(|e| {
        GrafeoError::Memory(format!("Failed to parse conflict classification response: {}", e))
    })?;

    let conflict_type = match raw.conflict_type.to_lowercase().as_str() {
        "evolution" => LlmConflictType::Evolution,
        "correction" => LlmConflictType::Correction,
        _ => LlmConflictType::Ambiguous,
    };

    let suggested_action = raw.suggested_action.to_lowercase();

    Ok(ConflictClassification {
        conflict_type,
        reasoning: raw.reasoning,
        confidence: raw.confidence.clamp(0.0, 1.0),
        suggested_action,
        evidence_verified: raw.evidence_verified,
    })
}

#[derive(Debug, Deserialize)]
struct RawClassification {
    conflict_type: String,
    reasoning: String,
    confidence: f32,
    suggested_action: String,
    evidence_verified: bool,
}

/// Extract a JSON object from potentially markdown-wrapped content.
fn extract_json_object(content: &str) -> String {
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
    use crate::consolidation::triple_extraction::LlmResponse;

    struct MockConflictLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl TripleExtractorLlm for MockConflictLlm {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String> {
            Ok(LlmResponse {
                content: self.response.clone(),
                usage_tokens: Some(100),
            })
        }
    }

    // =====================================================================
    // Test: Classify Evolution conflict
    // =====================================================================

    #[tokio::test]
    async fn test_classify_evolution() {
        let llm = MockConflictLlm {
            response: r#"{"conflict_type":"evolution","reasoning":"User moved from Beijing to Shanghai","confidence":0.9,"suggested_action":"replace","evidence_verified":true}"#.to_string(),
        };

        let result = classify_conflict(
            "user", "lives_in", "Beijing",
            "user", "lives_in", "Shanghai",
            Some("User said: I moved to Shanghai last month"),
            &llm,
        ).await.unwrap();

        assert_eq!(result.conflict_type, LlmConflictType::Evolution);
        assert_eq!(result.suggested_action, "replace");
        assert!(result.evidence_verified);
        assert!((result.confidence - 0.9).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: Classify Correction conflict
    // =====================================================================

    #[tokio::test]
    async fn test_classify_correction() {
        let llm = MockConflictLlm {
            response: r#"{"conflict_type":"correction","reasoning":"User corrected their birthday from March to May","confidence":0.95,"suggested_action":"replace","evidence_verified":true}"#.to_string(),
        };

        let result = classify_conflict(
            "user", "birthday", "March",
            "user", "birthday", "May",
            Some("User said: Not March, it's May"),
            &llm,
        ).await.unwrap();

        assert_eq!(result.conflict_type, LlmConflictType::Correction);
        assert_eq!(result.suggested_action, "replace");
    }

    // =====================================================================
    // Test: Classify Ambiguous conflict
    // =====================================================================

    #[tokio::test]
    async fn test_classify_ambiguous() {
        let llm = MockConflictLlm {
            response: r#"{"conflict_type":"ambiguous","reasoning":"User could like both Chinese and Western food","confidence":0.7,"suggested_action":"keep_both","evidence_verified":false}"#.to_string(),
        };

        let result = classify_conflict(
            "user", "likes", "Chinese food",
            "user", "likes", "Western food",
            None,
            &llm,
        ).await.unwrap();

        assert_eq!(result.conflict_type, LlmConflictType::Ambiguous);
        assert_eq!(result.suggested_action, "keep_both");
        assert!(!result.evidence_verified);
    }

    // =====================================================================
    // Test: Parse classification with markdown wrapper
    // =====================================================================

    #[test]
    fn test_parse_classification_markdown() {
        let content = "```json\n{\"conflict_type\":\"evolution\",\"reasoning\":\"test\",\"confidence\":0.8,\"suggested_action\":\"replace\",\"evidence_verified\":true}\n```";
        let result = parse_classification(content).unwrap();
        assert_eq!(result.conflict_type, LlmConflictType::Evolution);
    }

    // =====================================================================
    // Test: Unknown conflict type defaults to Ambiguous
    // =====================================================================

    #[test]
    fn test_parse_unknown_type_defaults_ambiguous() {
        let content = r#"{"conflict_type":"unknown","reasoning":"test","confidence":0.5,"suggested_action":"ask_user","evidence_verified":false}"#;
        let result = parse_classification(content).unwrap();
        assert_eq!(result.conflict_type, LlmConflictType::Ambiguous);
    }

    // =====================================================================
    // Test: Confidence is clamped
    // =====================================================================

    #[test]
    fn test_confidence_clamped() {
        let content = r#"{"conflict_type":"evolution","reasoning":"test","confidence":1.5,"suggested_action":"replace","evidence_verified":true}"#;
        let result = parse_classification(content).unwrap();
        assert!((result.confidence - 1.0).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: LlmConflictType serialization roundtrip
    // =====================================================================

    #[test]
    fn test_conflict_type_serde() {
        for ct in [LlmConflictType::Evolution, LlmConflictType::Correction, LlmConflictType::Ambiguous] {
            let json = serde_json::to_string(&ct).unwrap();
            let decoded: LlmConflictType = serde_json::from_str(&json).unwrap();
            assert_eq!(ct, decoded);
        }
    }
}
