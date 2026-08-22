//! ACowork Desktop App — Tauri v2 backend
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
mod mqtt_client;
mod state;
mod tray;
#[cfg(target_os = "windows")]
mod win_wndproc;
use state::AppState;
use std::time::Duration;
use tauri::Manager;

// ── Windows Job Object for Gateway process tree cleanup ────────────────────
//
// On Windows, Ctrl+C in dev mode (npm run tauri dev) sends CTRL_C_EVENT.
// The default handler calls ExitProcess without unwinding Rust destructors,
// so RunEvent::Exit and Drop may not fire, leaving orphaned processes.
//
// This module creates a Windows Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
// When the desktop app exits (Ctrl+C, crash, task-kill — *any* reason), the OS
// closes all handles, which closes the job handle, which automatically
// terminates EVERY process in the job (Gateway → Runtime → Embed → LSP).
//
// Child processes automatically inherit job membership, so assigning only the
// Gateway process is sufficient to cover the entire process tree.
//
// On non-Windows platforms SIGINT is sent to the entire foreground process
// group automatically, so no special handling is needed.
#[cfg(target_os = "windows")]
pub mod win_job {
    #![allow(non_upper_case_globals)]
    use std::ffi::c_void;

    type HANDLE = *mut c_void;
    type BOOL = i32;
    type DWORD = u32;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: DWORD = 0x2000;
    const JobObjectExtendedLimitInformation: u32 = 9;
    const PROCESS_SET_QUOTA: DWORD = 0x0100;
    const PROCESS_TERMINATE: DWORD = 0x0001;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        PerProcessUserTimeLimit: i64,
        PerJobUserTimeLimit: i64,
        LimitFlags: DWORD,
        MinimumWorkingSetSize: usize,
        MaximumWorkingSetSize: usize,
        ActiveProcessLimit: DWORD,
        Affinity: usize,
        PriorityClass: DWORD,
        SchedulingClass: DWORD,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION,
        IoInfo: [u8; 48],
        ProcessMemoryLimit: usize,
        JobMemoryLimit: usize,
        PeakProcessMemoryUsed: usize,
        PeakJobMemoryUsed: usize,
    }

    unsafe extern "system" {
        fn CreateJobObjectW(
            lpJobAttributes: *const c_void,
            lpName: *const u16,
        ) -> HANDLE;
        fn SetInformationJobObject(
            hJob: HANDLE,
            JobObjectInfoClass: u32,
            lpJobObjectInfo: *const c_void,
            cbJobObjectInfoLength: DWORD,
        ) -> BOOL;
        fn AssignProcessToJobObject(
            hJob: HANDLE,
            hProcess: HANDLE,
        ) -> BOOL;
        fn OpenProcess(
            dwDesiredAccess: DWORD,
            bInheritHandle: BOOL,
            dwProcessId: DWORD,
        ) -> HANDLE;
        fn CloseHandle(
            hObject: HANDLE,
        ) -> BOOL;
    }

    /// Owned Windows Job Object handle.
    ///
    /// Dropping this handle (including via OS handle-table cleanup on process
    /// exit) triggers JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, terminating all
    /// processes associated with the job.
    pub struct JobHandle(HANDLE);

    // JobHandle is a kernel handle — the underlying HANDLE can be used from
    // any thread.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0); }
            }
        }
    }

    /// Create a new Job Object configured to kill all processes when the last
    /// handle is closed.
    pub fn create_gateway_job() -> Result<JobHandle, String> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err("Failed to create Windows Job Object".into());
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let result = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info) as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
            );
            if result == 0 {
                CloseHandle(job);
                return Err("Failed to set Job Object KILL_ON_CLOSE limit".into());
            }

            tracing::info!("Created Windows Job Object with KILL_ON_JOB_CLOSE");
            Ok(JobHandle(job))
        }
    }

    /// Assign the process identified by `pid` to the given job.
    /// After assignment, the process and all its future children are in the job
    /// and will be terminated when the job handle is closed.
    pub fn assign_pid_to_job(job: &JobHandle, pid: u32) -> Result<(), String> {
        unsafe {
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return Err(format!(
                    "Failed to open process PID {} for job assignment",
                    pid
                ));
            }

            let result = AssignProcessToJobObject(job.0, process);
            CloseHandle(process);

            if result == 0 {
                return Err(format!(
                    "Failed to assign PID {} to Job Object (process may already be in a job)",
                    pid
                ));
            }

            tracing::info!(pid = pid, "Assigned Gateway process to Job Object");
            Ok(())
        }
    }
}

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

    /// Timestamp (biased clock) of the last reload that was triggered.
    /// Used to enforce a cooldown period and prevent rapid successive
    /// reloads from causing GPU process accumulation.
    static LAST_RELOAD_MS: AtomicU64 = AtomicU64::new(0);

    /// Minimum *actual* sleep duration (ms) to trigger recovery.
    /// We measure real sleep, not wall-clock gaps, so even a few seconds
    /// is significant.  5 s filters timer imprecision.
    const SLEEP_THRESHOLD_MS: u64 = 5_000;

    /// Cooldown after a reload before another one can be triggered.
    ///
    /// `window.reload()` is asynchronous — the old renderer process is
    /// torn down and a new one is spawned, but this takes time.  If the
    /// system sleeps and wakes again while a previous reload is still in
    /// progress, a second reload would fire, spawning yet another set of
    /// renderer/GPU processes before the old ones are cleaned up.  Over
    /// rapid sleep/wake cycles this leads to GPU process leaks.
    ///
    /// 15 s is long enough for the reload to complete (typically < 2 s)
    /// plus a safety margin, while short enough that a genuine second
    /// sleep/wake (e.g. user closes and reopens the laptop after a brief
    /// check) will still be recovered within a reasonable time.
    const RELOAD_COOLDOWN_MS: u64 = 15_000;

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
    ///
    /// Updates the clock baseline on every call so that subsequent calls
    /// measure sleep since the *last* call, not since the last reload.
    /// No cooldown – callers use [`allow_reload_now`] to gate expensive
    /// recovery actions like `window.reload()`.
    pub fn detect_resume() -> bool {
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
                "Actual system sleep detected"
            );
            true
        } else {
            false
        }
    }

    /// Returns `true` if a webview reload is permitted (i.e. the
    /// `RELOAD_COOLDOWN_MS` window has elapsed since the last reload).
    ///
    /// On success, records the current biased clock as the last reload
    /// time.  Call this *only* when about to actually reload – it has
    /// a side effect of updating the cooldown timestamp.
    pub fn allow_reload_now() -> bool {
        let Some((biased_ms, _)) = sample() else {
            return false; // API failure or unsupported platform
        };

        let last_reload = LAST_RELOAD_MS.load(Ordering::Relaxed);
        let elapsed_since_reload = biased_ms.saturating_sub(last_reload);
        if last_reload > 0 && elapsed_since_reload < RELOAD_COOLDOWN_MS {
            tracing::info!(
                elapsed_since_reload_ms = elapsed_since_reload,
                cooldown_remaining_ms = RELOAD_COOLDOWN_MS - elapsed_since_reload,
                "Reload cooldown active – skipping webview reload"
            );
            false
        } else {
            LAST_RELOAD_MS.store(biased_ms, Ordering::Relaxed);
            true
        }
    }
}

