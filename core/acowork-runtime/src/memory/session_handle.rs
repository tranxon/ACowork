//! Memory session handle - shared state between agent loop and memory tools.
//!
//! Memory tools (memory_recall, memory_store) are created once per agent,
//! but sessions change dynamically and the memory provider may be initialized
//! lazily (after tool creation). This handle provides a shared, lock-protected
//! context for session-scoped operations without changing the Tool trait.
//!
//! ADR-051 C3: Primary type is now `Arc<dyn MemoryProvider>`.
//! ADR-051 C4: grafeo_store compat field removed; all callers use trait methods.

use std::sync::{Arc, RwLock};

use acowork_memory::{MemoryManagerConfig, MemoryProvider};

use crate::embedding::EmbeddingProvider;

/// Lightweight session context shared between the agent loop (writer)
/// and memory tools (readers).
pub struct MemorySessionHandle {
    /// Memory provider (lazily initialized, shared across all sessions).
    /// ADR-051 C3: Changed from `Arc<GrafeoStore>` to `Arc<dyn MemoryProvider>`.
    provider: RwLock<Option<Arc<dyn MemoryProvider>>>,
    /// ID of the currently active session.
    current_session_id: RwLock<Option<String>>,
    /// Embedding provider (set once at construction, immutable thereafter).
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Agent-level `MemoryManagerConfig`, set once at memory initialization.
    ///
    /// Config consistency: the `memory_recall` tool reads this so it uses the
    /// SAME quality config (min_score / graph_expand / …) as auto-inject,
    /// instead of hardcoded defaults. Falls back to default when unset.
    memory_config: RwLock<Option<MemoryManagerConfig>>,
}

impl MemorySessionHandle {
    /// Create a new handle with no provider (lazy initialization).
    pub fn new(embedding_provider: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self {
            provider: RwLock::new(None),
            current_session_id: RwLock::new(None),
            embedding_provider,
            memory_config: RwLock::new(None),
        }
    }

    /// Set the memory provider once it becomes available.
    ///
    /// Called by `AgentCore` when memory initialization completes.
    /// Both the trait object and the concrete GrafeoStore reference are set.
    pub fn set_provider(&self, provider: Arc<dyn MemoryProvider>) {
        let mut guard = self
            .provider
            .write()
            .expect("MemorySessionHandle provider lock poisoned");
        assert!(
            guard.is_none(),
            "MemorySessionHandle provider already initialized"
        );
        *guard = Some(provider);
    }

    /// Read a clone of the provider, if initialized.
    pub fn provider(&self) -> Option<Arc<dyn MemoryProvider>> {
        self.provider.read().ok().and_then(|guard| guard.clone())
    }

    /// Set the current session ID.
    pub fn set_session_id(&self, id: String) {
        if let Ok(mut guard) = self.current_session_id.write() {
            *guard = Some(id);
        }
    }

    /// Clear the current session ID (e.g. when a session ends).
    pub fn clear_session_id(&self) {
        if let Ok(mut guard) = self.current_session_id.write() {
            *guard = None;
        }
    }

    /// Read the current session ID.
    pub fn current_session_id(&self) -> Option<String> {
        self.current_session_id
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Read a clone of the embedding provider, if set.
    pub fn embedding(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedding_provider.clone()
    }

    /// Set the agent's memory manager config (called once at memory init).
    pub fn set_memory_config(&self, config: MemoryManagerConfig) {
        if let Ok(mut guard) = self.memory_config.write() {
            *guard = Some(config);
        }
    }

    /// Read a clone of the agent's memory manager config, if set.
    pub fn memory_config(&self) -> Option<MemoryManagerConfig> {
        self.memory_config
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }
}
