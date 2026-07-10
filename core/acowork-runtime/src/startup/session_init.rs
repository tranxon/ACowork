//! Phase B: per-session initialization (Gateway mode).
//!
//! Covers Steps 2.5 + session-related parts of Step 9:
//!   - Initialize conversation session (resume latest or create new)
//!   - Validate provider/model against cached provider list
//!   - Build AgentCore + inject provider list, key vault, memory
//!   - Create SessionManager + set resolver + set default workspace
//!   - Create initial session with resumed/created conversation
//!   - SessionState is assembled with the persisted provider; no extra
//!     ModelSwitch message is needed at startup
//!   - Apply workspace context and runtime overrides

use std::sync::Arc;

use acowork_core::timeout_config::constants;
use crate::agent::agent_core::AgentCore;
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session::{SessionManager, SessionManagerConfig};
use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext, build_session_manager_config};

/// Cached result of the background scan that finds the most recently active
/// session — `(session_id, title)`. Held in an `Arc<RwLock<…>>` so the
/// SessionManager can read it on the main thread after construction.
type LatestSessionScan = Arc<std::sync::RwLock<Option<(String, Option<String>)>>>;

/// Phase B: assemble per-session state on the main thread (Gateway mode).
///
/// This must complete synchronously (no spawn) before Phase C so that
/// the full `SessionState` is ready before `SessionTask` starts.
pub(crate) async fn phase_b_init_session(
    ctx: &mut AgentBootContext,
    config: &RuntimeConfig,
) -> Result<SessionBootContext> {
    let _span = tracing::info_span!("startup_phase_b").entered();

    let work_dir_path = std::path::Path::new(&config.work_dir);

    // ── Step 2.5: Initialize conversation session ───────────────────
    let conversations_dir = work_dir_path.join("conversations");
    std::fs::create_dir_all(&conversations_dir)?;

    // ADR-022: Shared counter updated by writer thread after each disk write.
    let committed_lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // ADR-024: load agent_config.json early so max_sessions override is available.
    let mut agent_cfg = crate::agent_config::load_agent_config(work_dir_path)
        .unwrap_or_default()
        .unwrap_or_default();

    let conversation_session =
        if let Some(latest_id) = crate::conversation::find_latest_session(&conversations_dir) {
            tracing::info!(session_id = %latest_id, "Resuming latest conversation session");
            Some(crate::conversation::ConversationSession::resume(
                work_dir_path,
                &latest_id,
                committed_lines.clone(),
            )?)
        } else {
            let new_id = crate::conversation::generate_session_id();
            tracing::info!(session_id = %new_id, "Creating new conversation session");
            Some(crate::conversation::ConversationSession::new(
                work_dir_path,
                &new_id,
                crate::conversation::SessionConfig {
                    agent_id: config.agent_id.clone(),
                    workspace_id: None,
                    model: None,
                    provider: None,
                },
                agent_cfg.max_sessions.unwrap_or(config.max_sessions),
                committed_lines.clone(),
            )?)
        };

    // Validate the resumed session's model/provider against the cached provider list.
    if let Some(ref conv) = conversation_session {
        let session_model = conv.model();
        let session_provider = conv.provider();

        let is_valid = match (&session_model, &session_provider) {
            (Some(model), Some(provider_id)) => {
                let in_cache =
                    ctx.resource_cache
                        .providers
                        .as_ref()
                        .is_none_or(|providers| {
                            providers
                                .iter()
                                .any(|p| p.id == *provider_id && p.models.iter().any(|m| m.id == *model))
                        });
                if !in_cache {
                    false
                } else {
                    ctx.gateway_current_provider_id.as_deref() == Some(provider_id.as_str())
                        || ctx.hello_config.as_ref().is_some_and(|cfg| {
                            cfg.provider_key_vault
                                .iter()
                                .any(|k| k.provider_id == *provider_id)
                        })
                }
            }
            _ => true,
        };

        if !is_valid {
            let fallback_model = ctx
                .resource_cache
                .providers
                .as_ref()
                .and_then(|p| p.first())
                .and_then(|p| p.models.first())
                .map(|m| m.id.clone());

            if let Some(ref fallback) = fallback_model {
                tracing::warn!(
                    session_id = %conv.session_id(),
                    invalid_model = ?session_model,
                    invalid_provider = ?session_provider,
                    fallback = %fallback,
                    "Session model/provider invalid, falling back"
                );
                conv.update_model_provider(fallback, None);
            }
        }
    }

    // Spawn background session scan.
    // The result (latest session by last_active_at) is stored in a shared
    // Arc so SessionManager can read it after construction, avoiding a
    // duplicate full scan when the frontend calls fetchLatestSession.
    let latest_session_scan: LatestSessionScan = Arc::new(std::sync::RwLock::new(None));
    let latest_session_scan_clone = latest_session_scan.clone();
    let conversations_dir_clone = conversations_dir.clone();
    let _session_scan_handle = tokio::spawn(async move {
        let handle = crate::conversation::scan_sessions_async(conversations_dir_clone, None, None);
        let (sessions, _, _agent_totals) =
            handle.await.unwrap_or((Vec::new(), 0, (0, 0)));
        if let Some(s) = sessions.first() {
            *latest_session_scan_clone.write().unwrap() =
                Some((s.session_id.clone(), s.title.clone()));
        }
        tracing::info!(count = sessions.len(), "Background session scan complete");
    });

    // ── Step 9 (Gateway mode): Build AgentCore ───────────────────────
    let provider = ctx.provider.clone();
    let active_tools = ctx.active_tools.clone();

    let mut core = Arc::new(AgentCore::new(
        config.clone(),
        ctx.loaded.manifest.clone(),
        provider,
        active_tools,
    ));

    // Inject global provider list, key vault, and memory into AgentCore.
    if let Some(c) = Arc::get_mut(&mut core) {
        let providers_for_init: Option<&Vec<acowork_core::protocol::ProviderListItem>> =
            ctx.hello_config
                .as_ref()
                .and_then(|cfg| cfg.provider_list.as_ref())
                .or(ctx.resource_cache.providers.as_ref());

        if let Some(providers) = providers_for_init {
            for p in providers {
                c.provider_compact_models
                    .insert(p.id.clone(), p.compact_model.clone());
            }
            {
                let mut list = c.global_provider_list.write().unwrap();
                *list = providers.clone();
            }
            tracing::info!(
                provider_count = providers.len(),
                compact_count = c.provider_compact_models.len(),
                "Populated AgentCore.global_provider_list from hello_config / resource cache"
            );
        }

        if let Some(ref cfg) = ctx.hello_config {
            c.provider_list_version = cfg.provider_list_version;
            let mut vault = c.provider_key_vault.write().unwrap();
            vault.clear();
            for entry in &cfg.provider_key_vault {
                vault.insert(entry.provider_id.clone(), entry.api_key.clone());
            }
            tracing::info!(
                version = c.provider_list_version,
                key_count = vault.len(),
                "Populated AgentCore provider_key_vault from hello_config"
            );
        }

        c.memory_session = Some(ctx.memory_session.clone());
        c.embedding_provider = ctx.emb_provider.clone();
        c.init_memory_store(work_dir_path);

        // ── Resolve & persist agent_config.json defaults ─────────────
        //
        // agent_config.json is the single source of truth for the
        // frontend setup panel.  Resolve any field still None through
        // its fallback chain and persist the concrete value so the
        // frontend always sees a value instead of "empty = use default".
        //
        // Fields intentionally NOT auto-resolved:
        //   system_prompt_override — None = "use compiled manifest prompt"
        //   max_output_tokens      — None = "use each model's native limit"
        //   avatar / builtin_avatar — resolved via resolve_effective_avatar();
        //     seeded from manifest on first start (package author's default),
        //     then left alone (user may explicitly clear to fallback).
        {
            let mut updated = agent_cfg.clone();
            let mut dirty = false;

            // ── First-start avatar seeding ──────────────────────────
            let is_first_start = !work_dir_path
                .join("config")
                .join("agent_config.json")
                .exists();
            if is_first_start {
                if updated.avatar.is_none() {
                    updated.avatar = ctx.loaded.manifest.avatar.clone();
                    dirty = true;
                }
                if updated.builtin_avatar.is_none() {
                    updated.builtin_avatar = ctx.loaded.manifest.builtin_avatar.clone();
                    dirty = true;
                }
            }

            // ── context_window: manifest.llm.context_window → 200K ──
            // Note: manifest may set Some(0) meaning "no limit" (use
            // model's full window).  We preserve that intent.
            if updated.context_window.is_none() {
                let manifest_cw = ctx.loaded.manifest.llm.context_window;
                updated.context_window = Some(
                    manifest_cw.unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW),
                );
                dirty = true;
            }
            c.context_window_override = updated.context_window;

            // ── temperature: manifest.llm.temperature → 0.3 ─────────
            if updated.temperature.is_none() {
                let manifest_temp = ctx.loaded.manifest.llm.temperature;
                updated.temperature = Some(
                    manifest_temp.unwrap_or(crate::config::DEFAULT_TEMPERATURE),
                );
                dirty = true;
            }
            c.temperature_override = updated.temperature;

            // ── max_iterations: RuntimeConfig.max_iterations (200) ──
            if updated.max_iterations.is_none() {
                updated.max_iterations = Some(config.max_iterations);
                dirty = true;
            }

            // ── max_sessions: RuntimeConfig.max_sessions (1000) ─────
            if updated.max_sessions.is_none() {
                updated.max_sessions = Some(config.max_sessions);
                dirty = true;
            }

            // ── shell_approval_threshold: config default ("medium") ─
            if updated.shell_approval_threshold.is_none() {
                updated.shell_approval_threshold =
                    Some(config.shell_approval_threshold.clone());
                dirty = true;
            }

            // ── approval_timeout_secs: core constant (300) ──────────
            if updated.approval_timeout_secs.is_none() {
                updated.approval_timeout_secs = Some(constants::APPROVAL.as_secs());
                dirty = true;
            }
            c.approval_timeout_secs = updated.approval_timeout_secs;

            if dirty {
                if let Err(e) =
                    crate::agent_config::save_agent_config(work_dir_path, &updated)
                {
                    tracing::warn!(
                        error = %e,
                        "Failed to persist resolved defaults to agent_config.json",
                    );
                } else {
                    tracing::info!(
                        "Resolved and persisted agent_config.json defaults",
                    );
                }
            }
        }
    }

    // Reload agent_cfg from disk to pick up resolved defaults that were
    // persisted inside the AgentCore init block above. The outer variable
    // must be current so the has_overrides / apply_runtime_config_override
    // section below sees every resolved field.
    agent_cfg = crate::agent_config::load_agent_config(work_dir_path)
        .unwrap_or_default()
        .unwrap_or_default();

    // ── Step 9: Create SessionManager ───────────────────────────────
    let session_manager_config: SessionManagerConfig = build_session_manager_config(ctx, config);
    let mut session_manager = SessionManager::new(core, session_manager_config);

    session_manager.set_resolver(ctx.workspace_resolver.clone());

    if let Some(ws_id) = ctx
        .workspace_resolver
        .read()
        .unwrap()
        .last_active_workspace_id()
    {
        let ws_id_owned = ws_id.to_owned();
        session_manager.set_default_workspace_id(&ws_id_owned);
        tracing::info!(
            default_workspace_id = %ws_id_owned,
            "SessionManager: initialized default workspace from last_active"
        );
    }

    // Seed the latest session from the background scan (if it has completed).
    // If the scan hasn't finished yet, latest_session() returns None and the
    // frontend will retry — the scan result is written atomically so a
    // subsequent call will see it.
    if let Some((session_id, title)) = latest_session_scan.read().unwrap().clone() {
        session_manager.set_latest_session(session_id, title);
        tracing::info!("SessionManager: seeded latest session from startup scan");
    }

    // ── Step 9 (cont.): Create initial session ───────────────────────
    let initial_session_id = if let Some(conv) = conversation_session {
        // ADR-028: merge the resumed session's persisted token totals into
        // the AgentCore counters so the live context_usage WebSocket push
        // doesn't report agent_total < session_total after a process restart.
        if let Some(t) = conv.tokens() {
            session_manager.core().merge_token_totals((Some(t.total_input), Some(t.total_output)));
        }
        let sid = conv.session_id().to_string();
        session_manager
            .create_session_with_id_and_conversation(sid.clone(), Some(conv), Some(committed_lines.clone()))
            .await?;
        sid
    } else {
        session_manager.create_session().await?
    };
    tracing::info!(initial_session_id = %initial_session_id, "Initial session created");

    // Workspace context and prompt file are applied inside
    // create_session_with_id_and_conversation (single source of truth from
    // the shared WorkspaceResolver). No follow-up step required here.

    if ctx.hello_config.is_some() {
        // agent_config.json defaults are resolved & persisted above
        // (Step 9 — AgentCore init).  Apply any remaining overrides
        // from the loaded config to the session manager so new
        // sessions pick them up via runtime_overrides.

        let has_overrides = agent_cfg.max_output_tokens.is_some()
            || agent_cfg.max_iterations.is_some()
            || agent_cfg.temperature.is_some()
            || agent_cfg.context_window.is_some()
            || agent_cfg.system_prompt_override.is_some()
            || agent_cfg.shell_approval_threshold.is_some()
            || agent_cfg.approval_timeout_secs.is_some()
            // ADR-032: include the tool-result compression N in the override
            // detection so a user-set value in agent_config.json is applied
            // to the SessionManager override cache at boot.
            || agent_cfg.tool_result_keep_recent_n.is_some();
        if has_overrides {
            tracing::info!(
                max_output_tokens = ?agent_cfg.max_output_tokens,
                max_iterations = ?agent_cfg.max_iterations,
                temperature = ?agent_cfg.temperature,
                "Applying runtime config overrides from workspace agent_config.json"
            );
            session_manager
                .apply_runtime_config_override(&RuntimeConfigOverrides::from(&agent_cfg));
        }
    }

    Ok(SessionBootContext {
        initial_session_id,
        session_manager,
        committed_lines,
    })
}