/// Recovery actions after a system wake.
///
/// Called from both the 2-second polling task and the `Focused(true)`
/// window event handler when [`power::detect_resume`] reports genuine
/// sleep. Performs two independent actions:
///
/// 1. **Force-reconnect MQTT** – the OS may have killed the TCP socket
///    during sleep, leaving `eventloop.poll()` hung on a dead connection
///    that the kernel hasn't yet detected. `force_reconnect()` is cheap
///    (drops + recreates EventLoop) and idempotent, so it needs no
///    cooldown.
///
/// 2. **Reload webview** – gated by [`power::allow_reload_now`] to
///    prevent GPU process accumulation during rapid sleep/wake cycles.
///
/// **Concurrency guard**: `RECOVERY_IN_PROGRESS` prevents overlapping
/// calls.  Although `detect_resume()` atomically updates the clock
/// baseline (so it normally returns `true` only once per sleep event),
/// the polling task and the `Focused(true)` handler could theoretically
/// race on the very first call.  The guard ensures only one recovery
/// task runs at a time, preventing a double `force_reconnect()` that
/// would reset the connection the first task is waiting on.
static RECOVERY_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn recover_from_wake(app_handle: &tauri::AppHandle) {
    // Concurrency guard: only one recovery task at a time.
    if RECOVERY_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        tracing::debug!("recover_from_wake already in progress - skipping");
        return;
    }

    let window = app_handle.get_webview_window("main");
    let mqtt_client = app_handle.state::<AppState>().mqtt_client.clone();

    tauri::async_runtime::spawn(async move {
        // Ensure the guard is cleared even if the task panics.
        let _guard = RecoveryGuard;

        // 1. Force-reconnect MQTT.
        {
            let guard = mqtt_client.lock().await;
            if let Some(client) = guard.as_ref() {
                let client = client.lock().await;
                client.force_reconnect();
                tracing::info!("MQTT force-reconnect triggered by system wake");
            }
        }

        // 2. Wait for MQTT to reach Connected state before reloading the
        //    webview, so the frontend starts with a live connection instead
        //    of racing against the reconnection and showing "Connecting to
        //    agent..." for many seconds.
        let connected = {
            let guard = mqtt_client.lock().await;
            match guard.as_ref() {
                Some(client) => client.lock().await.wait_for_connected(Duration::from_secs(10)).await,
                None => false,
            }
        };

        if connected {
            tracing::info!("MQTT reconnected after wake - reloading webview now");
        } else {
            tracing::warn!("MQTT not connected within 10s after wake - reloading webview anyway");
        }

        // 3. Reload webview with recovery flag (gated by cooldown).
        //    Both setItem and reload are done in a single `eval` so they
        //    execute in the same JS context - the flag is set before the
        //    page unloads.  A separate `window.reload()` call after eval()
        //    can race against the eval's execution and cancel it.
        if power::allow_reload_now()
            && let Some(window) = &window
        {
            match window.eval(
                "sessionStorage.setItem('acowork_recovery_reload', '1');\
                 window.location.reload()",
            ) {
                Ok(()) => tracing::info!("Webview reload triggered with recovery flag"),
                Err(e) => tracing::error!(
                    error = %e,
                    "Failed to eval recovery reload script - \
                     webview may be in an invalid state after wake"
                ),
            }
        }
    });
}

