//! EpisodicDistiller — offline promotion of classified Episodes to semantic
//! nodes (ADR-068).
//!
//! Two-axis orthogonalization (ADR-068): the LLM writes ONLY to the Episodic
//! layer (via the `memory_store` tool, tagging episodes with a
//! `knowledge_subtype`). The semantic layer (Knowledge / Procedural /
//! Autobiographical nodes) is produced EXCLUSIVELY by this offline distiller:
//!
//! ```text
//! Step 1   scan unconsolidated episodes (knowledge_subtype.is_some())
//! Step 2a  server-side LLM structured extraction + autobio recognition
//! Step 2b  embedding-similarity clustering (cosine >= cluster_threshold)
//! Step 3   per-class promotion strategies (evidence gates)
//! Step 4   LLM judge (promote / skip / defer + confidence + reasoning)
//! Step 5   write semantic node + mark episodes consolidated
//! Step 6   emit DistillerResult with full audit trail
//! ```
//!
//! This module is the only producer of semantic-layer nodes (ADR-068 R3).
//! It is deliberately decoupled from the runtime: it consumes
//! `dyn MemoryProvider` and `dyn TripleExtractorLlm` (both from
//! `acowork_memory`), plus an optional `EmbeddingFn`.

use std::collections::HashMap;

use acowork_memory::consolidation::{
    AutobioAspect, AutobioCandidate, DistillerConfig, DistillerResult, EmbeddingFn,
    ExtractedKind, ExtractedStructure, LlmMessage, PromotionDecision, PromotionEvaluation,
    PromotionKind, PromotionMetadata, TripleExtractorLlm,
};
use acowork_memory::types::{
    AutobioCategory, AutobiographicalNode, Episode, KnowledgeNode, KnowledgeSubType, NodeStatus,
    PrivacyLevel, ProceduralNode,
};
use acowork_memory::MemoryProvider;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{GrafeoError, Result};

// ============================================================================
// Trait
// ============================================================================

/// Offline distiller that promotes classified Episodes to semantic nodes.
///
/// ADR-068 §3.4: `run` performs one distillation batch over unconsolidated
/// episodes and returns a [`DistillerResult`] with a full audit trail.
#[async_trait::async_trait]
pub trait EpisodicDistiller: Send + Sync {
    /// Run one distillation pass.
    ///
    /// * `provider` — the memory backend (read candidates, write promoted
    ///   nodes, mark episodes consolidated).
    /// * `llm` — server-side LLM used for Step 2a extraction and Step 4
    ///   judging. When `None`, the run degrades to a no-op (episodes remain
    ///   untouched) — matching ADR-068 §3.4.2 "Step 2 failure handling".
    /// * `embedding_fn` — text embedding for Step 2b clustering. When `None`,
    ///   clustering falls back to exact key-string equality.
    /// * `config` — evidence thresholds and cluster parameters.
    async fn run(
        &self,
        provider: &dyn MemoryProvider,
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: Option<&EmbeddingFn>,
        config: &DistillerConfig,
    ) -> Result<DistillerResult>;
}

// ============================================================================
// Default implementation
// ============================================================================

/// Reference implementation of [`EpisodicDistiller`].
///
/// Stateless and cheap to clone — holds no per-run state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultEpisodicDistiller;

// ---------------------------------------------------------------------------
// Internal cluster types (ADR-068 §3.4.2 Step 2b/3)
// ---------------------------------------------------------------------------

/// A candidate cluster: episodes sharing a semantically-equivalent key.
#[derive(Debug, Clone)]
struct Cluster {
    /// Knowledge subtype of the cluster.
    subtype: KnowledgeSubType,
    /// Embedding of the cluster's semantic key (Step 2b).
    key_embedding: Option<Vec<f32>>,
    /// Members with their extraction results.
    members: Vec<ClusterMember>,
}

/// One episode participating in a cluster.
#[derive(Debug, Clone)]
struct ClusterMember {
    episode_id: u64,
    episode: Episode,
    extracted: ExtractedStructure,
}

/// A cluster whose members all evidence the same autobiographical aspect.
#[derive(Debug, Clone)]
struct AutobioCluster {
    aspect: AutobioAspect,
    key_hint: String,
    members: Vec<ClusterMember>,
}

/// Parsed LLM judge output (Step 4).
#[derive(Debug, Clone)]
struct JudgeOutput {
    decision: JudgeDecision,
    confidence: f32,
    reasoning: String,
    merged_content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgeDecision {
    Promote,
    Skip,
    Defer,
}

// ---------------------------------------------------------------------------
// Run orchestration (Steps 1-6)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl EpisodicDistiller for DefaultEpisodicDistiller {
    async fn run(
        &self,
        provider: &dyn MemoryProvider,
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: Option<&EmbeddingFn>,
        config: &DistillerConfig,
    ) -> Result<DistillerResult> {
        let mut result = DistillerResult::default();

        // ---- Step 1: scan unconsolidated episodes -------------------------
        // ADR-068 §3.4.2: only episodes with knowledge_subtype.is_some()
        // participate. Pure dialogue fragments (subtype == None) stay in the
        // episodic layer forever.
        let raw = provider
            .get_episodes_by_subtype(None, config.batch_size)
            .map_err(grafeo_err)?;
        let candidates: Vec<(u64, Episode)> = raw
            .into_iter()
            .filter(|(_, ep)| ep.knowledge_subtype.is_some())
            .collect();
        result.episodes_scanned = candidates.len();

        if candidates.is_empty() {
            return Ok(result);
        }

        // ---- Step 2a: server-side LLM structured extraction ----------------
        // When the LLM is unavailable the run degrades to a no-op (ADR-068
        // §3.4.2 "Step 2 failure handling"): episodes are left untouched for
        // retry on a future run.
        let Some(llm) = llm else {
            tracing::debug!(
                episodes_scanned = result.episodes_scanned,
                "EpisodicDistiller: no server-side LLM; skipping extraction"
            );
            return Ok(result);
        };

        let extracted = extract_structures(&candidates, llm).await?;

        // ---- Step 2b: clustering -------------------------------------------
        let (knowledge_clusters, autobio_clusters) =
            cluster_candidates(&candidates, &extracted, embedding_fn, config);

        // ---- Steps 3-5: promote each cluster -------------------------------
        for cluster in knowledge_clusters {
            let eval =
                promote_knowledge_cluster(&cluster, provider, llm, embedding_fn, config).await?;
            apply_evaluation(&mut result, eval);
        }
        for cluster in autobio_clusters {
            let eval =
                promote_autobio_cluster(&cluster, provider, llm, embedding_fn, config).await?;
            apply_evaluation(&mut result, eval);
        }

        Ok(result)
    }
}

// ============================================================================
// Step 2a: server-side LLM structured extraction
// ============================================================================

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a memory consolidation extractor.
You are given dialogue episodes that were classified by the writing LLM as
<fact|preference|relation|procedure>. For each episode, output TWO independent
fields:

1. "structure": a normalized knowledge structure, or null.
   - For fact/preference/relation episodes: a triple
     {"kind": "triple", "subject": "...", "predicate": "...", "object": "..."}.
     The predicate is FREE-FORM — you are NOT restricted to any vocabulary.
     Use plain, stable phrasing (e.g. "lives_in", "prefers", "works_at").
   - For procedure episodes: {"kind": "procedure", "trigger": "...",
     "action": "..."} describing "when X happens, do Y".
   - Use null when the episode carries no structured knowledge.

