//! RuntimeSessionConfigService - implements SessionConfigService (ADR-047 §3.4.2).
//!
//! Uses interior mutability via `Arc<RwLock<HashMap>>` to access session
//! config stores, unblocking the ADR-040 `&mut self` problem. All methods
//! are `&self`, so the struct can be safely wrapped in `Arc<dyn SessionConfigService>`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::agent::session_config::{SessionConfigDelta, SessionConfigSnapshot};
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::tools::workspace_resolver::WorkspaceResolver;
use crate::usecases::session_config::SessionConfigService;

/// Shared map of session config stores, keyed by session_id.
///
/// Populated by `SessionManager` when sessions are created, and depopulated
/// when sessions are removed. Read by `RuntimeSessionConfigService` to
/// apply config changes without going through the serial inference queue.
pub type SharedSessionConfigs = Arc<RwLock<HashMap<String, Arc<ConversationSession>>>>;

pub struct RuntimeSessionConfigService {
    /// Shared session config stores, keyed by session_id.
    sessions: SharedSessionConfigs,
    /// For workspace validation (optional - CLI mode may not have one).
    resolver: Option<Arc<RwLock<WorkspaceResolver>>>,
}

impl RuntimeSessionConfigService {
    pub fn new(
        sessions: SharedSessionConfigs,
        resolver: Option<Arc<RwLock<WorkspaceResolver>>>,
    ) -> Self {
        Self { sessions, resolver }
    }
}

#[async_trait]
impl SessionConfigService for RuntimeSessionConfigService {
    async fn apply_config(&self, session_id: &str, delta: SessionConfigDelta) -> Result<()> {
        // Validate workspace_id if provided and resolver is available.
        if let Some(ref workspace_id) = delta.workspace_id
            && workspace_id != "__agent_home__"
            && let Some(ref resolver) = self.resolver
        {
            let guard = resolver.read().map_err(|e| {
                RuntimeError::Config(format!("WorkspaceResolver lock poisoned: {}", e))
            })?;
            if guard.find_by_id(workspace_id).is_none() {
                return Err(RuntimeError::Config(format!(
                    "Workspace not found: {}",
                    workspace_id
                )));
            }
        }

        let sessions = self.sessions.read().map_err(|e| {
            RuntimeError::Config(format!("SessionConfigs lock poisoned: {}", e))
        })?;

        let conv = sessions.get(session_id).ok_or_else(|| {
            RuntimeError::Config(format!("Session not found: {}", session_id))
        })?;

        // apply_config is &self (interior mutability via Mutex), so no
        // additional locking is needed beyond the RwLock read guard.
        conv.apply_config(&delta);

        tracing::info!(
            session_id = %session_id,
            has_model = delta.model.is_some(),
            has_provider = delta.provider.is_some(),
            has_workspace = delta.workspace_id.is_some(),
            has_effort = delta.reasoning_effort.is_some(),
            has_temperature = delta.temperature.is_some(),
            has_title = delta.title.is_some(),
            "SessionConfigService: apply_config completed"
        );

        Ok(())
    }

    async fn get_config(&self, session_id: &str) -> Result<SessionConfigSnapshot> {
        let sessions = self.sessions.read().map_err(|e| {
            RuntimeError::Config(format!("SessionConfigs lock poisoned: {}", e))
        })?;

        let conv = sessions.get(session_id).ok_or_else(|| {
            RuntimeError::Config(format!("Session not found: {}", session_id))
        })?;

        Ok(conv.config_snapshot())
    }
}
