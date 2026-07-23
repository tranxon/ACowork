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
//!
//! The tint color is also user-controllable via the accent preset — the
//! frontend passes a light/dark RGBA tuple so that even at zero CSS
//! opacity the native layer retains a "color anchor" matching the
//! user's chosen accent (e.g. a faint blue/green hue shift instead of
//! pure white/black that would otherwise blend into desktop wallpaper).

#[cfg(target_os = "macos")]
use tauri::Manager;

/// Set the macOS NSVisualEffectView material based on the current theme.
///
/// * `is_dark`      — whether the effective theme is dark
/// * `light_rgba`   — RGBA tuple applied to `UnderWindowBackground` (light mode)
/// * `dark_rgba`    — RGBA tuple applied to `Effect::Dark` (dark mode)
///
/// Mapping:
/// - `is_dark = true`  → `Effect::Dark` + `dark_rgba` tint
/// - `is_dark = false` → `Effect::UnderWindowBackground` + `light_rgba` tint
///
/// Even with `Effect::Dark` the native NSVisualEffectView remains
/// translucent and blends with the desktop wallpaper.  When CSS opacity
/// is dragged to 0 the CSS tint layer becomes transparent, exposing the
/// desktop content through the blurred material.  The per-theme RGBA
/// tint provides a base layer of color at the native level so the
/// window stays visually distinct from the desktop content, even at
/// zero CSS opacity.
///
/// The frontend derives `light_rgba` / `dark_rgba` from the active
/// accent preset — see `lib/accentPresets.ts`.  Each preset encodes
/// a near-neutral gray with 8% saturation toward the accent hue
/// (e.g. `hsl(217 8% 92%)` for blue light mode), pre-converted to RGB.
///
/// On non-macOS platforms this is a no-op.
#[tauri::command]
#[allow(deprecated)]
pub fn set_window_effect(
    app: tauri::AppHandle,
    is_dark: bool,
    light_rgba: (u8, u8, u8, u8),
    dark_rgba: (u8, u8, u8, u8),
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::utils::config::WindowEffectsConfig;
        use tauri::window::{Effect, EffectState};

        let (material, color) = if is_dark {
            // Effect::Dark (NSVisualEffectMaterialDark) + a hue-shifted dark
            // tint so the window stays dark even when CSS opacity=0 and the
            // desktop wallpaper is light.  The hue comes from the active
            // accent preset so the surface has a faint color anchor.
            (Effect::Dark, Some(dark_rgba.into()))
        } else {
            // UnderWindowBackground (NSVisualEffectMaterialUnderWindowBackground)
            // + a subtle near-neutral tint with an 8% hue shift toward the
            // active accent.  Same rationale as above.
            (Effect::UnderWindowBackground, Some(light_rgba.into()))
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
                "[vibrancy] set_window_effect(is_dark={is_dark}, light={light_rgba:?}, dark={dark_rgba:?}) -> {material:?} applied"
            );
        }
    }

    // Non-macOS: silent no-op (but still validate the args would compile)
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, is_dark, light_rgba, dark_rgba);
    }

    Ok(())
}
