import { useState, useRef, useEffect, useCallback } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";

/** Circular progress ring showing context usage percentage.
 *  Starts from bottom (6 o'clock), goes clockwise.
 *  16x16 SVG to match adjacent send button icon size. */
function CircularProgressIcon({ usagePercent }: { usagePercent: number }) {
  const size = 16;
  const strokeWidth = 2;
  const radius = (size - strokeWidth) / 2;
  const center = size / 2;
  const circumference = 2 * Math.PI * radius;
  const offset = circumference * (1 - usagePercent / 100);

  const fillColor = "var(--color-text-secondary, hsl(240 3.7% 46.9%))";
  const emptyColor = "var(--shimmer-mid, #e8e8ec)";

  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      {/* Background ring (total capacity) */}
      <circle
        cx={center}
        cy={center}
        r={radius}
        stroke={emptyColor}
        strokeWidth={strokeWidth}
        opacity={0.35}
      />
      {/* Progress ring (used amount) — starts from bottom, goes clockwise */}
      <circle
        cx={center}
        cy={center}
        r={radius}
        stroke={fillColor}
        strokeWidth={strokeWidth}
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        transform={`rotate(90 ${center} ${center})`}
        style={{ transition: "stroke-dashoffset 0.3s ease" }}
      />
    </svg>
  );
}

export function ContextUsageIcon() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const popoverRef = useRef<HTMLDivElement>(null);
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Selectors: each returns a primitive to avoid infinite re-render (shallow compare)
  const currentAgentId = useChatStore((s) => s.currentAgentId);
  const activeSessionId = useChatStore((s) => {
    if (!s.currentAgentId) return null;
    return s.agentStates[s.currentAgentId]?.activeSessionId ?? null;
  });
  const contextUsage = useChatStore((s) => {
    if (!s.currentAgentId) return null;
    const agent = s.agentStates[s.currentAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.contextUsage ?? null;
  });
  const isCompacting = useChatStore((s) => {
    if (!s.currentAgentId) return false;
    const agent = s.agentStates[s.currentAgentId];
    if (!agent?.activeSessionId) return false;
    return agent.sessionStates[agent.activeSessionId]?.isCompacting ?? false;
  });
  const sessionStatus = useChatStore((s) => {
    if (!s.currentAgentId) return null;
    const agent = s.agentStates[s.currentAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.sessionStatus ?? null;
  });
  const compactContext = useChatStore((s) => s.compactContext);

  // Open popover on hover (not click), with a small delay before closing
  const handleMouseEnter = useCallback(() => {
    if (closeTimerRef.current) {
      clearTimeout(closeTimerRef.current);
      closeTimerRef.current = null;
    }
    setOpen(true);
  }, []);

  const handleMouseLeave = useCallback(() => {
    closeTimerRef.current = setTimeout(() => setOpen(false), 150);
  }, []);

  // Keep popover open while hovering over it
  const handlePopoverEnter = useCallback(() => {
    if (closeTimerRef.current) {
      clearTimeout(closeTimerRef.current);
      closeTimerRef.current = null;
    }
  }, []);

  useEffect(() => {
    return () => {
      if (closeTimerRef.current) clearTimeout(closeTimerRef.current);
    };
  }, []);

  const usagePercent = contextUsage?.usage_percent ?? 0;
  const isIdle = !sessionStatus || sessionStatus.status === "idle";
  const canCompress =
    isIdle &&
    !isCompacting &&
    contextUsage != null &&
    currentAgentId != null &&
    activeSessionId != null;

  const handleCompress = () => {
    if (!canCompress || !currentAgentId || !activeSessionId) return;
    compactContext(currentAgentId, activeSessionId);
    setOpen(false);
  };

  const formatTokens = (n: number | undefined): string => {
    if (n == null) return "\u2014";
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
    return String(n);
  };

  return (
    <div
      className="relative"
      onMouseEnter={handleMouseEnter}
      onMouseLeave={handleMouseLeave}
    >
      {/* Icon button — matches the adjacent Send button exactly */}
      <button
        className={cn(
          "rounded-lg p-1.5 transition-colors",
          "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200",
        )}
        aria-label={t("contextUsage.ariaLabel")}
      >
        {isCompacting ? (
          <span className="h-4 w-4 flex items-center justify-center">
            <span className="h-3 w-3 rounded-full border-2 border-[var(--color-accent)] border-t-transparent animate-spin" />
          </span>
        ) : (
          <CircularProgressIcon usagePercent={usagePercent} />
        )}
      </button>

      {/* Popover — matches model/workspace/skills dropdown style */}
      {open && (
        <div
          ref={popoverRef}
          onMouseEnter={handlePopoverEnter}
          onMouseLeave={handleMouseLeave}
          className={cn(
            "absolute bottom-full right-0 z-50 mb-1 overflow-hidden rounded-lg border shadow-lg",
            "border-zinc-200 bg-white dark:border-zinc-700 dark:bg-zinc-800",
          )}
        >
          {/* Line 1: usage percentage + token stats */}
          <div className="px-3 pt-2.5 pb-1.5 text-xs text-zinc-600 dark:text-zinc-300 whitespace-nowrap select-none">
            <span
              className="font-semibold"
              style={{ color: "var(--color-accent)" }}
            >
              {usagePercent}%
            </span>
            <span className="mx-1.5 text-zinc-400 dark:text-zinc-500">|</span>
            <span className="font-mono">
              {formatTokens(contextUsage?.total_tokens ?? 0)}
            </span>
            <span className="text-zinc-400 dark:text-zinc-500"> / </span>
            <span className="font-mono">
              {formatTokens(contextUsage?.context_window ?? 0)}
            </span>
            <span className="text-zinc-400 dark:text-zinc-500">
              {" "}
              context used
            </span>
          </div>

          {/* Line 2: compress button — matches model menu "Add Model" button */}
          <button
            onClick={handleCompress}
            disabled={!canCompress}
            className={cn(
              "mx-1.5 mt-1 mb-2 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md",
              "bg-zinc-100 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-700 transition-colors",
              "hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600",
              "disabled:opacity-40 disabled:cursor-not-allowed",
            )}
          >
            {isCompacting
              ? t("contextUsage.compressing")
              : !isIdle
                ? t("contextUsage.agentRunning")
                : t("contextUsage.compressContext")}
          </button>
        </div>
      )}
    </div>
  );
}