/// RAII guard that clears `RECOVERY_IN_PROGRESS` when dropped.
struct RecoveryGuard;

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        RECOVERY_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Initialize tracing for the Desktop Rust backend.
///
/// Writes to a rolling file in the Gateway's data/logs directory
/// (so all ACowork logs are co-located) and to stderr in dev builds.
///
/// Without this, all `tracing::info!`/`tracing::warn!` calls in the
/// Desktop Rust code are silent no-ops, making runtime debugging
/// impossible without ad-hoc `eprintln!` additions.
fn init_desktop_tracing() {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,acowork_desktop=debug"));

    // Desktop writes to its OWN log directory, independent of the
    // Gateway.  The Gateway may be remote; even when local, Desktop
    // must not write into the Gateway's data tree.  Uses the same
    // `~/.acowork` root so all ACowork artifacts are co-located.
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    #[cfg(not(windows))]
    let home = std::env::var("HOME").unwrap_or_default();

    let log_dir = std::path::PathBuf::from(home)
        .join(".acowork")
        .join("desktop-app")
        .join("logs");

    let file_appender = match acowork_core::logging::SizeRollingFileAppender::new(
        log_dir.clone(),
        5, // 5 MB per file
        3, // keep 3 rotated files
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("WARN: failed to create file appender: {}; falling back to stderr-only", e);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
            return;
        }
    };

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_target(true);

    #[cfg(debug_assertions)]
    let stderr_layer = Some(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false),
    );
    #[cfg(not(debug_assertions))]
    let stderr_layer: Option<tracing_subscriber::fmt::Layer<_>> = None;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    acowork_core::logging::install_panic_hook();
    tracing::info!("Desktop tracing initialized (log_dir={:?})", log_dir);
}

