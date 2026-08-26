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

use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;

const NS_FILENAMES_PBOARD_TYPE: &str = "NSFilenamesPboardType";
const NS_UTF8_STRING_ENCODING: usize = 4;

pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
    // `propertyListForType:` returns an autoreleased object, so the whole
    // body must run inside an autorelease pool; manually releasing it would
    // over-release and crash when the pool drains.
    autoreleasepool(|_| {
        unsafe {
            let cls = AnyClass::get(c"NSPasteboard")?;
            let pasteboard: *mut AnyObject = msg_send![cls, generalPasteboard];
            if pasteboard.is_null() {
                return None;
            }

            // propertyListForType: returns `id` (NSArray of NSString) or nil.
            let type_nsstr = make_nsstring(NS_FILENAMES_PBOARD_TYPE)?;
            let array: *mut AnyObject = msg_send![pasteboard, propertyListForType: type_nsstr];
            let _: () = msg_send![type_nsstr, release];

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
                let utf8: *const i8 = msg_send![obj, UTF8String];
                if utf8.is_null() {
                    continue;
                }
                // Cocoa returns UTF-8 here for filesystem paths.
                let cstr = std::ffi::CStr::from_ptr(utf8);
                if let Ok(s) = cstr.to_str()
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
    })
}

/// Allocate a retained `NSString` from a Rust string. Returns nil only
/// if the Objective-C runtime fails to allocate (OOM).
fn make_nsstring(s: &str) -> Option<*mut AnyObject> {
    unsafe {
        let cls = AnyClass::get(c"NSString")?;
        let nsstr: *mut AnyObject = msg_send![cls, alloc];
        if nsstr.is_null() {
            return None;
        }
        let _: *mut AnyObject = msg_send![nsstr,
                                          initWithBytes: s.as_ptr(),
                                          length: s.len(),
                                          encoding: NS_UTF8_STRING_ENCODING];
        Some(nsstr)
    }
}
