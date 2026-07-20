//! Window effects commands — macOS NSVisualEffectView material switching
//!
//! On macOS the native visual effect material (`UnderWindowBackground`,
//! `WindowBackground`, etc.) dominates the glass appearance when the
//! CSS tint-layer opacity is low.  Since `UnderWindowBackground` is
//! always light (Apple's design), it washes out dark mode.
//!
//! This command lets the frontend dynamically swap the material whenever
//! the theme changes, so light mode gets an always-light material and
//! dark mode gets an always-dark material.

#[cfg(target_os = "macos")]
use tauri::Manager;

/// Set the macOS NSVisualEffectView material based on the current theme.
///
/// * `is_dark` — whether the effective theme is dark
///
/// Mapping:
/// - `is_dark = true`  → `Effect::Dark` + dark color tint
/// - `is_dark = false` → `Effect::UnderWindowBackground` + neutral color tint
///
/// Even with `Effect::Dark` the native NSVisualEffectView remains
/// translucent and blends with the desktop wallpaper.  When CSS opacity
/// is dragged to 0 the CSS tint layer becomes transparent, exposing the
/// desktop content through the blurred material.  On a light desktop
/// wallpaper this makes the window appear whitish despite dark mode.
///
/// The per-theme `color` provides a base tint at the native layer so
/// the window stays visually dark/light regardless of desktop content,
/// even at zero CSS opacity.
///
/// On non-macOS platforms this is a no-op.
#[tauri::command]
#[allow(deprecated)]
pub fn set_window_effect(app: tauri::AppHandle, is_dark: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::utils::config::WindowEffectsConfig;
        use tauri::window::{Effect, EffectState};

        let (material, color) = if is_dark {
            // Effect::Dark (NSVisualEffectMaterialDark) + a semi-transparent
            // black tint so the window stays dark even when CSS opacity=0
            // and the desktop wallpaper is light.
            (Effect::Dark, Some((0, 0, 0, 40).into()))
        } else {
            // UnderWindowBackground (NSVisualEffectMaterialUnderWindowBackground)
            // + a subtle neutral tint matching the initial setup in lib.rs.
            (Effect::UnderWindowBackground, Some((128, 128, 128, 30).into()))
        };

        let effects = WindowEffectsConfig {
            effects: vec![material],
            state: Some(EffectState::Active),
            radius: None,
            color,
        };

        if let Some(window) = app.get_webview_window("main") {
            window.set_effects(effects).map_err(|e| format!("set_effects failed: {e}"))?;
            eprintln!(
                "[vibrancy] set_window_effect(is_dark={is_dark}) -> {material:?} applied"
            );
        }
    }

    // Non-macOS: silent no-op
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, is_dark);
    }

    Ok(())
}
