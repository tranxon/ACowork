// src/hooks/useContextMenuPosition.ts
//
// React adapter around the pure `computeMenuPosition` (see
// `src/lib/contextMenuPosition.ts`). Feeds it the measured menu size and
// the live viewport, then exposes a `{menuRef, style}` pair ready to drop
// onto the menu element.
//
// What it does:
//   - Measures the menu with `getBoundingClientRect()` after mount, then
//     every time the menu's content size changes (`ResizeObserver`).
//   - Picks the visible viewport via `visualViewport` when available so
//     pinching / virtual keyboards don't crop the menu.
//   - Starts the menu at `visibility: hidden` so the user never sees the
//     first frame of unmeasured (and therefore possibly overflowing)
//     placement. The hook flips it to `visible` on the first measurement.
//
// What it deliberately does NOT do:
//   - Persist anything to a store / ref outside this component.
//   - Touch `pointer-events` — closing the menu on outside click stays the
//     caller's job; this hook only owns positioning.
//
// Why this does NOT use `pointer` as a layout-effect dependency:
//
//   Several callers pass `pointer` as a freshly-allocated inline object
//   every render (e.g. `pointer: { x: ctx.x, y: ctx.y }`). If `pointer`
//   were in the dep array, every render would tear down the ResizeObserver
//   and re-create it, and in some browsers the ResizeObserver callback
//   fires synchronously during `observe()` — which calls `setComputed`,
//   triggers a re-render, triggers the layout effect again, ad infinitum.
//
//   Instead, the hook uses a `useRef` to track the latest pointer and a
//   stringified-coordinate key (`measureKey`) to detect *meaningful*
//   changes — only then does it re-run the measurement.

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  type ComputedMenuPosition,
  type MenuSize,
  type PointerPosition,
  type ViewportSize,
  DEFAULT_MENU_MARGIN,
  computeMenuPosition,
} from "../lib/contextMenuPosition";

/** Returns the current visible viewport, falling back to the layout
 *  viewport when `visualViewport` is not present or returns zeros. */
function readViewport(): ViewportSize {
  if (typeof window === "undefined") {
    return { width: 0, height: 0 };
  }
  const vv = (window as Window & { visualViewport?: VisualViewport }).visualViewport;
  if (vv && vv.width > 0 && vv.height > 0) {
    return { width: vv.width, height: vv.height };
  }
  return { width: window.innerWidth, height: window.innerHeight };
}

/** Value-equality check for `ComputedMenuPosition` — same numeric payload
 *  ⇒ same effective position. Avoids producing a new object reference for
 *  a position that has not actually changed. */
function positionsEqual(a: ComputedMenuPosition | null, b: ComputedMenuPosition): boolean {
  if (a === null) return false;
  return a.x === b.x && a.y === b.y && a.placement === b.placement;
}

export interface UseContextMenuPositionOptions {
  /** Where the user right-clicked. Pass `null` to hide the menu. */
  pointer: PointerPosition | null;
  /** Optional override for the edge gap. Defaults to 4px. */
  margin?: number;
}

export interface UseContextMenuPositionResult {
  /** Attach to the menu container element so its size can be measured. */
  menuRef: React.RefObject<HTMLDivElement | null>;
  /** Style to spread onto the menu container. */
  style: React.CSSProperties & {
    left: number | undefined;
    top: number | undefined;
    visibility: "hidden" | "visible";
  };
}

/**
 * Hook that returns a ref + style for a viewport-aware context menu.
 *
 * Usage:
 *   const [pos, setPos] = useState<{ x: number; y: number } | null>(null);
 *   const { menuRef, style } = useContextMenuPosition({ pointer: pos });
 *   ...
 *   {pos && createPortal(
 *       <div ref={menuRef} className="context-menu" style={style}>...</div>,
 *       document.body)}
 */
