import { useEffect } from "react";
import { Bug, Pause, Play, StepForward } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useDebugStore } from "../../stores/debugStore";
import { useTranslation } from "../../i18n/useTranslation";

/**
 * Banner shown inside the chat panel when the selected agent is in debug mode
 * AND the debugger is currently in a paused/stepping state.
 *
 * Mirrors the visual style of the iteration-limit-paused banner but with
 * debug-specific colors and resume/step actions. The banner disappears
 * automatically when the debugger transitions back to "Running".
 */
export function DebugPausedBanner() {
  const { t } = useTranslation();
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const devMode = useAgentStore((s) => {
    if (!s.selectedAgentId) return false;
    return s.agents[s.selectedAgentId]?.meta?.dev_mode ?? false;
  });
  const currentSessionId = useChatStore((s) =>
    selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null,
  );
  const sessionDebugState = useDebugStore((s) =>
    currentSessionId ? s.sessionStates[currentSessionId] ?? null : null,
  );
  const debugState = sessionDebugState?.debugState ?? null;

  const isPaused = sessionDebugState?.paused === true;
  const visible = devMode && isPaused && !!currentSessionId;

  // Register F5 (resume) and F10 (step) shortcuts when banner is visible.
  // We use the capture phase so we run before main.tsx's bubble-phase
  // handler. The Monaco editor guard prevents stealing shortcuts while
  // the user is debugging source code in the file editor.
  useEffect(() => {
    if (!visible || !currentSessionId) return;

    const handler = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return;
      const target = e.target;
      if (target instanceof Element && target.closest(".monaco-editor")) return;

      if (e.key === "F5") {
        e.preventDefault();
        e.stopPropagation();
        void useDebugStore.getState().resume(currentSessionId);
        return;
      }
      if (e.key === "F10") {
        e.preventDefault();
        e.stopPropagation();
        void useDebugStore.getState().step(currentSessionId, "iteration");
        return;
      }
    };

    document.addEventListener("keydown", handler, true);
    return () => document.removeEventListener("keydown", handler, true);
  }, [visible, currentSessionId]);

  if (!visible || !currentSessionId) return null;

  const handleResume = () => {
    void useDebugStore.getState().resume(currentSessionId);
  };
  const handleStep = () => {
    void useDebugStore.getState().step(currentSessionId, "iteration");
  };

  const stateLabel = debugState === "Paused" ? t("debugPausedBanner.statePaused") : t("debugPausedBanner.stateStepping");

  return (
    <div
      role="status"
      aria-live="polite"
      className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-[var(--color-accent)]/30 bg-[var(--color-accent)]/10 px-4 py-2 text-[var(--color-accent)] select-none dark:border-[var(--color-accent)]/40 dark:bg-[var(--color-accent)]/15 dark:text-[var(--color-accent)]"
      style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
    >
      <span className="flex shrink-0 items-center gap-1.5">
        <Bug className="h-3.5 w-3.5 text-[var(--color-accent)] dark:text-[var(--color-accent)]" />
        <Pause className="h-3 w-3 text-[var(--color-accent)] dark:text-[var(--color-accent)]" />
        <span className="text-xs font-medium">
          {stateLabel} in debug mode
        </span>
      </span>

      <div className="ml-auto flex items-center gap-1.5">
        <button
          type="button"
          onClick={handleResume}
          className="flex items-center gap-1 rounded bg-[var(--color-accent)] px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:brightness-90"
        >
          <Play className="h-3 w-3" fill="currentColor" />
          <span>{t("debugPausedBanner.resume")}</span>
          <KbdHint>F5</KbdHint>
        </button>
        <button
          type="button"
          onClick={handleStep}
          className="flex items-center gap-1 rounded bg-[var(--color-accent)] px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:brightness-90"
        >
          <StepForward className="h-3 w-3" />
          <span>{t("debugPausedBanner.step")}</span>
          <KbdHint>F10</KbdHint>
        </button>
      </div>
    </div>
  );
}

function KbdHint({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="ml-0.5 rounded bg-black/15 px-1 py-px font-mono text-[9px] font-semibold leading-none dark:bg-white/15">
      {children}
    </kbd>
  );
}
