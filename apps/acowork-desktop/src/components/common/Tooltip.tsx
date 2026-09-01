import { type ReactElement, useRef, useState, useCallback, useEffect } from "react";
import { createPortal, flushSync } from "react-dom";
import { cn } from "../../lib/utils";

/**
 * Unified tooltip component.
 *
 * - Default (no tipClass): rendered via React Portal to document.body
 *   so it is never clipped by parent overflow-hidden containers.
 * - With tipClass: rendered inline (CSS-positioned) to preserve
 *   @container query support for toolbar collapse behavior.
 *
 * Usage:
 *   <Tooltip content="Send message">
 *     <button>...</button>
 *   </Tooltip>
 */

type TooltipPosition = "top" | "bottom" | "left" | "right";
/**
 * Alignment perpendicular to `position`.
 *
 * For `top` / `bottom`: horizontal — `start` = tooltip's left edge aligns
 * with trigger's left edge, `end` = tooltip's right edge aligns with
 * trigger's right edge.
 * For `left` / `right`: vertical — `start` = tooltip's top edge aligns
 * with trigger's top edge, `end` = tooltip's bottom edge aligns with
 * trigger's bottom edge.
 *
 * Used by chat bubbles to keep the timestamp tooltip anchored to the
 * inner edge of an off-center bubble (user bubble → `start`, assistant
 * bubble → `end`) so the tooltip never overflows the screen edge.
 */
type TooltipAlign = "start" | "center" | "end";
type TooltipVariant = "inverted" | "plain";

interface TooltipProps {
  /** Tooltip text content */
  content: string;
  /** The trigger element — must be a single ReactElement */
  children: ReactElement;
  /** Tooltip position relative to trigger. Default: 'top' */
  position?: TooltipPosition;
  /**
   * Perpendicular alignment relative to trigger. Default: 'center'.
   * See `TooltipAlign` for the per-position semantics.
   */
  align?: TooltipAlign;
  /** Visual variant. Default: 'inverted' */
  variant?: TooltipVariant;
  /** Max width for long content. Default: '200px' */
  maxWidth?: string;
  /**
   * CSS class for container-query tooltip collapse (e.g. "tb-model-tip").
   * When provided, the tooltip is rendered inline (not via portal) so that
   * @container queries on the parent toolbar can hide it.
   */
  tipClass?: string;
  /** Delay before showing tooltip (ms). Default: 400 */
  delayMs?: number;
}

const variantClasses: Record<TooltipVariant, string> = {
  inverted:
    "rounded-md shadow-lg bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-800",
  plain:
    "rounded-md shadow-lg bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-800",
};

const GAP = 6; // px gap between trigger and tooltip

// ── CSS-based positioning classes for inline (non-portal) mode ─────────
// Layout: position (top/bottom/left/right) × align (start/center/end).
// "start"/"end" drop the centered translate and pin the corresponding edge
// to the matching edge of the trigger — the tooltip then naturally grows
// away from that edge in the direction of the trigger's body.
const inlinePositionClasses: Record<TooltipPosition, Record<TooltipAlign, string>> = {
  top: {
    start: "bottom-full left-0 mb-1.5",
    center: "bottom-full left-1/2 -translate-x-1/2 mb-1.5",
    end: "bottom-full right-0 mb-1.5",
  },
  bottom: {
    start: "top-full left-0 mt-1.5",
    center: "top-full left-1/2 -translate-x-1/2 mt-1.5",
    end: "top-full right-0 mt-1.5",
  },
  left: {
    start: "right-full top-0 mr-1.5",
    center: "right-full top-1/2 -translate-y-1/2 mr-1.5",
    end: "right-full bottom-0 mr-1.5",
  },
  right: {
    start: "left-full top-0 ml-1.5",
    center: "left-full top-1/2 -translate-y-1/2 ml-1.5",
    end: "left-full bottom-0 ml-1.5",
  },
};

