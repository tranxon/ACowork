import { useEffect } from "react";
import { Bug, Pause, Play, StepForward } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useDebugStore } from "../../stores/debugStore";

/**
 * Banner shown inside the chat panel when the selected agent is in debug mode
 * AND the debugger is currently in a paused/stepping state.
 *
 * Mirrors the visual style of the iteration-limit-paused banner but with
 * debug-specific colors and resume/step actions. The banner disappears
 * automatically when the debugger transitions back to "Running".
 */
export function DebugPausedBanner() {
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

  const stateLabel = debugState === "Paused" ? "Paused" : "Stepping";

  return (
    <div
      role="status"
      aria-live="polite"
      className="mx-4 mt-1.5 flex flex-wrap items-center gap-2 rounded-md border border-amber-200 bg-amber-50/80 px-3 py-1.5 text-amber-900 select-none dark:border-amber-900/50 dark:bg-amber-950/40 dark:text-amber-100"
      style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
    >
      <span className="flex shrink-0 items-center gap-1.5">
        <Bug className="h-3.5 w-3.5 text-amber-600 dark:text-amber-400" />
        <Pause className="h-3 w-3 text-amber-600 dark:text-amber-400" />
        <span className="text-xs font-medium">
          {stateLabel} in debug mode
        </span>
      </span>

      <div className="ml-auto flex items-center gap-1.5">
        <button
          type="button"
          onClick={handleResume}
          className="flex items-center gap-1 rounded bg-amber-500 px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:bg-amber-600"
        >
          <Play className="h-3 w-3" fill="currentColor" />
          <span>Resume</span>
          <KbdHint>F5</KbdHint>
        </button>
        <button
          type="button"
          onClick={handleStep}
          className="flex items-center gap-1 rounded bg-amber-500 px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:bg-amber-600"
        >
          <StepForward className="h-3 w-3" />
          <span>Step</span>
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
