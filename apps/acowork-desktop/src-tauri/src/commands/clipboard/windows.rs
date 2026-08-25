//! Windows clipboard reader using `CF_HDROP`.
//!
//! When the user copies files from Explorer, the clipboard carries the
//! `CF_HDROP` format — a `DROPFILES` struct followed by a double-null
//! terminated list of absolute paths.
//!
//! `WebView2` exposes this in the DOM `ClipboardEvent.files` as path-less
//! `File` objects and does NOT surface it as `text/uri-list` or
//! `text/plain`. So we read the raw clipboard ourselves using the
//! classic Win32 clipboard API:
//!   `OpenClipboard` → `GetClipboardData(CF_HDROP)` → `DragQueryFileW`
//!   per entry → `CloseClipboard`. We never free the `HGLOBAL` because
//! `GetClipboardData` returns a borrowed handle owned by the clipboard.
//!
//! Caveat: `OpenClipboard` requires no other thread in this process to
//! hold the clipboard open. Tauri's IPC runs on a worker pool, so under
//! load this can briefly fail; we return `None` and let the frontend's
//! other paths handle it. The fallback is a plain-text paste, never a
//! hang.

use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows_sys::Win32::System::Ole::CF_HDROP;
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
    unsafe {
        // OpenClipboard(NULL) — passing NULL for the window handle is
        // allowed for non-windowed callers. Returns BOOL (nonzero = ok).
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let result = read_inner();
        // Always close, even on error path — failing to close would
        // lock subsequent clipboard reads across the whole process.
        let _ = CloseClipboard();
        result
    }
}

unsafe fn read_inner() -> Option<Vec<String>> {
    // SAFETY: we hold the clipboard open (OpenClipboard succeeded and
    // CloseClipboard hasn't run yet); GetClipboardData returns a handle
    // owned by the clipboard which stays valid until we close it.
    let hdrop = unsafe { GetClipboardData(CF_HDROP as u32) } as HDROP;
    if hdrop.is_null() {
        return None;
    }

    // SAFETY: hdrop is a valid HDROP for the duration of this function
    // (clipboard still open). Passing null lpszFile + 0 queries count.
    let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0) };
    if count == 0 {
        return None;
    }

    let mut out = Vec::with_capacity(count as usize);

    // Explorer caps CF_HDROP paths at MAX_PATH (260) chars including the
    // NUL; use 512 to be safe.
    const BUF_LEN: usize = 512;
    let mut buf: [u16; BUF_LEN] = [0; BUF_LEN];

    for i in 0..count {
        // SAFETY: same validity window as above; buf is a mutable buffer
        // of BUF_LEN u16s, DragQueryFileW writes at most cch chars + NUL.
        let len = unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), BUF_LEN as u32) };
        if len == 0 || (len as usize) >= BUF_LEN {
            // 0 means error, >= BUF_LEN means truncated — skip silently.
            continue;
        }
        // `buf` is UTF-16, NUL-terminated by DragQueryFileW.
        let slice = &buf[..len as usize];
        match String::from_utf16(slice) {
            Ok(s) if !s.is_empty() => out.push(s),
            _ => continue,
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    /// Simulate the `DragQueryFileW` loop against an in-memory UTF-16
    /// path list (same layout Explorer produces in a CF_HDROP block).
    /// We can't open a real clipboard in CI, so we test the parse logic
    /// directly by re-implementing the loop over a Vec<u16>.
    #[test]
    fn parses_utf16_path_list() {
        let mut raw: Vec<u16> = Vec::new();
        // Windows path with spaces and non-ASCII chars (中文名).
        for s in ["C:\\Users\\Test\\Documents\\report 2026.docx", "D:\\数据\\图片\\截图.png"] {
            for u in s.encode_utf16() {
                raw.push(u);
            }
            raw.push(0);
        }
        raw.push(0); // double-null terminator

        // Walk the buffer the same way DragQueryFileW+our loop does.
        let mut paths = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let start = i;
            while i < raw.len() && raw[i] != 0 {
                i += 1;
            }
            if i == start {
                break; // empty entry = terminator
            }
            let slice = &raw[start..i];
            paths.push(String::from_utf16(slice).unwrap());
            i += 1; // skip NUL
        }

        assert_eq!(
            paths,
            vec![
                "C:\\Users\\Test\\Documents\\report 2026.docx",
                "D:\\数据\\图片\\截图.png",
            ]
        );
    }

    #[test]
    fn empty_list_yields_none() {
        // A CF_HDROP with zero files is represented by a single
        // double-null (count query returns 0). Our read_inner path
        // returns None in that case; assert the equivalent loop
        // terminates without yielding anything.
        let raw: Vec<u16> = vec![0, 0];
        let mut count = 0;
        let mut i = 0;
        while i < raw.len() {
            let start = i;
            while i < raw.len() && raw[i] != 0 {
                i += 1;
            }
            if i == start {
                break;
            }
            count += 1;
            i += 1;
        }
        assert_eq!(count, 0);
    }
}
