//! P3-3: LLM-based retrieval quality Judge.
//!
//! Replaces the mock `evaluate_retrieval()` in `acowork-grafeo::judge`.
//! Uses the cheapest available model to evaluate retrieval quality on a
//! sampled basis (default 10% of retrievals, top-3 results only).
//!
//! The LLM Judge is intentionally lightweight:
//! - Single prompt, no conversation history
//! - Scores 1–5 (5 = highly relevant)
//! - Returns structured reasoning for debugging
//! - Cost controlled by `sample_rate` and `top_k` in JudgeConfig

use acowork_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider};
use acowork_grafeo::judge::{JudgeConfig, JudgeResult};

/// Evaluate retrieval quality using an actual LLM call.
///
/// Sends a single-shot prompt to the configured Judge model asking it
/// to rate the relevance of each result to the query on a 1–5 scale.
/// Returns the average relevance score and the LLM's reasoning.
///
/// Falls back to a score of 3 (neutral) if the LLM call fails or
/// returns an unparseable response — never blocks the retrieval pipeline.
pub async fn evaluate_retrieval_llm(
    provider: &dyn Provider,
    config: &JudgeConfig,
    query: &str,
    results: &[String],
) -> JudgeResult {
    let top_results: Vec<&String> = results.iter().take(config.top_k).collect();
    if top_results.is_empty() {
        return JudgeResult {
            relevance_score: 0,
            reason: "No results to evaluate.".to_string(),
        };
    }

    let results_text = top_results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("Result {}: {}", i + 1, r))
        .collect::<Vec<_>>()
        .join("\n\n");

    let prompt = format!(
        "You are a retrieval quality judge. Rate how relevant the following \
         search results are to the user's query on a scale of 1 to 5, where \
         5 = highly relevant and 1 = completely irrelevant.\n\n\
         Query: {query}\n\n\
         {results_text}\n\n\
         Respond with ONLY a single integer from 1 to 5, followed by a brief \
         reason on the next line. Example:\n4\nResults directly address the query."
    );

    let request = ChatRequest {
        model: config.model.clone(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: prompt,
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        temperature: Some(0.0), // Deterministic judging
        max_tokens: Some(128),  // Short response
        tools: None,
        reasoning_effort: None,
    thinking_mode: None,
    };

    match provider.chat(request).await {
        Ok(response) => {
            let text = response.content.trim();
            parse_judge_response(text)
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "LLM Judge call failed, using neutral score"
            );
            JudgeResult {
                relevance_score: 3,
                reason: format!("LLM Judge call failed: {e}"),
            }
        }
    }
}

/// Parse the LLM Judge response into a JudgeResult.
///
/// Expected format: a score (1-5) on the first line, reason on subsequent lines.
/// Falls back to score 3 if parsing fails.
fn parse_judge_response(text: &str) -> JudgeResult {
    let mut lines = text.lines();
    let first_line = lines.next().unwrap_or("").trim();

    // Try to extract a score from the first line.
    let score = if let Ok(n) = first_line.parse::<u8>() {
        n.clamp(1, 5)
    } else {
        // Try to find a digit anywhere in the first line.
        first_line
            .chars()
            .find_map(|c| {
                if c.is_ascii_digit() {
                    let n = c.to_string().parse::<u8>().ok()?;
                    Some(n.clamp(1, 5))
                } else {
                    None
                }
            })
            .unwrap_or(3)
    };

    let reason = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    let reason = if reason.is_empty() {
        format!("LLM Judge scored {}.", score)
    } else {
        reason
    };

    JudgeResult {
        relevance_score: score,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_judge_response_numeric() {
        let result = parse_judge_response("4\nResults are relevant to the query.");
        assert_eq!(result.relevance_score, 4);
        assert!(result.reason.contains("relevant"));
    }

    #[test]
    fn test_parse_judge_response_clamp_high() {
        let result = parse_judge_response("9\nToo high");
        assert_eq!(result.relevance_score, 5);
    }

    #[test]
    fn test_parse_judge_response_clamp_low() {
        let result = parse_judge_response("0\nToo low");
        assert_eq!(result.relevance_score, 1);
    }

    #[test]
    fn test_parse_judge_response_no_reason() {
        let result = parse_judge_response("3");
        assert_eq!(result.relevance_score, 3);
        assert!(result.reason.contains("scored 3"));
    }

    #[test]
    fn test_parse_judge_response_text_prefix() {
        let result = parse_judge_response("Score: 4\nGood match");
        assert_eq!(result.relevance_score, 4);
        assert!(result.reason.contains("Good match"));
    }

    #[test]
    fn test_parse_judge_response_garbled() {
        let result = parse_judge_response("I think they are somewhat relevant");
        // Should find a digit or default to 3
        assert!((1..=5).contains(&result.relevance_score));
    }
}
