//! Read file paths from the OS clipboard.
//!
//! Used by the chat panel as a fallback when the WebView's
//! `ClipboardEvent.clipboardData` does not expose paths — which is the
//! case on Tauri v2's WebView2 backend when the user copies files from
//! Windows Explorer (CF_HDROP), macOS Finder (NSFilenamesPboardType), or
//! a Linux file manager using the legacy `text/uri-list` MIME.
//!
//! The frontend calls this only after its own surface read yielded
//! nothing, so the cost is amortised across the lifetime of a paste
//! (typically once).
//!
//! ## Threading (macOS only)
//!
//! Tauri spawns every `async fn` `#[tauri::command]` body onto the global
//! tokio runtime's worker threads (see `tauri::ipc::InvokeResolver::
//! respond_async_serialized_inner`). The macOS implementation calls
//! `NSPasteboard` via `objc2::msg_send!` — Cocoa pasteboard code can
//! raise an `NSException` when invoked off the main thread (stale
//! pasteboard items, internal main-thread assertions). NSException
//! unwinds through Rust's `catch_unwind` boundary in tokio's
//! `task::harness::poll_future`, which Rust std treats as a foreign
//! exception and aborts the process (`__rust_foreign_exception` →
//! `abort()`). The fix is to dispatch the ObjC read to the main thread
//! via `AppHandle::run_on_main_thread` and recover the result over a
//! oneshot channel.
//!
//! Windows and Linux backends are pure FFI without this constraint, so
//! they keep the original synchronous call path — no main-thread hop,
//! no behavioural change.
//!
//! ## Cross-platform parameter note
//!
//! `app: tauri::AppHandle` is **always** present in the signature (never
//! `#[cfg]`-gated). `#[tauri::command]` generates its wrapper from the
//! raw source AST *before* `cfg` is resolved, so gating the parameter
//! itself causes the generated wrapper to call `get_clipboard_file_paths(app)`
//! on every platform, while on Windows / Linux the user function has zero
//! parameters after `cfg` processing → `E0061: this function takes 0
//! arguments but 1 argument was supplied`. Tauri auto-injects
//! `AppHandle` from its runtime when the parameter type is
//! `tauri::AppHandle`, so the frontend IPC signature is unchanged either
//! way (the frontend calls `invoke("get_clipboard_file_paths")` with no
//! args in both cases). The parameter is simply `_app`-ignored on
//! non-macOS platforms.

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

/// Return the absolute filesystem paths of any files currently on the
/// clipboard, or an empty vector if none.
///
/// Errors are NOT propagated up: the frontend treats an empty vector
/// as "nothing to upload, fall through to plain-text paste". A platform
/// API failure shouldn't break the paste UX — the user still sees their
/// text inserted.
#[tauri::command]
pub async fn get_clipboard_file_paths(
    // AppHandle is always present in the signature so the
    // `#[tauri::command]` macro generates a stable wrapper on every
    // platform (see module-level docs for the cfg-on-parameter pitfall).
    // Tauri auto-injects it from its runtime; the frontend does not
    // pass it. On non-macOS the parameter is unused — the
    // `#[allow(unused_variables)]` keeps the build warning-free.
    #[allow(unused_variables)]
    app: tauri::AppHandle,
) -> Result<Vec<String>, String> {
    // 1) Acquire the raw platform-specific paths.
    //
    //    macOS: hop to the main thread so NSPasteboard ObjC calls run
    //    where Cocoa expects them (avoids NSException → foreign-exception
    //    abort — see module-level docs).
    //
    //    Windows / Linux: call directly. The clipboard APIs are
    //    thread-safe (or expected to be called from any thread) and
    //    forcing a main-thread hop here would only add latency.
    #[cfg(target_os = "macos")]
    let raw: Vec<String> = {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<String>>>();

        // `run_on_main_thread` is fire-and-forget — it enqueues a closure
        // onto the tao event loop and returns immediately. The oneshot
        // sender is `Send + 'static`, satisfying the closure bound, and
        // we recover the (sync) return value on the worker side via
        // `rx.await`.
        let dispatch = app.run_on_main_thread(move || {
            let result = read_clipboard_file_paths();
            let _ = tx.send(result);
        });

        if let Err(e) = dispatch {
            // `run_on_main_thread` failed before enqueueing (rare; e.g.
            // app is tearing down). Log and return an empty vector so
            // the paste UX still works.
            tracing::warn!(
                target: "commands::clipboard",
                "run_on_main_thread failed; clipboard read skipped: {e}"
            );
            return Ok(Vec::new());
        }

        // Cap how long we wait for the main thread. If the event loop is
        // saturated we time out gracefully — the frontend treats empty
        // as "no files, fall through to plain-text paste", so a timeout
        // never blocks the user's paste UX.
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
            Ok(Ok(value)) => value.unwrap_or_default(),
            Ok(Err(_)) => {
                // oneshot sender dropped without sending (shouldn't
                // happen since the closure owns it, but be defensive).
                tracing::warn!(
                    target: "commands::clipboard",
                    "clipboard read oneshot dropped before completion"
                );
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    target: "commands::clipboard",
                    "clipboard read timed out after 2s; main thread busy or unresponsive"
                );
                Vec::new()
            }
        }
    };

    #[cfg(not(target_os = "macos"))]
    let raw: Vec<String> = read_clipboard_file_paths().unwrap_or_default();

    Ok(sanitize_clipboard_paths(raw))
}

