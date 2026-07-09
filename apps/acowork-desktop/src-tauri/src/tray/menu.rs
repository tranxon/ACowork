//! Tray menu items

use tauri::{App, menu::MenuItemBuilder};

/// Build the "Quit" menu item
pub fn build_quit(
    app: &App,
) -> Result<impl tauri::menu::IsMenuItem<tauri::Wry> + Clone, Box<dyn std::error::Error>> {
    let item = MenuItemBuilder::with_id("quit", "Quit ACowork").build(app)?;
    Ok(item)
}
