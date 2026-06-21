//! AgentCowork Desktop App — Tauri v2 backend
//!
//! This is the library entry point for the Tauri application.
//! It sets up the Tauri builder with all plugins, commands, and tray.
//!
//! ## Gateway boot flow
//!
//! The local Gateway is **NOT** spawned in the setup hook anymore —
//! that was the source of a long-standing bug where Rust unconditionally
//! spawned a child process on the hardcoded default URL, ignoring the
//! frontend's "remote gateway" setting.
//!
//! The new flow is:
//! 1. Setup hook only wires window/tray/single-instance plugins. No spawn.
//! 2. Frontend (`SplashScreen` init) reads its persisted `settingsStore`,
//!    calls `set_gateway_config(mode, url)` to push config into Rust.
//! 3. If mode = local, frontend then calls `init_local_gateway` which
//!    spawns the child Gateway on `defaults::GATEWAY_HTTP_URL` and waits
//!    for `/health`.
//! 4. If mode = remote, frontend skips spawn and just polls `/health`
//!    on the user-configured URL.
//! 5. After the gateway is reachable, frontend calls `ensure_system_agent`
//!    to auto-install the bundled System Agent if not already present.

mod commands;
mod gateway_client;
mod state;
mod tray;

use state::AppState;
use std::sync::atomic::{AtomicI64, Ordering};
use tauri::{Emitter, Listener, Manager};

/// Minimum seconds of inactivity before a focus-regained event is treated as
/// a potential system-resume (sleep/hibernate wake).  Shorter gaps are normal
/// window switching and are ignored by the frontend recovery logic.
const RESUME_GAP_SECS: i64 = 30;

/// Unix-epoch timestamp (seconds) of the last `Focused(true)` event.
/// Shared across all invocations of `on_window_event`.
static LAST_FOCUS_TS: AtomicI64 = AtomicI64::new(0);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Focus the main window when a second instance is launched
            let _ = app
                .get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::agent::list_agents,
            commands::agent::get_agent_detail,
            commands::agent::install_agent,
            commands::agent::install_bundled_agent,
            commands::agent::uninstall_agent,
            commands::agent::start_agent,
            commands::agent::stop_agent,
            commands::agent::restart_agent_in_debug,
            commands::agent::clone_agent,
            commands::agent::update_agent_manifest_avatar,
            commands::agent::upload_agent_file,
            commands::chat::send_message,
            commands::chat::upload_document,
            commands::vault::list_keys,
            commands::vault::add_key,
            commands::vault::remove_key,
            commands::vault::update_key,
            commands::vault::list_search_keys,
            commands::vault::add_search_key,
            commands::vault::remove_search_key,
            commands::vault::update_search_key,
            commands::publish::prepare_publish,
            commands::publish::build_publish,
            commands::publish::export_package,
            commands::create::create_agent,
            commands::gateway::set_gateway_config,
            commands::gateway::get_gateway_config,
            commands::gateway::init_local_gateway,
            commands::gateway::start_local_gateway,
            commands::gateway::stop_local_gateway,
            commands::gateway::get_local_gateway_status,
            commands::gateway::ensure_system_agent,
        ])
        .setup(|app| {
            tray::setup(app)?;

            // Show main window when frontend signals splash screen is rendered.
            // Window starts hidden (visible: false in tauri.conf.json) to prevent
            // white/transparent flash before React mounts the splash screen.
            let main_window = app.get_webview_window("main").expect("no main window");
            app.listen("splash-ready", move |_| {
                let _ = main_window.show();
            });

            // NOTE: The local Gateway is no longer spawned here. The frontend
            // is the source of truth for gateway configuration (mode + URL,
            // persisted in its settingsStore). On startup it pushes that into
            // Rust via `set_gateway_config`, then calls `init_local_gateway`
            // if mode == local. See module-level docs above.

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                // ── System-resume detection (backup for frontend heartbeat) ───
                // When the window regains focus after a long gap (sleep/hibernate),
                // emit "system-resume" so the frontend can reload the webview
                // even if its JS heartbeat was suspended and hasn't ticked yet.
                tauri::WindowEvent::Focused(true) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let prev = LAST_FOCUS_TS.swap(now, Ordering::Relaxed);
                    if prev > 0 && (now - prev) > RESUME_GAP_SECS {
                        tracing::info!(
                            gap_secs = now - prev,
                            "Window focused after long gap — emitting system-resume"
                        );
                        let _ = window.emit("system-resume", ());
                    }
                }

                // ── Hide to tray instead of closing ──────────────────────────
                // Only intercept close when window is visible and focused.
                // This prevents interference with system tray menu on Windows.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    match window.is_visible() {
                        Ok(true) => {
                            tracing::debug!("Intercepting close request, hiding to tray");
                            window.hide().unwrap();
                            api.prevent_close();
                        }
                        Ok(false) => {
                            tracing::debug!("Window not visible, allowing close to proceed");
                            // Don't intercept - let it close (for Quit menu)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to check window visibility: {}", e);
                            // Safe default: allow close
                        }
                    }
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
