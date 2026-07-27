import { useEffect, type RefObject } from "react";
import { useLayoutStore } from "../stores/layoutStore";

/**
 * Attach a `ResizeObserver` to the given element ref and report its
 * `getBoundingClientRect()` to `useLayoutStore.filePanelBounds` on every
 * layout change. Sets `mounted: false` on unmount so consumers (PR-2's
 * `<FileStatusCluster>` in the global status bar) can hide instead of
 * lingering at stale coordinates.
 *
 * Why a hook and not a wrapper component:
 *   - The element to observe is `FileEditorPanel`'s own root div, which
 *     already owns its `width = ${fileWidth}` style contract and its own
 *     outer wrapper layout. Forcing an extra wrapper would change that
 *     contract and could interfere with the rounded corner / overflow
 *     clipping (`rounded-xl overflow-hidden`).
 *   - It keeps the responsibility localized — the hook has no UI, no DOM
 *     children, just one subscription.
 *
 * Coalescing: a resize drag fires `ResizeObserver` at refresh-rate cadence.
 * We coalesce a frame's worth of callbacks via `requestAnimationFrame` so
 * Zustand does not see 60+ state mutations per second.
 *
 * Initial measurement: a synchronous `getBoundingClientRect()` runs on
 * mount so consumers do not flicker through `(0, 0)` for one frame before
 * the first observer callback.
 *
 * SSR / pre-hydration: no-op (the effect only runs client-side). Tauri
 * webview targets (WKWebView / WebView2 / WebKitGTK) all support
 * `ResizeObserver` natively, so no polyfill is required.
 */
export function useReportFilePanelBounds(
    ref: RefObject<HTMLElement | null>,
): void {
    const setFilePanelBounds = useLayoutStore((s) => s.setFilePanelBounds);
    // Subscribe to right-panel collapsed state so the effect re-runs (and
    // re-measures the panel's bounds) when the right panel is shown/hidden.
    // ResizeObserver only fires on *size* changes, but the FileEditorPanel's
    // viewport position can shift in the flex layout even when its own size
    // stays the same (fixed fileWidth + shrink-0).
    const resultsCollapsed = useLayoutStore((s) => s.resultsCollapsed);

    useEffect(() => {
        if (typeof ResizeObserver === "undefined") {
            return;
        }

        const el = ref.current;
        if (!el) {
            return;
        }

        const measure = () => {
            const rect = el.getBoundingClientRect();
            setFilePanelBounds({
                left: rect.left,
                right: rect.right,
                mounted: true,
            });
        };

        // Synchronous first read so subscribers see the correct bounds
        // immediately on mount rather than after the first rAF tick.
        measure();

        let rafId: number | null = null;
        const ro = new ResizeObserver(() => {
            if (rafId !== null) {
                return; // a frame is already pending — coalesce.
            }
            rafId = requestAnimationFrame(() => {
                rafId = null;
                measure();
            });
        });
        ro.observe(el);

        return () => {
            ro.disconnect();
            if (rafId !== null) {
                cancelAnimationFrame(rafId);
                rafId = null;
            }
            // Mark unmounted so any cluster hanging a fixed position over a
            // now-gone panel hides immediately rather than floating at the
            // last-known coordinates.
            setFilePanelBounds({ left: 0, right: 0, mounted: false });
        };
    }, [ref, setFilePanelBounds, resultsCollapsed]);
}