2. "autobio_candidate": whether this episode contains feedback about the AGENT
   itself (the assistant), or null.
   - The subject MUST be the agent, NOT the user.
   - "User prefers concise replies" is about the user -> null.
   - "You're too verbose, give shorter answers" is about the agent -> candidate.
   - aspect: limitation | preference | relationship | history
     - limitation: feedback about the agent's capability boundary
     - preference: feedback about the agent's style/behavior (self-preference)
     - relationship: feedback about the agent's relationship with the user
     - history: significant events in the agent's trajectory (rare)
   - key_hint: a short canonical hint for the key (e.g. "verbose_response",
     "forgetfulness", "style").

Output STRICT JSON (no markdown, no prose):
[
  {
    "episode_id": <int>,
    "structure": {...} | null,
    "autobio_candidate": {"aspect": "...", "key_hint": "..."} | null
  }
]
"#;

/// Raw LLM output for one episode (Step 2a).
#[derive(Debug, Deserialize)]
struct RawExtract {
    episode_id: u64,
    #[serde(default)]
    structure: Option<RawStructure>,
    #[serde(default)]
    autobio_candidate: Option<RawAutobioCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum RawStructure {
    #[serde(rename = "triple")]
    Triple {
        subject: String,
        predicate: String,
        object: String,
    },
    #[serde(rename = "procedure")]
    Procedure {
        #[serde(alias = "trigger_condition")]
        trigger: String,
        #[serde(alias = "action_pattern")]
        action: String,
    },
}

#[derive(Debug, Deserialize)]
struct RawAutobioCandidate {
    aspect: String,
    #[serde(default)]
    key_hint: String,
}

/// Call the server-side LLM once for the whole batch, then parse the result.
async fn extract_structures(
    candidates: &[(u64, Episode)],
    llm: &dyn TripleExtractorLlm,
) -> Result<Vec<ExtractedStructure>> {
    let combined: String = candidates
        .iter()
        .map(|(id, ep)| {
            format!(
                "[Episode {}] ({}): {}",
                id,
                ep.knowledge_subtype
                    .as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("unclassified"),
                ep.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let messages = vec![
        LlmMessage {
            role: "system".to_string(),
            content: EXTRACTION_SYSTEM_PROMPT.to_string(),
        },
        LlmMessage {
            role: "user".to_string(),
            content: combined,
        },
    ];

    let response = llm
        .chat(messages)
        .await
        .map_err(|e| GrafeoError::Memory(format!("Step 2a LLM call failed: {e}")))?;

    let raws: Vec<RawExtract> = parse_json_array(&response.content)
        .map_err(|e| GrafeoError::Memory(format!("Step 2a JSON parse failed: {e}")))?
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e: serde_json::Error| {
            GrafeoError::Memory(format!("Step 2a JSON schema mismatch: {e}"))
        })?;

    let by_id: HashMap<u64, RawExtract> = raws.into_iter().map(|r| (r.episode_id, r)).collect();

    let mut out = Vec::with_capacity(candidates.len());
    for (id, _ep) in candidates {
        let extracted = match by_id.get(id) {
            Some(raw) => raw_to_extracted(*id, raw),
            // LLM dropped an episode -> defer it (ExtractionFailed).
            None => ExtractedStructure {
                episode_id: *id,
                kind: ExtractedKind::ExtractionFailed {
                    reason: "LLM response missing episode".to_string(),
                },
                autobio_candidate: None,
            },
        };
        out.push(extracted);
    }
    Ok(out)
}

fn raw_to_extracted(episode_id: u64, raw: &RawExtract) -> ExtractedStructure {
    let autobio_candidate = raw.autobio_candidate.as_ref().and_then(|ab| {
        ab.aspect
            .parse::<AutobioAspect>()
            .ok()
            .map(|aspect| AutobioCandidate {
                aspect,
                key_hint: ab.key_hint.clone(),
            })
    });

    let kind = match &raw.structure {
        Some(RawStructure::Triple {
            subject,
            predicate,
            object,
        }) => ExtractedKind::Triple {
            subject: subject.clone(),
            predicate: predicate.clone(),
            object: object.clone(),
        },
        Some(RawStructure::Procedure { trigger, action }) => ExtractedKind::Procedure {
            trigger_condition: trigger.clone(),
            action_pattern: action.clone(),
        },
        None => match autobio_candidate.as_ref() {
            Some(cand) => ExtractedKind::AutobioCandidate {
                aspect: cand.aspect,
                key_hint: cand.key_hint.clone(),
            },
            None => ExtractedKind::ExtractionFailed {
                reason: "no structured knowledge extracted".to_string(),
            },
        },
    };

    ExtractedStructure {
        episode_id,
        kind,
        autobio_candidate,
    }
}

// ============================================================================
// Step 2b: embedding-similarity clustering
// ============================================================================

/// Partition extraction results into knowledge clusters and autobio clusters.
///
/// Knowledge clustering: group by `knowledge_subtype`; within each subtype
/// bucket, merge keys whose embedding cosine similarity >=
/// `config.cluster_threshold` (single-linkage greedy, ADR-068 §3.4.2 Step 2b).
/// Without an embedding function, falls back to exact key-string equality.
///
/// Autobio clustering: group by `AutobioAspect`; History is excluded (it is
/// event-triggered, not episode-clustered — ADR-068 §3.4.2 Step 3).
fn cluster_candidates(
    candidates: &[(u64, Episode)],
    extracted: &[ExtractedStructure],
    embedding_fn: Option<&EmbeddingFn>,
    config: &DistillerConfig,
) -> (Vec<Cluster>, Vec<AutobioCluster>) {
    let mut knowledge: Vec<Cluster> = Vec::new();
    let mut autobio: Vec<AutobioCluster> = Vec::new();

    for (cand, ext) in candidates.iter().zip(extracted.iter()) {
        let member = ClusterMember {
            episode_id: cand.0,
            episode: cand.1.clone(),
            extracted: ext.clone(),
        };

        // Autobio candidates go to their aspect bucket.
        if let Some(cand) = ext.autobio_candidate.as_ref()
            && cand.aspect != AutobioAspect::History
        {
            let found = autobio
                .iter_mut()
                .find(|c| c.aspect == cand.aspect && c.key_hint == cand.key_hint);
            match found {
                Some(c) => c.members.push(member.clone()),
                None => autobio.push(AutobioCluster {
                    aspect: cand.aspect,
                    key_hint: cand.key_hint.clone(),
                    members: vec![member.clone()],
                }),
            }
        }

        // Knowledge candidates go to their subtype bucket (also capturing
        // autobio-only episodes that carry an ExtractedKind::AutobioCandidate
        // so their triples — if any — are not lost).
        let (subtype, key_text, key_embedding) = match (&ext.kind, cand.1.knowledge_subtype.clone()) {
            (
                ExtractedKind::Triple {
                    subject,
                    predicate,
                    object,
                },
                Some(st),
            ) => {
                let text = format!("{subject} {predicate} {object}");
                let emb = embedding_fn.map(|f| f(&text));
                (st.clone(), text, emb)
            }
            (ExtractedKind::Procedure { .. }, _) => {
                let text = extract_procedure_key(ext);
                (
                    KnowledgeSubType::Procedure,
                    text.clone(),
                    embedding_fn.map(|f| f(&text)),
                )
            }
            _ => continue,
        };

        // Single-linkage merge against existing clusters of the same subtype.
        let mut merged = false;
        for cluster in knowledge.iter_mut() {
            if cluster.subtype != subtype {
                continue;
            }
            let threshold = config.cluster_threshold;
            let similar = match (&cluster.key_embedding, &key_embedding) {
                (Some(a), Some(b)) => cosine_similarity(a, b) >= threshold,
                _ => cluster.key_string() == key_text,
            };
            if similar {
                cluster.members.push(member.clone());
                merged = true;
                break;
            }
        }
        if !merged {
            knowledge.push(Cluster {
                subtype,
                key_embedding,
                members: vec![member],
            });
        }
    }

    (knowledge, autobio)
}

/// Reconstruct the cluster's canonical key string for audit/fallback use.
impl Cluster {
    fn key_string(&self) -> String {
        let first = self.members.first().map(|m| &m.extracted);
        match first {
            Some(ext) => match &ext.kind {
                ExtractedKind::Triple {
                    subject,
                    predicate,
                    object,
                } => format!("{subject} {predicate} {object}"),
                ExtractedKind::Procedure {
                    trigger_condition,
                    action_pattern,
                } => format!("{trigger_condition} then {action_pattern}"),
                _ => String::new(),
            },
            None => String::new(),
        }
    }
}

fn extract_procedure_key(ext: &ExtractedStructure) -> String {
    match &ext.kind {
        ExtractedKind::Procedure {
            trigger_condition,
            action_pattern,
        } => format!("{trigger_condition} then {action_pattern}"),
        _ => String::new(),
    }
}

// ============================================================================
// Step 3: promotion strategies (per-class evidence gates)
// ============================================================================

/// Evidence threshold per knowledge subtype (ADR-068 §3.4.2 Step 3).
fn knowledge_min_evidence(config: &DistillerConfig, subtype: &KnowledgeSubType) -> usize {
    match subtype {
        KnowledgeSubType::Fact => config.fact_min_evidence,
        KnowledgeSubType::Preference => config.preference_min_evidence,
        KnowledgeSubType::Relation => config.relation_min_evidence,
        KnowledgeSubType::Procedure => config.procedure_min_evidence,
    }
}

fn knowledge_promotion_kind(subtype: &KnowledgeSubType) -> PromotionKind {
    match subtype {
        KnowledgeSubType::Fact => PromotionKind::Fact,
        KnowledgeSubType::Preference => PromotionKind::Preference,
        KnowledgeSubType::Relation => PromotionKind::Relation,
        KnowledgeSubType::Procedure => PromotionKind::Procedure,
    }
}

/// Promote a knowledge cluster (Fact/Preference/Relation/Procedure).
async fn promote_knowledge_cluster(
    cluster: &Cluster,
    provider: &dyn MemoryProvider,
    llm: &dyn TripleExtractorLlm,
    embedding_fn: Option<&EmbeddingFn>,
    config: &DistillerConfig,
) -> Result<PromotionEvaluation> {
    let ids: Vec<u64> = cluster.members.iter().map(|m| m.episode_id).collect();
    let span_days = member_span_days(&cluster.members);
    let min_evidence = knowledge_min_evidence(config, &cluster.subtype);
    let kind = knowledge_promotion_kind(&cluster.subtype);

    // Step 3: evidence gate. Insufficient evidence -> Deferred (retry later).
    if cluster.members.len() < min_evidence {
        return Ok(PromotionEvaluation {
            source_episode_ids: ids,
            promoted_kind: kind,
            promoted_node_id: None,
            llm_reasoning: String::new(),
            llm_confidence: 0.0,
            evidence_score: evidence_score(cluster.members.len(), span_days, min_evidence, None),
            decision: PromotionDecision::Deferred {
                reason: format!(
                    "evidence {} < min_evidence {}",
                    cluster.members.len(),
                    min_evidence
                ),
            },
        });
    }

    // Step 4: LLM judge.
    let judge = judge_cluster(cluster, llm, config).await?;

    // Step 5: threshold gate + write + mark consolidated.
    let decision = if judge.confidence < config.promotion_confidence_threshold {
        PromotionDecision::Skipped {
            reason: format!(
                "judge confidence {:.2} < threshold {:.2}: {}",
                judge.confidence, config.promotion_confidence_threshold, judge.reasoning
            ),
        }
    } else {
        match judge.decision {
            JudgeDecision::Promote => {
                if cluster.subtype == KnowledgeSubType::Procedure {
                    write_procedural_node(cluster, &judge, embedding_fn, span_days, provider)?;
                } else {
                    write_knowledge_node(cluster, &judge, embedding_fn, span_days, provider)?;
                }
                provider.mark_consolidated(&ids).map_err(grafeo_err)?;
                PromotionDecision::Promoted
            }
            JudgeDecision::Skip => PromotionDecision::Skipped {
                reason: judge.reasoning.clone(),
            },
            JudgeDecision::Defer => PromotionDecision::Deferred {
                reason: judge.reasoning.clone(),
            },
        }
    };

    Ok(PromotionEvaluation {
        source_episode_ids: ids,
        promoted_kind: kind,
        promoted_node_id: None,
        llm_reasoning: judge.reasoning.clone(),
        llm_confidence: judge.confidence,
        evidence_score: evidence_score(cluster.members.len(), span_days, min_evidence, None),
        decision,
    })
}

/// Promote an autobiographical cluster (limitation/preference/relationship).
async fn promote_autobio_cluster(
    cluster: &AutobioCluster,
    provider: &dyn MemoryProvider,
    llm: &dyn TripleExtractorLlm,
    embedding_fn: Option<&EmbeddingFn>,
    config: &DistillerConfig,
) -> Result<PromotionEvaluation> {
    let ids: Vec<u64> = cluster.members.iter().map(|m| m.episode_id).collect();
    let span_days = member_span_days(&cluster.members);
    let kind = match cluster.aspect {
        AutobioAspect::Limitation => PromotionKind::AutobioLimitation,
        AutobioAspect::Preference => PromotionKind::AutobioPreference,
        AutobioAspect::Relationship => PromotionKind::AutobioRelationship,
        AutobioAspect::History => PromotionKind::AutobioHistory,
    };

    // Step 3: evidence count + cross-session span gate (ADR-068 §2.1/§3.4.2).
    if cluster.members.len() < config.autobio_min_evidence
        || span_days < config.autobio_min_span_days
    {
        let reason = if cluster.members.len() < config.autobio_min_evidence {
            format!(
                "autobio evidence {} < min_evidence {}",
                cluster.members.len(),
                config.autobio_min_evidence
            )
        } else {
            format!(
                "autobio span {span_days}d < min_span {}d",
                config.autobio_min_span_days
            )
        };
        return Ok(PromotionEvaluation {
            source_episode_ids: ids,
            promoted_kind: kind,
            promoted_node_id: None,
            llm_reasoning: String::new(),
            llm_confidence: 0.0,
            evidence_score: evidence_score(
                cluster.members.len(),
                span_days,
                config.autobio_min_evidence,
                Some(config.autobio_min_span_days),
            ),
            decision: PromotionDecision::Deferred { reason },
        });
    }

    // Step 4: LLM judge (retrospective judgement).
    let judge = judge_autobio_cluster(cluster, llm, config).await?;

    // Step 5: threshold gate + write + mark consolidated.
    let decision = if judge.confidence < config.promotion_confidence_threshold {
        PromotionDecision::Skipped {
            reason: format!(
                "judge confidence {:.2} < threshold {:.2}: {}",
                judge.confidence, config.promotion_confidence_threshold, judge.reasoning
            ),
        }
    } else {
        match judge.decision {
            JudgeDecision::Promote => {
                write_autobio_node(cluster, &judge, embedding_fn, span_days, provider)?;
                provider.mark_consolidated(&ids).map_err(grafeo_err)?;
                PromotionDecision::Promoted
            }
            JudgeDecision::Skip => PromotionDecision::Skipped {
                reason: judge.reasoning.clone(),
            },
            JudgeDecision::Defer => PromotionDecision::Deferred {
                reason: judge.reasoning.clone(),
            },
        }
    };

    Ok(PromotionEvaluation {
        source_episode_ids: ids,
        promoted_kind: kind,
        promoted_node_id: None,
        llm_reasoning: judge.reasoning.clone(),
        llm_confidence: judge.confidence,
        evidence_score: evidence_score(
            cluster.members.len(),
            span_days,
            config.autobio_min_evidence,
            Some(config.autobio_min_span_days),
        ),
        decision,
    })
}

// ============================================================================
// Step 4: LLM judge
// ============================================================================

const JUDGE_SYSTEM_PROMPT: &str = r#"You are a memory consolidation judge.
Given these episodes (raw dialogue fragments classified by the LLM that
produced them), decide whether they warrant promotion to a single semantic
memory node.

Rules:
- "promote": the episodes agree on a durable, non-obvious fact/preference/
  relation/procedure worth remembering.
- "defer": possibly true but needs more evidence — retry on a future run.
- "skip": contradictory, ephemeral, or not worth promoting — reject.

Output STRICT JSON (no markdown, no prose):
{
  "decision": "promote" | "skip" | "defer",
  "confidence": 0.0-1.0,
  "reasoning": "short explanation",
  "merged_content": "canonical merged statement (required when promote)"
}
"#;

/// Raw LLM judge output (Step 4).
#[derive(Debug, Deserialize)]
struct RawJudge {
    #[serde(default)]
    decision: String,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    merged_content: Option<String>,
}

fn judge_messages(cluster_label: &str, members: &[ClusterMember]) -> Vec<LlmMessage> {
    let episodes: Vec<String> = members
        .iter()
        .map(|m| {
            format!(
                "episode_id={}, session={}, ts={}, content=\"{}\"",
                m.episode_id,
                m.episode.session_id,
                m.episode.timestamp.to_rfc3339(),
                m.episode.content
            )
        })
        .collect();
    let user = format!(
        "Target: {}\n\nEpisodes:\n{}\n",
        cluster_label,
        episodes.join("\n")
    );
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: JUDGE_SYSTEM_PROMPT.to_string(),
        },
        LlmMessage {
            role: "user".to_string(),
            content: user,
        },
    ]
}

async fn judge_cluster(
    cluster: &Cluster,
    llm: &dyn TripleExtractorLlm,
    _config: &DistillerConfig,
) -> Result<JudgeOutput> {
    let label = format!(
        "KnowledgeNode({}) key=\"{}\"",
        cluster.subtype.as_str(),
        cluster.key_string()
    );
    let response = llm
        .chat(judge_messages(&label, &cluster.members))
        .await
        .map_err(|e| GrafeoError::Memory(format!("Step 4 judge LLM call failed: {e}")))?;
    parse_judge(&response.content)
}

async fn judge_autobio_cluster(
    cluster: &AutobioCluster,
    llm: &dyn TripleExtractorLlm,
    _config: &DistillerConfig,
) -> Result<JudgeOutput> {
    let label = format!(
        "AutobiographicalNode({:?}) key=\"{}\"",
        cluster.aspect, cluster.key_hint
    );
    let response = llm
        .chat(judge_messages(&label, &cluster.members))
        .await
        .map_err(|e| GrafeoError::Memory(format!("Step 4 judge LLM call failed: {e}")))?;
    parse_judge(&response.content)
}

fn parse_judge(content: &str) -> Result<JudgeOutput> {
    let raw: RawJudge = parse_json_value(content)
        .map_err(|e| GrafeoError::Memory(format!("Step 4 judge JSON parse failed: {e}")))
        .and_then(|v| {
            serde_json::from_value(v).map_err(|e: serde_json::Error| {
                GrafeoError::Memory(format!("Step 4 judge JSON schema mismatch: {e}"))
            })
        })?;
    let decision = match raw.decision.to_lowercase().as_str() {
        "promote" => JudgeDecision::Promote,
        "skip" => JudgeDecision::Skip,
        "defer" => JudgeDecision::Defer,
        other => {
            return Err(GrafeoError::Memory(format!(
                "Step 4 judge returned unknown decision: {other}"
            )))
        }
    };
    Ok(JudgeOutput {
        decision,
        confidence: raw.confidence.unwrap_or(0.0).clamp(0.0, 1.0),
        reasoning: raw.reasoning.unwrap_or_default(),
        merged_content: raw.merged_content,
    })
}

// ============================================================================
// Step 5: node writes
// ============================================================================

/// Representative triple from a knowledge cluster (first member that has one).
fn representative_triple(cluster: &Cluster) -> Option<(String, String, String)> {
    cluster.members.iter().find_map(|m| match &m.extracted.kind {
        ExtractedKind::Triple {
            subject,
            predicate,
            object,
        } => Some((subject.clone(), predicate.clone(), object.clone())),
        _ => None,
    })
}

fn write_knowledge_node(
    cluster: &Cluster,
    judge: &JudgeOutput,
    embedding_fn: Option<&EmbeddingFn>,
    span_days: i64,
    provider: &dyn MemoryProvider,
) -> Result<()> {
    let ids: Vec<u64> = cluster.members.iter().map(|m| m.episode_id).collect();
    let (subject, predicate, object) = representative_triple(cluster).unwrap_or_else(|| {
        (
            "unknown".to_string(),
            cluster.key_string(),
            "unknown".to_string(),
        )
    });
    let merged_content = judge
        .merged_content
        .clone()
        .unwrap_or_else(|| cluster.key_string());
    let embedding = embedding_fn.map(|f| f(&merged_content));

    let node = KnowledgeNode {
        subject,
        predicate,
        object,
        sub_type: cluster.subtype.clone(),
        confidence: judge.confidence,
        source_episode_id: ids.first().copied(),
        source_episode_ids: ids.clone(),
        promotion_metadata: Some(promotion_metadata(&ids, span_days, judge)),
        embedding,
        status: NodeStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        metadata: HashMap::new(),
        privacy: PrivacyLevel::Personal,
        importance: 0.6,
    };
    provider.store_knowledge(&node).map_err(grafeo_err)?;
    Ok(())
}

fn write_procedural_node(
    cluster: &Cluster,
    judge: &JudgeOutput,
    embedding_fn: Option<&EmbeddingFn>,
    span_days: i64,
    provider: &dyn MemoryProvider,
) -> Result<()> {
    let ids: Vec<u64> = cluster.members.iter().map(|m| m.episode_id).collect();
    let (trigger, action) = cluster.members.iter().find_map(|m| match &m.extracted.kind {
        ExtractedKind::Procedure {
            trigger_condition,
            action_pattern,
        } => Some((trigger_condition.clone(), action_pattern.clone())),
        _ => None,
    }).unwrap_or_else(|| {
        (
            cluster.key_string(),
            "unknown action".to_string(),
        )
    });
    let merged_content = judge
        .merged_content
        .clone()
        .unwrap_or_else(|| cluster.key_string());
    // ADR-057 §5.1: ProceduralNode construction requires a vector. An empty
    // vector means "no vector available" for round-tripped nodes; the
    // embedding fallback is supplied when an embedding function exists.
    let embedding = embedding_fn.map(|f| f(&merged_content)).unwrap_or_default();
    let name = format!("learned_procedure_{}", sanitize_name(&trigger));

    let node = ProceduralNode {
        id: None,
        name,
        trigger_condition: trigger,
        action_pattern: action,
        success_count: 0,
        fail_count: 0,
        confidence: judge.confidence,
        activation_count: 0,
        source_skill: None,
        learned_from: "offline_consolidation".to_string(),
        embedding,
        status: NodeStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        source_episode_ids: ids.clone(),
        promotion_metadata: Some(promotion_metadata(&ids, span_days, judge)),
        metadata: HashMap::new(),
    };
    provider.store_procedural(&node).map_err(grafeo_err)?;
    Ok(())
}

fn write_autobio_node(
    cluster: &AutobioCluster,
    judge: &JudgeOutput,
    embedding_fn: Option<&EmbeddingFn>,
    span_days: i64,
    provider: &dyn MemoryProvider,
) -> Result<()> {
    let ids: Vec<u64> = cluster.members.iter().map(|m| m.episode_id).collect();
    let category = match cluster.aspect {
        AutobioAspect::Limitation => AutobioCategory::Limitation,
        AutobioAspect::Preference => AutobioCategory::Preference,
        AutobioAspect::Relationship => AutobioCategory::Relationship,
        AutobioAspect::History => AutobioCategory::History,
    };
    let merged_content = judge
        .merged_content
        .clone()
        .unwrap_or_else(|| {
            cluster
                .members
                .first()
                .map(|m| m.episode.content.clone())
                .unwrap_or_default()
        });
    let embedding = embedding_fn.map(|f| f(&merged_content));

    let node = AutobiographicalNode {
        id: None,
        category,
        key: cluster.key_hint.clone(),
        value: merged_content,
        confidence: judge.confidence,
        source_episode_id: ids.first().copied(),
        source_episode_ids: ids.clone(),
        promotion_metadata: Some(promotion_metadata(&ids, span_days, judge)),
        embedding,
        status: NodeStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        source: "offline_consolidation".to_string(),
        metadata: HashMap::new(),
    };
    provider.store_autobiographical(&node).map_err(grafeo_err)?;
    Ok(())
}

fn promotion_metadata(
    ids: &[u64],
    span_days: i64,
    judge: &JudgeOutput,
) -> PromotionMetadata {
    PromotionMetadata {
        promoted_at: Utc::now(),
        promoted_by: "episodic_distiller".to_string(),
        evidence_episode_ids: ids.to_vec(),
        evidence_span_days: span_days,
        llm_judge_confidence: judge.confidence,
        llm_judge_reasoning: judge.reasoning.clone(),
    }
}

// ============================================================================
// Step 6: result aggregation
// ============================================================================

/// Fold one evaluation into the result counters (ADR-068 Step 6).
fn apply_evaluation(result: &mut DistillerResult, eval: PromotionEvaluation) {
    let promoted = matches!(eval.decision, PromotionDecision::Promoted);
    match eval.promoted_kind {
        PromotionKind::Fact => result.facts_promoted += promoted as usize,
        PromotionKind::Preference => result.preferences_promoted += promoted as usize,
        PromotionKind::Relation => result.relations_promoted += promoted as usize,
        PromotionKind::Procedure => result.procedures_promoted += promoted as usize,
        _ => result.autobio_promoted += promoted as usize,
    }
    if promoted {
        result.episodes_marked_consolidated += eval.source_episode_ids.len();
    }
    result.promotion_evaluations.push(eval);
}

// ============================================================================
// Helpers
// ============================================================================

/// Time span in days between the oldest and newest evidence episode.
fn member_span_days(members: &[ClusterMember]) -> i64 {
    let mut min_ts: Option<DateTime<Utc>> = None;
    let mut max_ts: Option<DateTime<Utc>> = None;
    for m in members {
        let ts = m.episode.timestamp;
        min_ts = Some(min_ts.map_or(ts, |cur| cur.min(ts)));
        max_ts = Some(max_ts.map_or(ts, |cur| cur.max(ts)));
    }
    match (min_ts, max_ts) {
        (Some(min), Some(max)) => (max - min).num_days(),
        _ => 0,
    }
}

/// Evidence strength [0.0, 1.0]: count component + optional span component.
fn evidence_score(
    count: usize,
    span_days: i64,
    min_evidence: usize,
    min_span_days: Option<i64>,
) -> f32 {
    let count_score = (count as f32 / (2.0 * min_evidence.max(1) as f32)).min(1.0);
    match min_span_days {
        Some(ms) if ms > 0 => {
            let span_score = (span_days as f32 / ms as f32).min(1.0);
            (count_score + span_score) / 2.0
        }
        _ => count_score,
    }
}

/// Cosine similarity between two vectors (0.0 when either is empty).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
}

