//! System tray module

mod events;
mod menu;

use tauri::{
    image::Image,
    menu::MenuBuilder,
    tray::TrayIconBuilder,
    App,
};

/// Set up the system tray
pub fn setup(app: &App) -> Result<(), Box<dyn std::error::Error>> {
    let show_dashboard = menu::build_show_dashboard(app)?;
    let show_chat = menu::build_show_chat(app)?;
    let quit = menu::build_quit(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&show_dashboard, &show_chat])
        .separator()
        .items(&[&quit])
        .build()?;

    // Load icon from embedded resources
    let icon = Image::from_bytes(include_bytes!("../../icons/icon.png"))?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Rollball")
        .on_menu_event(events::on_menu_event)
        .on_tray_icon_event(events::on_tray_icon_event)
        .build(app)?;

    Ok(())
}
