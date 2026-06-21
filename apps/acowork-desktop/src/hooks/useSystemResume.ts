import { useEffect, useRef, useCallback } from "react";
import { listen } from "@tauri-apps/api/event";

/**
 * Detects system resume from sleep/hibernation and triggers webview recovery.
 *
 * Three detection layers (whichever fires first wins):
 *
 * 1. **Heartbeat** — a `setInterval` every 5 s. When the system sleeps, JS is
 *    suspended; on wake the next tick sees a gap >> intervalMs.
 * 2. **`visibilitychange`** — secondary signal for cases where the document
 *    was hidden (minimised, locked screen) rather than the whole system
 *    sleeping.
 * 3. **Tauri `"system-resume"` event** — the Rust backend tracks
 *    `Focused(true)` events and emits this when the gap exceeds 30 s. This
 *    is a backup for cases where the JS event loop was fully suspended and
 *    hasn't ticked yet after wake.
 *
 * Recovery action: `location.reload()` — the only method proven to
 * reinitialise WebView2's GPU compositor after it crashes during sleep.
 * React state is lost, but Zustand persisted stores and localStorage
 * survive the reload, so the app returns to (nearly) the same state.
 *
 * The hook should be mounted once, as high in the tree as possible (App.tsx).
 */
export function useSystemResume() {
    const lastTickRef = useRef<number>(Date.now());

    const recover = useCallback(() => {
        console.warn(
            "[useSystemResume] System resume detected — reloading webview to recover GPU compositor",
        );
        window.location.reload();
    }, []);

    useEffect(() => {
        const INTERVAL_MS = 5_000;
        const SLEEP_GAP_MS = 30_000;

        // ── Layer 1: Heartbeat — detect sleep via time gap ──────────────────
        const timer = setInterval(() => {
            const now = Date.now();
            const gap = now - lastTickRef.current;
            lastTickRef.current = now;

            if (gap > SLEEP_GAP_MS) {
                recover();
            }
        }, INTERVAL_MS);

        // ── Layer 2: Visibility change — secondary signal ───────────────────
        const onVisibilityChange = () => {
            if (document.visibilityState === "visible") {
                const now = Date.now();
                const gap = now - lastTickRef.current;
                lastTickRef.current = now;

                if (gap > SLEEP_GAP_MS) {
                    recover();
                }
            }
        };

        document.addEventListener("visibilitychange", onVisibilityChange);

        // ── Layer 3: Tauri backend "system-resume" event (backup) ───────────
        // The Rust backend tracks Focused(true) events and emits "system-resume"
        // when the gap exceeds 30 s.  Catches the edge case where JS was fully
        // suspended and the heartbeat hasn't ticked yet after wake.
        let unlistenTauri: (() => void) | undefined;
        listen("system-resume", () => {
            console.warn("[useSystemResume] Received system-resume event from Tauri backend");
            recover();
        }).then((fn) => {
            unlistenTauri = fn;
        });

        return () => {
            clearInterval(timer);
            document.removeEventListener("visibilitychange", onVisibilityChange);
            unlistenTauri?.();
        };
    }, [recover]);
}
