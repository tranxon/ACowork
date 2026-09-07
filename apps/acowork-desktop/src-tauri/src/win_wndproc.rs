//! WndProc subclass: taskbar button fix + post-wake display signals.
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
//!
//! ## Post-wake display signals (renderer recovery)
//!
//! The same WndProc also observes the two Windows messages that say "the
//! display stack is usable again" after system sleep/wake:
//!
//!   - `WM_DISPLAYCHANGE` — the display mode was rebuilt.
//!   - `WM_POWERBROADCAST` / `PBT_POWERSETTINGCHANGE` with the
//!     `GUID_MONITOR_POWER_ON` power-setting GUID — the display was
//!     powered back on (registered via `RegisterPowerSettingNotification`
//!     in [`install`]).
//!
//! Both feed `crate::wake_recovery::signal_display_ready()`, the
//! event-driven trigger for the post-wake webview reload — see the
//! `wake_recovery` docs in lib.rs for why a real display event is used
//! instead of a hardcoded delay.

use std::ffi::c_void;
use std::sync::Mutex;

// ── Original WndProc ───────────────────────────────────────────────────────

static ORIG: Mutex<Option<unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize>> =
    Mutex::new(None);

/// Handle returned by `RegisterPowerSettingNotification`. Registration is
/// tied to the window handle, so the OS tears it down when the window is
/// destroyed; keeping the handle here (never unregistering) is fine for a
/// process-lifetime window. Stored as `usize` because raw pointers are not
/// `Send`, which a `static Mutex` requires.
static POWER_NOTIFY: Mutex<Option<usize>> = Mutex::new(None);

// ── Win32 FFI ──────────────────────────────────────────────────────────────

unsafe extern "system" {
    fn GetWindowLongPtrW(h: *mut c_void, n: i32) -> isize;
    fn SetWindowLongPtrW(h: *mut c_void, n: i32, v: isize) -> isize;
    fn IsIconic(h: *mut c_void) -> i32;
    fn ShowWindow(h: *mut c_void, cmd: i32) -> i32;
    /// user32!RegisterPowerSettingNotification — subscribes the window to
    /// power-setting change broadcasts (e.g. `GUID_MONITOR_POWER_ON`).
    fn RegisterPowerSettingNotification(
        h_recipient: *mut c_void,
        power_setting_guid: *const PowerSettingGuid,
        flags: u32,
    ) -> *mut c_void;
}

const SW_MINIMIZE: i32 = 6;
const SW_RESTORE: i32 = 9;

/// `POWERBROADCAST_SETTING`'s embedded GUID, laid out like the Win32
/// `GUID` (data1..3 are little-endian numeric fields; comparing
/// field-by-field against a constant built the same way is correct).
#[repr(C)]
struct PowerSettingGuid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Prefix of the `POWERBROADCAST_SETTING` structure delivered with
/// `WM_POWERBROADCAST` / `PBT_POWERSETTINGCHANGE`. The variable-length
/// `Data[1]` tail is only read (via `data`) when `data_length` says the
/// payload is at least 4 bytes.
#[repr(C)]
struct PowerBroadcastSetting {
    power_setting: PowerSettingGuid,
    data_length: u32,
    data: [u8; 4],
}

/// GUID_MONITOR_POWER_ON — the display has been powered back on.
/// `{02731015-4510-4526-99E6-E5A17EBD1AEA}`
const GUID_MONITOR_POWER_ON: PowerSettingGuid = PowerSettingGuid {
    data1: 0x0273_1015,
    data2: 0x4510,
    data3: 0x4526,
    data4: [0x99, 0xE6, 0xE5, 0xA1, 0x7E, 0xBD, 0x1A, 0xEA],
};

// Message ids / broadcast kinds (winuser.h).
const WM_DISPLAYCHANGE: u32 = 0x007E;
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_POWERSETTINGCHANGE: usize = 0x8013;
const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0x0000_0000;

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

        // WM_DISPLAYCHANGE — the display mode was rebuilt (common right
        // after resume). Part of the post-wake "display is usable"
        // signal set (see the module docs).
        WM_DISPLAYCHANGE => {
            crate::wake_recovery::signal_display_ready();
        }

        // WM_POWERBROADCAST / PBT_POWERSETTINGCHANGE — a registered
        // power setting changed. Only `GUID_MONITOR_POWER_ON` with data
        // == 1 matters: the display was powered back on, which is the
        // moment a manual F5 starts working after sleep.
        WM_POWERBROADCAST if wparam == PBT_POWERSETTINGCHANGE => {
            let setting = lparam as *const PowerBroadcastSetting;
            if !setting.is_null() {
                // SAFETY: for PBT_POWERSETTINGCHANGE the OS guarantees
                // lParam points to a POWERBROADCAST_SETTING.
                let s = unsafe { &*setting };
                let g = &s.power_setting;
                if g.data1 == GUID_MONITOR_POWER_ON.data1
                    && g.data2 == GUID_MONITOR_POWER_ON.data2
                    && g.data3 == GUID_MONITOR_POWER_ON.data3
                    && g.data4 == GUID_MONITOR_POWER_ON.data4
                    && s.data_length >= 4
                    && s.data[0] == 1
                {
                    crate::wake_recovery::signal_display_ready();
                }
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

    // Subscribe to power-setting broadcasts so `wndproc` sees
    // GUID_MONITOR_POWER_ON after system sleep/wake. Failure degrades
    // gracefully: wake recovery falls back to its 5 s timeout instead
    // of the display event.
    let notify = unsafe {
        RegisterPowerSettingNotification(hwnd, &GUID_MONITOR_POWER_ON, DEVICE_NOTIFY_WINDOW_HANDLE)
    };
    if notify.is_null() {
        tracing::warn!(
            "RegisterPowerSettingNotification failed - post-wake display signal unavailable (timeout fallback still applies)"
        );
    } else if let Ok(mut h) = POWER_NOTIFY.lock() {
        *h = Some(notify as usize);
    }

    tracing::info!("WndProc subclass installed (taskbar fix + wake display signals)");
    Ok(())
}
