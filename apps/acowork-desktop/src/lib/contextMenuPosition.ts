// src/lib/contextMenuPosition.ts
//
// Pure viewport-collision detection for `position: fixed` context menus.
//
// Why this exists:
//   Every context-menu site in the app previously used raw `e.clientX/Y` as
//   the menu's `left/top`. That guarantees an overflow when the user
//   right-clicks near the bottom (or right) edge of the viewport — the menu
//   gets clipped because nothing checks how much vertical/horizontal room is
//   actually available.
//
//   This module owns ONE piece of behaviour: given
//     - where the user clicked (`pointer`)
//     - how big the rendered menu is (`menuSize`)
//     - how big the visible viewport is (`viewport`)
//   …decide where the menu should land and whether it should flip above the
//   cursor.
//
// Design constraints:
//   - Zero React / DOM dependencies. Testable in isolation.
//   - The algorithm is the same one VS Code / Windows Explorer / macOS Finder
//     use: try the preferred side, flip when it overflows, clamp to the
//     viewport edge.
//
// Units: every input/output coordinate is in CSS pixels relative to the
// viewport's top-left — i.e. the same coordinate space as `MouseEvent.clientX`
// and `getBoundingClientRect()`. The hook layer (see
// `src/hooks/useContextMenuPosition.ts`) is responsible for feeding in the
// right numbers.

/** Sizing of the viewport visible to the user. Falls back to `window.inner*`
 *  when no `visualViewport` is available (desktop browsers without pinch-zoom
 *  always land here). */
export interface ViewportSize {
  /** Visible width in CSS pixels. */
  width: number;
  /** Visible height in CSS pixels. */
  height: number;
}

/** Measured size of the rendered menu. The hook layer produces this via
 *  `getBoundingClientRect()` + `ResizeObserver`. */
export interface MenuSize {
  width: number;
  height: number;
}

/** Position of the user's right-click in viewport coordinates. */
export interface PointerPosition {
  x: number;
  y: number;
}

export type MenuPlacement = "down" | "up";

export interface ComputedMenuPosition {
  /** Final `left` for `position: fixed`. Always inside `[margin, viewport.width - menu.width - margin]`. */
  x: number;
  /** Final `top` for `position: fixed`. Always inside `[margin, viewport.height - menu.height - margin]`. */
  y: number;
  /** Which side of the cursor the menu opened on. Useful for analytics or
   *  for callers that want to add an arrow/caret pointing back at the
   *  trigger. */
  placement: MenuPlacement;
}

/** Minimum gap (px) between the menu and the viewport edge. Keeps the menu
 *  from touching the OS window border — feels claustrophobic otherwise. */
export const DEFAULT_MENU_MARGIN = 4;

/**
 * Compute where a `position: fixed` context menu should be placed so it
 * stays fully inside the viewport and flips above the cursor when there is
 * not enough room below.
 *
 * Algorithm:
 *   1. Vertical placement:
 *      a. Try `down`: `y = pointer.y`. Fits iff `pointer.y + height + margin ≤ viewport.height`.
 *      b. Try `up`:   `y = pointer.y - height`. Fits iff `pointer.y - height - margin ≥ 0`.
 *      c. Pick the side with more remaining space; on a tie prefer `down` to
 *         match the natural reading flow.
 *   2. Horizontal clamp:
 *      a. Start at `x = pointer.x`.
 *      b. If overflowing right, pull left until `x + width + margin = viewport.width`.
 *      c. If that overshoots the left edge, pin to `margin`.
 *
 * The function never throws and never returns NaN — degenerate inputs
 * (`menuSize.width ≤ 0`, viewport of zero) collapse to `margin`.
 */
export function computeMenuPosition(
  pointer: PointerPosition,
  menuSize: MenuSize,
  viewport: ViewportSize,
  margin: number = DEFAULT_MENU_MARGIN,
): ComputedMenuPosition {
  const safeMargin = Math.max(0, margin);

  // Degenerate-input guard: nothing rendered yet or a zero-sized viewport.
  if (menuSize.width <= 0 || menuSize.height <= 0) {
    return { x: safeMargin, y: safeMargin, placement: "down" };
  }
  if (viewport.width <= 0 || viewport.height <= 0) {
    return { x: safeMargin, y: safeMargin, placement: "down" };
  }

  // ── Vertical: flip above when the cursor sits in the bottom band ──
  const downTop = pointer.y;
  const downOverflow = pointer.y + menuSize.height + safeMargin - viewport.height;
  const downFits = downOverflow <= 0;

  const upTop = pointer.y - menuSize.height;
  const upOverflow = safeMargin - upTop; // positive when menu would escape the top
  const upFits = upOverflow <= 0;

  let placement: MenuPlacement;
  let y: number;
  if (downFits) {
    placement = "down";
    y = downTop;
  } else if (upFits) {
    placement = "up";
    y = upTop;
  } else {
    // Neither side fits cleanly — pick the side with more leftover space.
    const downSpace = viewport.height - pointer.y - safeMargin;
    const upSpace = pointer.y - safeMargin;
    if (upSpace > downSpace) {
      placement = "up";
      y = upTop;
    } else {
      placement = "down";
      y = downTop;
    }
  }

  // ── Horizontal: clamp inside the viewport ──
  let x = pointer.x;
  const rightOverflow = x + menuSize.width + safeMargin - viewport.width;
  if (rightOverflow > 0) {
    x = x - rightOverflow;
  }
  if (x < safeMargin) {
    x = safeMargin;
  }
  // Belt-and-braces: if the menu is somehow wider than the viewport, pin
  // the left edge and accept that something will spill off the right.
  if (x + menuSize.width > viewport.width) {
    x = Math.max(safeMargin, viewport.width - menuSize.width - safeMargin);
  }

  return { x, y, placement };
}
