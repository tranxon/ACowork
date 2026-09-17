// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
//! Navigation guard — last-resort webview navigation firewall.
//!
//! Background: markdown links in chat surfaces used to fall through to the
//! webview's default navigation when their `href` survived neither URL
//! sanitisation nor the resolver. react-markdown rewrites a Windows drive
//! path (`D:/…`) to `""` — and an empty href navigates to the *current*
//! URL, a full reload that on WebView2 could crash the renderer. The
//! frontend fix (`markdownUrlTransform` + the `<a>` interceptor in
//! `MessageBubble` / `CompactionCard`) removes the known vectors; this
//! guard makes any future leak fail closed instead of reloading or
//! white-screening the app.
//!
//! Allow-list (scheme / host / path):
//!   - `tauri://localhost` and `http(s)://tauri.localhost` — the app's own
//!     origin. Only **root-path** navigations pass (`""` / `/`), which is
//!     exactly what `location.reload()` / the Rust `window.reload()` wake
//!     recovery issue. Deep paths on that origin are blocked: they are
//!     leftover relative-path links (`docs/x.md`) navigating the SPA,
//!     nothing the app ever intends.
//!   - `asset` / `asset.localhost` — `convertFileSrc` preview resources.
//!   - `http://localhost:*` / `127.0.0.1` in debug builds — the Vite dev
//!     server origin (`devUrl`). Release builds do not admit it.
//!   - `about` / `blob` / `data` — iframe placeholders and generated blobs.
//!
//! External `http(s)` (everything else) is blocked **on Windows only**.
//! WebView2's `NavigationStarting` fires for the main frame alone, so the
//! URL-preview `<iframe>` keeps loading embedded pages. The macOS / Linux
//! wry delegate (`decidePolicyForNavigationAction`) also reports *iframe*
//! navigations, where an embed is indistinguishable from a top-level
//! navigation — blocking there would white-screen every URL preview, so
//! the guard stays permissive on those platforms and relies on the
//! frontend layers instead.
//!
//! Everything else — `file:` and drive-letter schemes included — is
//! rejected everywhere: no legitimate app navigation uses them, and they
//! are exactly the shapes a leaked local-path link would navigate to.

use tauri::Url;

/// Build the navigation-guard plugin.
///
/// Registered in `lib.rs` before the windows are created so the handler
/// lands in every webview the config spawns (Tauri merges plugin
/// `on_navigation` handlers into each webview's navigation callback).
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("navigation-guard")
        .on_navigation(|_webview, url| {
            let allowed = is_allowed(url);
            if !allowed {
                tracing::warn!(url = %url, "navigation-guard: blocked unexpected webview navigation");
            }
            allowed
        })
        .build()
}

/// Scheme + host + path allow-list. See the module docs for the rationale
/// behind each arm.
fn is_allowed(url: &Url) -> bool {
    match url.scheme() {
        // macOS / Linux production origin of the bundled frontend.
        "tauri" => url.host_str() == Some("localhost") && is_app_root_navigation(url),
        // Local preview assets (`convertFileSrc`): `asset://localhost/…`.
        "asset" => true,
        "http" | "https" => {
            let host = url.host_str().unwrap_or_default();
            // Windows form of the asset protocol.
            if host == "asset.localhost" {
                return true;
            }
            // Windows production origin. Only root-path reloads pass.
            if host == "tauri.localhost" {
                return is_app_root_navigation(url);
            }
            // Vite dev-server origin (`devUrl`). Debug builds only — a
            // release binary never legitimately navigates to a dev server.
            #[cfg(debug_assertions)]
            if host == "localhost" || host == "127.0.0.1" {
                return true;
            }
            // External link: fail closed on Windows (main-frame-only
            // callback), stay permissive elsewhere (iframe embeds share
            // the callback — see the module docs).
            !cfg!(target_os = "windows")
        }
        // iframe placeholders (`about:blank`) and generated blobs. Not a
        // top-level app navigation, but harmless to admit.
        "about" | "blob" | "data" => true,
        // `file:`, drive letters (`d:`), and any custom scheme: no
        // legitimate webview navigation in this app. Fail closed.
        _ => false,
    }
}

/// True for the `""` / `/` paths — the URL a reload of the app origin
/// resolves to. Deeper paths on the app origin are leaked relative-path
/// navigations and are rejected.
fn is_app_root_navigation(url: &Url) -> bool {
    matches!(url.path(), "" | "/")
}
