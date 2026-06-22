import { useEffect, useState, useRef, useCallback } from "react";
import { Clock, SkipForward } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";

/**
 * Countdown banner shown when the LLM provider returns 429 (rate-limited)
 * and the retry wait exceeds 10 seconds.
 *
 * Displays a real-time countdown timer and a "Skip Wait" button that triggers
 * the existing `continueExecution` API to wake the retry loop immediately.
 *
 * Visual style: orange/amber tones, distinct from debug pause (amber) and
 * iteration limit pause (accent color).
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

  return (
    <div
      role="status"
      aria-live="polite"
      className="mx-4 mt-1.5 flex flex-wrap items-center gap-2 rounded-md border border-orange-200 bg-orange-50/80 px-3 py-1.5 text-orange-900 select-none dark:border-orange-900/50 dark:bg-orange-950/40 dark:text-orange-100"
      style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
    >
      <span className="flex shrink-0 items-center gap-1.5">
        <Clock className="h-3.5 w-3.5 text-orange-600 dark:text-orange-400" />
        <span className="text-xs font-medium">
          Rate limited — retrying in{" "}
          <span className="tabular-nums font-mono font-bold">
            {remainingSec}s
          </span>
          {" "}({retryWaitInfo.attempt}/{retryWaitInfo.maxAttempts})
        </span>
      </span>

      <span className="hidden sm:inline text-[11px] text-orange-600/70 dark:text-orange-400/70">
        {retryWaitInfo.provider}
      </span>

      <div className="ml-auto flex items-center gap-1.5">
        {/* Countdown progress bar */}
        <div className="hidden sm:block h-1.5 w-16 rounded-full bg-orange-200 dark:bg-orange-800/50">
          <div
            className="h-full rounded-full bg-orange-500 transition-[width] duration-1000 ease-linear"
            style={{
              width: `${Math.max(0, Math.min(100, ((totalSec - remainingSec) / totalSec) * 100))}%`,
            }}
          />
        </div>

        <button
          type="button"
          onClick={handleSkip}
          className="flex items-center gap-1 rounded bg-orange-500 px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:bg-orange-600"
        >
          <SkipForward className="h-3 w-3" />
          <span>Skip Wait</span>
        </button>
      </div>
    </div>
  );
}
