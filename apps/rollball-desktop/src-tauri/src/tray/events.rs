//! Tray event handlers

use tauri::{
    tray::TrayIconEvent,
    menu::MenuEvent,
    AppHandle, Manager,
};
use crate::state::AppState;

/// Handle tray menu events
pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "show_dashboard" | "agent_chat" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        "quit" => {
            // Kill local Gateway child process before exit
            let state = app.state::<AppState>();
            let gateway_handle = state.gateway_process.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(mut proc) = gateway_handle.try_lock() {
                    if let Some(mut child) = proc.take() {
                        tracing::info!("Killing Gateway child process on quit");
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            });
            // Give a short moment for the kill to propagate
            std::thread::sleep(std::time::Duration::from_millis(200));
            app.exit(0);
        }
        _ => {}
    }
}

/// Handle tray icon click events
pub fn on_tray_icon_event(tray: &tauri::tray::TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click { .. } = event {
        let app = tray.app_handle();
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}
