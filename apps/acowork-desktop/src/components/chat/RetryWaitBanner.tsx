import { useEffect, useState, useRef, useCallback } from "react";
import { Clock, SkipForward, RefreshCw } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";

/**
 * Threshold (ms) above which the retry wait is treated as a "timeout retry"
 * (response timeout / stream timeout) rather than a 429 rate-limit retry.
 * 5 minutes = 300_000 ms.
 */
const TIMEOUT_RETRY_THRESHOLD_MS = 5 * 60 * 1000;

/**
 * Countdown banner shown when the LLM provider returns 429 (rate-limited)
 * or the LLM stream times out, and the retry wait exceeds 10 seconds.
 *
 * Two modes:
 * - **429 mode** (waitMs < 5 min): orange/amber tones, "Rate limited" text,
 *   "Skip Wait" button.
 * - **Timeout mode** (waitMs >= 5 min): indigo/blue tones, "Response timeout"
 *   text, "Retry Now" button.
 *
 * In both modes, a real-time countdown timer is displayed and the user can
 * click the button to skip the wait. When the timer expires, the backend
 * automatically retries the LLM request.
 */
export function RetryWaitBanner() {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const currentSessionId = useChatStore((s) =>
    selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null,
  );
  const retryWaitInfo = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return null;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.retryWaitInfo ?? null;
  });

  // Determine if this is a timeout retry (long wait) or 429 retry (short wait)
  const isTimeoutMode = retryWaitInfo !== null && retryWaitInfo.waitMs >= TIMEOUT_RETRY_THRESHOLD_MS;

  // Local countdown state — derived from startedAt + waitMs
  const [remainingMs, setRemainingMs] = useState<number>(0);
  const rafRef = useRef<number | null>(null);

  // Recalculate remaining time every animation frame for smooth countdown
  const tick = useCallback(() => {
    if (!retryWaitInfo) {
      setRemainingMs(0);
      return;
    }
    const elapsed = Date.now() - retryWaitInfo.startedAt;
    const remaining = Math.max(0, retryWaitInfo.waitMs - elapsed);
    setRemainingMs(remaining);
    if (remaining > 0) {
      rafRef.current = requestAnimationFrame(tick);
    }
  }, [retryWaitInfo]);

  useEffect(() => {
    if (retryWaitInfo) {
      rafRef.current = requestAnimationFrame(tick);
    } else {
      setRemainingMs(0);
    }
    return () => {
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    };
  }, [retryWaitInfo, tick]);

  const handleSkip = () => {
    if (selectedAgentId) {
      void useChatStore.getState().continueExecution(selectedAgentId);
    }
  };

  if (!retryWaitInfo || !selectedAgentId || !currentSessionId) return null;

  const remainingSec = Math.ceil(remainingMs / 1000);
  const totalSec = Math.ceil(retryWaitInfo.waitMs / 1000);

  const label = isTimeoutMode
    ? "Response timeout"
    : "Rate limited";

  const buttonLabel = isTimeoutMode
    ? "Retry Now"
    : "Skip Wait";

  const ButtonIcon = isTimeoutMode ? RefreshCw : SkipForward;

  return (
    <div
      role="status"
      aria-live="polite"
      className={`mx-4 mt-1.5 flex flex-wrap items-center gap-2 rounded-md border px-3 py-1.5 select-none ${
        isTimeoutMode
          ? "border-indigo-200 bg-indigo-50/80 text-indigo-900 dark:border-indigo-900/50 dark:bg-indigo-950/40 dark:text-indigo-100"
          : "border-orange-200 bg-orange-50/80 text-orange-900 dark:border-orange-900/50 dark:bg-orange-950/40 dark:text-orange-100"
      }`}
      style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
    >
      <span className="flex shrink-0 items-center gap-1.5">
        <Clock className={`h-3.5 w-3.5 ${
          isTimeoutMode
            ? "text-indigo-600 dark:text-indigo-400"
            : "text-orange-600 dark:text-orange-400"
        }`} />
        <span className="text-xs font-medium">
          {label} — retrying in{" "}
          <span className="tabular-nums font-mono font-bold">
            {remainingSec}s
          </span>
          {" "}({retryWaitInfo.attempt}/{retryWaitInfo.maxAttempts})
        </span>
      </span>

      <span className={`hidden sm:inline text-[11px] ${
        isTimeoutMode
          ? "text-indigo-600/70 dark:text-indigo-400/70"
          : "text-orange-600/70 dark:text-orange-400/70"
      }`}>
        {retryWaitInfo.provider}
      </span>

      <div className="ml-auto flex items-center gap-1.5">
        {/* Countdown progress bar */}
        <div className={`hidden sm:block h-1.5 w-16 rounded-full ${
          isTimeoutMode
            ? "bg-indigo-200 dark:bg-indigo-800/50"
            : "bg-orange-200 dark:bg-orange-800/50"
        }`}>
          <div
            className={`h-full rounded-full transition-[width] duration-1000 ease-linear ${
              isTimeoutMode
                ? "bg-indigo-500"
                : "bg-orange-500"
            }`}
            style={{
              width: `${Math.max(0, Math.min(100, ((totalSec - remainingSec) / totalSec) * 100))}%`,
            }}
          />
        </div>

        <button
          type="button"
          onClick={handleSkip}
          className={`flex items-center gap-1 rounded px-2 py-0.5 text-[11px] font-medium text-white transition-colors ${
            isTimeoutMode
              ? "bg-indigo-500 hover:bg-indigo-600"
              : "bg-orange-500 hover:bg-orange-600"
          }`}
        >
          <ButtonIcon className="h-3 w-3" />
          <span>{buttonLabel}</span>
        </button>
      </div>
    </div>
  );
}