// ── Portal-mode anchor + transform lookup ──────────────────────────────
// Each entry says: from the chosen anchor point on the trigger's rect,
// apply this CSS transform to position the tooltip's matching corner.
//   position=top,  align=start  → anchor at (left, top), translate(0,-100%)
//     = tooltip's bottom-LEFT sits at trigger's top-LEFT
//   position=top,  align=end    → anchor at (right, top), translate(-100%,-100%)
//     = tooltip's bottom-RIGHT sits at trigger's top-RIGHT
//   position=top,  align=center → anchor at center-top, translate(-50%,-100%)
//     = tooltip centered above the trigger
type AnchorEdge = "left" | "center" | "right" | "top" | "bottom";
interface PortalAnchor {
  /** Which edge of the trigger's bounding rect to anchor against. */
  anchorH: AnchorEdge;
  /** Which edge of the trigger's bounding rect to anchor against. */
  anchorV: AnchorEdge;
  /** Translate applied after anchoring to align the tooltip's matching edge. */
  translateX: "0" | "-50%" | "-100%";
  translateY: "0" | "-50%" | "-100%";
}
const portalAnchorMap: Record<TooltipPosition, Record<TooltipAlign, PortalAnchor>> = {
  top: {
    start:  { anchorH: "left",   anchorV: "top",    translateX: "0",      translateY: "-100%" },
    center: { anchorH: "center", anchorV: "top",    translateX: "-50%",   translateY: "-100%" },
    end:    { anchorH: "right",  anchorV: "top",    translateX: "-100%",  translateY: "-100%" },
  },
  bottom: {
    start:  { anchorH: "left",   anchorV: "bottom", translateX: "0",      translateY: "0" },
    center: { anchorH: "center", anchorV: "bottom", translateX: "-50%",   translateY: "0" },
    end:    { anchorH: "right",  anchorV: "bottom", translateX: "-100%",  translateY: "0" },
  },
  left: {
    start:  { anchorH: "left",   anchorV: "top",    translateX: "-100%",  translateY: "0" },
    center: { anchorH: "left",   anchorV: "center", translateX: "-100%",  translateY: "-50%" },
    end:    { anchorH: "left",   anchorV: "bottom", translateX: "-100%",  translateY: "-100%" },
  },
  right: {
    start:  { anchorH: "right",  anchorV: "top",    translateX: "0",      translateY: "0" },
    center: { anchorH: "right",  anchorV: "center", translateX: "0",      translateY: "-50%" },
    end:    { anchorH: "right",  anchorV: "bottom", translateX: "0",      translateY: "-100%" },
  },
};

/** Resolve a trigger-relative coordinate for the given anchor edge. */
function resolveAnchor(edge: AnchorEdge, rect: DOMRect, axis: "h" | "v"): number {
  if (axis === "h") {
    switch (edge) {
      case "left":   return rect.left;
      case "center": return rect.left + rect.width / 2;
      case "right":  return rect.right;
    }
  } else {
    switch (edge) {
      case "top":    return rect.top;
      case "center": return rect.top + rect.height / 2;
      case "bottom": return rect.bottom;
    }
  }
  // Exhaustiveness fallback — never reached if all AnchorEdge cases handled.
  return 0;
}

// ── Inline tooltip (CSS-positioned, supports @container queries) ───────

function InlineTooltip({
  content,
  children,
  position,
  align,
  variant,
  maxWidth,
  tipClass,
  delayMs,
}: Required<Omit<TooltipProps, "tipClass">> & { tipClass: string }) {
  const [visible, setVisible] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const handleEnter = useCallback(() => {
    // Defense 1: clear any pending timer from a previous enter that hasn't
    // fired yet. Without this, rapid hover-out-and-in races leave a stale
    // timer scheduled to fire and call setVisible(true) after we already
    // cleared state via handleLeave.
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    timerRef.current = setTimeout(() => {
      // Defense 2: clear ref after fire so handleLeave doesn't no-op
      // clearTimeout on a stale (already-fired) id.
      timerRef.current = null;
      setVisible(true);
    }, delayMs);
  }, [delayMs]);

  const handleLeave = useCallback(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    // Force synchronous commit. React 18 auto-batches state updates from
    // event handlers — if a parent re-render (e.g. a zustand store update)
    // queues a state change between our setVisible(false) and React's next
    // commit, the deferred hide can be overwritten by a quick re-enter's
    // setVisible(true) (or merged with another store update), leaving the
    // tooltip "stuck visible" until the user does another full hover cycle.
    // flushSync forces this update to commit before any other queued update.
    flushSync(() => {
      setVisible(false);
    });
  }, []);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, []);

  return (
    <div
      className="relative inline-flex"
      onMouseEnter={handleEnter}
      onMouseLeave={handleLeave}
    >
      {children}
      {/*
        width: max-content lets the tooltip size to its natural content
        width (capped at maxWidth by the inner div), independent of the
        trigger's width. Without this, a position:absolute child with
        width:auto fills the containing block, so an icon-only trigger
        (~28px) would squeeze the tooltip to a 28px column and wrap
        the text one character per line.
      */}
      <div
        className={cn(
          inlinePositionClasses[position][align],
          "pointer-events-none absolute z-50 w-max",
          tipClass,
          visible ? "block" : "hidden",
        )}
      >
        <div
          className={cn(
            // Use whitespace-pre-wrap + break-words instead of nowrap so long
            // content (e.g. file paths) wraps within maxWidth instead of
            // overflowing the background. Short tooltips stay on one line
            // because they have natural break points (or none at all).
            "whitespace-pre-wrap break-words px-2.5 py-1.5 text-[11px] leading-tight",
            variantClasses[variant],
          )}
          style={{ maxWidth }}
        >
          {content}
        </div>
      </div>
    </div>
  );
}

