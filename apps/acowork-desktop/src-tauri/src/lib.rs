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
use tauri::{Emitter, Listener, Manager};

// ── System-sleep detection (Windows / macOS / Linux) ────────────────────────
//
// The frontend's old time-gap heuristic (heartbeat + visibilitychange) could
// not distinguish "window minimised for N seconds" from "system slept for N
// seconds", causing false `location.reload()` triggers on normal minimise →
// restore cycles.
//
// Instead, the Rust backend samples two monotonic clocks on each `Focused(true)`
// event:
//
//   • **biased**   — includes time spent in sleep / suspend
//   • **unbiased** — excludes time spent in sleep / suspend
//
// If `biased_delta - unbiased_delta > threshold`, the system was genuinely
// asleep — not merely backgrounded.
//
// Platform implementations:
//   • Windows: `GetTickCount64()` (biased) vs `QueryUnbiasedInterruptTime()` (unbiased)
//   • macOS:   `clock_gettime(CLOCK_MONOTONIC_RAW)` (biased) vs `CLOCK_UPTIME_RAW` (unbiased)
//   • Linux:   `clock_gettime(CLOCK_BOOTTIME)` (biased) vs `CLOCK_MONOTONIC` (unbiased)

mod power {
    use std::sync::atomic::{AtomicU64, Ordering};

    static LAST_BIASED_MS: AtomicU64 = AtomicU64::new(0);
    static LAST_UNBIASED_MS: AtomicU64 = AtomicU64::new(0);

    /// Minimum *actual* sleep duration (ms) to trigger recovery.
    /// We measure real sleep, not wall-clock gaps, so even a few seconds
    /// is significant.  5 s filters timer imprecision.
    const SLEEP_THRESHOLD_MS: u64 = 5_000;

    // ── Windows FFI ──────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe extern "system" {
        fn GetTickCount64() -> u64;
        fn QueryUnbiasedInterruptTime(unbiased_time: *mut u64) -> i32;
    }

    // ── Platform-specific clock sampling ─────────────────────────────────

    /// Returns `(biased_ms, unbiased_ms)` where biased includes sleep time
    /// and unbiased excludes it.  Returns `None` on API failure or on
    /// unsupported platforms.
    fn sample() -> Option<(u64, u64)> {
        #[cfg(target_os = "windows")]
        {
            unsafe {
                let biased_ms = GetTickCount64();
                let mut unbiased_100ns: u64 = 0;
                if QueryUnbiasedInterruptTime(&mut unbiased_100ns) == 0 {
                    return None; // API failure
                }
                Some((biased_ms, unbiased_100ns / 10_000))
            }
        }

        #[cfg(target_os = "macos")]
        {
            // CLOCK_MONOTONIC_RAW advances during sleep; CLOCK_UPTIME_RAW does not.
            sample_unix(libc::CLOCK_MONOTONIC_RAW, libc::CLOCK_UPTIME_RAW)
        }

        #[cfg(target_os = "linux")]
        {
            // CLOCK_BOOTTIME includes suspend time; CLOCK_MONOTONIC does not.
            sample_unix(libc::CLOCK_BOOTTIME, libc::CLOCK_MONOTONIC)
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            None // Unsupported platform — no sleep detection
        }
    }

    /// Shared `clock_gettime` helper for macOS and Linux.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sample_unix(
        biased_clk: libc::clockid_t,
        unbiased_clk: libc::clockid_t,
    ) -> Option<(u64, u64)> {
        fn read_clk(clk: libc::clockid_t) -> Option<u64> {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if unsafe { libc::clock_gettime(clk, &mut ts) } != 0 {
                return None;
            }
            Some((ts.tv_sec as u64) * 1_000 + (ts.tv_nsec as u64) / 1_000_000)
        }
        Some((read_clk(biased_clk)?, read_clk(unbiased_clk)?))
    }

    /// Returns `true` if the system was genuinely asleep (not merely
    /// minimised or backgrounded) since the last call.
    pub fn check_resume() -> bool {
        let Some((biased_ms, unbiased_ms)) = sample() else {
            return false; // API failure or unsupported platform
        };

        let prev_biased = LAST_BIASED_MS.swap(biased_ms, Ordering::Relaxed);
        let prev_unbiased = LAST_UNBIASED_MS.swap(unbiased_ms, Ordering::Relaxed);

        if prev_biased == 0 || prev_unbiased == 0 {
            return false; // First call — seed values, don't trigger
        }

        let biased_delta = biased_ms.saturating_sub(prev_biased);
        let unbiased_delta = unbiased_ms.saturating_sub(prev_unbiased);
        let sleep_ms = biased_delta.saturating_sub(unbiased_delta);

        if sleep_ms > SLEEP_THRESHOLD_MS {
            tracing::info!(
                sleep_ms,
                biased_delta_ms = biased_delta,
                unbiased_delta_ms = unbiased_delta,
                "Actual system sleep detected — emitting system-resume"
            );
            true
        } else {
            false
        }
    }
}

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

            // Spawn async task for automatic sleep detection.
            // Polls biased/unbiased monotonic clocks every 2 s via the
            // existing tokio runtime — no dedicated thread needed.  The
            // webview reloads within ~2 s of waking, without user interaction.
            // The `Focused(true)` handler below provides immediate detection
            // when the user clicks the window.  Both paths share the same
            // atomic state in `power::check_resume`, so the event fires
            // exactly once per sleep cycle.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    interval.tick().await;
                    if power::check_resume() {
                        let _ = app_handle.emit("system-resume", ());
                    }
                }
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
                // ── System-resume detection ────────────────────────────────
                // Compares biased vs unbiased monotonic clocks to detect
                // *actual* system sleep — not merely window minimise/restore.
                // See the `power` module docs above for platform details.
                tauri::WindowEvent::Focused(true) => {
                    if power::check_resume() {
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
