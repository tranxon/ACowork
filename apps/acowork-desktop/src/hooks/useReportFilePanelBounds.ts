import { useEffect, useLayoutEffect, type RefObject } from "react";
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
 *
 * Right-panel visibility: ResizeObserver only fires on *size* changes, but
 * the FileEditorPanel's viewport position shifts in the flex layout when
 * the right panel is shown/hidden even though its own size stays the same
 * (fixed fileWidth + shrink-0).  We subscribe to `resultsCollapsed` and
 * re-measure via a separate `useLayoutEffect` + `requestAnimationFrame` to
 * guarantee the DOM reflow has settled before reading the rect.
 */
export function useReportFilePanelBounds(
    ref: RefObject<HTMLElement | null>,
): void {
    const setFilePanelBounds = useLayoutStore((s) => s.setFilePanelBounds);
    const resultsCollapsed = useLayoutStore((s) => s.resultsCollapsed);

    // ── ResizeObserver — catches size-driven layout changes ────────────
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
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [ref, setFilePanelBounds]);

    // ── Right-panel visibility — re-measure immediately ────────────────
    // ResizeObserver only fires on *size* changes.  When the right panel is
    // shown/hidden the FileEditorPanel's viewport position shifts but its
    // own size stays the same (fixed fileWidth + shrink-0).  We use a
    // separate useLayoutEffect keyed on resultsCollapsed so the re-measure
    // is guaranteed to run before the next paint.
    useLayoutEffect(() => {
        const el = ref.current;
        if (!el) return;

        const rect = el.getBoundingClientRect();
        setFilePanelBounds({
            left: rect.left,
            right: rect.right,
            mounted: true,
        });
    }, [resultsCollapsed, setFilePanelBounds, ref]);
}
