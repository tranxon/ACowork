//! Memory session handle — shared state between agent loop and memory tools.
//!
//! Memory tools (memory_recall, memory_store) are created once per agent,
//! but sessions change dynamically and the Grafeo store may be initialized
//! lazily (after tool creation). This handle provides a shared, lock-protected
//! context for session-scoped operations without changing the Tool trait.

use std::sync::{Arc, RwLock};

use rollball_grafeo::GrafeoStore;

use crate::embedding::EmbeddingProvider;

/// Lightweight session context shared between the agent loop (writer)
/// and memory tools (readers).
///
/// # Design
///
/// - `store`: lazily initialized; tools check availability on each call.
/// - `current_session_id`: written by `SessionTask` before each turn,
///   read by tools during `execute()`.  Uses `RwLock` because writes are
///   infrequent (once per turn switch) and reads far more common.
/// - `embedding_provider`: set once at construction, immutable thereafter.
///   Tools and agent loop pass it to [`MemoryManager::retrieve`] for
///   auto-embedding.
///
/// This separation avoids the need to inject session context through the
/// [`Tool`](rollball_core::tools::traits::Tool) trait, keeping tool
/// signatures simple while still providing session-aware behaviour.
pub struct MemorySessionHandle {
    /// Grafeo memory store (lazily initialized, shared across all sessions).
    store: RwLock<Option<Arc<GrafeoStore>>>,
    /// ID of the currently active session.
    ///
    /// `None` when no session is active (e.g. between session switches).
    /// Memory tools use this to exclude current-session nodes from recall,
    /// since they are already present in the conversation context window.
    current_session_id: RwLock<Option<String>>,
    /// Embedding provider (set once at construction, immutable thereafter).
    /// Used by memory tools and agent loop for vector-based retrieval.
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl MemorySessionHandle {
    /// Create a new handle with no store (lazy initialization).
    pub fn new(embedding_provider: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self {
            store: RwLock::new(None),
            current_session_id: RwLock::new(None),
            embedding_provider,
        }
    }

    /// Set the Grafeo store once it becomes available.
    ///
    /// Called by `AgentCore` when memory initialization completes.
    /// Panics if a store is already set (store is set exactly once).
    pub fn set_store(&self, store: Arc<GrafeoStore>) {
        let mut guard = self.store.write().expect("MemorySessionHandle store lock poisoned");
        assert!(guard.is_none(), "MemorySessionHandle store already initialized");
        *guard = Some(store);
    }

    /// Read a clone of the store, if initialized.
    pub fn store(&self) -> Option<Arc<GrafeoStore>> {
        self.store
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Set the current session ID.
    ///
    /// Called by `SessionTask` whenever a session becomes active or switches.
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
    ///
    /// Returns a cloned copy so readers don't hold the lock.
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
}
