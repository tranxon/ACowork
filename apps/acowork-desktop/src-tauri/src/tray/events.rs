//! Tray event handlers

use crate::state::AppState;
use tauri::{AppHandle, Manager, menu::MenuEvent, tray::TrayIconEvent};
use tauri::tray::MouseButton;

/// Handle tray menu events
pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    if event.id().as_ref() == "quit" {
        let state = app.state::<AppState>();

        // Exit policy for an *owned* Gateway: three-way choice
        // (single-topology exit UX — the Gateway is a peer that Desktop
        // may or may not manage, never a process it silently force-kills):
        //   - Yes (Quit and Stop Gateway)  → stop the tree, then exit
        //   - No (Quit, Keep Gateway Running) → leave it, then exit
        //   - Cancel                        → abort the quit entirely
        // A foreign (adopted) Gateway — or no Gateway at all — skips the
        // dialog and exits directly (nothing owned to stop).
        let has_owned_gateway = match state.gateway_process.try_lock() {
            Ok(proc) => proc.is_some(),
            // Mutex held by an in-flight spawn; fall through to the direct
            // exit path (safe: RunEvent::Exit still stops it if it exists).
            Err(_) => false,
        };

        if has_owned_gateway {
            use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};

            // Clone the pieces the dialog callback needs into 'static
            // handles (the callback is FnOnce + Send + 'static).
            let app = app.clone();
            let gateway_process = state.gateway_process.clone();
            let keep_running = state.gateway_keep_running_on_exit.clone();

            // Use `MessageDialogButtons::YesNoCancel` (no Custom) on
            // purpose. The tauri-plugin-dialog layer rewrites every
            // button press of `YesNoCancelCustom` into
            // `MessageDialogResult::Custom(label_text)`, which would
            // force the callback to match on the label string —
            // fragile for any future i18n. `YesNoCancel` falls through
            // the plugin's match (no specific arm) and rfd's native
            // `Yes` / `No` / `Cancel` variants reach us verbatim, so
            // the match below is purely on the enum, not on text.
            //
            // Trade-off: button labels are the fixed English "Yes" /
            // "No" / "Cancel" instead of the more descriptive custom
            // labels. Acceptable for an exit-confirmation prompt.
            app.dialog()
                .message("A local Gateway is running. Stop it too, or keep it running after ACowork quits?")
                .title("Quit ACowork")
                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                .buttons(MessageDialogButtons::YesNoCancel)
                .show_with_result(move |reply| {
                    // `show_with_result` fires the callback on a
                    // `std::thread::spawn`-ed worker (not the main
                    // thread). On Windows, `app.exit(0)` from a non-main
                    // thread is unreliable — `Message::RequestExit`
                    // posts to the event loop, but the WebView2/Wry
                    // message loop occasionally fails to fully unwind,
                    // leaving the process alive. Hop back to the main
                    // thread before calling `app.exit(0)`, which is the
                    // same path the pre-dialog tray handler used.
                    let app_for_exit = app.clone();
                    let dispatch_exit = move || {
                        app_for_exit.exit(0);
                    };
                    match reply {
                        MessageDialogResult::Yes => {
                            tracing::info!("User chose: quit and stop Gateway");
                            stop_owned_gateway(gateway_process.as_ref());
                            let _ = app.run_on_main_thread(dispatch_exit);
                        }
                        MessageDialogResult::No => {
                            tracing::info!("User chose: quit, keep Gateway running");
                            keep_running.store(true, std::sync::atomic::Ordering::Relaxed);
                            let _ = app.run_on_main_thread(dispatch_exit);
                        }
                        _ => {
                            tracing::info!("User cancelled quit");
                        }
                    }
                });
        } else {
            tracing::info!("No owned Gateway — exiting directly");
            app.exit(0);
        }
    }
}

/// Stop an owned Gateway process tree (taskkill /T /F on Windows, SIGINT
/// elsewhere), then reap the child. Used by the tray-quit "stop" path.
fn stop_owned_gateway(gateway_process: &tokio::sync::Mutex<Option<std::process::Child>>) {
    let Ok(mut proc) = gateway_process.try_lock() else {
        return;
    };
    if let Some(child) = proc.take() {
        let pid = child.id();
        tracing::info!(pid = pid, "Stopping Gateway process tree on quit");
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
        // Reap the child to avoid zombies
        let mut child = child; // Child::wait needs &mut
        let _ = child.wait();
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
