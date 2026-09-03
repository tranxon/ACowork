import { useState, useRef, useEffect, useCallback } from "react";
import { X } from "lucide-react";
import { useChatStore } from "../../stores/chatStore";
import { useDebugStore } from "../../stores/debugStore";
import { useTranslation } from "../../i18n/useTranslation";
import {
  computeContextUsageBreakdown,
  formatDetailedPercent,
  type ContextUsageCategoryKey,
} from "../../lib/contextUsageBreakdown";
import { cn } from "../../lib/utils";
import { getProcessingPhase } from "../../lib/types";
import { computeCacheHitStats, formatCacheHitRate, hasCacheData } from "../../lib/cacheHitRate";

const CATEGORY_META: Record<ContextUsageCategoryKey, { labelKey: string; color: string }> = {
  system: { labelKey: "contextUsage.categories.systemPrompt", color: "#6366f1" },
  tools: { labelKey: "contextUsage.categories.tools", color: "#14b8a6" },
  messages: { labelKey: "contextUsage.categories.messages", color: "#f59e0b" },
  connectors: { labelKey: "contextUsage.categories.connectors", color: "#a855f7" },
  skills: { labelKey: "contextUsage.categories.skills", color: "#ec4899" },
};

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
  const latestContextSnapshot = useDebugStore((state) => {
    if (state.debugAgentId !== agentId) return null;
    const snapshots = state.sessionStates[sessionId]?.snapshots;
    return snapshots && snapshots.length > 0 ? snapshots[snapshots.length - 1] : null;
  });

  // ADR-066 §6: cache hit rate, provider-aware.  `null` means "no
  // signal" — either the provider doesn't report cache tokens, no LLM
  // call has happened yet, or the denominator would be zero.  We use
  // the session-lifetime (`cumulative`) window here to match the
  // right-hand agent-status panel's session-status block and the
  // bottom status bar — keeping one consistent number for the same
  // session rather than mixing per-turn (volatile) and cumulative
  // (stable) views across surfaces.
  const cacheStats = computeCacheHitStats(
    sessionProvider,
    contextUsage,
    "cumulative",
  );
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
  const usageBreakdown = computeContextUsageBreakdown(
    latestContextSnapshot?.sections ?? [],
    usagePercent,
  );
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

      {/* Detailed usage popover. Sized and styled to match the sibling
          ModelMenu / SkillsPanel / SessionPanel dropdowns above the input
          toolbar (w-80, rounded-md, bg-modal-surface, zinc borders).
          The five-category list and stacked progress bar come from the
          latest debug context snapshot (see ADR-066). */}
      {open && (
        <div
          ref={popoverRef}
          role="dialog"
          aria-label={t("contextUsage.title")}
          onMouseEnter={handlePopoverEnter}
          onMouseLeave={handleMouseLeave}
          className="absolute bottom-full right-0 z-50 mb-2 w-72 max-h-[min(calc(100vh-120px),460px)] select-none overflow-y-auto overscroll-contain rounded-md border border-zinc-200 bg-modal-surface text-zinc-700 shadow-lg dark:border-zinc-700 dark:text-zinc-200"
        >
          <div className="flex items-center justify-between px-3 pt-2.5">
            <h2 className="text-sm font-semibold text-zinc-700 dark:text-zinc-200">
              {t("contextUsage.title")}
            </h2>
            <button
              type="button"
              onClick={() => setOpen(false)}
              aria-label={t("contextUsage.close")}
              title={t("contextUsage.close")}
              className="rounded p-1 text-zinc-400 transition-colors hover:bg-zinc-100 hover:text-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-700/50 dark:hover:text-zinc-100"
            >
              <X size={14} strokeWidth={2.25} />
            </button>
          </div>

          <div className="px-3 pt-2">
            <div className="flex items-baseline gap-2 whitespace-nowrap">
              <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-semibold leading-none tracking-[-0.01em] text-zinc-700 tabular-nums dark:text-zinc-200">
                {formatDetailedPercent(usagePercent)}%
              </span>
              <span className="text-xs text-zinc-500 dark:text-zinc-400">
                {t("contextUsage.used")} {" "}
                <span className="font-mono text-zinc-700 dark:text-zinc-300">
                  {formatTokens(contextUsage?.total_tokens ?? 0)}
                </span>
                <span className="text-zinc-400 dark:text-zinc-500"> / </span>
                <span className="font-mono text-zinc-700 dark:text-zinc-300">
                  {formatTokens(contextUsage?.context_window ?? 0)}
                </span>
              </span>
            </div>

            <div
              className="mt-3 flex h-2 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700"
              role="progressbar"
              aria-label={t("contextUsage.usageBarLabel")}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.floor(usagePercent)}
            >
              {usageBreakdown.map((category) => {
                if (category.percentage <= 0) return null;
                const meta = CATEGORY_META[category.key];
                return (
                  <span
                    key={category.key}
                    className="h-full transition-[width] duration-300"
                    style={{ width: `${category.percentage}%`, backgroundColor: meta.color }}
                  />
                );
              })}
            </div>
          </div>

          <div className="px-3 py-1">
            {usageBreakdown.map((category) => {
              const meta = CATEGORY_META[category.key];
              return (
                <div
                  key={category.key}
                  className="flex min-h-7 items-center gap-2 px-1 text-xs"
                  title={t(meta.labelKey)}
                >
                  <span
                    className="h-2 w-2 shrink-0 rounded-full"
                    style={{ backgroundColor: meta.color }}
                  />
                  <span className="font-medium text-zinc-700 dark:text-zinc-200">
                    {t(meta.labelKey)}
                  </span>
                  <span className="ml-auto font-mono tabular-nums text-zinc-700 dark:text-zinc-300">
                    {category.percentage.toFixed(1)}%
                  </span>
                </div>
              );
            })}
          </div>

          {hasCacheData(contextUsage, "cumulative") ? (
            <>
              <div className="border-t border-zinc-200 dark:border-zinc-700" />
              <div className="px-3 pt-2 pb-3">
                <div className="text-sm font-semibold text-zinc-700 dark:text-zinc-200">
                  {t("contextUsage.cacheHitLabel")}
                </div>
                <div className="mt-1 flex items-baseline gap-2 whitespace-nowrap">
                  <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-semibold leading-none tracking-[-0.01em] tabular-nums text-zinc-700 dark:text-zinc-200">
                    {cacheHitRateLabel ?? "\u2014"}
                  </span>
                  <span className="text-xs text-zinc-500 dark:text-zinc-400">
                    {t("contextUsage.cached")}{" "}
                    <span className="font-mono text-zinc-700 dark:text-zinc-300">
                      {formatTokens(cacheStats.numerator)}
                    </span>
                    <span className="text-zinc-400 dark:text-zinc-500"> / </span>
                    <span className="font-mono text-zinc-700 dark:text-zinc-300">
                      {formatTokens(cacheStats.denominator)}
                    </span>
                  </span>
                </div>
              </div>
            </>
          ) : null}

          <div className="border-t border-zinc-200 dark:border-zinc-700" />
          <button
            onClick={handleCompressSummary}
            disabled={!canAct}
            className={cn(
              "mx-3 mb-2.5 mt-2 flex w-[calc(100%-1.5rem)] items-center justify-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
              "bg-zinc-100 text-zinc-700 hover:bg-zinc-200 hover:text-zinc-900",
              "dark:bg-white/10 dark:text-zinc-300 dark:hover:bg-white/15 dark:hover:text-zinc-100",
              "disabled:cursor-not-allowed disabled:opacity-40",
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
