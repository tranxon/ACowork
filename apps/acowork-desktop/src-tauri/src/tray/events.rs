//! Tray event handlers

use crate::state::AppState;
use tauri::{AppHandle, Manager, menu::MenuEvent, tray::TrayIconEvent};
use tauri::tray::MouseButton;

/// Handle tray menu events
pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    if event.id().as_ref() == "quit" {
            // Kill local Gateway process tree before exit.
            // On Windows, taskkill /T /F kills the Gateway AND all its child
            // processes (Runtime + Embed) in one shot, preventing orphans.
            // On Unix, kill -INT sends SIGINT which triggers the Gateway's
            // ctrl_c handler to clean up children before exiting.
            let state = app.state::<AppState>();
            let gateway_handle = state.gateway_process.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(mut proc) = gateway_handle.try_lock()
                    && let Some(child) = proc.take()
                {
                    let pid = child.id();
                    tracing::info!(pid = pid, "Killing Gateway process tree on quit");
                    #[cfg(target_os = "windows")]
                    {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/PID", &pid.to_string(), "/T", "/F"])
                            .output();
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        // Send SIGINT so Gateway's signal handler cleans up children
                        let _ = std::process::Command::new("kill")
                            .args(["-INT", &pid.to_string()])
                            .output();
                    }
                    // Reap the child process
                    let mut child = child; // Child::wait needs &mut
                    let _ = child.wait();
                }
            });
            // Give a short moment for the kill to propagate
            std::thread::sleep(std::time::Duration::from_millis(500));
            app.exit(0);
        }
}

/// Bring the main window to the foreground, restoring it from minimized if needed.
///
/// Tauri/Wry/Tao on Windows has a quirk that makes `show() + set_focus()`
/// insufficient for minimized windows:
///   - `show()` calls `ShowWindow(SW_SHOW)`, which preserves the WS_MINIMIZE
///     flag, so a minimized window stays minimized.
///   - `set_focus()` (tao::platform_impl::windows::Window::set_focus) bails
///     out early when `is_minimized` is true.
///
/// Calling `unminimize()` first invokes `ShowWindow(SW_RESTORE)`, which
/// clears the minimize state and brings the window back.  Then `show()` is
/// idempotent on an already-visible window and `set_focus()` actually does
/// its job now that the window isn't minimized.
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        // Order matters: unminimize first, then show (no-op if already visible),
        // then set_focus (no-op if already foregrounded).
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Handle tray icon click events
///
/// Left-click: restore (if minimized) and focus the main window
///             (like WeChat/QQ).
/// Right-click: system shows the attached menu automatically — do nothing.
pub fn on_tray_icon_event(tray: &tauri::tray::TrayIcon, event: TrayIconEvent) {
    match event {
        TrayIconEvent::Click { button: MouseButton::Left, .. } => {
            show_main_window(tray.app_handle());
        }
        TrayIconEvent::DoubleClick { .. } => {
            // Double-click (Windows only): also restore & focus
            show_main_window(tray.app_handle());
        }
        _ => {} // Right-click → menu auto-shown by .menu()
    }
}
