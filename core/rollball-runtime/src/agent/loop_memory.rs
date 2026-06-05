//! Memory system integration for AgentLoop.
//!
//! Extracted from loop_.rs as part of ADR-014 Phase 6.
//!
//! Contains:
//! - Memory store initialization
//! - Long-term memory retrieval and context injection
//! - Document entry persistence to conversation JSONL

use crate::agent::context::ContextBuilder;

impl super::loop_::AgentLoop {
    // ── Memory system methods ──────────────────────────────────────────────

    /// Initialize the Grafeo memory store at the given workspace path.
    ///
    /// Delegates to `AgentCore::init_memory_store()`.
    /// Opens or creates `{work_dir}/memory/private.grafeo`.
    pub fn init_memory_store(&mut self, work_dir: &std::path::Path) {
        self.core.init_memory_store(work_dir);
    }

    /// Retrieve relevant long-term memories from Grafeo and inject them into
    /// the ContextBuilder for the next LLM call.
    ///
    /// Runs once per `run()` invocation, before the first LLM iteration.
    /// When the memory store is unavailable, this is a silent no-op.
    ///
    /// Returns the list of Grafeo node IDs that were retrieved (P2-4 fix).
    /// These IDs are passed to `record_turn_to_memory` so that future
    /// retrieval can trace which memories influenced each turn.
    pub(crate) async fn retrieve_and_inject_memories(
        &self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
    ) -> Vec<String> {
        // P0 fix: Always clear stale memory from previous turns first.
        // ContextBuilder is reused across turns (SessionTask loop), so
        // without this, stale memory leaks into the next LLM call.
        context_builder.clear_retrieved_memory();

        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return vec![], // No store available, already cleared above
        };

        let manager = self.core.init_memory_manager();

        // Build exclude_session_id filter to avoid re-injecting Episode
        // summaries that are already in the current session's context window.
        let current_session_id = self
            .session
            .conversation
            .as_ref()
            .map(|c| c.session_id().to_string());

        // Update MemorySessionHandle so memory_recall tool can see the
        // current session_id for its own exclude_session_id filtering.
        if let Some(ref handle) = self.core.memory_session {
            if let Some(ref sid) = current_session_id {
                handle.set_session_id(sid.clone());
            }
        }

        let mut query = rollball_memory::MemoryQuery::auto_inject(
            user_message.to_string(),
            current_session_id,
        );

        // Pass embedding provider from AgentCore so retrieve() can auto-generate
        // query embeddings on-demand (Ollama local → Remote fallback chain).
        let emb_provider = self.core.embedding_provider.as_deref();

        // P2-4 fix: Use retrieve + inject separately (instead of process_turn)
        // so we can capture the node IDs of retrieved memories for traceability.
        match manager.retrieve(store, &mut query, emb_provider).await {
            Ok(retrieval) => {
                // Capture node IDs before inject (inject discards the RetrievalResult)
                let memory_ids: Vec<String> = retrieval
                    .memories
                    .iter()
                    .filter(|m| m.node_id != 0) // 0 = RAG result, not Grafeo local
                    .map(|m| m.node_id.to_string())
                    .collect();

                let metrics = retrieval.metrics.clone();
                let injected = manager.inject(&retrieval);
                if !injected.formatted_text.is_empty() {
                    tracing::info!(
                        memory_count = injected.memory_count,
                        token_count = injected.token_count,
                        avg_score = metrics.avg_score,
                        "Retrieved and injected long-term memories into context"
                    );
                    context_builder.set_retrieved_memory(injected.formatted_text);
                }
                memory_ids
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to retrieve memories from Grafeo (non-fatal)"
                );
                vec![]
            }
        }
    }

    /// Write document upload entries to the conversation JSONL.
    ///
    /// Each document is persisted as a `ConversationEntry` with `role: "system"`
    /// and `metadata.type: "document_upload"` so that the Desktop App can render
    /// document chips when loading historical sessions.
    pub fn write_document_entries(&self, documents: &[serde_json::Value]) {
        if let Some(ref conversation) = self.session.conversation {
            for doc in documents {
                let filename = doc.get("filename").and_then(|v| v.as_str()).unwrap_or("");
                let format = doc.get("format").and_then(|v| v.as_str()).unwrap_or("");
                let size = doc.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let content = format!("Uploaded file: {} ({}, {} bytes)", filename, format, size);
                let metadata = serde_json::json!({
                    "type": "document_upload",
                    "document_id": doc.get("id"),
                    "filename": filename,
                    "format": format,
                    "size_bytes": size,
                    "path": doc.get("abs_path"),
                });
                conversation.append_message("system", &content, Some(metadata));
            }
        }
    }
}
