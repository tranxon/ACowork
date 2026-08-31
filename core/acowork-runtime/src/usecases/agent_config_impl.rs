//! RuntimeAgentConfigService — implements [`AgentConfigService`].
//!
//! Owns the load-merge-save cycle against `agent_config.json`. The
//! HTTP handler stays a thin protocol converter that:
//!   1. decodes the wire shape into [`PutAgentConfigBody`],
//!   2. hands it to [`RuntimeAgentConfigService::put_config`],
//!   3. re-PUBLISHes the retained MQTT snapshot using the returned
//!      [`AgentConfig`],
//!   4. broadcasts the returned [`RuntimeConfigOverrides`] to active
//!      sessions via the existing dispatch channel.
//!
//! The trait impl is the **single audit point** for `agent_config.json`
//! persistence on the HTTP path: field dispatch, type-checked
//! `serde_json::from_value` per field, and the post-persist
//! `RuntimeConfigOverrides` projection. Wire shape → patch list
//! translation lives in [`PutAgentConfigBody::from_request_fields`]
//! so the handler doesn't repeat it.
//!
//! ## State
//!
//! Holds the agent `work_dir` resolved at boot (no async resource
//! dependencies), so the service can be constructed immediately after
//! the workspace services in `session_init.rs` Phase B and doesn't
//! require a late-bind slot's typical "wait for Phase B" setup.
//!
//! ## Type validation
//!
//! Every field is decoded via `serde_json::from_value::<T>()` where
//! `T` is the concrete `AgentConfig` field type. A wrong-typed JSON
//! value (e.g. `{"temperature": "hot"}`) collapses to `None` (i.e.
//! "leave on-disk alone") and emits a `tracing::warn!` — matching the
//! pre-refactor handler behaviour so the migration is invisible to
//! the desktop.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent_config::{self, AgentConfig};
use crate::usecases::agent_config::{
    AgentConfigError, AgentConfigService, ConfigField, ConfigFieldPatch, FieldPatch,
    GetAgentConfigResponse, PutAgentConfigBody, PutAgentConfigResult,
};

/// Concrete [`AgentConfigService`] backed by the
/// `workspace/config/agent_config.json` file.
pub struct RuntimeAgentConfigService {
    work_dir: PathBuf,
}

impl RuntimeAgentConfigService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }
}

#[async_trait]
impl AgentConfigService for RuntimeAgentConfigService {
    async fn get_config(&self, agent_id: &str) -> GetAgentConfigResponse {
        let config = agent_config::load_agent_config(&self.work_dir)
            .ok()
            .flatten();
        GetAgentConfigResponse {
            agent_id: agent_id.to_string(),
            config,
            manifest_path: self.work_dir.join("manifest.toml"),
            work_dir: self.work_dir.clone(),
        }
    }

    async fn put_config(
        &self,
        agent_id: &str,
        body: PutAgentConfigBody,
    ) -> Result<PutAgentConfigResult, AgentConfigError> {
        // 1. Load current (or default for fresh-install).
        let mut cfg: AgentConfig = agent_config::load_agent_config(&self.work_dir)
            .map_err(|e| AgentConfigError::Persistence(e.to_string()))?
            .unwrap_or_default();

        // 2. Apply patches. Same dispatch loop the pre-refactor
        //    handler used, lifted here so the persistence + type
        //    validation has exactly one canonical home.
        for ConfigFieldPatch { field, op } in &body.patches {
            apply_field_patch(&mut cfg, *field, op);
        }

        // 3. Persist (atomic write-tmp-rename).
        agent_config::save_agent_config(&self.work_dir, &cfg)
            .map_err(|e| AgentConfigError::Persistence(e.to_string()))?;

        tracing::info!(
            agent_id,
            patch_count = body.patches.len(),
            "RuntimeAgentConfigService::put_config: agent_config.json persisted"
        );

        // 4. Project the new on-disk state to a RuntimeConfigOverrides
        //    so the HTTP handler can broadcast to active sessions.
        //    Using the existing `From<&AgentConfig>` impl keeps the
        //    projection schema-locked to the live-broadcast path.
        let overrides = RuntimeConfigOverrides::from(&cfg);

        // Serialize the persisted config so the HTTP handler can
        // re-PUBLISH the retained MQTT snapshot without re-reading
        // the file from disk (ADR-040: handler must not touch fs).
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());

        Ok(PutAgentConfigResult {
            agent_id: agent_id.to_string(),
            config: cfg,
            overrides,
            config_json,
        })
    }
}

// ── Field dispatch ─────────────────────────────────────────────────────

/// Apply one [`ConfigFieldPatch`] to an in-memory [`AgentConfig`].
/// Mirrors the pre-refactor handler's dispatch loop so the migration
/// is invisible to the desktop (same per-field type checks, same
/// `tracing::warn!` on type mismatch).
fn apply_field_patch(cfg: &mut AgentConfig, field: ConfigField, op: &FieldPatch<serde_json::Value>) {
    match field {
        ConfigField::MaxOutputTokens => {
            cfg.max_output_tokens = patch_typed::<u64>(field, op);
        }
        ConfigField::MaxIterations => {
            cfg.max_iterations = patch_typed::<u32>(field, op);
        }
        ConfigField::MaxSessions => {
            cfg.max_sessions = patch_typed::<u64>(field, op).map(|v| v as usize);
        }
        ConfigField::Temperature => {
            cfg.temperature = patch_typed::<f32>(field, op);
        }
        ConfigField::ContextWindow => {
            cfg.context_window = patch_typed::<u64>(field, op);
        }
        ConfigField::ShellApprovalThreshold => {
            cfg.shell_approval_threshold = patch_typed::<String>(field, op);
        }
        ConfigField::ApprovalTimeoutSecs => {
            cfg.approval_timeout_secs = patch_typed::<u64>(field, op);
        }
        ConfigField::IdleTimeoutSecs => {
            cfg.idle_timeout_secs = patch_typed::<u64>(field, op);
        }
        ConfigField::CompressionRatioThreshold => {
            cfg.compression_ratio_threshold = patch_typed::<f64>(field, op);
        }
    }
}

/// Type-erase a `FieldPatch<serde_json::Value>` into the
/// persistence-loop-friendly `Option<T>` shape (`Set(v)` -> `Some(T)`,
/// `Clear` -> `None`). A wrong-typed JSON value (e.g. `Set("foo")` for
/// a `u64` field) collapses to `None` and emits a `tracing::warn!`,
/// matching the pre-refactor handler so the two write paths can never
/// disagree about what landed on disk.
fn patch_typed<T>(field: ConfigField, patch: &FieldPatch<serde_json::Value>) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    match patch {
        FieldPatch::Clear => None,
        FieldPatch::Set(v) => match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(
                    field = field.as_str(),
                    value = ?v,
                    error = %e,
                    "AgentConfigService::put_config: type mismatch — leaving on-disk value"
                );
                None
            }
        },
    }
}