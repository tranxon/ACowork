//! SessionConfigDelta and SessionConfigSnapshot (ADR-047 §3.2).
//!
//! `SessionConfigDelta` is the **single payload type** for all config
//! mutations coming from any external interface (HTTP, MQTT, CLI).
//! Each field is `None` (unchanged) or `Some(new_value)`.
//!
//! `SessionConfigSnapshot` is a read-only view of the current config,
//! used by HTTP GET, MQTT retained messages, and LLM-side effect
//! application.

use serde::{Deserialize, Serialize};

/// Partial session config update.
///
/// Adding a new config parameter:
/// 1. Add field here
/// 2. Add handling in `ConversationSession::apply_config()`
/// 3. Add to `SessionConfig` proto + `build_session_config_snapshot()`
/// 4. (Optional) Add LLM-side effect in `llm_effects.rs`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfigDelta {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
    /// ADR-074: per-session context window override.
    ///
    /// - `Some(0)` = clear the override (inherit the per-agent chain)
    /// - `Some(n)` where `n` is valid (`FLOOR..=CEILING`) = set
    /// - `None` = unchanged (field absent or `null` in the request body)
    ///
    /// Out-of-range values are rejected with HTTP 400 at
    /// `put_session_config` before reaching `apply_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub title: Option<String>,
}

/// Read-only snapshot of current session config.
///
/// Used by HTTP GET, MQTT retained, and LLM-side effect application.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionConfigSnapshot {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
    /// ADR-074: raw per-session context window override. `null` = no
    /// override (session inherits the per-agent chain). The effective
    /// session window is NOT exposed here — its authoritative source is
    /// the `contextUsage.context_window` push (§11.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub title: Option<String>,
}
