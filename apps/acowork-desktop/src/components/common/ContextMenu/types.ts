// src/components/common/ContextMenu/types.ts
//
// Shared shapes for the unified context-menu subsystem.
//
// Three layers, smallest to largest:
//
//   1. `ContextMenuItem<TPayload>` — declarative description of one row.
//      Stable `key`, optional icon, visible label, click handler, optional
//      disabled / variant / divider-before flags. TPayload is the type of
//      context that the menu was opened with (e.g. `{ agentId: string }`)
//      — propagated into the click handler so item builders don't need to
//      reach for stale closures.
//
//   2. `ContextMenuClickContext<TPayload>` — the bag passed into every
//      `onClick`. Holds the original mouse event (for `stopPropagation` /
//      analytics), the typed payload captured at right-click, and
//      `selectionAtOpen` — the snapshot of `window.getSelection()` taken at
//      the moment the menu opened. The selection snapshot is what lets a
//      Copy item still read the text the user actually dragged over,
//      despite button focus having cleared the live selection by click
//      time.
//
//   3. Item variant — one of the three CSS modifier classes already
//      declared in `styles/globals.css` (`.context-menu-item`,
//      `.context-menu-item--danger`, `.context-menu-item--warning`). Kept
//      as a string-literal union so a typo is a type error.

import type { MouseEvent as ReactMouseEvent, ReactNode } from "react";

/**
 * Visual variant — maps to the CSS modifier classes declared in
 * `styles/globals.css` next to `.context-menu-item`. The default variant
 * uses no modifier class.
 */
export type ContextMenuItemVariant = "default" | "danger" | "warning";

/**
 * The bag passed into every menu-item `onClick`. Keep it small — every
 * field here is in scope for every item, even those that don't need it.
 *
 * - `event`           — the original React mouse event from the click.
 *                       Useful for `event.stopPropagation()` when a click
 *                       should not bubble into an outer container's click
 *                       handler.
 * - `payload`         — whatever the caller captured at right-click time
 *                       (e.g. `{ agentId }`, `{ sessionId }`,
 *                       `{ fileId }`). `undefined` for menus without a
 *                       payload.
 * - `selectionAtOpen` — the document selection's text content at the
 *                       instant the user right-clicked. **This is the
 *                       field that fixes the MessageBubble copy bug.** It is
 *                       captured before any focus shift from the menu's
 *                       own `<button>` elements can clear the selection.
 */
export interface ContextMenuClickContext<TPayload> {
  event: ReactMouseEvent<HTMLButtonElement>;
  payload: TPayload | undefined;
  selectionAtOpen: string;
}

/**
 * Declarative description of a single row in a context menu. Items are
 * declared as plain objects so callers can compose them with the usual
 * React data-flow (`useMemo`, conditional spread, …) without learning a
 * new API.
 *
 * The wrapper component (`ContextMenu`) closes the menu after invoking
 * `onClick`, and short-circuits when `disabled` is true. Callers should
 * NOT call `close()` from inside `onClick` themselves.
 */
export interface ContextMenuItem<TPayload = undefined> {
  /** Stable key for React reconciliation. Required. */
  key: string;
  /** Lucide-style icon shown on the left of the label. Optional. */
  icon?: ReactNode;
  /** Visible label — usually `t("...")`. */
  label: ReactNode;
  /**
   * Click handler. The wrapper closes the menu after invocation. Receives
   * a `ContextMenuClickContext` so the handler can read the captured
   * payload and the selection snapshot.
   */
  onClick: (ctx: ContextMenuClickContext<TPayload>) => void | Promise<void>;
  /** When true, the row is rendered disabled and `onClick` is skipped. */
  disabled?: boolean;
  /** Tooltip — rendered as the row's native `title` attribute. */
  title?: string;
  /** Visual variant — `"danger"` or `"warning"` add the matching CSS class. */
  variant?: ContextMenuItemVariant;
  /** When true, a `<div class="context-menu-divider" />` is rendered before this row. */
  dividerBefore?: boolean;
}