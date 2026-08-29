//! macOS clipboard reader using `NSPasteboard`.
//!
//! Files copied from Finder are exposed on the pasteboard under
//! `NSFilenamesPboardType` (legacy) or `NSPasteboardTypeFileURL` /
//! `NSPasteboardTypeURL` (modern UTI `public.file-url` / `public.url`).
//! The DOM `ClipboardEvent` in WKWebView does not surface these for
//! security / sandboxing reasons, so we ask the pasteboard ourselves
//! via the Objective-C runtime.
//!
//! ## Crash history
//!
//! 2026-08-29: `CrashReport acowork-desktop-2026-08-29-212327.ips`
//! showed a `SIGABRT` with stack
//! `__rust_foreign_exception → catch_unwind::cleanup` triggered from
//! inside `respond_async_serialized_inner` — i.e. NSException leaked
//! through tokio's `task::harness::poll_future` catch_unwind boundary,
//! Rust std treated it as a foreign exception and aborted the process.
//!
//! The fix is two-layered:
//!
//! 1. **Main-thread dispatch** (in `commands::clipboard`): the
//!    `#[tauri::command]` body now hops to the main thread via
//!    `AppHandle::run_on_main_thread` before calling
//!    `read_clipboard_file_paths()`. Cocoa pasteboard code expects
//!    main-thread access in several places and will throw NSException
//!    if called otherwise.
//!
//! 2. **NSException catch** (this file): every `msg_send!` is wrapped
//!    in `objc2::exception::catch`. NSException that does fire (e.g.
//!    from `propertyListForType:` when the pasteboard item is not a
//!    property list, from `NSString initWithBytes:` on bytes the
//!    receiver considers malformed, or from `unrecognized selector`
//!    after AppKit updates) is converted into `Err(...)` instead of
//!    unwinding through Rust's `catch_unwind` and aborting.
//!
//! ## Strategy
//!
//!   1. Wrap the entire pasteboard read in `objc2::exception::catch`.
//!   2. Try modern UTI types first (`NSPasteboardTypeFileURL`,
//!      `NSPasteboardTypeURL`), fall back to legacy
//!      `NSFilenamesPboardType` for older macOS / Finder versions.
//!   3. Use `readObjectsForClasses:options:` — pasteboard-typed read
//!      that returns `nil` (not an NSException) when types do not
//!      match, instead of `propertyListForType:` which assumes the
//!      item is a property list and throws NSException otherwise.
//!   4. Decode each returned `NSURL` via `path` (filesystem path) or
//!      `absoluteString` (file:// URI fallback).

use objc2::exception::catch;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{class, msg_send};

// Modern UTI strings (preferred). These are the recommended Apple
// pasteboard types for file URLs — `NSFilenamesPboardType` is deprecated
// since macOS 10.14 and may be removed in future releases.
const NS_PASTEBOARD_TYPE_FILE_URL: &str = "public.file-url";
const NS_PASTEBOARD_TYPE_URL: &str = "public.url";
// Legacy type — kept as a fallback for users on older OS versions or
// third-party apps that still write the deprecated type.
const NS_FILENAMES_PBOARD_TYPE: &str = "NSFilenamesPboardType";

const NS_UTF8_STRING_ENCODING: usize = 4;

pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
    // MAIN-THREAD CONTRACT (see `commands::clipboard` module docs):
    //
    // Tauri spawns the surrounding `#[tauri::command]` onto a tokio
    // worker. The caller dispatches this function to the main thread
    // via `AppHandle::run_on_main_thread`. We assert it here so a
    // future refactor that drops the dispatch fails loudly in debug
    // builds instead of silently crashing production with NSException.
    debug_assert!(
        objc2::MainThreadMarker::new().is_some(),
        "macos::read_clipboard_file_paths must run on the main thread — \
         dispatch via AppHandle::run_on_main_thread (see commands::clipboard)"
    );

    // Wrap the entire ObjC read in `@try/@catch` via objc2. NSException
    // becomes `Err(...)` and is logged; the function returns `None`
    // (which the caller treats as "no files on clipboard", causing a
    // safe fall-through to plain-text paste). Without this guard, any
    // NSException here would unwind across tokio's `catch_unwind` in
    // `task::harness::poll_future` and abort the process via
    // `__rust_foreign_exception`.
    //
    // The closure is `UnwindSafe` because we do not expose Rust
    // `RefUnwindSafe` state into it; all values inside are either
    // plain `String`s or raw `*mut AnyObject` pointers that we never
    // dereference past the function boundary.
    let result = catch(|| -> Option<Vec<String>> {
        autoreleasepool(|_| unsafe { read_pasteboard_paths_inner() })
    });

    match result {
        Ok(paths) => paths,
        Err(Some(exc)) => {
            // objc2 retained the NSException object for us. Pull its name
            // and reason for the log. NOTE: `catch()` has already unwound
            // the @try boundary by the time we get here, so wrap the
            // description reads in a *second* `catch` — `-[NSException
            // name]`/`-[NSException reason]` are well-behaved in practice,
            // but an exception raised here would otherwise unwind into
            // tokio's `catch_unwind` and abort the process we just saved.
            let described = catch(|| unsafe { describe_exception(&exc) });
            let (name, reason) = match described {
                Ok(v) => v,
                Err(_) => (None, None),
            };
            tracing::warn!(
                target: "commands::clipboard",
                "NSPasteboard read raised NSException name={name:?} reason={reason:?}; \
                 falling through to plain-text paste"
            );
            None
        }
        Err(None) => {
            tracing::warn!(
                target: "commands::clipboard",
                "NSPasteboard read raised a null NSException; \
                 falling through to plain-text paste"
            );
            None
        }
    }
}

