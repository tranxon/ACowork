//! Manifest bootstrap of Identity / Capability autobiographical nodes.
//!
//! ADR-068 M8 — bootstrap scope is intentionally narrow.
//!
//! We bootstrap ONLY the categories that are **statically declared in the
//! agent manifest** (Identity, Capability). These are facts about the
//! agent's identity/abilities that do not need observational evidence to
//! assert, so the direct `store_autobiographical` fast path is appropriate.
//!
//! We DO NOT bootstrap Relationship, Limitation, Preference, or History
//! nodes — those require observational evidence that only emerges at
//! runtime and are produced by the EpisodicDistiller (see
//! `acowork_grafeo::consolidation::EpisodicDistiller::promote_*`).
//!
//! This module is a free function (not an `AgentCore` method) so the e2e
//! suite can exercise the bootstrap path without constructing a full agent
//! (ADR-068 E1).

use std::collections::HashMap;

use acowork_core::manifest::AgentManifest;
use acowork_memory::types::{AutobioCategory, AutobiographicalNode, NodeStatus};
use acowork_memory::MemoryProvider;
use chrono::Utc;

/// Outcome counters of one bootstrap run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapOutcome {
    /// Identity nodes written (agent_id / name / description / …).
    pub identity_written: usize,
    /// Capability nodes written.
    pub capability_written: usize,
    /// True when the bootstrap was skipped because Identity nodes already
    /// exist (idempotency).
    pub skipped_existing: bool,
}

/// Write manifest-declared Identity + Capability nodes through `provider`.
///
/// Idempotent: when any `AutobioCategory::Identity` node already exists the
/// whole bootstrap is skipped (the store is considered already bootstrapped).
/// Individual write failures are logged and skipped rather than aborting the
/// remaining writes.
pub fn bootstrap_autobiographical_from_manifest(
    manifest: &AgentManifest,
    provider: &dyn MemoryProvider,
) -> BootstrapOutcome {
    match provider.find_autobiographical_by_category(AutobioCategory::Identity) {
        Ok(existing) if !existing.is_empty() => {
            tracing::debug!(count = existing.len(), "Autobiographical nodes already exist, skipping manifest bootstrap");
            return BootstrapOutcome {
                skipped_existing: true,
                ..BootstrapOutcome::default()
            };
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to probe existing Autobiographical nodes, attempting bootstrap anyway");
        }
        _ => {}
    }

    let now = Utc::now();
    let mut outcome = BootstrapOutcome::default();

    let identity_entries: Vec<(&str, String)> = {
        let mut v = vec![
            ("agent_id", manifest.agent_id.clone()),
            ("name", manifest.name.clone()),
            ("description", manifest.description.clone()),
        ];
        if let Some(ref dn) = manifest.display_name {
            v.push(("display_name", dn.clone()));
        }
        if let Some(ref role) = manifest.role {
            v.push(("role", role.clone()));
        }
        v
    };
    for (key, value) in &identity_entries {
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Identity,
            key: key.to_string(),
            value: value.clone(),
            confidence: 1.0,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: now,
            updated_at: now,
            // Bootstrapped from the agent manifest — not a user statement.
            source: "manifest".to_string(),
            metadata: HashMap::new(),
        };
        match provider.store_autobiographical(&node) {
            Ok(_) => outcome.identity_written += 1,
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "Failed to bootstrap Autobiographical/Identity node");
            }
        }
    }
    for (cap_key, cap_def) in &manifest.capabilities {
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Capability,
            key: cap_key.clone(),
            value: cap_def.description.clone(),
            confidence: 1.0,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: now,
            updated_at: now,
            // Bootstrapped from the agent manifest — not a user statement.
            source: "manifest".to_string(),
            metadata: HashMap::new(),
        };
        match provider.store_autobiographical(&node) {
            Ok(_) => outcome.capability_written += 1,
            Err(e) => {
                tracing::warn!(capability = %cap_key, error = %e, "Failed to bootstrap Autobiographical/Capability node");
            }
        }
    }
    tracing::info!(
        identity_count = outcome.identity_written,
        capability_count = outcome.capability_written,
        "Bootstrapped Autobiographical/Identity+Capability nodes from manifest (ADR-068 M8: Relationship/Limitation/Preference/History are runtime-observed, owned by EpisodicDistiller)"
    );
    outcome
}
