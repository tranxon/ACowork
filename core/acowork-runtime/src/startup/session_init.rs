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

use crate::agent::agent_core::AgentCore;
use crate::agent::session::{SessionManager, SessionManagerConfig};
use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext, build_session_manager_config};

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

    let conversation_session =
        if let Some(latest_id) = crate::conversation::find_latest_session(&conversations_dir) {
            tracing::info!(session_id = %latest_id, "Resuming latest conversation session");
            Some(crate::conversation::ConversationSession::resume(
                work_dir_path,
                &latest_id,
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

    // Spawn background session scan (fire-and-forget).
    let conversations_dir_clone = conversations_dir.clone();
    let _session_scan_handle = tokio::spawn(async move {
        let handle = crate::conversation::scan_sessions_async(conversations_dir_clone, None, None);
        let (sessions, _) = handle.await.unwrap_or((Vec::new(), 0));
        tracing::info!(count = sessions.len(), "Background session scan complete");
    });

    // ── Step 9 (Gateway mode): Build AgentCore ───────────────────────
    let provider = ctx.provider.clone();
    let active_tools = ctx.active_tools.clone();
    let chunk_tx = ctx.chunk_tx.clone();

    let mut core = Arc::new(AgentCore::new(
        config.clone(),
        ctx.loaded.manifest.clone(),
        provider,
        active_tools,
        chunk_tx,
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
        c.embedding_provider = Some(ctx.emb_provider.clone());
        c.init_memory_store(work_dir_path);
    }

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

    // ── Step 9 (cont.): Create initial session ───────────────────────
    let initial_session_id = if let Some(conv) = conversation_session {
        let sid = conv.session_id().to_string();
        session_manager
            .create_session_with_id_and_conversation(sid.clone(), Some(conv))
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
        let agent_cfg = crate::agent_config::load_agent_config(work_dir_path)
            .unwrap_or_default()
            .unwrap_or_default();

        // Seed avatar fields from manifest.toml on first start so the
        // effective avatar resolves to the package author's default.
        let is_first_start = !work_dir_path
            .join("config")
            .join("agent_config.json")
            .exists();
        if is_first_start {
            let mut seeded = agent_cfg.clone();
            seeded.avatar = ctx.loaded.manifest.avatar.clone();
            seeded.builtin_avatar = ctx.loaded.manifest.builtin_avatar.clone();
            let _ = crate::agent_config::save_agent_config(work_dir_path, &seeded);
        }

        let has_overrides = agent_cfg.max_output_tokens.is_some()
            || agent_cfg.max_iterations.is_some()
            || agent_cfg.temperature.is_some()
            || agent_cfg.system_prompt_override.is_some()
            || agent_cfg.shell_approval_threshold.is_some();
        if has_overrides {
            tracing::info!(
                max_output_tokens = ?agent_cfg.max_output_tokens,
                max_iterations = ?agent_cfg.max_iterations,
                temperature = ?agent_cfg.temperature,
                "Applying runtime config overrides from workspace agent_config.json"
            );
            session_manager.apply_runtime_config_override(
                agent_cfg.max_output_tokens,
                agent_cfg.max_iterations,
                agent_cfg.temperature,
                agent_cfg.system_prompt_override.clone(),
                agent_cfg.shell_approval_threshold.clone(),
            );
        }
    }

    Ok(SessionBootContext {
        initial_session_id,
        session_manager,
    })
}
