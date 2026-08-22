// src/components/common/ContextMenu/useContextMenu.ts
//
// The state machine behind every context menu in the app.
//
// Before this hook existed, every site that wanted a right-click menu
// hand-rolled the same five pieces:
//
//   1. `useState` for `{ x, y } | null` (sometimes augmented with a
//      payload: `{ agentId, x, y }`, `{ sessionId, x, y }`, …).
//   2. A `useEffect([state])` that registered `document.mousedown` and
//      `document.keydown` listeners to close the menu on outside click or
//      Escape.
//   3. A `useContextMenuPosition` hook call for flip-above + edge-clamp.
//   4. A `<div ref={menuRef} className="context-menu" style={style}>` plus
//      a `createPortal(..., document.body)` so the menu escapes any
//      `transform: translateY(...)` ancestor.
//   5. Manual `setState(null)` calls inside every item's onClick so the
//      menu disappears after the user picks something.
//
// All five were duplicated across MessageBubble, AgentList, SessionTabBar,
// FileEditorPanel, and FileTreeNode. The duplicated parts also shipped
// subtly different close semantics — and all five share the same latent
// bug fixed here: clicking a menu `<button>` previously cleared the page's
// text selection (WebKit / WKWebView behaviour), so any "Copy" item that
// read `window.getSelection()` inside its click handler saw an empty
// string. The fix is to **snapshot the selection at right-click time**,
// before any focus shift can clear it.
//
// Usage:
//
//   type Payload = { sessionId: string };
//   const menu = useContextMenu<Payload>();
//
//   return (
//     <div onContextMenu={(e) => menu.openAt(e, { sessionId })}>
//       …children…
//       <ContextMenu
//         isOpen={menu.isOpen}
//         menuProps={menu.menuProps}
//         items={[
//           { key: "close", label: "Close", onClick: ({ payload }) =>
//             payload && closeSession(payload.sessionId) },
//         ]}
//         payload={menu.payload}
//         selectionAtOpen={menu.selectionAtOpen}
//         onClose={menu.close}
//       />
//     </div>
//   );

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type RefObject,
} from "react";
import { useContextMenuPosition } from "../../../hooks/useContextMenuPosition";
import { snapshotSelection } from "../../../lib/clipboard";

interface OpenState<TPayload> {
  /** Pointer position at right-click, in viewport coords. */
  pointer: { x: number; y: number };
  /** Whatever the caller passed to `openAt`. */
  payload: TPayload;
  /**
   * `window.getSelection().toString()` at right-click time. Stored in
   * state (not just a ref) so React renders that depend on it (e.g. a
   * disabled flag on a Copy item) re-run when a new selection exists.
   *
   * Note: the bug we are fixing here is that **reading the selection at
   * click time returns empty** in WKWebView — `<button>` focus clears it.
   * Capturing it at open time sidesteps the problem entirely.
   */
  selection: string;
}

/**
 * What the hook returns. Three grouped fields:
 *
 * - `open / close / isOpen / payload / selectionAtOpen` — the data.
 * - `menuProps` — the `{ ref, style }` pair to spread onto the menu div.
 *   Pre-wired through `useContextMenuPosition` so callers don't have to
 *   call it themselves.
 */
export interface UseContextMenuResult<TPayload> {
  /** Open the menu at the right-clicked cursor position. */
  openAt: (e: ReactMouseEvent, payload?: TPayload) => void;
  /** Imperatively close. Normally the wrapper component calls this for you. */
  close: () => void;
  /** True between `openAt` and `close` (or an outside-click / Escape). */
  isOpen: boolean;
  /** The payload from the most recent `openAt`, or `undefined`. */
  payload: TPayload | undefined;
  /**
   * The text the user had selected at the moment the menu opened. Use this
   * instead of `window.getSelection()` in click handlers — see the file
   * header for why.
   */
  selectionAtOpen: string;
  /** Spread onto the menu div: `<div {...menuProps}>…</div>`. */
  menuProps: {
    ref: RefObject<HTMLDivElement | null>;
    style: React.CSSProperties & {
      left: number | undefined;
      top: number | undefined;
      visibility: "hidden" | "visible";
    };
  };
}

