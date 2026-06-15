//! Heuristic conflict detection for memory nodes.
//!
//! Two-layer detection:
//! - Layer 1: Semantic similarity (embedding cosine distance) — gate
//! - Layer 2: Temporal proximity (< 24 h) — confidence boost
//!
//! All heuristic conflicts default to [`ConflictType::Ambiguous`].
//! Actual classification (Evolution / Correction / Ambiguous) is deferred to
//! Phase 3 offline LLM arbitration ([`crate::consolidation::conflict_llm`]).

use acowork_memory::{ConflictSignal, ConflictType};
use grafeo_common::types::NodeId;

use crate::types::NodeStatus;

/// Default semantic similarity thresholds by node sub-type.
pub const FACT_THRESHOLD: f32 = 0.85;
pub const PREFERENCE_THRESHOLD: f32 = 0.80;
pub const RELATION_THRESHOLD: f32 = 0.90;
pub const PROCEDURE_THRESHOLD: f32 = 0.85;

/// Default temporal conflict window in hours.
pub const TEMPORAL_WINDOW_HOURS: u64 = 24;

/// Action recommended by the conflict resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    /// Auto-resolve: new replaces old.
    AutoReplace {
        old_node_id: NodeId,
        new_status: NodeStatus,
    },
    /// Both kept, marked for user confirmation.
    MarkAmbiguous { conflict_group_id: String },
    /// Defer to LLM offline arbitration (Phase 3).
    DeferToLLM,
}

/// Extended conflict resolution with action recommendations.
#[derive(Debug, Clone)]
pub struct ConflictResolution {
    /// The original conflict signal.
    pub signal: ConflictSignal,
    /// Recommended action.
    pub action: ConflictAction,
    /// Whether LLM arbitration is required.
    pub requires_llm: bool,
}

/// Resolve a conflict signal into an actionable resolution.
///
/// Heuristic fast-path:
/// - `Evolution` → auto-replace (old → Dormant).
/// - `Correction` → auto-replace (old → Dormant).
/// - `Ambiguous` → mark for user confirmation.
pub fn resolve_conflict(signal: &ConflictSignal, existing_node_id: NodeId) -> ConflictResolution {
    let action = match signal.suggested_type {
        ConflictType::Evolution => ConflictAction::AutoReplace {
            old_node_id: existing_node_id,
            new_status: NodeStatus::Dormant,
        },
        ConflictType::Correction => ConflictAction::AutoReplace {
            old_node_id: existing_node_id,
            new_status: NodeStatus::Dormant,
        },
        ConflictType::Ambiguous => ConflictAction::MarkAmbiguous {
            conflict_group_id: format!("cg_{}", existing_node_id.as_u64()),
        },
    };

    let requires_llm = signal.suggested_type == ConflictType::Ambiguous;

    ConflictResolution {
        signal: signal.clone(),
        action,
        requires_llm,
    }
}

/// Detect conflict signals between a new node and an existing node.
///
/// Returns `None` if the semantic score is below the given threshold (no conflict).
/// Otherwise, applies temporal proximity heuristics and returns an Ambiguous
/// conflict signal — actual classification is deferred to Phase 3 LLM arbitration.
///
/// # Arguments
/// * `semantic_score` — Embedding cosine similarity between the two nodes.
/// * `threshold` — Minimum semantic similarity to consider a conflict.
/// * `time_diff_hours` — Time difference between the two nodes in hours.
pub fn detect_conflict(
    semantic_score: f32,
    threshold: f32,
    time_diff_hours: f64,
) -> Option<ConflictSignal> {
    // Layer 1: Semantic gate — must exceed threshold.
    if semantic_score < threshold {
        return None;
    }

    // Layer 2: Temporal proximity — recent conflicts (< 24 h) get higher
    // confidence, but we never auto-classify.  Phase 3 LLM arbitration
    // performs the actual Evolution / Correction / Ambiguous classification.
    let temporal_conflict = time_diff_hours < TEMPORAL_WINDOW_HOURS as f64;

    let heuristic_confidence = if temporal_conflict {
        0.7 // Recent overlap → more likely a real conflict
    } else {
        0.5 // Older overlap → could be evolution, needs LLM judgment
    };

    Some(ConflictSignal {
        semantic_score,
        temporal_conflict,
        context_negation: false,
        suggested_type: ConflictType::Ambiguous,
        heuristic_confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_conflict_below_threshold() {
        let result = detect_conflict(0.5, 0.85, 1.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_conflict_recent() {
        let result = detect_conflict(0.90, 0.85, 2.0);
        assert!(result.is_some());
        let signal = result.unwrap();
        assert!(signal.temporal_conflict);
        assert_eq!(signal.suggested_type, ConflictType::Ambiguous);
        assert!((signal.heuristic_confidence - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_conflict_old() {
        let result = detect_conflict(0.88, 0.85, 200.0);
        assert!(result.is_some());
        let signal = result.unwrap();
        assert!(!signal.temporal_conflict);
        assert_eq!(signal.suggested_type, ConflictType::Ambiguous);
        assert!((signal.heuristic_confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_conflict_at_boundary() {
        let result = detect_conflict(0.87, 0.85, 50.0);
        assert!(result.is_some());
        let signal = result.unwrap();
        assert_eq!(signal.suggested_type, ConflictType::Ambiguous);
    }

    // =====================================================================
    // resolve_conflict tests
    // =====================================================================

    #[test]
    fn test_resolve_conflict_always_ambiguous() {
        // All heuristic conflicts are Ambiguous — resolve_conflict always
        // returns MarkAmbiguous + requires_llm.
        let signal = detect_conflict(0.88, 0.85, 200.0).unwrap();
        let existing = NodeId::new(42);
        let resolution = resolve_conflict(&signal, existing);
        assert_eq!(resolution.signal.suggested_type, ConflictType::Ambiguous);
        assert!(resolution.requires_llm);
        assert_eq!(
            resolution.action,
            ConflictAction::MarkAmbiguous {
                conflict_group_id: format!("cg_{}", existing.as_u64()),
            }
        );
    }
}
