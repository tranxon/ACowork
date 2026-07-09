//! System tray module

mod events;
mod menu;

use tauri::{App, image::Image, menu::MenuBuilder, tray::TrayIconBuilder};

/// Set up the system tray
pub fn setup(app: &App) -> Result<(), Box<dyn std::error::Error>> {
    let quit = menu::build_quit(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&quit])
        .build()?;

    // Load icon from embedded resources
    let icon = Image::from_bytes(include_bytes!("../../icons/icon.png"))?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("ACowork")
        // Left-click must NOT show the menu — instead, the Click event handler
        // in events.rs brings the main window to the foreground. By default
        // tray-icon shows the menu on left-click (the only way to surface the
        // menu on Windows), which makes left-click feel identical to right-click
        // and prevents users from quickly restoring the window.
        .show_menu_on_left_click(false)
        .on_menu_event(events::on_menu_event)
        .on_tray_icon_event(events::on_tray_icon_event)
        .build(app)?;

    Ok(())
}