pub fn run() {
    init_desktop_tracing();
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
            commands::chat::upload_file,
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
            commands::effects::set_window_effect,
            // ADR-048 D6: Debug Protocol RPC relay (HTTP via Gateway)
            commands::debug::debug_rpc,
            // ADR-048 follow-up: runtime DevMode activation (no agent restart).
            commands::debug::enable_agent_debug,
            // ADR-048 follow-up: symmetric counterpart to the enable
            // command — tears DevMode down at runtime so the user can
            // exit the Debug Panel without stopping + restarting the
            // agent.
            commands::debug::disable_agent_debug,
            // ADR-033 Phase 3: MQTT real-time event commands
            commands::chat_mqtt::connect_mqtt,
            commands::chat_mqtt::disconnect_mqtt,
            commands::chat_mqtt::force_reconnect_mqtt,
            commands::chat_mqtt::get_mqtt_status,
            commands::chat_mqtt::mqtt_publish_control,
            // ADR-XXX: MQTT broker debug controls (status bar test buttons)
            commands::gateway::debug_mqtt_shutdown,
            commands::gateway::debug_mqtt_start,
            // Local-mode "Reveal in File Explorer" — opens the OS file
            // manager with the entry selected. Only meaningful when the
            // Desktop App and Gateway share a machine (local mode); the
            // frontend hides the corresponding menu item in remote mode.
            commands::reveal::reveal_in_file_explorer,
        ])
        .setup(|app| {
            tray::setup(app)?;

            // ── macOS vibrancy ────────────────────────────────────────────
            // The initial NSVisualEffectView material is now applied by
            // the frontend via the set_window_effect Tauri command (see
            // commands/effects.rs and AppLayout.tsx).  The frontend picks
            // the correct material (Effect::Dark or UnderWindowBackground)
            // based on the effective theme before the window is shown
            // (window starts with visible:false in tauri.conf.json), so
            // there is no flash and no race between the Rust setup retry
            // loop and the frontend's theme-aware effect.
            //
            // We intentionally do NOT apply any effect here — doing so
            // with UnderWindowBackground in a delayed retry loop races
            // with the frontend's set_window_effect call and can clobber
            // the dark-mode material, causing the window to appear whitish
            // at low opacity.

            // ── Windows acrylic blur ──────────────────────────────────────
            // Apply DWM Acrylic so the desktop shows through the transparent
            // window with a native blur.  Without this the WebView2 has
            // nothing for CSS `backdrop-filter` to blur on Windows — the
            // browser's stacking context ends at the transparent body and
            // there is no rendered content behind the root element to blur.
            //
            // Acrylic requires Windows 10+; on older Windows Tauri logs the
            // error and the window falls back to a plain transparent surface.
            // `radius` is ignored for Acrylic (system-controlled) but kept
            // for parity with the pre-c8f031a frontend `setEffects` call.
            //
            // `color` provides a subtle neutral tint that blends with the
            // acrylic backdrop, reducing the jarring transparency gap when
            // the window is resized and WebView2 content lags behind DWM.
            #[cfg(target_os = "windows")]
            {
                use tauri::utils::config::WindowEffectsConfig;
                use tauri::window::EffectState;

                let main_window = app.get_webview_window("main").expect("no main window");
                let effects = WindowEffectsConfig {
                    effects: vec![tauri::window::Effect::Acrylic],
                    state: Some(EffectState::Active),
                    radius: Some(12.0),
                    color: Some((128, 128, 128, 30).into()),
                };
                let _ = main_window.set_effects(effects);
            }

            // ── Disable native decorations ──────────────────────────────
            // set_decorations(false) removes the title bar on Linux and Windows.
            // macOS uses native traffic lights with titleBarStyle: Overlay
            // (configured in tauri.conf.json), so decorations stay On.
            #[cfg(not(target_os = "macos"))]
            {
                let main_window = app.get_webview_window("main").expect("no main window");
                let _ = main_window.set_decorations(false);
            }

            // ── Windows: restore WS_SYSMENU | WS_MINIMIZEBOX ─────────
            // set_decorations(false) strips WS_SYSMENU and WS_MINIMIZEBOX,
            // which breaks taskbar button behavior:
            //   • Explorer needs WS_SYSMENU → system menu for right-click
            //   • Explorer needs WS_MINIMIZEBOX → minimize/restore on left-click
            // Without these, Explorer falls back to showing the system menu
            // on both left-click and right-click.  We put them back into the
            // window style and force a fresh system menu so the taskbar
            // button behaves correctly.  The title bar stays hidden because
            // decorations are already disabled.
            //
            // A WndProc subclass (win_wndproc.rs) catches any remaining
            // SC_MOUSEMENU/SC_KEYMENU that Explorer might send as fallback.
            #[cfg(target_os = "windows")]
            {
                use std::ffi::c_void;

                unsafe extern "system" {
                    fn GetWindowLongPtrW(h: *mut c_void, n: i32) -> isize;
                    fn SetWindowLongPtrW(h: *mut c_void, n: i32, v: isize) -> isize;
                    fn SetWindowPos(
                        h: *mut c_void,
                        insert_after: *mut c_void,
                        x: i32, y: i32, cx: i32, cy: i32,
                        flags: u32,
                    ) -> i32;
                    fn GetSystemMenu(h: *mut c_void, b: i32) -> *mut c_void;
                }

                const GWL_STYLE: i32 = -16;
                const WS_SYSMENU: isize = 0x0008_0000;
                const WS_MINIMIZEBOX: isize = 0x0002_0000;
                const SWP_FRAMECHANGED: u32 = 0x0020;
                const SWP_NOMOVE: u32 = 0x0002;
                const SWP_NOSIZE: u32 = 0x0001;
                const SWP_NOZORDER: u32 = 0x0004;
                const SWP_NOACTIVATE: u32 = 0x0010;

                let main_window = app.get_webview_window("main").expect("no main window");
                if let Ok(hwnd) = main_window.hwnd() {
                    let raw = hwnd.0 as *mut c_void;
                    unsafe {
                        let style = GetWindowLongPtrW(raw, GWL_STYLE);
                        let needed = WS_SYSMENU | WS_MINIMIZEBOX;
                        if style & needed != needed {
                            SetWindowLongPtrW(raw, GWL_STYLE, style | needed);
                            // SWP_FRAMECHANGED triggers WM_NCCALCSIZE to recalc the
                            // non-client area, which DWM uses to render the frame.
                            // SWP_NOACTIVATE prevents focus change.
                            SetWindowPos(
                                raw, std::ptr::null_mut(),
                                0, 0, 0, 0,
                                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            // TRUE → force fresh copy of default system menu
                            GetSystemMenu(raw, 1);
                            tracing::info!(
                                "Restored WS_SYSMENU|WS_MINIMIZEBOX + rebuilt system menu"
                            );
                        }
                    }
                }
            }

            // ── Windows: WndProc subclass for taskbar left-click ────
            #[cfg(target_os = "windows")]
            {
                let main_window = app.get_webview_window("main").expect("no main window");
                if let Ok(hwnd) = main_window.hwnd() {
                    let raw = hwnd.0 as *mut std::ffi::c_void;
                    let _ = unsafe { crate::win_wndproc::install(raw) };
                }
            }

            // Spawn async task for automatic sleep detection.
            // Polls biased/unbiased monotonic clocks every 2 s.  On
            // detecting real sleep, `recover_from_wake` force-reconnects
            // the MQTT client (OS may have killed the TCP socket) and
            // reloads the webview (gated by cooldown).  The `Focused(true)`
            // handler below provides immediate detection when the user
            // clicks the window.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    interval.tick().await;
                    if power::detect_resume() {
                        recover_from_wake(&app_handle);
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
                    if power::detect_resume() {
                        recover_from_wake(window.app_handle());
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
        // ── Cleanup: Kill local Gateway process tree on exit ──────────
        // Covers Ctrl+C (dev mode), window close, tray quit, and OS shutdown.
        // On Windows, uses taskkill /T /F to kill Gateway + all children
        // (Runtime, Embed) in one shot. On Unix, sends SIGINT for clean
        // shutdown via Gateway's own signal handler.
        if matches!(
            event,
            tauri::RunEvent::Exit
                | tauri::RunEvent::ExitRequested { .. }
        ) {
            let state = app_handle.state::<AppState>();
            let gateway_handle = state.gateway_process.clone();
            // try_lock: if the mutex is held by an inflight init_local_gateway,
            // that command will store the child and this handler won't see it,
            // but the next exit attempt will catch it. This is non-blocking
            // because RunEvent::Exit fires in the main thread context.
            if let Ok(mut proc) = gateway_handle.try_lock()
                && let Some(mut child) = proc.take()
            {
                let pid = child.id();
                tracing::info!(pid = pid, "App exiting, killing Gateway process tree");
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .output();
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = std::process::Command::new("kill")
                        .args(["-INT", &pid.to_string()])
                        .output();
                }
                let _ = child.wait();
            }
        }

        // Handle dock icon click on macOS.
        //
        // When the window is hidden to tray, clicking the dock icon fires
        // RunEvent::Reopen.  We show the window and focus it.
        #[cfg(target_os = "macos")]
        {
            if let tauri::RunEvent::Reopen { .. } = event
                && let Some(window) = app_handle.get_webview_window("main")
            {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }

        // On non-macOS platforms there are no special run events to handle.
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (app_handle, event);
        }
    });
}
