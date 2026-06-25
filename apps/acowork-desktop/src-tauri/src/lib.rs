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
use tauri::Manager;

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
                "Actual system sleep detected — triggering native webview reload"
            );
            true
        } else {
            false
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
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
            commands::agent::upload_user_avatar_file,
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

            // ── macOS vibrancy ────────────────────────────────────────────
            // Apply NSVisualEffectView vibrancy for frosted glass look.
            // Native rounded corners are provided by decorations:true.
            // titleBarStyle:"Overlay" makes the title bar transparent so
            // the vibrancy shows through uniformly across the title bar,
            // navigation bar, and status bar.
            //
            // `UnderWindowBackground` (macOS 10.14+) provides frosted-glass
            // translucency that shows desktop content behind the window.
            #[cfg(target_os = "macos")]
            {
                use tauri::utils::config::WindowEffectsConfig;
                use tauri::window::EffectState;

                let main_window = app.get_webview_window("main").expect("no main window");
                let effects = WindowEffectsConfig {
                    effects: vec![tauri::window::Effect::UnderWindowBackground],
                    state: Some(EffectState::Active),
                    radius: None,
                    color: None,
                };
                let _ = main_window.set_effects(effects);
            }

            // ── Disable native decorations on non-macOS ─────────────────
            // On Windows/Linux, titleBarStyle:"Overlay" is ignored but
            // decorations:true from tauri.conf.json still renders a native
            // OS title bar with wrong colors.  We disable it here so the
            // frontend's custom TitleBar component is the only one visible.
            #[cfg(not(target_os = "macos"))]
            {
                let main_window = app.get_webview_window("main").expect("no main window");
                let _ = main_window.set_decorations(false);
            }

            // Spawn async task for automatic sleep detection.
            // Polls biased/unbiased monotonic clocks every 2 s via the
            // existing tokio runtime — no dedicated thread needed.  On
            // detecting real sleep, the webview is reloaded natively
            // (equivalent to F5) within ~2 s of waking, without user
            // interaction.  The `Focused(true)` handler below provides
            // immediate detection when the user clicks the window.  Both
            // paths share the same atomic state in `power::check_resume`,
            // so the reload fires exactly once per sleep cycle.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    interval.tick().await;
                    if power::check_resume() {
                        // Native webview reload — calls ICoreWebView2::Reload()
                        // (or equivalent), the same code path as pressing F5.
                        // This works even when the WebView2 renderer/IPC is
                        // broken after a GPU compositor crash during sleep.
                        //
                        // The previous approach (emit "system-resume" →
                        // frontend JS listener → window.location.reload())
                        // failed when the IPC channel was broken, leaving
                        // the screen black until the user pressed F5.
                        if let Some(window) = app_handle.get_webview_window("main") {
                            // Set recovery flag so App.tsx skips the splash
                            // screen.  Only effective if the JS renderer is
                            // still alive; if not, the splash screen will
                            // show briefly (gateway is already running).
                            let _ = window.eval(
                                "sessionStorage.setItem('acowork_recovery_reload', '1');",
                            );
                            // Native reload — primary recovery mechanism.
                            let _ = window.reload();
                        }
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
                        // Same native reload as the polling task.  Provides
                        // immediate recovery when the user clicks the window
                        // after wake, without waiting for the next poll tick.
                        // `window` here is a `&Window` (OS-level); we look up
                        // the associated `WebviewWindow` to access eval/reload.
                        if let Some(webview) = window.get_webview_window(window.label()) {
                            let _ = webview.eval(
                                "sessionStorage.setItem('acowork_recovery_reload', '1');",
                            );
                            let _ = webview.reload();
                        }
                    }
                }

                // ── Hide to tray instead of closing ──────────────────────────
                // Only intercept close when window is visible.
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
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // Handle dock icon click on macOS.
        //
        // When the window is hidden to tray, clicking the dock icon fires
        // RunEvent::Reopen.  We show the window and focus it.
        #[cfg(target_os = "macos")]
        {
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        }

        // On non-macOS platforms there are no special run events to handle.
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (app_handle, event);
        }
    });
}
