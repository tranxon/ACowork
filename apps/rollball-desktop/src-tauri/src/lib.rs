//! Rollball Desktop App — Tauri v2 backend
//!
//! This is the library entry point for the Tauri application.
//! It sets up the Tauri builder with all plugins, commands, and tray.

mod commands;
mod gateway_client;
mod state;
mod tray;

use state::AppState;
use tauri::Manager;

/// System Agent ID — always bundled with Desktop App
const SYSTEM_AGENT_ID: &str = "com.rollball.system";

/// Bundled system-agent resource directory name
const SYSTEM_AGENT_RESOURCE: &str = "system-agent";

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
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::agent::list_agents,
            commands::agent::get_agent_detail,
            commands::agent::install_agent,
            commands::agent::uninstall_agent,
            commands::agent::start_agent,
            commands::agent::stop_agent,
            commands::agent::clone_agent,
            commands::chat::send_message,
            commands::chat::upload_document,
            commands::vault::list_keys,
            commands::vault::add_key,
            commands::vault::remove_key,
            commands::vault::update_key,
            commands::publish::prepare_publish,
            commands::publish::build_publish,
            commands::publish::export_package,
            commands::create::create_agent,
        ])
        .setup(|app| {
            tray::setup(app)?;

            // Auto-install bundled System Agent on first launch
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = auto_install_system_agent(&app_handle).await {
                    tracing::warn!("Failed to auto-install System Agent: {}", e);
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Hide to tray instead of closing
            // Only intercept close when window is visible and focused
            // This prevents interference with system tray menu on Windows
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Only hide if window is currently visible
                // When tray menu is showing, window won't be visible/focused
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
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Auto-install the bundled System Agent if not already installed.
async fn auto_install_system_agent(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::time::{sleep, Duration};

    // Wait for Gateway to be ready (max 30 seconds)
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let gateway_url = rollball_core::defaults::GATEWAY_HTTP_URL;

    for i in 0..60 {
        if client.get(format!("{}/health", gateway_url)).send().await.is_ok() {
            break;
        }
        sleep(Duration::from_millis(500)).await;
        if i % 10 == 0 {
            tracing::debug!("Waiting for Gateway to be ready...");
        }
    }

    // Check if System Agent is already installed
    match client.get(format!("{}/api/agents/{}", gateway_url, SYSTEM_AGENT_ID)).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("System Agent already installed, skipping auto-install");
            return Ok(());
        }
        _ => {}
    }

    // Get the bundled system-agent path
    let resource_dir = app.path().resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    let system_agent_path = resource_dir.join(SYSTEM_AGENT_RESOURCE);

    if !system_agent_path.exists() {
        tracing::warn!("Bundled System Agent not found at {:?}", system_agent_path);
        return Ok(());
    }

    // Verify manifest exists
    if !system_agent_path.join("manifest.toml").exists() {
        tracing::warn!("Bundled System Agent missing manifest.toml");
        return Ok(());
    }

    tracing::info!("Auto-installing bundled System Agent from {:?}", system_agent_path);

    // Install the System Agent via Gateway API
    let body = serde_json::json!({
        "package_path": system_agent_path.to_string_lossy(),
        "dev_mode": true
    });

    match client.post(format!("{}/api/agents/install", gateway_url))
        .json(&body)
        .send()
        .await {
        Ok(resp) => {
            if resp.status().is_success() {
                tracing::info!("Successfully auto-installed bundled System Agent");
            } else {
                let error = resp.text().await.unwrap_or_default();
                tracing::warn!("Failed to install System Agent: {}", error);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to call install API: {}", e);
        }
    }

    Ok(())
}
