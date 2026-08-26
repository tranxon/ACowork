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
pub async fn get_clipboard_file_paths() -> Result<Vec<String>, String> {
    let paths = read_clipboard_file_paths().unwrap_or_default();
    // Sanitise every entry:
    //   - strip whitespace (WebView clipboard APIs sometimes include
    //     a literal `\0` sentinel when only one file is present);
    //   - require absolute (defends against relative paths slipping in
    //     from a non-conformant OS payload);
    //   - require `Path::exists()` so we never return a path that
    //     `tokio::fs::read` would reject with `os error 53/161` during
    //     the subsequent upload. This is the last line of defence
    //     after the frontend's path-detection heuristic was removed
    //     — if a stale CF_HDROP entry (e.g. user deleted the file
    //     between Ctrl+C and Ctrl+V) is still on the clipboard, we
    //     silently drop it instead of failing the upload.
    let cleaned: Vec<String> = paths
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| {
            if p.is_empty() {
                return false;
            }
            let path = std::path::Path::new(p);
            path.is_absolute() && path.exists()
        })
        .collect();
    Ok(cleaned)
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