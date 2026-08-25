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
    // Strip empty / relative entries — WebView clipboard APIs sometimes
    // include a literal `\0` sentinel when only one file is present.
    let cleaned: Vec<String> = paths
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty() && std::path::Path::new(p).is_absolute())
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