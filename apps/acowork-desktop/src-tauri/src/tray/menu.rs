//! Tray menu items

use tauri::{App, menu::MenuItemBuilder};

/// Build the "Show Dashboard" menu item
pub fn build_show_dashboard(
    app: &App,
) -> Result<impl tauri::menu::IsMenuItem<tauri::Wry> + Clone, Box<dyn std::error::Error>> {
    let item = MenuItemBuilder::with_id("show_dashboard", "Show Dashboard").build(app)?;
    Ok(item)
}

/// Build the "Agent Chat" menu item
pub fn build_show_chat(
    app: &App,
) -> Result<impl tauri::menu::IsMenuItem<tauri::Wry> + Clone, Box<dyn std::error::Error>> {
    let item = MenuItemBuilder::with_id("agent_chat", "Agent Chat").build(app)?;
    Ok(item)
}

/// Build the "Quit" menu item
pub fn build_quit(
    app: &App,
) -> Result<impl tauri::menu::IsMenuItem<tauri::Wry> + Clone, Box<dyn std::error::Error>> {
    let item = MenuItemBuilder::with_id("quit", "Quit AgentCowork").build(app)?;
    Ok(item)
}
