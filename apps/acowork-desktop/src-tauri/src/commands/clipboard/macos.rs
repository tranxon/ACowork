//! macOS clipboard reader using `NSPasteboard`.
//!
//! Files copied from Finder are exposed on the pasteboard under
//! `NSFilenamesPboardType` (the legacy filename-list type). The DOM
//! `ClipboardEvent` in WKWebView does not surface this for security /
//! sandboxing reasons, so we ask the pasteboard ourselves via the
//! Objective-C runtime.
//!
//! Strategy:
//!   1. Get `[NSPasteboard generalPasteboard]`
//!   2. Read `propertyListForType:@"NSFilenamesPboardType"` which
//!      returns an `NSArray` of `NSString` absolute paths.
//!   3. Convert to `Vec<String>`. Each path is UTF-8 already because
//!      Cocoa normalises to UTF-8 on read.

use objc::msg_send;
use objc::runtime::{Class, Object};

const NS_FILENAMES_PBOARD_TYPE: &str = "NSFilenamesPboardType";
const NS_UTF8_STRING_ENCODING: usize = 4;

pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
    unsafe {
        let cls = Class::get("NSPasteboard")?;
        let pasteboard: *mut Object = msg_send![cls, generalPasteboard];
        if pasteboard.is_null() {
            return None;
        }

        // propertyListForType: returns `id` (NSArray of NSString) or nil.
        let type_nsstr = make_nsstring(NS_FILENAMES_PBOARD_TYPE)?;
        let array: *mut Object = msg_send![pasteboard, propertyListForType: type_nsstr];
        let _: () = msg_send![type_nsstr, release];

        if array.is_null() {
            return None;
        }

        let count: usize = msg_send![array, count];
        if count == 0 {
            let _: () = msg_send![array, release];
            return None;
        }

        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let obj: *mut Object = msg_send![array, objectAtIndex: i];
            if obj.is_null() {
                continue;
            }
            let utf8: *const i8 = msg_send![obj, UTF8String];
            if utf8.is_null() {
                continue;
            }
            // Cocoa returns UTF-8 here for filesystem paths.
            let cstr = std::ffi::CStr::from_ptr(utf8);
            if let Ok(s) = cstr.to_str() {
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
        }
        let _: () = msg_send![array, release];

        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

/// Allocate a retained `NSString` from a Rust string. Returns nil only
/// if the Objective-C runtime fails to allocate (OOM).
fn make_nsstring(s: &str) -> Option<*mut Object> {
    unsafe {
        let cls = Class::get("NSString")?;
        let nsstr: *mut Object = msg_send![cls, alloc];
        if nsstr.is_null() {
            return None;
        }
        let _: *mut Object = msg_send![nsstr, initWithBytes: s.as_ptr()
                                          length: s.len()
                                          encoding: NS_UTF8_STRING_ENCODING];
        Some(nsstr)
    }
}