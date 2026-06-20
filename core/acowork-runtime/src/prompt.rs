//! Centralized prompt constants for the AgentCowork Agent Runtime.
//!
//! All hardcoded prompt strings that appear in production code should be
//! defined here as named constants to ensure consistency and ease of maintenance.

/// Default system prompt when no prompt files are found in the package.
pub const PROMPT_BUILDER_FALLBACK: &str = "You are a helpful AI assistant.";

/// System prompt used for context compaction via LLM.
/// Replaces the agent's full system prompt during compaction to ensure
/// the LLM focuses on summarization rather than tool usage.
pub const COMPACTION_SYSTEM_PROMPT: &str = "You are an AI assistant that summarizes conversations.";

/// System prompt for the Perplexity (Sonar) web search integration.
pub const SEARCH_SYSTEM_PROMPT: &str =
    "You are a web search assistant. Search the web and return results with citations. Be concise.";

/// Prompt for context compaction and episode distillation.
///
/// Per [ADR-011], the LLM outputs a plain natural-language summary — not JSON.
/// The summary serves both as in-memory context replacement and as a Grafeo
/// episodic memory entry.
///
/// Memory-hint extraction (entities + triples) was moved from per-round LLM
/// output to compaction-time extraction. The compact model produces entities
/// and triples alongside the summary — zero per-round token cost, higher
/// quality extraction from full conversation context.
pub const COMPACT_PROMPT: &str = r#"You are a conversation summarization assistant. Your task is to produce a comprehensive natural-language summary of the conversation below, then extract key entities and knowledge triples.

Instructions:
- Write a concise but complete summary covering all key topics discussed, decisions made, problems solved, and code written.
- Include technical details that would be needed to resume work later.
- Preserve the chronological flow of the conversation.
- After the summary, append entity and triple sections using the exact format below.

Output format (plain text):
<summary>
Your natural-language summary text goes here...
</summary>
<entities>
Entity1, Entity2, Entity3
</entities>
<triples>
subject | predicate | object
subject | predicate | object
</triples>

Entities: core people, places, technologies, projects, or concepts that persist across the conversation (max 10, comma-separated).
Triples: factual knowledge expressed as subject|predicate|object. One per line. Only extract explicit facts — do not invent or speculate.

Conversation:
{messages_text}

Output:"#;

/// Build the final system prompt for context compaction by concatenating the
/// base [`COMPACTION_SYSTEM_PROMPT`] with the user's identity context.
///
/// The user's `identity_context` is a small (~200B) text block produced by
/// [`super::session::session_manager::format_user_profile_context`], e.g.:
///
/// ```text
/// - Display Name: ...
/// - Language: zh-CN
/// - Timezone: Asia/Shanghai
/// ...
/// ```
///
/// We embed it inline (rather than parsing the `Language:` line with a regex)
/// so the LLM itself reads the language field — no schema, no fragile parsing,
/// and any future field added to identity is automatically picked up.
///
/// Behaviour:
/// - `None` or empty/whitespace identity → returns `base` unchanged
///   (English default — safe fallback for sessions with no user profile).
/// - Non-empty identity → returns `base` + identity block + language directive.
pub fn build_compaction_system_prompt(base: &str, identity_context: Option<&str>) -> String {
    let Some(ctx) = identity_context.map(str::trim).filter(|s| !s.is_empty()) else {
        return base.to_string();
    };
    format!(
        "{base}\n\n\
         User identity context (use the Language field to determine what language \
         to write the summary in):\n\
         {ctx}\n\n\
         Write the summary, entities list, and knowledge triples in the user's \
         preferred language as indicated above."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_compaction_system_prompt_none_identity_returns_base_unchanged() {
        let base = "You are a summarizer.";
        assert_eq!(build_compaction_system_prompt(base, None), base);
    }

    #[test]
    fn build_compaction_system_prompt_empty_identity_returns_base_unchanged() {
        let base = "You are a summarizer.";
        assert_eq!(build_compaction_system_prompt(base, Some("")), base);
    }

    #[test]
    fn build_compaction_system_prompt_whitespace_identity_returns_base_unchanged() {
        let base = "You are a summarizer.";
        assert_eq!(build_compaction_system_prompt(base, Some("   \n\t  ")), base);
    }

    #[test]
    fn build_compaction_system_prompt_with_identity_includes_directive_and_context() {
        let base = "You are a summarizer.";
        let identity = "- Display Name: Alice\n- Language: zh-CN\n- Timezone: Asia/Shanghai";
        let out = build_compaction_system_prompt(base, Some(identity));
        // base preserved at the head
        assert!(out.starts_with(base), "base must be preserved at the start");
        // identity text embedded verbatim
        assert!(out.contains(identity), "identity text must be embedded verbatim");
        // explicit pointers so the LLM knows where to look for the language field
        assert!(out.contains("User identity context"), "must label the identity block");
        assert!(out.contains("Language field"), "must point the LLM at the Language field");
        assert!(out.contains("preferred language"), "must include a language directive");
    }

    #[test]
    fn build_compaction_system_prompt_trims_surrounding_whitespace() {
        // Identity surrounded by whitespace should be accepted; the surrounding
        // whitespace is stripped before concatenation, but the inner content
        // (e.g. "  Language: en-US  ") is preserved verbatim.
        let base = "base";
        let out = build_compaction_system_prompt(base, Some("  - Language: en-US  \n"));
        assert!(out.contains("- Language: en-US"));
    }
}
