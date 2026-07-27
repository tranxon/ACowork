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
            match crate::conversation::ConversationSession::resume(
                work_dir_path,
                &latest_id,
                committed_lines.clone(),
            ) {
                Ok((conv, config_rx, state_rx)) => {
                    // ADR-043: Spawn config + state change relays so
                    // persisted-session updates flow through MQTT to the
                    // Desktop.
                    if let Some(chunk_tx) = ctx.chunk_tx.clone() {
                        crate::startup::subsystems::spawn_config_change_relay(
                            config_rx,
                            chunk_tx.clone(),
                            conv.clone(),
                            latest_id.clone(),
                        );
                        crate::startup::subsystems::spawn_state_change_relay(
                            state_rx,
                            chunk_tx,
                            conv.clone(),
                            latest_id.clone(),
                        );
                    }
                    Some(conv)
                }
                Err(e) => {
                    let msg = format!(
                        "session_persistence_unavailable: cannot resume session \"{}\" in {:?}: {}",
                        latest_id, conversations_dir, e
                    );
                    eprintln!("⚠️ {}; runtime will run with an in-memory session (history will be lost on restart)", msg);
                    if let Ok(mut reasons) = ctx.degraded_reasons.write() {
                        reasons.push(msg);
                    }
                    None
                }
            }
        } else {
            let new_id = crate::conversation::generate_session_id();
            tracing::info!(session_id = %new_id, "Creating new conversation session");
            let (conv, config_rx, state_rx) = crate::conversation::ConversationSession::new(
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
            )?;
            // Drop the meta-change receiver — new-session creation writes
            // the initial meta file, but no subscriber cares about it yet
            // (the session_task spawns its own relay after this returns to
            // the SessionManager path). Leaving the receiver unhandled
            // here would silently buffer `UnboundedSender`s and prevent
            // Drop-time notifications from being observed. Dropping is
            // the explicit "I don't care" signal.
            drop(config_rx);
            drop(state_rx);
            Some(conv)
        };

    // ADR-033 (MQTT path): pre-compute the set of provider IDs that have a
    // decrypted API key in the cached `AvailableProviders` payload. This is
    // the MQTT-path counterpart of the legacy gRPC `provider_key_vault`.
    let available_cache_provider_ids: std::collections::HashSet<String> =
        if let Some(ref cache) = ctx.available_cache {
            let cache_read = cache.read().await;
            cache_read
                .providers
                .as_ref()
                .map(|available| {
                    available
                        .providers
                        .iter()
                        .filter(|p| !p.api_key.is_empty())
                        .map(|p| p.id.clone())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            std::collections::HashSet::new()
        };

    // Validate the resumed session's model/provider against the cached provider list.
    if let Some(ref conv) = conversation_session {
        let session_model = conv.model();
        let session_provider = conv.provider();

        let is_valid = match (&session_model, &session_provider) {
            (Some(model), Some(provider_id)) => {
                let in_cache =
                    ctx.provider_config
                        .as_ref()
                        .map(|c| &c.providers)
                        .is_none_or(|providers| {
                            providers
                                .iter()
                                .any(|p| p.id == *provider_id && p.models.iter().any(|m| m.id == *model))
                        });
                if !in_cache {
                    false
                } else {
                    // Two independent sources can authorize a provider_id:
                    //   1. Phase A default (`gateway_current_provider_id`) —
                    //      the first provider with a key in the MQTT cache.
                    //   2. MQTT `available_cache` (ADR-033) — keys
                    //      are shipped inline per `mqtt.md` §3.1.1.
                    ctx.gateway_current_provider_id.as_deref() == Some(provider_id.as_str())
                        || available_cache_provider_ids.contains(provider_id)
                }
            }
            _ => true,
        };

        if !is_valid {
            let fallback_model = ctx
                .provider_config
                .as_ref()
                .map(|c| &c.providers)
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

    // ADR-046: build the attachment blob store up front so we can both
    // inject it into `AgentCore` (so SessionTask's image-derivation
    // pipeline can read image bytes — see `attachment_to_image.rs`) and
    // publish it to the HTTP server's `attachment_slot` (so
    // `POST /sessions/{sid}/files` and `GET /files/{document_id}` work).
    // `RuntimeAttachmentService` only needs the boot-time `work_dir`;
    // no async resource dependency, so the construction is sync.
    let attach_svc: Arc<dyn crate::usecases::AttachmentService> = Arc::new(
        crate::usecases::RuntimeAttachmentService::new(work_dir_path.to_path_buf()),
    );

    // Inject global provider list, key vault, and memory into AgentCore.
    if let Some(c) = Arc::get_mut(&mut core) {
        // Inject the per-agent compatibility cache so `build_provider_for`
        // can wire it into rebuilt providers (429-retry / session-resume).
        c.compat_cache = ctx.compat_cache.take();

        // Provider list is loaded from agent_provider.json (persisted by the
        // MQTT handler on receiving acowork/global/providers).
        let providers_for_init = ctx.provider_config.as_ref().map(|c| &c.providers);

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
                "Populated AgentCore.global_provider_list from resource cache"
            );
        }

        // ADR-033: MQTT available_cache is the only source of provider API keys
        // (gRPC hello_config path removed per ADR-040). The available cache
        // is updated by the Runtime MQTT event loop whenever the Gateway
        // publishes `acowork/global/providers` (retained). Each `ProviderRef`
        // carries the decrypted API key inline (see mqtt.md §3.1.1) so the
        // model_switch path can look up the key by provider_id.
        if let Some(ref cache) = ctx.available_cache {
            let cache_read = cache.read().await;
            if let Some(available) = cache_read.providers.as_ref() {
                let mut vault = c.provider_key_vault.write().unwrap();
                vault.clear();
                for p in &available.providers {
                    if !p.api_key.is_empty() {
                        vault.insert(p.id.clone(), p.api_key.clone());
                    }
                }
                tracing::info!(
                    version = available.version,
                    provider_count = available.providers.len(),
                    key_count = vault.len(),
                    "Populated AgentCore provider_key_vault from MQTT available cache"
                );
            }
        }

        // Replace AgentCore's internally-created search vault/list with the
        // shared Arcs from Phase A (same instances held by WebSearchEngine).
        // This ensures SessionManager::update_search_config writes are visible
        // to the search engine without re-registration.
        c.search_key_vault = ctx.search_key_vault.clone();
        c.search_provider_list = ctx.search_provider_list.clone();

        // Populate search key vault and provider list from MQTT available
        // cache (mirrors the provider_key_vault pattern above). The
        // AvailableSearches payload carries SearchRef entries with inline
        // decrypted API keys.
        if let Some(ref cache) = ctx.available_cache {
            let cache_read = cache.read().await;
            if let Some(searches) = cache_read.searches.as_ref() {
                let search_refs = &searches.providers;
                let list_items = crate::mqtt::client::map_search_refs_to_list_items(search_refs);
                let key_entries = crate::mqtt::client::extract_search_keys(search_refs);

                {
                    let mut vault = c.search_key_vault.write().unwrap();
                    vault.clear();
                    for entry in &key_entries {
                        vault.insert(entry.provider_id.clone(), entry.api_key.clone());
                    }
                }
                {
                    let mut list = c.search_provider_list.write().unwrap();
                    *list = list_items.clone();
                }
                tracing::info!(
                    version = searches.version,
                    provider_count = list_items.len(),
                    key_count = key_entries.len(),
                    "Populated AgentCore search_key_vault + search_provider_list from MQTT available cache"
                );
            }
        }

        c.memory_session = Some(ctx.memory_session.clone());
        c.embedding_provider = ctx.emb_provider.clone();
        c.init_memory_store(work_dir_path);

        // ADR-033 (Phase 2): Publish the late-bound Grafeo store to the
        // shared handle consumed by the Runtime HTTP server. The HTTP
        // server was already started in Phase A holding this same Arc,
        // so handlers that fired while `init_memory_store` was still
        // running will get a stable "no store" response until this
        // mutation completes. After this point every memory_* endpoint
        // (/memory/nodes, /memory/stats, /memory/nodes/{nid},
        // /memory/consolidate) sees the live store.
        let published_store = c.memory_store().cloned();
        if let Some(store) = published_store {
            match ctx.memory_store_shared.write() {
                Ok(mut slot) => *slot = Some(store),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to publish memory_store_shared — HTTP memory endpoints will report 'no store'"
                    );
                }
            }
        } else {
            tracing::warn!("init_memory_store completed but no store was assigned — HTTP memory endpoints will report 'no store'");
        }

        // ADR-040: Publish GrafeoMemoryAdapter to the HTTP server's
        // late-bind slot so memory handlers can use the trait path.
        {
            let adapter: Arc<dyn crate::usecases::MemoryQueryService> =
                Arc::new(crate::usecases::memory_query_impl::GrafeoMemoryAdapter::new(
                    ctx.memory_store_shared.clone(),
                    ctx.embed_dim_shared.clone(),
                ));
            let mut slot = ctx.memory_query_slot.lock().await;
            *slot = Some(adapter);
        }

        // ADR-040: Publish workspace query + mutation services. Workspace
        // services only need the agent's `work_dir` (resolved at boot);
        // they don't depend on async resources like memory_store, so we
        // can wire them immediately. Both services must be published
        // BEFORE the workspace HTTP handlers can serve a real response.
        {
            let query_svc: Arc<dyn crate::usecases::WorkspaceQueryService> = Arc::new(
                crate::usecases::RuntimeWorkspaceQueryService::new(
                    work_dir_path.to_path_buf(),
                    ctx.agent_id.clone(),
                ),
            );
            let mut slot = ctx.workspace_query_slot.lock().await;
            *slot = Some(query_svc);
        }
        {
            let mutation_svc: Arc<dyn crate::usecases::WorkspaceMutationService> = Arc::new(
                crate::usecases::RuntimeWorkspaceMutationService::new(work_dir_path.to_path_buf()),
            );
            let mut slot = ctx.workspace_mutation_slot.lock().await;
            *slot = Some(mutation_svc);
        }

        // ADR-040 follow-up: Publish Tools-panel persistence service.
        // The four `/agents/{id}/mcp-servers` and `/agents/{id}/search-config`
        // HTTP handlers route through this trait. The service holds only
        // the agent's `work_dir` (sync, no async dependency), so it can
        // be wired immediately alongside the workspace services.
        {
            let tools_svc: Arc<dyn crate::usecases::AgentToolsService> = Arc::new(
                crate::usecases::RuntimeAgentToolsService::new(work_dir_path.to_path_buf()),
            );
            let mut slot = ctx.agent_tools_slot.lock().await;
            *slot = Some(tools_svc);
        }

        // ADR-040 follow-up: Publish per-agent runtime config service.
        // The `PUT /agents/{id}/config` handler routes through this trait.
        // Same sync-work_dir pattern as `agent_tools_slot` above.
        {
            let config_svc: Arc<dyn crate::usecases::AgentConfigService> = Arc::new(
                crate::usecases::RuntimeAgentConfigService::new(work_dir_path.to_path_buf()),
            );
            let mut slot = ctx.agent_config_slot.lock().await;
            *slot = Some(config_svc);
        }

        // ADR-046: Inject attachment blob store into AgentCore so that
        // `SessionTask` can derive multimodal image parts from
        // `AttachedItem::ImageUpload` (see
        // `crate::agent::attachment_to_image::derive_image_parts`).
        // Without this injection, the slot stays `None` and every
        // image-attached chat turn degrades to plain text. The HTTP
        // server sees the same Arc via `ctx.attachment_slot`, published
        // outside this `if let` block below.
        c.set_attachment_service(attach_svc.clone());

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

        // ADR-046: Publish attachment blob store to the HTTP server's
        // late-bind slot. The same `Arc` instance is shared with
        // `AgentCore::attachment_service` (set above), so any upload
        // posted to `POST /sessions/{sid}/files` becomes visible to
        // `derive_image_parts` in the same session turn. Must run
        // AFTER the `if let Some(c)` block because the slot write
        // borrows `ctx` for `.lock().await`, which conflicts with the
        // mutable `Arc::get_mut` borrow.
        let mut slot = ctx.attachment_slot.lock().await;
        *slot = Some(attach_svc);
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

    // ADR-040: Construct usecase services before `core` is moved
    // into SessionManager.
    {
        let core_clone = Arc::clone(&core);
        if let Ok(mut slot) = ctx.agent_core_shared.write() {
            *slot = Some(core_clone.clone());
        } else {
            tracing::warn!("Failed to publish AgentCore to shared slot");
        }

        // Build the AgentTokenService (thin wrapper around AgentCore).
        let agent_token: Arc<dyn crate::usecases::AgentTokenService> =
            Arc::new(crate::usecases::RuntimeAgentTokenService::new(core_clone));

        // Build the SessionMetadataService.
        let session_metadata: Arc<dyn crate::usecases::SessionMetadataService> =
            Arc::new(crate::usecases::RuntimeSessionMetadataService::new(
                work_dir_path.to_path_buf(),
                agent_token,
                ctx.session_snapshots.clone(),
                ctx.latest_session.clone(),
            ));

        // Publish to the HTTP server's late-bind slot.
        {
            let mut slot = ctx.session_metadata_slot.lock().await;
            *slot = Some(session_metadata);
        }
    }

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
        let sid = session_manager.create_session().await?;
        // Register the new session as "latest" so that /latest-session
        // (used by the frontend's selectAgent → loadLatestSession flow)
        // returns it immediately on the next query, even before any
        // message is sent.  Without this, a freshly started agent with
        // zero sessions on disk returns found:false / 404 and the
        // frontend ChatPanel stays blank.
        session_manager.set_latest_session(sid.clone(), None);
        sid
    };
    tracing::info!(initial_session_id = %initial_session_id, "Initial session created");

    // Workspace context and prompt file are applied inside
    // create_session_with_id_and_conversation (single source of truth from
    // the shared WorkspaceResolver). No follow-up step required here.

    // ADR-040: gRPC hello_config path removed. Runtime config overrides
    // from agent_config.json are always applied (all paths are Gateway mode).
    // agent_config.json defaults are resolved & persisted above
    // (Step 9 — AgentCore init). Apply any remaining overrides
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

    Ok(SessionBootContext {
        session_manager,
        committed_lines,
    })
}
