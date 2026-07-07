import { type ReactElement, useRef, useState, useCallback, useEffect } from "react";
import { createPortal } from "react-dom";
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
type TooltipVariant = "inverted" | "plain";

interface TooltipProps {
  /** Tooltip text content */
  content: string;
  /** The trigger element — must be a single ReactElement */
  children: ReactElement;
  /** Tooltip position relative to trigger. Default: 'top' */
  position?: TooltipPosition;
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
const inlinePositionClasses: Record<TooltipPosition, string> = {
  top: "bottom-full left-1/2 -translate-x-1/2 mb-1.5",
  bottom: "top-full left-1/2 -translate-x-1/2 mt-1.5",
  left: "right-full top-1/2 -translate-y-1/2 mr-1.5",
  right: "left-full top-1/2 -translate-y-1/2 ml-1.5",
};

// ── Transform strings for portal mode ──────────────────────────────────
const portalTransformMap: Record<TooltipPosition, string> = {
  top: "translate(-50%, -100%)",
  bottom: "translate(-50%, 0)",
  left: "translate(-100%, -50%)",
  right: "translate(0, -50%)",
};

// ── Inline tooltip (CSS-positioned, supports @container queries) ───────

function InlineTooltip({
  content,
  children,
  position,
  variant,
  maxWidth,
  tipClass,
  delayMs,
}: Required<Omit<TooltipProps, "tipClass">> & { tipClass: string }) {
  const [visible, setVisible] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const handleEnter = useCallback(() => {
    timerRef.current = setTimeout(() => setVisible(true), delayMs);
  }, [delayMs]);

  const handleLeave = useCallback(() => {
    if (timerRef.current) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    setVisible(false);
  }, []);

  useEffect(() => {
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
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
          inlinePositionClasses[position],
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
  variant,
  maxWidth,
  delayMs,
}: Required<Omit<TooltipProps, "tipClass">> & { tipClass?: undefined }) {
  const triggerRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [coords, setCoords] = useState<{ top: number; left: number }>({ top: 0, left: 0 });
  const timerRef = useRef<ReturnType<typeof setTimeout>>(null);

  const calcPosition = useCallback(() => {
    if (!triggerRef.current) return;
    const rect = triggerRef.current.getBoundingClientRect();

    let top = 0;
    let left = 0;

    switch (position) {
      case "top":
        top = rect.top - GAP;
        left = rect.left + rect.width / 2;
        break;
      case "bottom":
        top = rect.bottom + GAP;
        left = rect.left + rect.width / 2;
        break;
      case "left":
        top = rect.top + rect.height / 2;
        left = rect.left - GAP;
        break;
      case "right":
        top = rect.top + rect.height / 2;
        left = rect.right + GAP;
        break;
    }

    setCoords({ top, left });
  }, [position]);

  const handleEnter = useCallback(() => {
    timerRef.current = setTimeout(() => {
      calcPosition();
      setVisible(true);
    }, delayMs);
  }, [calcPosition, delayMs]);

  const handleLeave = useCallback(() => {
    if (timerRef.current) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    setVisible(false);
  }, []);

  useEffect(() => {
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
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
            className="pointer-events-none fixed z-[9999]"
            style={{
              top: coords.top,
              left: coords.left,
              transform: portalTransformMap[position],
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
  variant = "inverted",
  maxWidth = "200px",
  tipClass,
  delayMs = 400,
}: TooltipProps) {
  // When content is empty, render children without tooltip wrapper
  if (!content) {
    return children;
  }

  const props = { content, children, position, variant, maxWidth, delayMs };

  // tipClass → inline mode (preserves @container query support)
  if (tipClass) {
    return <InlineTooltip {...props} tipClass={tipClass} />;
  }

  // Default → portal mode (escapes overflow-hidden clipping)
  return <PortalTooltip {...props} />;
}