/// Sanitise raw platform paths (pure Rust, no platform constraint):
///   - strip whitespace **and a literal `\0` sentinel** from both ends
///     (WebView clipboard APIs sometimes include a `\0` when only one
///     file is present — `trim()` alone does not remove it, and a path
///     carrying NUL would fail `exists()` and be silently dropped);
///   - require absolute (defends against relative paths slipping in
///     from a non-conformant OS payload);
///   - require `Path::exists()` so we never return a path that
///     `tokio::fs::read` would reject with `os error 53/161` during
///     the subsequent upload. This is the last line of defence
///     after the frontend's path-detection heuristic was removed
///     — if a stale CF_HDROP entry (e.g. user deleted the file
///     between Ctrl+C and Ctrl+V) is still on the clipboard, we
///     silently drop it instead of failing the upload.
///
/// Kept as a pure function so the filter semantics are unit-testable
/// on every platform.
fn sanitize_clipboard_paths(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|p| {
            p.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string()
        })
        .filter(|p| {
            if p.is_empty() {
                return false;
            }
            let path = std::path::Path::new(p);
            path.is_absolute() && path.exists()
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    windows::read_clipboard_file_paths()
}

#[cfg(target_os = "macos")]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    macos::read_clipboard_file_paths()
}

#[cfg(target_os = "linux")]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    linux::read_clipboard_file_paths()
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::sanitize_clipboard_paths;

    fn touch(p: &std::path::Path) {
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn sanitize_keeps_existing_absolute_paths() {
        let dir = std::env::temp_dir().join(format!("acowork-clipboard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        touch(&file);

        let out = sanitize_clipboard_paths(vec![file.to_string_lossy().into_owned()]);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("a.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_drops_relative_paths() {
        let out = sanitize_clipboard_paths(vec!["Users/foo/a.txt".to_string()]);
        assert!(out.is_empty(), "relative path must be dropped");
    }

    #[test]
    fn sanitize_drops_non_existent_paths() {
        let missing = std::env::temp_dir().join("definitely-not-a-real-file-982173.txt");
        let out = sanitize_clipboard_paths(vec![missing.to_string_lossy().into_owned()]);
        assert!(out.is_empty(), "stale/nonexistent path must be dropped");
    }

    #[test]
    fn sanitize_trims_whitespace_and_null_sentinel() {
        // WebView clipboard APIs sometimes include a literal `\0` sentinel
        // when only one file is present — trim must handle it.
        let dir = std::env::temp_dir().join(format!("acowork-clipboard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("b.txt");
        touch(&file);
        let raw = format!("  {}\u{0}  ", file.to_string_lossy());

        let out = sanitize_clipboard_paths(vec![raw]);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("b.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_drops_empty_entries() {
        let out = sanitize_clipboard_paths(vec!["".to_string(), "   ".to_string()]);
        assert!(out.is_empty());
    }
}