/**
 * Default `TPayload` is `undefined`. When a caller does not need any
 * context (e.g. a flat "New File / New Folder" menu), `openAt` may be
 * called with no second argument.
 */
export function useContextMenu<TPayload = undefined>(): UseContextMenuResult<TPayload> {
  const [openState, setOpenState] = useState<OpenState<TPayload> | null>(null);

  // Tracking the latest open state through a ref so the outside-click
  // listener effect does not need to re-bind on every render. Without
  // this, an effect dep on `openState` would force teardown/rebind every
  // time the open/close transitions through React's batched updates.
  const openStateRef = useRef<OpenState<TPayload> | null>(null);
  openStateRef.current = openState;

  // ── Position computation ───────────────────────────────────────────────
  // We feed `useContextMenuPosition` only the pointer coords; the rest of
  // its logic (size measurement, ResizeObserver, flip-above) is the same
  // one the legacy per-site menus were using.
  const { menuRef, style } = useContextMenuPosition({
    pointer: openState ? openState.pointer : null,
  });

  // ── Open / close ───────────────────────────────────────────────────────
  const openAt = useCallback((e: ReactMouseEvent, payload?: TPayload) => {
    // Always preventDefault the native context menu — we own this gesture.
    e.preventDefault();
    // Bubble-stop is the caller's choice (callers usually want this when
    // the menu lives inside a virtualized list / draggable container);
    // legacy implementations all called stopPropagation here, so we keep
    // that contract.
    e.stopPropagation();

    // Snapshot the selection BEFORE any focus shift can clear it. This is
    // the single line that fixes the MessageBubble copy bug — see the
    // file header.
    const selection = snapshotSelection();

    setOpenState({
      pointer: { x: e.clientX, y: e.clientY },
      payload: payload as TPayload,
      selection,
    });
  }, []);

  const close = useCallback(() => {
    setOpenState(null);
  }, []);

  // ── Outside-click + Escape ─────────────────────────────────────────────
  // The five legacy implementations all did the same dance. Two important
  // details that the new one MUST preserve (and that the old code
  // accidentally preserved only because the menu was rendered via
  // `createPortal` to `document.body`):
  //
  //   1. The "menu container" check uses `menuRef.current.contains(...)`.
  //      When the click hits a button INSIDE the menu, that check is
  //      `true`, so we DON'T close. This is what lets the item's
  //      onClick fire on the same gesture as the mousedown — without
  //      this, the menu would vanish the instant the user pressed the
  //      mouse down on a Copy / Delete button.
  //
  //   2. We listen on `mousedown`, not `click`. A `click` listener would
  //      fire AFTER the button's own onClick — meaning a button click
  //      would both run the action AND close the menu via the outside
  //      handler. Listening on `mousedown` is the documented "click
  //      outside" pattern for popovers.
  //
  // React 18 batches state updates from native listeners, so the close()
  // call schedules a re-render but does not synchronously unmount the
  // menu div. The user's click on the button therefore completes
  // (button's onClick fires the action) before React commits the close.
  useEffect(() => {
    if (!openState) return;

    const handleMouseDown = (e: MouseEvent) => {
      const node = menuRef.current;
      if (!node) return;
      if (!node.contains(e.target as Node)) {
        close();
      }
    };
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };

    document.addEventListener("mousedown", handleMouseDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handleMouseDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [openState, close, menuRef]);

  return {
    openAt,
    close,
    isOpen: openState !== null,
    // The cast is safe: when `isOpen` is true, `payload` is whatever the
    // caller passed at openAt time. When false, callers should not read it.
    payload: openState?.payload as TPayload | undefined,
    selectionAtOpen: openState?.selection ?? "",
    menuProps: { ref: menuRef, style },
  };
}