/// Inner read — assumes we are inside an `objc2::exception::catch`
/// boundary AND an autorelease pool. Returns `None` if the pasteboard
/// holds no readable file paths (or holds only text).
unsafe fn read_pasteboard_paths_inner() -> Option<Vec<String>> {
    let cls = AnyClass::get(c"NSPasteboard")?;
    let pasteboard: *mut AnyObject = msg_send![cls, generalPasteboard];
    if pasteboard.is_null() {
        return None;
    }

    // Preferred path: `readObjectsForClasses:options:` with the
    // `NSURL` class. This is the modern, type-safe pasteboard read.
    // It returns `nil` (NOT an NSException) when the pasteboard item
    // is not an NSURL, which is what we want.
    if let Some(paths) = unsafe { read_urls_via_classes(pasteboard) }
        && !paths.is_empty()
    {
        return Some(paths);
    }

    // Fallback: legacy `NSFilenamesPboardType` (deprecated but still
    // emitted by some Finder / third-party copy operations).
    unsafe { read_filenames_via_property_list(pasteboard) }
}

/// Read the pasteboard via the typed `readObjectsForClasses:options:`
/// API. Returns the resolved filesystem paths for any NSURLs the
/// pasteboard currently holds.
unsafe fn read_urls_via_classes(pasteboard: *mut AnyObject) -> Option<Vec<String>> {
    // Build an `NSArray` containing the `NSURL` class.
    //
    // `readObjectsForClasses:options:` REQUIRES an NSArray of classes
    // (the first argument is `NSArray<Class>`). The previous code passed
    // an NSSet here; NSPasteboard iterates the classes with
    // `objectAtIndex:`, which an NSSet does not implement, so every read
    // raised `NSInvalidArgumentException` (caught upstream → logged → the
    // modern NSURL path silently never worked).
    let url_class = class!(NSURL);
    let classes_obj: *mut AnyObject = msg_send![class!(NSArray),
        arrayWithObject: url_class
    ];
    if classes_obj.is_null() {
        return None;
    }

    // -readObjectsForClasses:options: returns NSArray<id<NSPasteboardReading>> *
    // or nil. We pass an empty options dictionary.
    let opts: *mut AnyObject = std::ptr::null_mut();
    let array: *mut AnyObject = msg_send![pasteboard,
        readObjectsForClasses: classes_obj,
        options: opts
    ];
    if array.is_null() {
        return None;
    }

    let count: usize = msg_send![array, count];
    if count == 0 {
        return None;
    }

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let obj: *mut AnyObject = msg_send![array, objectAtIndex: i];
        if obj.is_null() {
            continue;
        }
        // Only accept file URLs. A pasteboard can legitimately hold
        // http(s) URLs (e.g. copied web links); `fileSystemRepresentation`
        // on a non-file URL returns its path component — a bare-domain
        // URL yields "/" which would pass the absolute-path + exists()
        // sanitizer in `commands::clipboard` and be mis-treated as a
        // file to upload. Gate on `isFileURL` so non-file URLs fall
        // through to plain-text paste.
        let is_file: bool = msg_send![obj, isFileURL];
        if !is_file {
            continue;
        }
        // Each object is an NSURL. Ask for its filesystem path first;
        // fall back to `path` (the URL's path component).
        let path_ptr: *const i8 = msg_send![obj, fileSystemRepresentation];
        let path = if !path_ptr.is_null() {
            unsafe { std::ffi::CStr::from_ptr(path_ptr) }
                .to_str()
                .ok()
                .map(|s| s.to_string())
        } else {
            None
        };
        let resolved = path.or_else(|| {
            // `path` returns an NSString * or nil — use `UTF8String`
            // to get a C string we can decode as UTF-8.
            let ns: *mut AnyObject = msg_send![obj, path];
            if ns.is_null() {
                None
            } else {
                let utf8: *const i8 = msg_send![ns, UTF8String];
                if utf8.is_null() {
                    None
                } else {
                    unsafe { std::ffi::CStr::from_ptr(utf8) }
                        .to_str()
                        .ok()
                        .map(|s| s.to_string())
                }
            }
        });
        if let Some(p) = resolved
            && !p.is_empty()
        {
            out.push(p);
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Legacy path — decode `NSFilenamesPboardType` as a property list
/// (`NSArray<NSString>`). Kept for older Finder / third-party sources
/// that still write the deprecated type. This is the path that the
/// original 2026-08-29 crash stack traversed.
unsafe fn read_filenames_via_property_list(pasteboard: *mut AnyObject) -> Option<Vec<String>> {
    let type_nsstr = unsafe { make_nsstring(NS_FILENAMES_PBOARD_TYPE) }?;
    let array: *mut AnyObject = msg_send![pasteboard, propertyListForType: type_nsstr];
    let _: () = msg_send![type_nsstr, release];

    if array.is_null() {
        // Also probe the modern UTI types via propertyList for the
        // rare case of a pasteboard that stores a string instead of
        // an NSArray.
        if let Some(urls) = unsafe { read_string_type(pasteboard, NS_PASTEBOARD_TYPE_FILE_URL) } {
            return Some(urls);
        }
        if let Some(urls) = unsafe { read_string_type(pasteboard, NS_PASTEBOARD_TYPE_URL) } {
            return Some(urls);
        }
        return None;
    }

    let count: usize = msg_send![array, count];
    if count == 0 {
        return None;
    }

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let obj: *mut AnyObject = msg_send![array, objectAtIndex: i];
        if obj.is_null() {
            continue;
        }
        let utf8: *const i8 = msg_send![obj, UTF8String];
        if utf8.is_null() {
            continue;
        }
        if let Ok(s) = unsafe { std::ffi::CStr::from_ptr(utf8) }.to_str()
            && !s.is_empty()
        {
            out.push(s.to_string());
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Read a single string type (`public.file-url`, `public.url`) and
/// decode it as a single file path. Most modern pasteboards put file
/// URLs here as a single newline-separated string.
unsafe fn read_string_type(pasteboard: *mut AnyObject, uti: &str) -> Option<Vec<String>> {
    let type_nsstr = unsafe { make_nsstring(uti) }?;
    let s: *mut AnyObject = msg_send![pasteboard, stringForType: type_nsstr];
    let _: () = msg_send![type_nsstr, release];
    if s.is_null() {
        return None;
    }
    let utf8: *const i8 = msg_send![s, UTF8String];
    if utf8.is_null() {
        return None;
    }
    let raw = unsafe { std::ffi::CStr::from_ptr(utf8) }
        .to_str()
        .ok()?
        .to_string();
    // A file:// URL; strip the scheme. URL-decode in case of %20 etc.
    let mut paths = Vec::new();
    for line in raw.lines() {
        if let Some(path) = file_url_to_path(line) {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Convert one clipboard line into a filesystem path.
///
/// Accepts three forms:
///   - `file:///Users/foo/bar`           → `/Users/foo/bar`
///   - `file://localhost/Users/foo/bar`  → `/Users/foo/bar`
///   - an already-plain absolute path    → as-is
///
/// The `file://` form is percent-decoded (`%20` → space, …). Returns
/// `None` for empty lines, for `file://` URLs whose path segment is
/// not absolute, and for non-`file://` lines that are not absolute
/// paths (a bare web URL such as `https://example.com/` carried by the
/// `public.url` pasteboard type must NOT be treated as a file path).
///
/// Kept as a pure function so the URL→path conversion is unit-testable
/// without touching the pasteboard.
fn file_url_to_path(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("file://") {
        // `file://localhost/Users/foo` ≡ `file:///Users/foo` — drop an
        // optional `localhost` authority but KEEP the leading '/' of the
        // absolute path. (The previous implementation chained
        // `trim_start_matches('/')`, which turned "/Users/foo" into the
        // relative "Users/foo"; the absolute-path sanitizer in
        // `commands::clipboard` then silently discarded every entry, so
        // this fallback never returned anything.)
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        if !path.starts_with('/') {
            return None;
        }
        let decoded = percent_decode(path);
        return if decoded.is_empty() { None } else { Some(decoded) };
    }
    // Non-file:// lines: only absolute filesystem paths are acceptable.
    if trimmed.starts_with('/') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Minimal percent-decode (only the chars we expect to see in a
/// `file://` URL). We avoid pulling in the `percent-encoding` crate
/// here — this path is rare (modern pasteboards emit NSURLs, which
/// `read_urls_via_classes` already handles), so keeping the
/// dependency surface small is worth a hand-rolled implementation.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Allocate a retained `NSString` from a Rust string. Returns nil only
/// if the Objective-C runtime fails to allocate (OOM).
///
/// CRITICAL: return the object produced by `initWithBytes:length:encoding:`,
/// NOT the `alloc` result. `[NSString alloc]` returns an abstract
/// `NSPlaceholderString`; the init method may return a *different* object
/// (e.g. `__NSCFString`). Handing the placeholder to pasteboard APIs raises
/// `NSInvalidArgumentException` ("-length only defined for abstract class")
/// — which production catches and turns into a silent "no files" read.
unsafe fn make_nsstring(s: &str) -> Option<*mut AnyObject> {
    let cls = AnyClass::get(c"NSString")?;
    let nsstr: *mut AnyObject = msg_send![cls, alloc];
    if nsstr.is_null() {
        return None;
    }
    let initialized: *mut AnyObject = msg_send![nsstr,
                                      initWithBytes: s.as_ptr(),
                                      length: s.len(),
                                      encoding: NS_UTF8_STRING_ENCODING];
    if initialized.is_null() {
        return None;
    }
    Some(initialized)
}

/// Best-effort extraction of the NSException's `name` and `reason`.
/// Returns `(name, reason)` as owned `Option<String>`s.
///
/// Must itself be called inside an `objc2::exception::catch` boundary:
/// `msg_send!` to the exception object can in theory raise (e.g.
/// `unrecognized selector` after an AppKit update). `read_clipboard_file_paths`
/// wraps this call in a nested `catch` for exactly that reason.
unsafe fn describe_exception(exc: &objc2::exception::Exception) -> (Option<String>, Option<String>) {
    let name_obj: *mut AnyObject = msg_send![exc, name];
    let reason_obj: *mut AnyObject = msg_send![exc, reason];
    (
        unsafe { nsstring_to_owned(name_obj) },
        unsafe { nsstring_to_owned(reason_obj) },
    )
}

unsafe fn nsstring_to_owned(ns: *mut AnyObject) -> Option<String> {
    if ns.is_null() {
        return None;
    }
    let utf8: *const i8 = msg_send![ns, UTF8String];
    if utf8.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(utf8) }
        .to_str()
        .ok()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::class;
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;

    // ── percent_decode / hex ────────────────────────────────────────────

    #[test]
    fn percent_decode_decodes_escaped_bytes() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("no-escapes"), "no-escapes");
    }

    #[test]
    fn percent_decode_preserves_invalid_sequences() {
        // Lone '%', truncated escape, invalid hex pair — all left as-is.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%2"), "%2");
        assert_eq!(percent_decode("%zz"), "%zz");
        // Trailing bare '%' at the very end is preserved.
        assert_eq!(percent_decode("abc%"), "abc%");
    }

    #[test]
    fn hex_maps_ascii_hex_digits() {
        assert_eq!(hex(b'0'), Some(0));
        assert_eq!(hex(b'9'), Some(9));
        assert_eq!(hex(b'a'), Some(10));
        assert_eq!(hex(b'f'), Some(15));
        assert_eq!(hex(b'A'), Some(10));
        assert_eq!(hex(b'F'), Some(15));
        assert_eq!(hex(b'g'), None);
        assert_eq!(hex(b' '), None);
    }

    // ── file_url_to_path (P1 regression: leading '/' must survive) ────

    #[test]
    fn file_url_to_path_keeps_leading_slash() {
        // Regression for the P1 bug: the old implementation chained
        // `trim_start_matches('/')` and produced a RELATIVE "Users/foo",
        // which the absolute-path sanitizer in commands::clipboard then
        // silently discarded.
        assert_eq!(
            file_url_to_path("file:///Users/foo/bar.txt"),
            Some("/Users/foo/bar.txt".to_string())
        );
    }

    #[test]
    fn file_url_to_path_strips_localhost_authority() {
        assert_eq!(
            file_url_to_path("file://localhost/Users/foo/bar.txt"),
            Some("/Users/foo/bar.txt".to_string())
        );
    }

    #[test]
    fn file_url_to_path_percent_decodes() {
        assert_eq!(
            file_url_to_path("file:///Users/foo/my%20file.txt"),
            Some("/Users/foo/my file.txt".to_string())
        );
        assert_eq!(
            file_url_to_path("file://localhost/Users/%E4%B8%AD%E6%96%87.txt"),
            Some("/Users/中文.txt".to_string())
        );
    }

    #[test]
    fn file_url_to_path_passes_through_plain_paths() {
        assert_eq!(
            file_url_to_path("/Users/foo/bar.txt"),
            Some("/Users/foo/bar.txt".to_string())
        );
    }

    #[test]
    fn file_url_to_path_rejects_non_absolute_and_empty() {
        assert_eq!(file_url_to_path(""), None);
        assert_eq!(file_url_to_path("   "), None);
        // file:// with no path segment (or a relative one) is not a file.
        assert_eq!(file_url_to_path("file://Users/foo"), None);
        assert_eq!(file_url_to_path("file://"), None);
        // Non-file:// lines must be absolute paths — a web URL carried by
        // the public.url pasteboard type is not a file path.
        assert_eq!(file_url_to_path("https://example.com/"), None);
        assert_eq!(file_url_to_path("Users/foo/bar.txt"), None);
    }

    #[test]
    fn file_url_to_path_trims_surrounding_whitespace() {
        assert_eq!(
            file_url_to_path("  file:///Users/foo/bar.txt  "),
            Some("/Users/foo/bar.txt".to_string())
        );
    }

    // ── Real NSPasteboard end-to-end (ignored by default) ────────────────
    //
    // These tests exercise the FULL pasteboard read path against the real
    // macOS NSPasteboard: they programmatically write file URLs / strings /
    // plain text into the system clipboard and verify
    // `read_pasteboard_paths_inner` resolves them. They are `#[ignore]`d
    // because they (a) mutate the user's real clipboard and (b) require a
    // logged-in GUI session (CI runners cannot open NSPasteboard). Run
    // locally with:
    //
    //     cargo test -p acowork-desktop -- --ignored clipboard::macos
    //
    // The production main-thread dispatch (`AppHandle::run_on_main_thread`)
    // is exercised by the running app, not by this harness — cargo test
    // threads are never the main thread, so `read_clipboard_file_paths`'s
    // `debug_assert!` cannot pass here. We test the ObjC core it dispatches
    // to; the pasteboard read itself is thread-agnostic in the normal case
    // (the main-thread hop exists to avoid the *exception* case).

    /// Read the plain-text representation of the clipboard (for backup/restore).
    fn read_plain_text() -> Option<String> {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            if pb.is_null() {
                return None;
            }
            let typ = make_nsstring("public.utf8-plain-text")?;
            let s: *mut AnyObject = msg_send![pb, stringForType: typ];
            let _: () = msg_send![typ, release];
            if s.is_null() {
                return None;
            }
            nsstring_to_owned(s)
        })
    }

    fn set_plain_text(text: &str) {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            let _: () = msg_send![pb, clearContents];
            let ns = make_nsstring(text).unwrap();
            let typ = make_nsstring("public.utf8-plain-text").unwrap();
            let _: bool = msg_send![pb, setString: ns, forType: typ];
            let _: () = msg_send![ns, release];
            let _: () = msg_send![typ, release];
        });
    }

    /// Write a single string under the given UTI (e.g. `public.file-url`).
    fn set_string_type(uti: &str, value: &str) {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            let _: () = msg_send![pb, clearContents];
            let ns = make_nsstring(value).unwrap();
            let typ = make_nsstring(uti).unwrap();
            let _: bool = msg_send![pb, setString: ns, forType: typ];
            let _: () = msg_send![ns, release];
            let _: () = msg_send![typ, release];
        });
    }

    /// Write `NSURL` objects to the pasteboard via `writeObjects:` — this is
    /// exactly what Finder does when you copy files.
    fn write_url_objects(urls: &[&str]) {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            let _: () = msg_send![pb, clearContents];
            let arr: *mut AnyObject = msg_send![class!(NSMutableArray), array];
            for u in urls {
                let ns = make_nsstring(u).unwrap();
                let url: *mut AnyObject = msg_send![class!(NSURL), URLWithString: ns];
                let _: () = msg_send![ns, release];
                if !url.is_null() {
                    let _: () = msg_send![arr, addObject: url];
                }
            }
            let _: bool = msg_send![pb, writeObjects: arr];
        });
    }

    fn clear_pasteboard() {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            let _: () = msg_send![pb, clearContents];
        });
    }

    /// The exact read core production dispatches to (main-thread contract is
    /// enforced at `read_clipboard_file_paths`, see module docs).
    fn read_inner() -> Option<Vec<String>> {
        autoreleasepool(|_| unsafe { read_pasteboard_paths_inner() })
    }

    /// RAII-lite: back up plain text, clear, restore on drop.
    struct ClipboardGuard {
        backup: Option<String>,
    }

    impl ClipboardGuard {
        fn new() -> Self {
            let backup = read_plain_text();
            clear_pasteboard();
            Self { backup }
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            clear_pasteboard();
            if let Some(ref t) = self.backup {
                set_plain_text(t);
            }
        }
    }

    /// Serialise the real-clipboard E2E tests: cargo test runs tests in
    /// parallel by default, and two tests mutating the shared system
    /// pasteboard at the same time corrupt each other's autorelease pools
    /// (SIGSEGV). Each E2E body must hold this lock.
    fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    #[ignore = "mutates the real clipboard; run with -- --ignored clipboard::macos"]
    fn e2e_reads_file_urls_from_real_pasteboard() {
        let _l = clipboard_lock();
        let _guard = ClipboardGuard::new();
        let dir = std::env::temp_dir().join(format!(
            "acowork-clipboard-e2e-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("alpha.txt");
        let f2 = dir.join("beta.txt");
        std::fs::write(&f1, b"").unwrap();
        std::fs::write(&f2, b"").unwrap();

        // Simulate Finder "copy files": NSURL objects on the pasteboard.
        write_url_objects(&[
            &format!("file://{}", f1.to_string_lossy()),
            &format!("file://{}", f2.to_string_lossy()),
        ]);

        let paths = read_inner().expect("read should succeed");
        assert!(paths.iter().any(|p| p == f1.to_str().unwrap()));
        assert!(paths.iter().any(|p| p == f2.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mutates the real clipboard; run with -- --ignored clipboard::macos"]
    fn e2e_reads_file_url_string_type_from_real_pasteboard() {
        let _l = clipboard_lock();
        let _guard = ClipboardGuard::new();
        let dir = std::env::temp_dir().join(format!(
            "acowork-clipboard-e2e-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("spaced file.txt");
        std::fs::write(&f, b"").unwrap();

        // Some apps only write the modern UTI as a plain string.
        set_string_type("public.file-url", &format!("file://{}", f.to_string_lossy()));

        let paths = read_inner().expect("read should succeed");
        // Regression: the old code stripped the leading '/' and produced a
        // relative path that the sanitizer silently dropped.
        assert!(
            paths.iter().any(|p| p == f.to_str().unwrap()),
            "expected {:?} in {:?}",
            f,
            paths
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "mutates the real clipboard; run with -- --ignored clipboard::macos"]
    fn e2e_ignores_plain_text_on_real_pasteboard() {
        use objc2::exception::catch;
        let r = catch(|| {
            let _l = clipboard_lock();
            let _guard = ClipboardGuard::new();
            set_plain_text("just some text, not a file");
            let out = read_inner();
            assert_eq!(out, None);
        });
        if let Err(e) = r {
            let (name, reason) = match catch(|| unsafe { describe_exception(&e.unwrap()) }) {
                Ok(v) => v,
                Err(_) => (None, None),
            };
            panic!("NSException name={name:?} reason={reason:?}");
        }
    }

    #[test]
    #[ignore = "mutates the real clipboard; run with -- --ignored clipboard::macos"]
    fn e2e_ignores_non_file_urls_on_real_pasteboard() {
        let _l = clipboard_lock();
        let _guard = ClipboardGuard::new();
        // A copied web link: NSURL object that is NOT a file URL.
        write_url_objects(&["https://example.com/"]);
        // isFileURL gate must reject it (a bare-domain URL would otherwise
        // resolve to "/" and slip past the absolute-path sanitizer).
        let out = read_inner();
        assert_eq!(out, None, "expected None for non-file URL, got {out:?}");
    }
}
