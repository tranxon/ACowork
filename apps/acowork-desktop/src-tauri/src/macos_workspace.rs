//! macOS: post-wake display signal via `NSWorkspaceDidWakeNotification`.
//!
//! On macOS, `NSWorkspace` posts `NSWorkspaceDidWakeNotification` to
//! `[NSWorkspace sharedWorkspace].notificationCenter` when the system
//! resumes from sleep. The notification is the AppKit analogue of
//! Windows' `GUID_MONITOR_POWER_ON` (delivered via the WndProc subclass
//! in `win_wndproc.rs`) and is the right event to feed into
//! `wake_recovery::signal_display_ready()`: it arrives after the
//! display stack has come back, and the post-wake webview reload
//! converges on the same code path as Windows and Linux.
//!
//! Without this signal source, macOS recovery falls back to the
//! cross-platform `Focused(true)` (gained only when the user clicks
//! the window) plus the 5 s `wake_recovery` timeout — both of which
//! converge correctly via the reload/verify/retry loop in
//! `recover_from_wake`, but the first reload lands a few seconds later
//! than on Windows. The WndProc `GUID_MONITOR_POWER_ON` analogue is
//! what closes that gap.
//!
//! # Threading
//!
//! `[NSNotificationCenter addObserverForName:object:queue:usingBlock:]`
//! invokes the supplied block on the thread that posted the
//! notification. NSWorkspace notifications are posted on the main
//! thread, so the block runs on the main thread. The block body is a
//! single mutex write (`signal_display_ready()`), so the thread
//! affinity does not matter — but the block itself must be a
//! `copy()`-retained block, because Foundation retains only the heap
//! block and a stack block is invalid after its declaring function
//! returns. `block2::StackBlock::new(...).copy()` is the canonical
//! idiom for that.
//!
//! # Failure modes
//!
//! Each step is a separate `objc2::msg_send!` whose return value we
//! null-check. Any failure logs and returns without installing the
//! observer — the caller's `Focused(true)` + 5 s timeout path is
//! unaffected, so this is a pure best-effort signal source (the
//! `recover_from_wake` reload/verify/retry still converges without
//! it).
//!
//! # Why a separate file
//!
//! Lives in `macos_workspace.rs` rather than `clipboard/macos.rs`
//! because the clipboard file is a synchronous, on-demand pasteboard
//! read; this module installs a long-lived observer at process
//! startup. Mixing the two would force the startup observer to drag
//! in the pasteboard module's NSException handling and main-thread
//! dispatch guard for no reason.
//!
//! # Why not the `macos-private-api` Tauri feature
//!
//! Tauri exposes some macOS lifecycle events through its
//! `RunEvent::Reopen` path, but there is no `RunEvent::Wake` — macOS
//! does not surface a system-wake event to NSApplication at all. The
//! only programmatic way to observe a system wake on macOS is
//! `NSWorkspace`'s notification center, which is what we use here.

/// Install an `NSWorkspace.didWakeNotification` observer that feeds
/// `wake_recovery::signal_display_ready()` on every system wake.
///
/// Must be called from the main thread (matches AppKit's requirement
/// for `NSWorkspace` / `NSNotificationCenter` access). The Tauri
/// `setup` hook runs on the main thread, so the caller is
/// responsible for invoking this from there.
pub fn install() {
    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use std::ffi::c_void;

    // MAIN-THREAD CONTRACT: NSWorkspace and NSNotificationCenter
    // expect main-thread access (they interact with NSApplication
    // state). Tauri invokes `setup` on the main thread, so this
    // function is only safe to call from there. A debug assert guards
    // against future refactors that move the call site.
    debug_assert!(
        objc2::MainThreadMarker::new().is_some(),
        "macos_workspace::install must run on the main thread (call from Tauri setup hook)"
    );

    // `autoreleasepool` scopes autoreleased ObjC objects (NSString
    // returned by `stringWithUTF8String:`, the observer token) to
    // this function. Outside an autoreleasepool these objects would
    // leak into the main thread's per-run default pool, which is
    // fine but unidiomatic.
    let install_result = autoreleasepool(|_| -> Result<(), String> {
        // SAFETY: each `msg_send!` below calls a method that is
        // safe to invoke on the main thread with the given argument
        // types. Return values are null-checked before use. NSException
        // is not caught here because none of these calls are known
        // to raise in normal use (unlike NSPasteboard); a misstep
        // logs and bails, which is the right degradation.
        unsafe {
            let workspace_cls = class!(NSWorkspace);
            let workspace: *mut AnyObject = msg_send![workspace_cls, sharedWorkspace];
            if workspace.is_null() {
                return Err("+[NSWorkspace sharedWorkspace] returned nil".into());
            }

            let center: *mut AnyObject = msg_send![workspace, notificationCenter];
            if center.is_null() {
                return Err("-[NSWorkspace notificationCenter] returned nil".into());
            }

            // `NSWorkspaceDidWakeNotification` is declared as an
            // `extern NSNotificationName const` in AppKit. We get
            // the same constant by name through NSString — the
            // notification-center key comparison is `isEqualToString:`,
            // so an equivalent string is correct.
            let name_cstr = b"NSWorkspaceDidWakeNotification\0";
            let name_ns: *mut AnyObject = msg_send![
                class!(NSString),
                stringWithUTF8String: name_cstr.as_ptr().cast::<c_void>()
            ];
            if name_ns.is_null() {
                return Err("+[NSString stringWithUTF8String:] returned nil".into());
            }

            // The block runs on the main thread (NSWorkspace posts
            // on main) and only writes a mutex-backed atomic. We do
            // NOT call back into Tauri from here — `signal_display_ready`
            // is safe from any thread and that's the whole point of
            // going through the `wake_recovery` module instead of
            // touching `recover_from_wake` directly.
            //
        // `StackBlock::new` takes a `'static` closure, and
        // the body captures nothing — it only references
        // `crate::wake_recovery::signal_display_ready`, a free
        // function, so the closure is trivially `'static`.
        let handler = block2::StackBlock::new(move |_notif: *mut AnyObject| {
            crate::wake_recovery::signal_display_ready();
        });
            // `.copy()` returns a heap-allocated, retain-counted
            // block. NSNotificationCenter retains the block, so a
            // copy is required — a stack block is invalid after this
            // function returns.
            let block = handler.copy();

            // `queue: std::ptr::null_mut()` means "post on the
            // posting thread" — we don't need a serial queue because
            // the block does no AppKit work.
            //
            // The selector is
            // `addObserverForName:object:queue:usingBlock:` and
            // returns an observer token (an opaque NSObject*) we
            // intentionally drop: Foundation ties the observer's
            // lifetime to the center's, and we never want to remove
            // it (the wake signal must work for the whole process
            // lifetime).
            let observer: *mut AnyObject = msg_send![
                center,
                addObserverForName: name_ns,
                object: std::ptr::null_mut::<AnyObject>(),
                queue: std::ptr::null_mut::<AnyObject>(),
                usingBlock: &*block,
            ];
            if observer.is_null() {
                return Err(
                    "-[NSNotificationCenter addObserverForName:object:queue:usingBlock:] returned nil"
                        .into(),
                );
            }
        }
        Ok(())
    });

    match install_result {
        Ok(()) => tracing::info!(
            "macOS NSWorkspace wake notification observer installed (NSWorkspaceDidWakeNotification)"
        ),
        Err(e) => tracing::warn!(
            "Failed to install macOS NSWorkspace wake observer: {e} - post-wake display signal \
             falls back to Focused(true) + 5s timeout (recover_from_wake still converges)"
        ),
    }
}
