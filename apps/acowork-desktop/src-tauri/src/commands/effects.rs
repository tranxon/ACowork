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

/// Set the macOS NSVisualEffectView material based on the current theme.
///
/// * `is_dark` — whether the effective theme is dark
///
/// Mapping:
/// - `is_dark = true`  → `Effect::Dark`    (always dark tint)
/// - `is_dark = false` → `Effect::UnderWindowBackground` (always light tint)
///
/// On non-macOS platforms this is a no-op.
#[tauri::command]
#[allow(deprecated)]
pub fn set_window_effect(app: tauri::AppHandle, is_dark: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::utils::config::WindowEffectsConfig;
        use tauri::window::{Effect, EffectState};

        let material = if is_dark {
            Effect::Dark
        } else {
            Effect::UnderWindowBackground
        };

        let effects = WindowEffectsConfig {
            effects: vec![material],
            state: Some(EffectState::Active),
            radius: None,
            color: None,
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
