import { useState, useRef, useEffect, useCallback } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useTranslation } from "../../i18n/useTranslation";
import { cn, formatPercent } from "../../lib/utils";
import { getProcessingPhase } from "../../lib/types";
import { computeCacheHitStats, formatCacheHitRate, hasCacheData } from "../../lib/cacheHitRate";

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

export function ContextUsageIcon({ agentId, sessionId }: { agentId: string; sessionId: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const popoverRef = useRef<HTMLDivElement>(null);
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const contextUsage = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.contextUsage ?? null);
  const isCompacting = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.isCompacting ?? false);
  const sessionStatus = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.sessionStatus ?? null);
  // ADR-066: prompt-cache hit ratio is provider-specific (denominator
  // differs between Anthropic Messages and OpenAI Chat Completions).
  // We read `provider` from the per-session chatStore so the helper
  // can pick the right formula; falls back to null (= hide) for
  // providers that don't surface cache accounting (ollama, etc.).
  const sessionProvider = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.provider ?? null);
  const sendCompressAction = useChatStore((s) => s.sendCompressAction);

  // ADR-066 §6: cache hit rate, provider-aware.  `null` means "no
  // signal" — either the provider doesn't report cache tokens, no LLM
  // call has happened yet, or the denominator would be zero.
  const cacheStats = computeCacheHitStats(sessionProvider, contextUsage);
  const cacheHitRateLabel = formatCacheHitRate(cacheStats.ratio);

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
  // ADR-049: derive from `getProcessingPhase()` instead of comparing status
  // string literals. The compiler checks exhaustiveness — adding a new
  // non-idle phase will not silently bypass this check.
  const isIdle = getProcessingPhase(sessionStatus) === "idle";
  const canAct = isIdle && !isCompacting && contextUsage != null;

const handleCompressSummary = () => {
    if (!canAct) return;
    // 1 = CompressType::SUMMARY (see core/acowork-core/proto/mqtt_payload.proto).
    sendCompressAction(agentId, sessionId, 1);
    setOpen(false);
  };

  // Same precision contract as `formatTokenCount` in `ResultsPanel.tsx`:
  // 2 decimals for M (= 10K granularity), 1 decimal for K.  Kept in
  // sync because the two values rendered side by side (e.g.
  // `2.71M / 2.78M` for cache vs. input) must speak the same
  // precision — otherwise a "2.7M vs 2.8M" pair would read as a
  // tie when the actual gap is 70K.
  const formatTokens = (n: number | undefined): string => {
    if (n == null) return "\u2014";
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
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
          "rounded-md p-1.5 transition-colors",
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
            "absolute bottom-full right-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-modal-surface dark:border-zinc-700",
          )}
        >
          {/* Line 1: usage percentage + token stats */}
          <div className="px-3 pt-2.5 pb-3 text-xs text-zinc-600 dark:text-zinc-300 whitespace-nowrap select-none">
            <span
              className="font-semibold"
              style={{ color: "var(--color-accent)" }}
            >
              {formatPercent(usagePercent)}%
            </span>
            <span className="mx-2 text-zinc-400 dark:text-zinc-500">|</span>
            <span className="font-mono">
              {formatTokens(contextUsage?.total_tokens ?? 0)}
            </span>
            <span className="text-zinc-400 dark:text-zinc-500"> / </span>
            <span className="font-mono">
              {formatTokens(contextUsage?.context_window ?? 0)}
            </span>
            <span className="ml-3 text-zinc-400 dark:text-zinc-500">
              {t("contextUsage.contextUsedSuffix")}
            </span>
          </div>

          {/* Divider — separates 上下文用量 from 缓存命中率.  Style
              mirrors the right-side Status panel dividers so the popover
              reads as a miniature version of that surface.  Combined with
              the py-3 padding on the surrounding rows, the visual gap
              between the two text lines is ~25px (was ~15px before
              bumping). */}
          <div className="border-t border-zinc-100 dark:border-zinc-700/50" />

          {/* Line 2 (ADR-066): prompt-cache hit ratio + its numerator /
              denominator.  Rendered in the exact same style as the
              context-usage line above (`90% | 90K / 100K 缓存命中率`) so
              the two rows read as a pair — the two numbers are the actual
              ratio components (cache-hit tokens / input tokens), not two
              independent cache counters.  Shown whenever the Runtime
              reports any cache accounting (read or write); the ratio
              itself is hidden behind a dash when it isn't computable yet
              (e.g. a fresh Anthropic session that has only seeded the
              cache). */}
          {hasCacheData(contextUsage) ? (
            <>
              <div className="px-3 py-3 text-xs text-zinc-600 dark:text-zinc-300 whitespace-nowrap select-none">
                <span
                  className="font-semibold"
                  style={{ color: "var(--color-accent)" }}
                >
                  {cacheHitRateLabel ?? "\u2014"}
                </span>
                <span className="mx-2 text-zinc-400 dark:text-zinc-500">|</span>
                <span className="font-mono">{formatTokens(cacheStats.numerator)}</span>
                <span className="text-zinc-400 dark:text-zinc-500"> / </span>
                <span className="font-mono">{formatTokens(cacheStats.denominator)}</span>
                <span className="ml-3 text-zinc-400 dark:text-zinc-500">
                  {t("contextUsage.cacheHitLabel")}
                </span>
              </div>
              {/* Divider — separates 缓存命中率 from the Compress
                  Summary button below.  The button's mt-3 below matches
                  the py-3 padding on this row, keeping the gap symmetric
                  with the gap above (~25px between content edges). */}
              <div className="border-t border-zinc-100 dark:border-zinc-700/50" />
            </>
          ) : null}

          {/* Compress Summary button */}
          <button
            onClick={handleCompressSummary}
            disabled={!canAct}
            className={cn(
              "mx-1.5 mt-3 mb-2 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md",
              "bg-zinc-100 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-700 transition-colors",
              "hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600",
              "disabled:opacity-40 disabled:cursor-not-allowed",
            )}
          >
            {isCompacting
              ? t("contextUsage.compressing")
              : t("contextUsage.compressSummary")}
          </button>
        </div>
      )}
    </div>
  );
}