/// Sanitize a string for use inside a node name.
fn sanitize_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_');
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.chars().take(32).collect()
    }
}

/// Parse a top-level JSON array from an LLM response (tolerates markdown).
fn parse_json_array(content: &str) -> std::result::Result<Vec<serde_json::Value>, String> {
    let value = parse_json_value(content)?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| "expected a JSON array".to_string())
}

/// Extract and parse a JSON value from an LLM response (tolerates markdown
/// fences and surrounding prose).
fn parse_json_value(content: &str) -> std::result::Result<serde_json::Value, String> {
    let trimmed = content.trim();
    let candidate = if trimmed.starts_with("```") {
        trimmed
            .lines()
            .filter(|l| !l.starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    } else {
        trimmed.to_string()
    };
    serde_json::from_str(&candidate).map_err(|e| format!("invalid JSON: {e}"))
}

fn grafeo_err(e: acowork_core::error::AcoworkError) -> GrafeoError {
    GrafeoError::Memory(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use acowork_memory::consolidation::LlmResponse;
    use acowork_memory::types::{AutobioCategory, MemoryQuery, SearchResult};
    use acowork_memory::{
        DecayConfig, DecayScanResult, GeneralizationConfig, GeneralizationResult,
        MemoryQualityConfig, OfflineConsolidationConfig, OfflineConsolidationResult,
        PurgeResult, SchedulerConfig, StoreHealth, StoreStats,
    };
    use async_trait::async_trait;
    use std::time::Duration;

    // ========================================================================
    // Test doubles
    // ========================================================================

    /// A fake LLM with programmable response sequences.
    #[derive(Clone)]
    struct MockLlm {
        responses: Arc<Mutex<Vec<String>>>,
    }

    impl MockLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
            }
        }

        fn pop(&self) -> String {
            self.responses.lock().unwrap().remove(0)
        }
    }

    #[async_trait::async_trait]
    impl TripleExtractorLlm for MockLlm {
        async fn chat(
            &self,
            _messages: Vec<LlmMessage>,
        ) -> std::result::Result<LlmResponse, String> {
            let resp = self.pop();
            Ok(LlmResponse {
                content: resp,
                usage_tokens: None,
            })
        }
    }

    /// A fake embedding function returning vectors that are mutually
    /// orthogonal (cosine 0.0) — two "same" keys share a vector so they merge.
    fn embedding_same() -> EmbeddingFn {
        Arc::new(|_text: &str| vec![1.0, 0.0])
    }

    /// Minimal in-memory MemoryProvider for distiller tests.
    #[derive(Default)]
    struct TestProvider {
        episodes: Mutex<Vec<(u64, Episode)>>,
        knowledge_nodes: Mutex<Vec<KnowledgeNode>>,
        procedural_nodes: Mutex<Vec<ProceduralNode>>,
        autobio_nodes: Mutex<Vec<AutobiographicalNode>>,
        next_id: Mutex<u64>,
    }

    impl TestProvider {
        fn add_episode(&self, ep: Episode) -> u64 {
            let mut next = self.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            drop(next);
            self.episodes.lock().unwrap().push((id, ep));
            id
        }
    }

    fn mk_episode(
        session: &str,
        content: &str,
        subtype: KnowledgeSubType,
        ts: DateTime<Utc>,
    ) -> Episode {
        Episode {
            session_id: session.to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: content.to_string(),
            embedding: None,
            timestamp: ts,
            consolidated: false,
            metadata: HashMap::new(),
            importance: 0.5,
            knowledge_subtype: Some(subtype),
        }
    }

    #[async_trait]
    impl MemoryProvider for TestProvider {
        fn store_episode(&self, _episode: &Episode) -> acowork_core::error::Result<()> {
            Ok(())
        }
        fn search_episodes(&self, _q: &MemoryQuery) -> acowork_core::error::Result<Vec<SearchResult>> {
            Ok(vec![])
        }
        fn mark_consolidated(&self, ids: &[u64]) -> acowork_core::error::Result<()> {
            let mut eps = self.episodes.lock().unwrap();
            for (id, ep) in eps.iter_mut() {
                if ids.contains(id) {
                    ep.consolidated = true;
                }
            }
            Ok(())
        }
        fn cleanup_episodes(&self, _o: Duration) -> acowork_core::error::Result<u64> {
            Ok(0)
        }
        fn get_episodes(&self, _s: Option<&str>, _l: usize) -> acowork_core::error::Result<Vec<Episode>> {
            Ok(self.episodes.lock().unwrap().iter().map(|(_, e)| e.clone()).collect())
        }
        fn get_episodes_by_subtype(
            &self,
            _subtype: Option<KnowledgeSubType>,
            _limit: usize,
        ) -> acowork_core::error::Result<Vec<(u64, Episode)>> {
            Ok(self
                .episodes
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, e)| !e.consolidated)
                .map(|(id, e)| (*id, e.clone()))
                .collect())
        }
        fn store_knowledge(&self, node: &KnowledgeNode) -> acowork_core::error::Result<()> {
            self.knowledge_nodes.lock().unwrap().push(node.clone());
            Ok(())
        }
        fn store_procedural(&self, node: &ProceduralNode) -> acowork_core::error::Result<()> {
            self.procedural_nodes.lock().unwrap().push(node.clone());
            Ok(())
        }
        fn store_autobiographical(&self, node: &AutobiographicalNode) -> acowork_core::error::Result<()> {
            self.autobio_nodes.lock().unwrap().push(node.clone());
            Ok(())
        }
        fn hybrid_search(&self, _q: &MemoryQuery) -> acowork_core::error::Result<Vec<SearchResult>> {
            Ok(vec![])
        }
        fn graph_expand(&self, _s: &[SearchResult], _h: u8) -> acowork_core::error::Result<Vec<SearchResult>> {
            Ok(vec![])
        }
        fn run_decay_scan(&self, _c: &DecayConfig) -> acowork_core::error::Result<DecayScanResult> {
            unreachable!()
        }
        fn reactivate_node(&self, _id: u64) -> acowork_core::error::Result<()> {
            unreachable!()
        }
        fn purge_expired(&self, _m: Duration) -> acowork_core::error::Result<PurgeResult> {
            unreachable!()
        }
        fn health_check(&self) -> acowork_core::error::Result<StoreHealth> {
            unreachable!()
        }
        fn stats(&self) -> acowork_core::error::Result<StoreStats> {
            unreachable!()
        }
        fn close(&self) -> acowork_core::error::Result<()> {
            Ok(())
        }
        #[allow(clippy::too_many_arguments)]
        fn hybrid_search_full(
            &self,
            _l: &str,
            _q: &str,
            _e: &[f32],
            _k: usize,
            _tw: f64,
            _vw: f64,
            _ms: Option<f32>,
        ) -> acowork_core::error::Result<Vec<(u64, f64)>> {
            Ok(vec![])
        }
        fn text_search_with_filter(
            &self,
            _l: &str,
            _f: &str,
            _q: &str,
            _k: usize,
            _ms: Option<f32>,
        ) -> acowork_core::error::Result<Vec<(u64, f64)>> {
            Ok(vec![])
        }
        fn process_memory_store(
            &self,
            _i: &acowork_memory::consolidation::MemoryStoreInput,
        ) -> acowork_core::error::Result<
            Option<acowork_memory::consolidation::MemoryStoreResult>,
        > {
            unreachable!()
        }
        fn should_trigger_confirmation(&self) -> acowork_core::error::Result<bool> {
            unreachable!()
        }
        fn generate_confirmation_hint(&self) -> acowork_core::error::Result<Option<String>> {
            unreachable!()
        }
        async fn run_generalization(
            &self,
            _s: Option<&str>,
            _e: &EmbeddingFn,
            _c: &GeneralizationConfig,
        ) -> acowork_core::error::Result<GeneralizationResult> {
            unreachable!()
        }
        fn compress_history_nodes(&self, _k: usize) -> acowork_core::error::Result<usize> {
            unreachable!()
        }
        fn get_all_procedural_nodes(&self) -> acowork_core::error::Result<Vec<ProceduralNode>> {
            Ok(self.procedural_nodes.lock().unwrap().clone())
        }
        fn find_procedural_by_trigger(
            &self,
            _t: &str,
            _l: usize,
        ) -> acowork_core::error::Result<Vec<ProceduralNode>> {
            Ok(vec![])
        }
        fn get_procedural(&self, _id: u64) -> acowork_core::error::Result<Option<ProceduralNode>> {
            unreachable!()
        }
        fn update_procedural(&self, _n: &ProceduralNode) -> acowork_core::error::Result<()> {
            unreachable!()
        }
        fn find_autobiographical_by_key(
            &self,
            _k: &str,
        ) -> acowork_core::error::Result<Option<AutobiographicalNode>> {
            unreachable!()
        }
        fn find_autobiographical_by_category(
            &self,
            _c: AutobioCategory,
        ) -> acowork_core::error::Result<Vec<AutobiographicalNode>> {
            unreachable!()
        }
        fn update_autobiographical(&self, _n: &AutobiographicalNode) -> acowork_core::error::Result<()> {
            unreachable!()
        }
        fn create_memory_edge(
            &self,
            _f: u64,
            _t: u64,
            _e: &str,
            _p: Vec<(&str, String)>,
        ) -> acowork_core::error::Result<()> {
            unreachable!()
        }
        fn graph_expand_seeded(
            &self,
            _s: &[(u64, f64)],
            _h: &str,
        ) -> acowork_core::error::Result<Vec<(u64, f64, String)>> {
            unreachable!()
        }
        fn get_node_content(&self, _id: u64) -> acowork_core::error::Result<Option<String>> {
            unreachable!()
        }
        fn get_node_session_id(&self, _id: u64) -> acowork_core::error::Result<Option<String>> {
            unreachable!()
        }
        fn get_node_status(&self, _id: u64) -> acowork_core::error::Result<Option<NodeStatus>> {
            unreachable!()
        }
        fn get_node_created_at(&self, _id: u64) -> acowork_core::error::Result<Option<DateTime<Utc>>> {
            unreachable!()
        }
        fn apply_quality_config(&self, _c: &MemoryQualityConfig) -> acowork_core::error::Result<()> {
            Ok(())
        }
        fn apply_pagerank_boost(
            &self,
            _s: &mut [(u64, f64)],
            _w: f64,
        ) -> acowork_core::error::Result<()> {
            Ok(())
        }
        fn start_consolidation(&self, _c: &SchedulerConfig) -> acowork_core::error::Result<()> {
            unreachable!()
        }
        fn stop_consolidation(&self) {}
        async fn notify_consolidation_active(&self) {}
        fn get_pending_consolidation_count(&self) -> acowork_core::error::Result<usize> {
            unreachable!()
        }
        async fn run_offline_consolidation(
            &self,
            _c: &OfflineConsolidationConfig,
            _l: Option<&dyn TripleExtractorLlm>,
            _e: Option<EmbeddingFn>,
            _g: Option<&GeneralizationConfig>,
        ) -> acowork_core::error::Result<OfflineConsolidationResult> {
            unreachable!()
        }
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn default_config() -> DistillerConfig {
        DistillerConfig::default()
    }

    fn extraction_response(items: &[(u64, &str, &str, &str)]) -> String {
        // items: (episode_id, kind, a, b)
        let arr: Vec<String> = items
            .iter()
            .map(|(id, kind, a, b)| match *kind {
                "triple" => format!(
                    "{{\"episode_id\": {id}, \"structure\": {{\"kind\": \"triple\", \"subject\": \"user\", \"predicate\": \"{a}\", \"object\": \"{b}\"}}, \"autobio_candidate\": null}}"
                ),
                "procedure" => format!(
                    "{{\"episode_id\": {id}, \"structure\": {{\"kind\": \"procedure\", \"trigger\": \"{a}\", \"action\": \"{b}\"}}, \"autobio_candidate\": null}}"
                ),
                "autobio" => format!(
                    "{{\"episode_id\": {id}, \"structure\": null, \"autobio_candidate\": {{\"aspect\": \"{a}\", \"key_hint\": \"{b}\"}}}}"
                ),
                "failed" => format!(
                    "{{\"episode_id\": {id}, \"structure\": null, \"autobio_candidate\": null}}"
                ),
                _ => String::new(),
            })
            .collect();
        format!("[{}]", arr.join(","))
    }

    fn judge_response(decision: &str, confidence: f32, content: &str) -> String {
        format!(
            "{{\"decision\": \"{decision}\", \"confidence\": {confidence}, \"reasoning\": \"r\", \"merged_content\": \"{content}\"}}"
        )
    }

    // ========================================================================
    // Tests
    // ========================================================================

    #[tokio::test]
    async fn test_d1_promotes_facts_with_evidence() {
        let provider = TestProvider::default();
        let now = Utc::now();
        for i in 0..5 {
            provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            ));
        }
        let ids: Vec<u64> = provider
            .episodes
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect();

        // extraction: all triples same predicate
        let mut extract_items = vec![];
        for id in &ids {
            extract_items.push((*id, "triple", "lives_in", "Shanghai"));
        }
        let mut judge_resps = vec![];
        for _ in &ids {
            judge_resps.push(judge_response("promote", 0.95, "user lives_in Shanghai"));
        }

        let llm = MockLlm::new(vec![
            extraction_response(&extract_items),
            judge_resps[0].clone(),
        ]);

        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();

        assert_eq!(result.facts_promoted, 1);
        assert_eq!(result.episodes_marked_consolidated, 5);
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 1);
        // D4: source_episode_ids written with all N episodes
        let node = &provider.knowledge_nodes.lock().unwrap()[0];
        assert_eq!(node.source_episode_ids.len(), 5);
        // D6: PromotionMetadata fields complete
        let meta = node.promotion_metadata.as_ref().unwrap();
        assert_eq!(meta.promoted_by, "episodic_distiller");
        assert_eq!(meta.evidence_episode_ids.len(), 5);
        assert!(meta.llm_judge_confidence > 0.0);
        assert!(!meta.llm_judge_reasoning.is_empty());
        assert!(meta.evidence_span_days >= 0);
        // D5: episodes marked consolidated
        let eps = provider.episodes.lock().unwrap();
        assert!(eps.iter().all(|(_, e)| e.consolidated));
    }

    #[tokio::test]
    async fn test_d2_insufficient_evidence_defers() {
        let provider = TestProvider::default();
        let now = Utc::now();
        // Only 1 episode for a Fact (min_evidence = 2) -> Deferred.
        provider.add_episode(mk_episode(
            "sess-1",
            "User lives in Shanghai",
            KnowledgeSubType::Fact,
            now,
        ));
        let id = provider.episodes.lock().unwrap()[0].0;

        let llm = MockLlm::new(vec![extraction_response(&[(id, "triple", "lives_in", "Shanghai")])]);

        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();

        assert_eq!(result.facts_promoted, 0);
        assert_eq!(result.episodes_marked_consolidated, 0);
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 0);
        assert_eq!(result.promotion_evaluations.len(), 1);
        assert!(matches!(
            result.promotion_evaluations[0].decision,
            PromotionDecision::Deferred { .. }
        ));
        // Episodes remain unconsolidated for retry.
        assert!(!provider.episodes.lock().unwrap()[0].1.consolidated);
    }

    #[tokio::test]
    async fn test_d3_low_confidence_skips() {
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..3 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User prefers concise replies",
                KnowledgeSubType::Preference,
                now - chrono::Duration::days(i as i64),
            )));
        }

        // Judge returns confidence 0.7 < 0.85 threshold -> Skip.
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "triple", "prefers", "concise_replies"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.7, "user prefers concise"),
        ]);

        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();

        assert_eq!(result.preferences_promoted, 0);
        assert!(matches!(
            result.promotion_evaluations[0].decision,
            PromotionDecision::Skipped { .. }
        ));
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_d5_consolidated_flag_persists_after_promotion() {
        // Already covered inside test_d1; this asserts the provider behavior
        // is applied for a Preference promotion too.
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..3 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User prefers dark mode",
                KnowledgeSubType::Preference,
                now - chrono::Duration::days(i as i64),
            )));
        }
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "triple", "prefers", "dark_mode"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.9, "user prefers dark_mode"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.preferences_promoted, 1);
        assert!(provider
            .episodes
            .lock()
            .unwrap()
            .iter()
            .all(|(_, e)| e.consolidated));
    }

    #[tokio::test]
    async fn test_d7_autobio_span_gate() {
        let provider = TestProvider::default();
        let now = Utc::now();
        // 3 limitation episodes but within 1 day span < 14 days -> Deferred.
        let mut ids = vec![];
        for i in 0..3 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "You're too verbose, give shorter answers",
                KnowledgeSubType::Preference,
                now - chrono::Duration::hours(i as i64),
            )));
        }
        let llm = MockLlm::new(vec![extraction_response(
            &ids.iter()
                .map(|id| (*id, "autobio", "limitation", "verbose_response"))
                .collect::<Vec<_>>(),
        )]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.autobio_promoted, 0);
        assert_eq!(provider.autobio_nodes.lock().unwrap().len(), 0);
        assert!(matches!(
            result.promotion_evaluations[0].decision,
            PromotionDecision::Deferred { .. }
        ));
    }

    #[tokio::test]
    async fn test_d7b_autobio_promotes_when_span_satisfied() {
        let provider = TestProvider::default();
        let now = Utc::now();
        // 3 limitation episodes spread over 14+ days -> Promoted.
        let mut ids = vec![];
        for i in 0..3 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "You're too verbose, give shorter answers",
                KnowledgeSubType::Preference,
                now - chrono::Duration::days((i * 7) as i64),
            )));
        }
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "autobio", "limitation", "verbose_response"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.95, "agent should be concise"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.autobio_promoted, 1);
        assert_eq!(provider.autobio_nodes.lock().unwrap().len(), 1);
        let node = &provider.autobio_nodes.lock().unwrap()[0];
        assert_eq!(node.category, AutobioCategory::Limitation);
        assert_eq!(node.key, "verbose_response");
        assert_eq!(node.source_episode_ids.len(), 3);
        assert!(node.promotion_metadata.is_some());
    }

    #[tokio::test]
    async fn test_d9_procedure_promotes_with_five_episodes() {
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..5 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "When asking for weather, fetch via http_request",
                KnowledgeSubType::Procedure,
                now - chrono::Duration::days(i as i64),
            )));
        }
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "procedure", "user asks for weather", "fetch via http_request"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.92, "when weather asked, fetch via http_request"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.procedures_promoted, 1);
        assert_eq!(provider.procedural_nodes.lock().unwrap().len(), 1);
        let node = &provider.procedural_nodes.lock().unwrap()[0];
        assert_eq!(node.source_episode_ids.len(), 5);
        assert_eq!(node.trigger_condition, "user asks for weather");
    }

    #[tokio::test]
    async fn test_d11_extractor_free_predicate_generation() {
        // The extraction prompt does not constrain predicates; the parser
        // accepts arbitrary predicate strings (D11).
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..2 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            )));
        }
        // Predicate is arbitrary: "random_custom_predicate_xyz".
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "triple", "random_custom_predicate_xyz", "Shanghai"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.9, "user random_custom_predicate_xyz Shanghai"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.facts_promoted, 1);
        let node = &provider.knowledge_nodes.lock().unwrap()[0];
        assert_eq!(node.predicate, "random_custom_predicate_xyz");
    }

    #[tokio::test]
    async fn test_d12_extractor_detects_autobio_limitation() {
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..3 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "You are too verbose",
                KnowledgeSubType::Preference,
                now - chrono::Duration::days((i * 7) as i64),
            )));
        }
        // Step 2a returns AutobioCandidate{limitation, verbose_response}.
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "autobio", "limitation", "verbose_response"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.95, "agent verbose; be concise"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.autobio_promoted, 1);
        let node = &provider.autobio_nodes.lock().unwrap()[0];
        assert_eq!(node.category, AutobioCategory::Limitation);
        assert_eq!(node.key, "verbose_response");
    }

    #[tokio::test]
    async fn test_d13_extractor_rejects_non_autobio() {
        // Episodes about the user produce autobio_candidate: null and are
        // treated purely as knowledge triples (D13).
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..2 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            )));
        }
        // Note: the "triple" response template has autobio_candidate: null.
        let llm = MockLlm::new(vec![
            extraction_response(
                &ids.iter()
                    .map(|id| (*id, "triple", "lives_in", "Shanghai"))
                    .collect::<Vec<_>>(),
            ),
            judge_response("promote", 0.9, "user lives_in Shanghai"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.facts_promoted, 1);
        assert_eq!(result.autobio_promoted, 0);
        assert_eq!(provider.autobio_nodes.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_d14_llm_timeout_defers_batch() {
        // A failing LLM (simulated timeout) must not panic: the run propagates
        // the error (the caller decides retry policy). Episodes remain
        // unconsolidated because no write path executed.
        let provider = TestProvider::default();
        let now = Utc::now();
        for i in 0..3 {
            provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            ));
        }

        struct FailingLlm;
        #[async_trait::async_trait]
        impl TripleExtractorLlm for FailingLlm {
            async fn chat(
                &self,
                _messages: Vec<LlmMessage>,
            ) -> std::result::Result<LlmResponse, String> {
                Err("simulated timeout".to_string())
            }
        }

        let distiller = DefaultEpisodicDistiller;
        let err = distiller
            .run(&provider, Some(&FailingLlm), None, &default_config())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Step 2a"));
        // Episodes untouched.
        assert!(provider
            .episodes
            .lock()
            .unwrap()
            .iter()
            .all(|(_, e)| !e.consolidated));
    }

    #[tokio::test]
    async fn test_d15_single_episode_failure_isolated() {
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..2 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            )));
        }
        // One episode fails extraction ("failed"), the other extracts fine.
        // The failed one must not participate in clustering; the good one
        // alone has insufficient evidence -> Deferred (no panic, no node).
        let llm = MockLlm::new(vec![extraction_response(&[
            (ids[0], "failed", "", ""),
            (ids[1], "triple", "lives_in", "Shanghai"),
        ])]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.facts_promoted, 0);
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 0);
        // The failed episode remains unconsolidated (retry next run).
        assert!(provider
            .episodes
            .lock()
            .unwrap()
            .iter()
            .all(|(_, e)| !e.consolidated));
    }

    #[tokio::test]
    async fn test_d17_embedding_clustering_unifies_synonyms() {
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..5 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            )));
        }
        // Different predicates, but the embedding function maps all keys to
        // the SAME vector -> one cluster (cosine 1.0 >= 0.85).
        let items: Vec<(u64, &str, &str, &str)> = vec![
            (ids[0], "triple", "lives_in", "Shanghai"),
            (ids[1], "triple", "is_located_in", "Shanghai"),
            (ids[2], "triple", "home_city", "Shanghai"),
            (ids[3], "triple", "based_in", "Shanghai"),
            (ids[4], "triple", "resides_in", "Shanghai"),
        ];
        let llm = MockLlm::new(vec![
            extraction_response(&items),
            judge_response("promote", 0.95, "user lives_in Shanghai"),
        ]);
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), Some(&embedding_same()), &default_config())
            .await
            .unwrap();
        assert_eq!(result.facts_promoted, 1);
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 1);
        assert_eq!(provider.knowledge_nodes.lock().unwrap()[0].source_episode_ids.len(), 5);
    }

    #[tokio::test]
    async fn test_d18_embedding_below_threshold_not_merged() {
        let provider = TestProvider::default();
        let now = Utc::now();
        let mut ids = vec![];
        for i in 0..4 {
            ids.push(provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            )));
        }
        // Even-numbered ids map to vector A, odd to vector B (cosine 0.0).
        // Two clusters of 2 members each. Each has 2 >= fact_min_evidence,
        // so both are judged -> 2 promotions.
        let items: Vec<(u64, &str, &str, &str)> = vec![
            (ids[0], "triple", "lives_in", "Shanghai"),
            (ids[1], "triple", "works_in", "Beijing"),
            (ids[2], "triple", "lives_in", "Shanghai"),
            (ids[3], "triple", "works_in", "Beijing"),
        ];
        let llm = MockLlm::new(vec![
            extraction_response(&items),
            judge_response("promote", 0.9, "a"),
            judge_response("promote", 0.9, "b"),
        ]);
        // Embedding axis alternates by position: even idx -> [1,0], odd -> [0,1].
        let emb = Arc::new(|_text: &str| vec![1.0, 0.0]);
        // Use a custom fn that maps "lives_in" vs "works_in" differently.
        let emb2: EmbeddingFn = Arc::new(move |text: &str| {
            if text.contains("lives_in") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            }
        });
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, Some(&llm), Some(&emb2), &default_config())
            .await
            .unwrap();
        assert_eq!(result.facts_promoted, 2);
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 2);
        let _ = emb;
    }

    #[tokio::test]
    async fn test_no_llm_is_noop() {
        // ADR-068: without a server-side LLM the run degrades to a no-op.
        let provider = TestProvider::default();
        let now = Utc::now();
        for i in 0..3 {
            provider.add_episode(mk_episode(
                &format!("sess-{i}"),
                "User lives in Shanghai",
                KnowledgeSubType::Fact,
                now - chrono::Duration::days(i as i64),
            ));
        }
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, None, None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.episodes_scanned, 3);
        assert_eq!(result.facts_promoted, 0);
        assert_eq!(provider.knowledge_nodes.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_empty_batch_is_noop() {
        let provider = TestProvider::default();
        let distiller = DefaultEpisodicDistiller;
        let result = distiller
            .run(&provider, None, None, &default_config())
            .await
            .unwrap();
        assert_eq!(result.episodes_scanned, 0);
        assert_eq!(result.promotion_evaluations.len(), 0);
    }

    #[test]
    fn test_cosine_similarity() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn test_parse_json_tolerates_markdown() {
        let v = parse_json_value("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
        let arr = parse_json_array("[{\"a\":1}]").unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_sanitize_name() {
        assert_eq!(sanitize_name("When user asks weather"), "when_user_asks_weather");
        assert_eq!(sanitize_name("!!!"), "unknown");
    }
}
