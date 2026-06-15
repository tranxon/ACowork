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