export function useContextMenuPosition({
  pointer,
  margin = DEFAULT_MENU_MARGIN,
}: UseContextMenuPositionOptions): UseContextMenuPositionResult {
  const menuRef = useRef<HTMLDivElement>(null);
  const [computed, setComputed] = useState<ComputedMenuPosition | null>(null);

  // ── Track the latest pointer via ref so the layout effect does NOT need
  //    to depend on the pointer object reference (which is a new object
  //    every render for callers that create `{ x, y }` inline). ──────────
  const pointerRef = useRef(pointer);
  pointerRef.current = pointer;

  // ── Detect meaningful coordinate changes via a string key. Only when
  //    this key changes do we re-run the measurement. ────────────────────
  const [measureKey, setMeasureKey] = useState(0);
  const lastPointerStrRef = useRef<string | null>(null);
  const pointerStr = pointer ? `${pointer.x}:${pointer.y}` : null;
  if (pointerStr !== lastPointerStrRef.current) {
    lastPointerStrRef.current = pointerStr;
    // Reset to "unmeasured" so the next effect run starts from scratch.
    setComputed(null);
    setMeasureKey((k) => k + 1);
  }

  // ── Stable callback: compare by value, skip re-render when nothing changed. ──
  const setComputedIfChanged = useCallback((next: ComputedMenuPosition) => {
    setComputed((prev) => (positionsEqual(prev, next) ? prev : next));
  }, []);

  // ── Layout effect: measure & position the menu. ──────────────────────
  //    Dependencies: [measureKey, margin, setComputedIfChanged].
  //    - measureKey changes only when pointer coordinates change or the
  //      menu opens/closes — NOT on every render.
  //    - margin is stable (or changes rarely).
  //    - setComputedIfChanged is a useCallback with [] deps — stable.
  useLayoutEffect(() => {
    const p = pointerRef.current;
    if (!p) return; // menu is closed — nothing to measure
    const el = menuRef.current;
    if (!el) return;

    let rafId = 0;

    const measure = () => {
      const node = menuRef.current;
      if (!node) return;
      const rect = node.getBoundingClientRect();
      const menuSize: MenuSize = { width: rect.width, height: rect.height };
      const viewport = readViewport();

      // Menu might still be 0×0 on the very first paint (items committing,
      // fonts loading). Defer one animation frame and try again.
      if (menuSize.width <= 0 || menuSize.height <= 0) {
        if (rafId !== 0) return; // already scheduled a retry
        rafId = requestAnimationFrame(() => {
          rafId = 0;
          const node2 = menuRef.current;
          if (!node2) return;
          const r2 = node2.getBoundingClientRect();
          if (r2.width <= 0 || r2.height <= 0) return;
          setComputedIfChanged(
            computeMenuPosition(p, { width: r2.width, height: r2.height }, readViewport(), margin),
          );
        });
        return;
      }

      setComputedIfChanged(computeMenuPosition(p, menuSize, viewport, margin));
    };

    measure();

    if (typeof ResizeObserver === "undefined") {
      return () => {
        if (rafId !== 0) cancelAnimationFrame(rafId);
      };
    }
    const ro = new ResizeObserver(() => measure());
    ro.observe(el);
    return () => {
      ro.disconnect();
      if (rafId !== 0) cancelAnimationFrame(rafId);
    };
  }, [measureKey, margin, setComputedIfChanged]);

  // ── Viewport resize listener: re-measure when the window changes. ────
  useEffect(() => {
    const p = pointerRef.current;
    if (!p) return;
    const handler = () => {
      const node = menuRef.current;
      if (!node) return;
      const rect = node.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return;
      setComputedIfChanged(
        computeMenuPosition(p, { width: rect.width, height: rect.height }, readViewport(), margin),
      );
    };
    window.addEventListener("resize", handler);
    return () => window.removeEventListener("resize", handler);
  }, [measureKey, margin, setComputedIfChanged]);

  const visibility: "hidden" | "visible" = computed ? "visible" : "hidden";

  const style: React.CSSProperties & {
    left: number | undefined;
    top: number | undefined;
    visibility: "hidden" | "visible";
  } = {
    position: "fixed",
    left: computed?.x,
    top: computed?.y,
    visibility,
  };

  return { menuRef, style };
}