// ── Portal tooltip (escapes overflow-hidden parents) ───────────────────

function PortalTooltip({
  content,
  children,
  position,
  align,
  variant,
  maxWidth,
  delayMs,
}: Required<Omit<TooltipProps, "tipClass">> & { tipClass?: undefined }) {
  const triggerRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [coords, setCoords] = useState<{ top: number; left: number }>({ top: 0, left: 0 });
  const timerRef = useRef<ReturnType<typeof setTimeout>>(null);

  // Resolve the current anchor once per render — `coords` only stores the
  // anchor point on the trigger's rect (with GAP applied), and the
  // translate values come straight from the anchor entry. This keeps the
  // portal element's transform reactive to both `position` AND `align`.
  const anchor = portalAnchorMap[position][align];

  const calcPosition = useCallback(() => {
    if (!triggerRef.current) return;
    const rect = triggerRef.current.getBoundingClientRect();
    const a = portalAnchorMap[position][align];

    // Offset the anchor point by GAP depending on which side of the trigger
    // the tooltip lives on. For "top" / "left" the tooltip is BEFORE the
    // trigger along that axis, so the anchor moves by -GAP; for "bottom" /
    // "right" it moves by +GAP. The perpendicular coordinate stays at the
    // matching edge of the trigger (start/center/end).
    let top = resolveAnchor(a.anchorV, rect, "v");
    let left = resolveAnchor(a.anchorH, rect, "h");
    switch (position) {
      case "top":
        top -= GAP;
        break;
      case "bottom":
        top += GAP;
        break;
      case "left":
        left -= GAP;
        break;
      case "right":
        left += GAP;
        break;
    }

    setCoords({ top, left });
  }, [position, align]);

  const handleEnter = useCallback(() => {
    // Defense 1: clear any pending timer from a previous enter that hasn't
    // fired yet. Without this, rapid hover-out-and-in races leave a stale
    // timer scheduled to fire and call setVisible(true) after we already
    // cleared state via handleLeave.
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    timerRef.current = setTimeout(() => {
      // Defense 2: clear ref after fire so handleLeave doesn't no-op
      // clearTimeout on a stale (already-fired) id.
      timerRef.current = null;
      calcPosition();
      setVisible(true);
    }, delayMs);
  }, [calcPosition, delayMs]);

  const handleLeave = useCallback(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    // Force synchronous commit. React 18 auto-batches state updates from
    // event handlers — if a parent re-render (e.g. a zustand store update)
    // queues a state change between our setVisible(false) and React's next
    // commit, the deferred hide can be overwritten by a quick re-enter's
    // setVisible(true) (or merged with another store update), leaving the
    // tooltip "stuck visible" until the user does another full hover cycle.
    // flushSync forces this update to commit before any other queued update.
    flushSync(() => {
      setVisible(false);
    });
  }, []);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, []);

  return (
    <div
      ref={triggerRef}
      className="relative inline-flex"
      onMouseEnter={handleEnter}
      onMouseLeave={handleLeave}
    >
      {children}
      {visible &&
        createPortal(
          <div
            // w-max: width follows the inner content's natural width (fit-content),
            // independent of the trigger's width. Without this, a position:fixed
            // child without an explicit width can collapse to ~0 in some
            // renderers for CJK / pre-wrap content, squeezing text into a
            // single-character-per-line column. See InlineTooltip above for
            // the analogous rationale with position:absolute.
            className="pointer-events-none fixed z-[9999] w-max"
            style={{
              top: coords.top,
              left: coords.left,
              transform: `translate(${anchor.translateX}, ${anchor.translateY})`,
            }}
          >
            <div
              className={cn(
                // See InlineTooltip above for why whitespace-pre-wrap +
                // break-words is used here instead of whitespace-nowrap.
                "whitespace-pre-wrap break-words px-2.5 py-1.5 text-[11px] leading-tight",
                variantClasses[variant],
              )}
              style={{ maxWidth }}
            >
              {content}
            </div>
          </div>,
          document.body,
        )}
    </div>
  );
}

// ── Public component ───────────────────────────────────────────────────

export function Tooltip({
  content,
  children,
  position = "top",
  align = "center",
  variant = "inverted",
  maxWidth = "200px",
  tipClass,
  delayMs = 400,
}: TooltipProps) {
  // When content is empty, render children without tooltip wrapper
  if (!content) {
    return children;
  }

  const props = { content, children, position, align, variant, maxWidth, delayMs };

  // tipClass → inline mode (preserves @container query support)
  if (tipClass) {
    return <InlineTooltip {...props} tipClass={tipClass} />;
  }

  // Default → portal mode (escapes overflow-hidden clipping)
  return <PortalTooltip {...props} />;
}
