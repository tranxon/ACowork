//! WndProc subclass: taskbar button fix for frameless windows.
//!
//! set_decorations(false) strips WS_SYSMENU and WS_MINIMIZEBOX from the
//! window style, which confuses Explorer's taskbar button:
//!   - Without WS_SYSMENU → right-click menu is broken
//!   - Without WS_MINIMIZEBOX → left-click shows the system menu instead
//!     of minimizing/restoring
//!
//! lib.rs restores WS_SYSMENU | WS_MINIMIZEBOX and rebuilds the system menu.
//! This WndProc subclass provides a safety net: if Explorer still sends
//! SC_MOUSEMENU/SC_KEYMENU on left-click, we convert it to a proper
//! minimize/restore toggle.

use std::ffi::c_void;
use std::sync::Mutex;

// ── Original WndProc ───────────────────────────────────────────────────────

static ORIG: Mutex<Option<unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize>> =
    Mutex::new(None);

// ── Win32 FFI ──────────────────────────────────────────────────────────────

unsafe extern "system" {
    fn GetWindowLongPtrW(h: *mut c_void, n: i32) -> isize;
    fn SetWindowLongPtrW(h: *mut c_void, n: i32, v: isize) -> isize;
    fn IsIconic(h: *mut c_void) -> i32;
    fn ShowWindow(h: *mut c_void, cmd: i32) -> i32;
}

const SW_MINIMIZE: i32 = 6;
const SW_RESTORE: i32 = 9;

// ── Custom WndProc ─────────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let orig = ORIG.lock().unwrap().expect("ORIG not set");

    match msg {
        // WM_SYSCOMMAND — taskbar clicks come through here
        0x0112 => {
            let cmd = wparam & 0xFFF0;
            match cmd {
                // SC_MOUSEMENU (0xF150) / SC_KEYMENU (0xF100):
                // explorer sends this on taskbar left-click for frameless
                // windows that lack WS_SYSMENU.  Instead of showing the
                // system menu (which immediately disappears), toggle
                // minimize/restore like a normal window.
                0xF150 | 0xF100 => {
                    if unsafe { IsIconic(hwnd) } != 0 {
                        unsafe { ShowWindow(hwnd, SW_RESTORE) };
                    } else {
                        unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
                    }
                    return 0;
                }
                _ => {} // Pass SC_MINIMIZE/SC_RESTORE/SC_CLOSE/etc. to orig
            }
        }

        _ => {}
    }

    unsafe { orig(hwnd, msg, wparam, lparam) }
}

// ── Public API ─────────────────────────────────────────────────────────────

pub unsafe fn install(hwnd: *mut c_void) -> Result<(), String> {
    let prev = unsafe { GetWindowLongPtrW(hwnd, -4) }; // GWL_WNDPROC = -4
    if prev == 0 {
        return Err("GetWindowLongPtrW(GWL_WNDPROC) returned 0".into());
    }
    {
        let mut g = ORIG.lock().unwrap();
        if g.is_some() {
            return Ok(());
        }
        *g = Some(unsafe { std::mem::transmute(prev) });
    }
    if unsafe { SetWindowLongPtrW(hwnd, -4, wndproc as *const () as isize) } == 0 {
        return Err("SetWindowLongPtrW(GWL_WNDPROC) failed".into());
    }
    tracing::info!("WndProc subclass installed (taskbar fix)");
    Ok(())
}
