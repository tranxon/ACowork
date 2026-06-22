import { useEffect, useCallback } from "react";
import { listen } from "@tauri-apps/api/event";

/**
 * Detects system resume from sleep/hibernation and triggers webview recovery.
 *
 * **Detection is handled entirely by the Rust backend** (Windows / macOS / Linux).
 * The backend samples two monotonic clocks on each `Focused(true)` event — one
 * that includes sleep time (biased) and one that excludes it (unbiased).  The
 * difference between their deltas is the *exact* amount of time the system spent
 * asleep — zero for a normal minimise/restore, non-zero for a real sleep/wake.
 *
 * Platform clock pairs:
 *   • Windows: `GetTickCount64()` vs `QueryUnbiasedInterruptTime()`
 *   • macOS:   `clock_gettime(CLOCK_MONOTONIC_RAW)` vs `CLOCK_UPTIME_RAW`
 *   • Linux:   `clock_gettime(CLOCK_BOOTTIME)` vs `CLOCK_MONOTONIC`
 *
 * Previous versions used frontend time-gap heuristics (heartbeat +
 * visibilitychange) which could not distinguish "window minimised for N
 * seconds" from "system slept for N seconds", causing false `reload()`
 * triggers on normal minimise → restore cycles.
 *
 * When real sleep is detected, the backend emits `"system-resume"` and this
 * hook calls `location.reload()` to reinitialise the webview's GPU compositor,
 * which can crash during sleep on some configurations.
 *
 * The hook should be mounted once, as high in the tree as possible (App.tsx).
 */
export function useSystemResume() {
    const recover = useCallback(() => {
        console.warn(
            "[useSystemResume] System resume detected — reloading webview to recover GPU compositor",
        );
        // Flag for App.tsx to skip the splash screen on recovery reload.
        // sessionStorage survives location.reload() but is cleared on tab close,
        // so it won't interfere with future cold starts.
        sessionStorage.setItem("acowork_recovery_reload", "1");
        window.location.reload();
    }, []);

    useEffect(() => {
        let unlisten: (() => void) | undefined;
        listen("system-resume", () => {
            console.warn("[useSystemResume] Received system-resume event from Tauri backend");
            recover();
        }).then((fn) => {
            unlisten = fn;
        });

        return () => {
            unlisten?.();
        };
    }, [recover]);
}
