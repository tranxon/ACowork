//! RuntimeAgentTokenService — wraps AgentCore's atomic token counters.
//!
//! ADR-028 / ADR-040: this is the sole source of truth for token accounting.

use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::Mutex;

use acowork_core::providers::traits::UsageInfo;

use crate::agent::agent_core::AgentCore;
use crate::usecases::agent_token::AgentTokenService;

pub struct RuntimeAgentTokenService {
    core: Arc<AgentCore>,
    /// Per-session token snapshots. Keyed by session_id.
    /// Updated by the agent loop push path; read by list_sessions.
    session_tokens: Mutex<HashMap<String, (u64, u64)>>,
}

impl RuntimeAgentTokenService {
    pub fn new(core: Arc<AgentCore>) -> Self {
        Self {
            core,
            session_tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Record a per-session token snapshot (called from loop context push).
    pub async fn record_session_tokens(&self, session_id: &str, input: u64, output: u64) {
        let mut map = self.session_tokens.lock().await;
        map.insert(session_id.to_string(), (input, output));
    }

    /// Remove a session's token tracking (e.g. on session delete).
    pub async fn forget_session(&self, session_id: &str) {
        let mut map = self.session_tokens.lock().await;
        map.remove(session_id);
    }
}

impl AgentTokenService for RuntimeAgentTokenService {
    fn accumulate_llm_usage(&self, usage: &UsageInfo) {
        self.core.accumulate_llm_usage(usage);
    }

    fn merge_token_totals(&self, scanned: (Option<u64>, Option<u64>)) {
        self.core.merge_token_totals(scanned);
    }

    fn agent_token_totals(&self) -> (u64, u64) {
        self.core.agent_token_totals()
    }

    fn session_token_totals(&self, session_id: &str) -> Option<(u64, u64)> {
        self.session_tokens
            .try_lock()
            .ok()
            .and_then(|map| map.get(session_id).copied())
    }
}

// ── Test helpers ────────────────────────────────────────────────────

/// No-op token service for tests that don't need real token accounting.
#[cfg(test)]
pub(crate) struct NoopAgentTokenService;

#[cfg(test)]
impl AgentTokenService for NoopAgentTokenService {
    fn accumulate_llm_usage(&self, _: &UsageInfo) {}
    fn merge_token_totals(&self, _: (Option<u64>, Option<u64>)) {}
    fn agent_token_totals(&self) -> (u64, u64) {
        (0, 0)
    }
    fn session_token_totals(&self, _: &str) -> Option<(u64, u64)> {
        None
    }
}

/// In-memory token service for tests that require token merge semantics.
#[cfg(test)]
pub(crate) struct InMemoryAgentTokenService {
    totals: std::sync::Mutex<(u64, u64)>,
}

#[cfg(test)]
impl InMemoryAgentTokenService {
    pub(crate) fn new() -> Self {
        Self { totals: std::sync::Mutex::new((0, 0)) }
    }
}

#[cfg(test)]
impl AgentTokenService for InMemoryAgentTokenService {
    fn accumulate_llm_usage(&self, _: &UsageInfo) {}
    fn merge_token_totals(&self, scanned: (Option<u64>, Option<u64>)) {
        let mut t = self.totals.lock().unwrap();
        if let Some(input) = scanned.0 {
            t.0 = t.0.max(input);
        }
        if let Some(output) = scanned.1 {
            t.1 = t.1.max(output);
        }
    }
    fn agent_token_totals(&self) -> (u64, u64) {
        *self.totals.lock().unwrap()
    }
    fn session_token_totals(&self, _: &str) -> Option<(u64, u64)> {
        None
    }
}
