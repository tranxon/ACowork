//! System sleep/wake detection for the Node Agent.
//!
//! The node shares the machine with the desktop app; on Windows Modern
//! Standby / macOS / Linux suspend the whole process freezes. After
//! wake the MQTT poll task can stall on the stale TCP connection until
//! its 20 s watchdog fires (2026-08 incident: 5 sleep/wake cycles,
//! each freezing node + embed for 3-16 minutes; the LSP relay
//! heartbeat accumulated the exact sleep durations, e.g. 944 s).
//!
//! The desktop app already recovers via this biased/unbiased clock
//! trick (apps/acowork-desktop/src-tauri/src/lib.rs `mod power`); this
//! module is the portable core of that detection so the node can force
//! a fresh MQTT connection immediately on resume instead of waiting
//! for the watchdog.

use std::sync::atomic::{AtomicU64, Ordering};

static LAST_BIASED_MS: AtomicU64 = AtomicU64::new(0);
static LAST_UNBIASED_MS: AtomicU64 = AtomicU64::new(0);

/// Minimum *actual* sleep duration (ms) to trigger recovery. We
/// measure real sleep, not wall-clock gaps, so even a few seconds is
/// significant; 5 s filters timer imprecision (same threshold as the
/// desktop app).
const SLEEP_THRESHOLD_MS: u64 = 5_000;

// ── Windows FFI ──────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
unsafe extern "system" {
    fn GetTickCount64() -> u64;
    fn QueryUnbiasedInterruptTime(unbiased_time: *mut u64) -> i32;
}

/// Returns `(biased_ms, unbiased_ms)` where biased includes time spent
/// in sleep / suspend and unbiased excludes it. Returns `None` on API
/// failure or on unsupported platforms.
fn sample() -> Option<(u64, u64)> {
    #[cfg(target_os = "windows")]
    {
        unsafe {
            let biased_ms = GetTickCount64();
            let mut unbiased_100ns: u64 = 0;
            if QueryUnbiasedInterruptTime(&mut unbiased_100ns) == 0 {
                return None; // API failure
            }
            Some((biased_ms, unbiased_100ns / 10_000))
        }
    }

    #[cfg(target_os = "macos")]
    {
        // CLOCK_MONOTONIC_RAW advances during sleep; CLOCK_UPTIME_RAW does not.
        sample_unix(libc::CLOCK_MONOTONIC_RAW, libc::CLOCK_UPTIME_RAW)
    }

    #[cfg(target_os = "linux")]
    {
        // CLOCK_BOOTTIME includes suspend time; CLOCK_MONOTONIC does not.
        sample_unix(libc::CLOCK_BOOTTIME, libc::CLOCK_MONOTONIC)
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None // Unsupported platform — no sleep detection
    }
}

/// Shared `clock_gettime` helper for macOS and Linux.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sample_unix(
    biased_clk: libc::clockid_t,
    unbiased_clk: libc::clockid_t,
) -> Option<(u64, u64)> {
    fn read_clk(clk: libc::clockid_t) -> Option<u64> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(clk, &mut ts) } != 0 {
            return None;
        }
        Some((ts.tv_sec as u64) * 1_000 + (ts.tv_nsec as u64) / 1_000_000)
    }
    Some((read_clk(biased_clk)?, read_clk(unbiased_clk)?))
}

/// Returns `true` if the system was genuinely asleep (not merely idle
/// or backgrounded) since the last call.
///
/// Updates the clock baseline on every call so that subsequent calls
/// measure sleep since the *last* call, not since boot. The first call
/// only seeds the baseline and returns `false`. Callers poll this on a
/// fixed interval and run their recovery action on `true`.
pub fn detect_resume() -> bool {
    let Some((biased_ms, unbiased_ms)) = sample() else {
        return false; // API failure or unsupported platform
    };

    let prev_biased = LAST_BIASED_MS.swap(biased_ms, Ordering::Relaxed);
    let prev_unbiased = LAST_UNBIASED_MS.swap(unbiased_ms, Ordering::Relaxed);

    if prev_biased == 0 || prev_unbiased == 0 {
        return false; // First call — seed values, don't trigger
    }

    let biased_delta = biased_ms.saturating_sub(prev_biased);
    let unbiased_delta = unbiased_ms.saturating_sub(prev_unbiased);
    let sleep_ms = biased_delta.saturating_sub(unbiased_delta);

    if sleep_ms > SLEEP_THRESHOLD_MS {
        tracing::info!(
            sleep_ms,
            biased_delta_ms = biased_delta,
            unbiased_delta_ms = unbiased_delta,
            "Actual system sleep detected"
        );
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_seeds_baseline_without_triggering() {
        // The first invocation only seeds the baseline, so it must
        // never report a resume.
        assert!(!detect_resume());
        // An immediate second call sees no sleep and must not trigger.
        assert!(!detect_resume());
    }

    #[test]
    fn sample_reads_monotonic_pairs() {
        // On every supported platform the clocks must return increasing
        // pairs; biased >= unbiased always (biased includes sleep).
        let (biased, unbiased) = sample().expect("clock sampling must work");
        assert!(biased >= unbiased);
    }
}
