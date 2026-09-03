import { useState, useEffect, useRef, useCallback } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useDebugStore } from "../../stores/debugStore";
import type { ChatMessage, SessionStatus } from "../../lib/types";
import { getProcessingPhase } from "../../lib/types";
import { cn, formatPercent } from "../../lib/utils";
import { log } from "../../lib/logger";
import { computeCacheHitStats, formatCacheHitRate, hasCacheData, getCacheProtocol } from "../../lib/cacheHitRate";
import {
  WifiOff,
  Play,
  Pause,
  StepForward,
  Square,
  RefreshCw,
  RotateCcw,
  Bug,
} from "lucide-react";
import { AgentSetupTab } from "./AgentSetupTab";
import { ToolsTab } from "./ToolsTab";
import { MemoryPanel } from "../memory/MemoryPanel";
import { WorkspaceExplorer } from "../workspace/WorkspaceExplorer";
import { ControlButton, StateLabel, SnapshotNode } from "../debug/DebugPanel";
import { CompressionHistoryCard } from "../debug/CompressionHistoryCard";
// ADR-063 §3.7 — package prompt override editor. Always rendered at
// the TOP of the Debug tab regardless of DevMode state (operators can
// prepare overrides before clicking "Enter Debug"; the reload button
// stays reachable once DevMode is on).
import { PromptList } from "../debug/PromptList";
import { Switch } from "../common/Switch";
import { isGatewayLocal, getGatewayUrl } from "../../lib/config";

interface ResultsPanelProps {
  onCollapse: () => void;
  isDebugMode?: boolean;
  onResizeStart?: (e: React.MouseEvent) => void;
  activeTab: "debug" | "status" | "setup" | "tools" | "memory" | "workspace";
  onTabChange: (tab: "debug" | "status" | "setup" | "tools" | "memory" | "workspace") => void;
}

// Stable empty array reference to avoid Zustand selector infinite loop
const EMPTY_MESSAGES: ChatMessage[] = [];

export function ResultsPanel({ width, isDebugMode = false, onResizeStart, activeTab, onTabChange }: ResultsPanelProps & { width: number }) {
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);
  const activeSessionId = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null);
  const tokenUsage = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.tokenUsage ?? null;
  });
  const contextUsage = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.contextUsage ?? null;
  });
  // ADR-028: fallback data source for the agent-total token display.
  // The live `contextUsage` push is preferred; this fallback covers the
  // gap between Runtime start and the first LLM call (when no
  // `context_usage` event has fired yet for the active session, but the
  // session-list scan has already stashed historical totals).
  const agentTokenTotals = useAgentStore((s) => {
    if (!selectedAgentId) return null;
    return s.agents[selectedAgentId]?.agentTokenTotals ?? null;
  });
  const sessionStatus: SessionStatus | null = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.sessionStatus ?? null;
  });
  const sessionModel = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.model ?? null;
  });
  const sessionProvider = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.provider ?? null;
  });
  // ADR-066 §6: only Anthropic-protocol providers surface
  // `cache_creation_input_tokens` (the cache-write counter); OpenAI
  // Chat Completions and OpenAI-compatible providers never do. We use
  // this to gate the agent-total cache-WRITE row in the Agent Status
  // panel below — for OpenAI sessions that value is hard-wired to 0
  // by the Runtime, so showing "0 累计缓存写入" next to a live cache
  // READ counter is misleading (the metric simply doesn't exist for
  // the chosen protocol). The cache-READ row is unconditional, since
  // both protocol families report it.
  const cacheProtocol = getCacheProtocol(sessionProvider);
  const modelRatio = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.ratio ?? null;
  });
  const reasoningEffort = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.reasoningEffort ?? null;
  });
  const temperature = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.temperature ?? null;
  });
  const openSessionCount = useChatStore((s) => {
    if (!selectedAgentId) return 0;
    const agent = s.agentStates[selectedAgentId];
    return agent?.openSessionIds?.length ?? 0;
  });
  // Historical session total for the agent — from the backend session list
  // scan (`total_count` in GET /api/agents/{id}/sessions), refreshed by
  // `agentStore.fetchSessions`. Distinct from `openSessionCount` (active tabs).
  const totalSessionCount = useAgentStore((s) => {
    if (!selectedAgentId) return 0;
    return s.agents[selectedAgentId]?.pagination.totalCount ?? 0;
  });
  const isCompacting = useChatStore((s) => {
    if (!selectedAgentId) return false;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return false;
    return agent.sessionStates[agent.activeSessionId]?.isCompacting ?? false;
  });
  const messages = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[agent.activeSessionId]?.messages ?? EMPTY_MESSAGES;
  });

  // ADR-066 §6: cache-hit ratio.  Provider billing model differs:
  //   - Anthropic Messages: `cache_read / (input + cache_read + cache_write)`
  //     (write is a real billable event — the denominator includes it).
  //   - OpenAI Chat Completions: `cache_read / prompt_tokens`
  //     (OpenAI has no write concept; auto-cache only).
  //   - Other providers (ollama, deepseek, zhipuai, …): no cache
  //     accounting at all → helper returns `null` and we hide the row.
  // This block is the **session-status** surface, so it shows the
  // session-lifetime (cumulative) rate — the input-box popover shows
  // the per-turn rate instead.  We deliberately do NOT compute this on
  // the backend — the choice of denominator is a UX policy owned by
  // the front-end so designers can tweak it without re-shipping the
  // runtime.
  const cacheStats = computeCacheHitStats(sessionProvider, contextUsage, "cumulative");
  const cacheHitRateLabel = formatCacheHitRate(cacheStats.ratio);
  // Numeric 0-100 for the progress-bar width (mirrors `usage_percent`).
  const cacheHitRatePercent =
    cacheStats.ratio != null ? Math.round(cacheStats.ratio * 100) : 0;

  // ── Debug store (always called, conditionally used) ──────────────
  const {
    connected,
    debugAgentId,
    sessionStates,
    connect,
    disconnect,
    disableDebugMode,
    resume,
    pause: pauseDebug,
    step,
    stop,
    restart,
    getSection,
    rewind,
    reExecute,
    patchContext,
  } = useDebugStore();
  const { t } = useTranslation();
  const sessionDebugState = activeSessionId ? sessionStates[activeSessionId] : null;
  const iteration = sessionDebugState?.iteration ?? 0;
  const phase = sessionDebugState?.phase ?? "Idle";
  const debugState = sessionDebugState?.debugState ?? "Stepping";
  const promptTokens = sessionDebugState?.promptTokens ?? 0;
  const completionTokens = sessionDebugState?.completionTokens ?? 0;
  const snapshots = sessionDebugState?.snapshots ?? [];
  const sectionCache = sessionDebugState?.sectionCache ?? new Map();
  const hasPendingPatches = sessionDebugState?.hasPendingPatches ?? false;
  const autoConnectAttempted = useRef(false);
  const prevAgentId = useRef<string | null>(null);

  // Debug section expansion / editing state
  const [expandedSections, setExpandedSections] = useState<Set<string>>(new Set());
  const [loadedSections, setLoadedSections] = useState<Set<string>>(new Set());
  // ADR-048 follow-up: in-flight flag for the runtime DevMode enable
  // button. Disables the button + changes its label while the
  // `enable_agent_debug` Tauri command is awaiting a response, so the
  // user can't double-click and double-fire the wiring.
  const [enablingDebug, setEnablingDebug] = useState(false);
  // Symmetric to `enablingDebug`: disables the "Exit Debug" button
  // while the runtime `disable_agent_debug` Tauri command +
  // `fetchAgents()` round-trip is in flight. The double-click
  // scenario matters here too — Runtime `disable_debug_mode` is
  // idempotent, but `fetchAgents()` would race with itself if the
  // user double-clicks.
  const [disablingDebug, setDisablingDebug] = useState(false);
  const [editingSection, setEditingSection] = useState<{
    iteration: number;
    section: string;
    original: string;
    current: string;
  } | null>(null);

  // Selected agent info (already derived above)

  // Count iterations (number of assistant messages)
  const iterations = messages.filter((m) => m.type === "assistant").length;

  // ── Debug auto-connect effect ────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode || !selectedAgentId) return;

    // ADR-048 D6: debug RPC goes through the Gateway HTTP reverse proxy
    // and events ride the shared MQTT subscription, but the Desktop MQTT
    // client still connects to the broker on 127.0.0.1 - in remote mode
    // (Desktop on a different machine than Gateway/Runtime) debug events
    // would not flow. Skip silently.
    if (!isGatewayLocal()) return;

    const agentChanged = selectedAgentId !== prevAgentId.current;

    // ADR-048 follow-up: `debug_state === "enabled"` covers both
    // startup `--dev-mode` and runtime enable (dev_mode stays false
    // after POST /api/agents/{id}/debug/enable).
    if (selectedAgent?.debug_state === "enabled" && selectedAgent.running) {
      if (agentChanged || !connected || debugAgentId !== selectedAgentId) {
        connect(selectedAgentId);
      }
      autoConnectAttempted.current = true;
    }

    if (agentChanged) {
      prevAgentId.current = selectedAgentId;
    }
  }, [isDebugMode, selectedAgentId, selectedAgent?.debug_state, selectedAgent?.running, connected, debugAgentId, connect]);

  // ── Debug disconnect effect ──────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode) return;
    if (connected && selectedAgent && (selectedAgent.debug_state !== "enabled" || !selectedAgent.running)) {
      disconnect();
    }
  }, [isDebugMode, selectedAgent?.debug_state, selectedAgent?.running, connected, disconnect]);

  // ── Debug toggle section callback ────────────────────────────────
  const toggleSection = useCallback(
    async (iteration: number, section: string) => {
      const key = `${iteration}:${section}`;
      setExpandedSections((prev) => {
        const next = new Set(prev);
        if (next.has(key)) {
          next.delete(key);
        } else {
          next.add(key);
          if (!loadedSections.has(key)) {
            getSection(activeSessionId, iteration, section);
            setLoadedSections((l) => new Set(l).add(key));
          }
        }
        return next;
      });
    },
    [activeSessionId, getSection, loadedSections]
  );

  // ── Switch to debug tab when entering debug mode ─────────────────
  const prevIsDebugMode = useRef(isDebugMode);
  useEffect(() => {
    if (isDebugMode && !prevIsDebugMode.current) {
      onTabChange("debug");
    }
    prevIsDebugMode.current = isDebugMode;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isDebugMode]);

  // ADR-034 Phase 5: Agent status query (fire-and-forget, no UI rendering yet).
  // Guarded on running+ready — the status endpoint proxies through the
  // Runtime and 503s against an unregistered one.  When the user starts the
  // agent, this selector re-fires the effect.
  useEffect(() => {
    if (!selectedAgentId) return;
    if (!selectedAgent?.running || !selectedAgent?.ready) return;
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/status`)
      .then((r) => r.ok ? r.json() : null)
      .then((data) => {
        if (data) log.debug("[ResultsPanel] Agent status:", data);
      })
      .catch(() => {/* ignore */});
  }, [selectedAgentId, selectedAgent?.running, selectedAgent?.ready]);

  return (
    <div className="relative flex flex-col shrink-0 bg-zinc-100 dark:bg-[#1F1F22] rounded-xl ml-1" style={{ width }}>
      {/* Resize handle overlay — sits at the left edge */}
      <div
        className="absolute -left-1 top-0 bottom-0 w-1 cursor-col-resize z-10 group"
        onMouseDown={onResizeStart}
      >
        <div className="absolute inset-y-0 left-0 w-1 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors" />
      </div>
      {/* Tab title header */}
      <div className="border-b border-zinc-200 px-3 pt-[11px] pb-[7px] text-xs font-medium text-zinc-500 dark:border-zinc-800 dark:text-zinc-400">
        {t(`resultsPanel.${activeTab}`)}
      </div>

      {/* ── Debug tab content ─────────────────────────────────────── */}
      {/* Restructured: the debug tab always renders an action block
          (with a two-state Enter/Exit Debug button on the left and the
          session debug controls on the right, gated by DevMode). The
          4 DevMode branches now live BELOW the action block:
            1. agent not running            → "no agent in debug mode"
               (replaces the action block — no point showing Enter Debug
                for an agent that isn't running)
            2. running, DevMode off         → action block only
               (the Enter Debug button is the only interactive element;
                the 4 session controls + lower cards are hidden)
            3. running, DevMode on, remote  → remote-unavailable
            4. running, DevMode on, local,
               not connected                → "debug connection lost"
            5. running, DevMode on, local,
               connected                    → state + snapshots + prompts
                                              + compression history */}
      {activeTab === "debug" && (
        <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
          {/* ADR-063 §3.7 — package prompt override editor. Always
              visible at the TOP of the Debug tab, BEFORE the
              "no agent running" placeholder, the action block, and
              the debug-only state/snapshots/compression cards.
              Operators must be able to browse and prepare overrides
              regardless of agent running state or DevMode. */}
          <PromptList />
          {!selectedAgent?.running ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
              <Bug className="h-5 w-5" />
              <span className="text-center">
                {t("resultsPanel.noAgentDebug")}
              </span>
            </div>
          ) : (
            <div className="p-3 space-y-3">
              {/* Section title — matches the visual weight of other
                  section headings in this tab (e.g. Status tab's
                  "Token statistics" h3). */}
              <div className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
                {t("resultsPanel.debugSection")}
              </div>

              {/* Action block — always shown when the agent is running.
                  Left: two-state text button (Enter/Exit Debug, btn-solid,
                  no icon). Right: 4 session debug buttons, only rendered
                  once DevMode is on. */}
              <div className="rounded-md border border-zinc-200 bg-modal-surface p-2 dark:border-zinc-700">
                <div className="flex min-h-[26px] items-center gap-1">
                  <Switch
                    checked={selectedAgent?.debug_state === "enabled"}
                    onChange={async (checked) => {
                      if (checked) {
                        /* Enter Debug — ADR-048 follow-up runtime DevMode
                           activation. Flips DevMode on at the Runtime via
                           POST /api/agents/{id}/debug/enable without
                           restarting the agent. Don't wait for the auto-
                           connect useEffect — its run is deferred to after
                           commit and would race the tab switch, leaving
                           `connected` momentarily false. Attach the debug
                           session synchronously. */
                        if (!selectedAgentId) return;
                        setEnablingDebug(true);
                        try {
                          log.debug("[ResultsPanel] enable_agent_debug: invoking", { agentId: selectedAgentId });
                          await invoke<{ enabled: boolean; already_enabled: boolean; debug_port: number }>(
                            "enable_agent_debug",
                            { agentId: selectedAgentId, debugPort: 0 },
                          );
                          log.debug("[ResultsPanel] enable_agent_debug: invoke ok, refreshing agents");
                          await useAgentStore.getState().fetchAgents();
                          log.debug("[ResultsPanel] enable_agent_debug: calling connect() directly");
                          useDebugStore.getState().connect(selectedAgentId);
                          onTabChange("debug");
                        } catch (err) {
                          log.error("[ResultsPanel] enable_agent_debug failed:", err);
                        } finally {
                          setEnablingDebug(false);
                        }
                      } else {
                        /* Exit Debug — tears down agent-wide DevMode via
                           useDebugStore.disableDebugMode(). No agent restart. */
                        if (disablingDebug) return;
                        setDisablingDebug(true);
                        try {
                          log.debug("[ResultsPanel] exit_debug: invoking disableDebugMode");
                          await disableDebugMode();
                          log.debug("[ResultsPanel] exit_debug: disableDebugMode ok");
                        } catch (err) {
                          log.error("[ResultsPanel] exit_debug failed:", err);
                        } finally {
                          setDisablingDebug(false);
                        }
                      }
                    }}
                    disabled={enablingDebug || disablingDebug}
                    size="sm"
                    label={
                      enablingDebug
                        ? t("resultsPanel.enteringDebug")
                        : disablingDebug
                          ? t("resultsPanel.exitingDebug")
                          : selectedAgent?.debug_state === "enabled"
                            ? t("resultsPanel.buttonExitDebug")
                            : t("resultsPanel.enterDebug")
                    }
                    labelPosition="right"
                  />
                  <div className="ml-auto" />
                  {selectedAgent?.debug_state === "enabled" && (
                    <>
                      <ControlButton
                        onClick={() => {
                          if (debugState === "Paused") void resume(activeSessionId);
                          else if (debugState === "Stopped") void restart(activeSessionId);
                          else void pauseDebug(activeSessionId);
                        }}
                        title={
                          debugState === "Paused"
                            ? "Resume (F5)"
                            : debugState === "Stopped"
                              ? t("resultsPanel.buttonRestart")
                              : "Pause (F6)"
                        }
                        active={debugState === "Paused"}
                      >
                        {debugState === "Paused"
                          ? <Play className="h-3.5 w-3.5" />
                          : <Pause className="h-3.5 w-3.5" />
                        }
                      </ControlButton>
                      <ControlButton
                        onClick={() => step(activeSessionId, "iteration")}
                        title={t("resultsPanel.buttonStep")}
                        disabled={debugState === "Stopped"}
                      >
                        <StepForward className="h-3.5 w-3.5" />
                      </ControlButton>
                      <ControlButton
                        onClick={() => stop(activeSessionId)}
                        title={t("resultsPanel.buttonStop")}
                        disabled={debugState === "Stopped"}
                      >
                        <Square className="h-3.5 w-3.5" />
                      </ControlButton>
                      <ControlButton onClick={() => restart(activeSessionId)} title={t("resultsPanel.buttonRestart")} disabled={!debugAgentId}>
                        <RefreshCw className="h-3.5 w-3.5" />
                      </ControlButton>
                      {hasPendingPatches && (
                        <>
                          <div className="mx-1 h-4 w-px bg-zinc-200 dark:bg-zinc-700" />
                          <ControlButton
                            onClick={() => reExecute(activeSessionId).catch(log.error)}
                            title={t("resultsPanel.buttonReExecute")}
                            active
                          >
                            <RotateCcw className="h-3.5 w-3.5" />
                          </ControlButton>
                        </>
                      )}
                    </>
                  )}
                </div>
              </div>

              {/* Below action block — only when in debug mode. The
                  lower cards (state, snapshots, prompts, compression
                  history) are intentionally hidden until the operator
                  commits to a debug session. */}
              {selectedAgent?.debug_state === "enabled" && (
                !isGatewayLocal() ? (
                  <div className="flex flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
                    <WifiOff className="h-5 w-5" />
                    <span className="text-center text-xs">
                      {t("resultsPanel.debugUnavailableRemote")}
                    </span>
                    <span className="text-center text-xs text-zinc-400">
                      {t("resultsPanel.debugRemoteDesc")}
                    </span>
                  </div>
                ) : !connected ? (
                  <div className="flex flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
                    <WifiOff className="h-5 w-5" />
                    <span className="text-center">
                      {t("resultsPanel.debugConnectionLost")}
                    </span>
                  </div>
                ) : (
                  <>
                    {/* State card */}
                    <div className="rounded-md border border-zinc-200 bg-modal-surface p-3 dark:border-zinc-700">
                      <div className="grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
                        <StateLabel label={t("resultsPanel.iteration")} value={`#${iteration}`} />
                        <StateLabel label={t("resultsPanel.phase")} value={phase} highlight />
                        <StateLabel label={t("resultsPanel.tokens")} value={`${promptTokens + completionTokens}`} />
                        <StateLabel
                          label={t("resultsPanel.sessionStatusLabel")}
                          value={debugState}
                          highlight={debugState !== "Running" && debugState !== "Stepping"}
                        />
                      </div>
                    </div>
                    {/* Context snapshots card */}
                    <div className="rounded-md border border-zinc-200 bg-modal-surface p-3 dark:border-zinc-700">
                      <div className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
                        {t("resultsPanel.contextSnapshots", { count: snapshots.length })}
                      </div>
                      {snapshots.length === 0 && (
                        <div className="py-3 text-center text-xs text-zinc-400">
                          {t("resultsPanel.noSnapshots")}
                          <br />
                          {t("resultsPanel.sendMessageToGenerate")}
                        </div>
                      )}
                      {snapshots.map((snap) => (
                        <SnapshotNode
                          key={snap.iteration}
                          snapshot={snap}
                          expandedSections={expandedSections}
                          sectionCache={sectionCache}
                          editingSection={editingSection}
                          onToggleSection={(section) => toggleSection(snap.iteration, section)}
                          onStartEdit={(section, original) =>
                            setEditingSection({ iteration: snap.iteration, section, original, current: original })
                          }
                          onCancelEdit={() => setEditingSection(null)}
                          onSaveEdit={(section, content) => {
                            const patches: Record<string, unknown> = {};
                            patches[section] = content;
                            patchContext(activeSessionId, patches).catch(log.error);
                            setEditingSection(null);
                          }}
                          onEditChange={(content) =>
                            setEditingSection((prev) => (prev ? { ...prev, current: content } : null))
                          }
                          onRewind={(iter) => rewind(activeSessionId, iter).catch(log.error)}
                          getSection={(iteration, section) => getSection(activeSessionId, iteration, section)}
                        />
                      ))}
                    </div>
                    <CompressionHistoryCard
                      agentId={selectedAgentId}
                      sessionId={activeSessionId}
                    />
                  </>
                )
              )}
            </div>
          )}
        </div>
      )}

      {/* ── Status tab content ───────────────────────────────────── */}
      {activeTab === "status" && (
        <div className="flex-1 overflow-y-auto p-3">
          {/* Token statistics */}
          <div className="mb-4">
            <h3 className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
              {t("resultsPanel.sessionStatus")}
            </h3>
            <div className="rounded-md bg-modal-surface p-3 text-xs">
              {/* Context usage progress bar */}
              {contextUsage ? (
                <div className="mb-3">
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-zinc-500">{t("resultsPanel.contextUsage")}</span>
                    <span className="font-mono font-medium" style={{ color: "var(--color-accent)" }}>
                      {formatPercent(contextUsage.usage_percent)}%
                    </span>
                  </div>
                  <div className="h-1.5 rounded-full bg-zinc-200 overflow-hidden dark:bg-zinc-700 mb-1.5">
                    <div
                      className="h-full rounded-full transition-all duration-300"
                      style={{ backgroundColor: "var(--color-accent)", width: `${contextUsage.usage_percent}%` }}
                    />
                  </div>
                  <div className="flex justify-between text-zinc-400 dark:text-zinc-500">
                    <span>{formatTokenCount(contextUsage.total_tokens)} {t("resultsPanel.used")}</span>
                    <span>{formatTokenCount(contextUsage.usable_context)} / {formatTokenCount(contextUsage.context_window)} {t("resultsPanel.available")}</span>
                  </div>
                  {/* Compacting indicator */}
                  {isCompacting && (
                    <div className="flex items-center gap-1.5 mt-1">
                      <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                      <span className="thinking-shimmer text-zinc-500">{t("resultsPanel.compacting")}</span>
                    </div>
                  )}
                </div>
              ) : (
                <div className="mb-3 text-zinc-400 dark:text-zinc-500 italic">{t("resultsPanel.noContextData")}</div>
              )}
              {/* ADR-066: cache hit ratio — same progress-bar style as the
                  context-usage block above.  This is the session-status
                  surface, so it shows the session-lifetime (cumulative)
                  rate.  Shown whenever the Runtime reports any cumulative
                  cache accounting (read or write); the ratio itself falls
                  back to a dash when it isn't computable yet (e.g. a fresh
                  Anthropic session that has only seeded the cache), in
                  which case the progress bar is hidden.  The two numbers
                  below are the ratio's numerator (cumulative cache-hit
                  tokens) and denominator (cumulative input tokens) — never
                  two independent cache counters. */}
              {hasCacheData(contextUsage, "cumulative") && (
                <div className="mb-3">
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-zinc-500">{t("resultsPanel.cacheHitRatio")}</span>
                    <span className="font-mono font-medium" style={{ color: "var(--color-accent)" }}>
                      {cacheHitRateLabel ?? "\u2014"}
                    </span>
                  </div>
                  {cacheHitRateLabel !== null && (
                    <div className="h-1.5 rounded-full bg-zinc-200 overflow-hidden dark:bg-zinc-700 mb-1.5">
                      <div
                        className="h-full rounded-full transition-all duration-300"
                        style={{ backgroundColor: "var(--color-accent)", width: `${cacheHitRatePercent}%` }}
                      />
                    </div>
                  )}
                  <div className="flex justify-between text-zinc-400 dark:text-zinc-500">
                    <span>{formatTokenCount(cacheStats.numerator)} {t("resultsPanel.cached")}</span>
                    <span>{formatTokenCount(cacheStats.denominator)} {t("resultsPanel.promptTokens")}</span>
                  </div>
                </div>
              )}
              {/* Divider */}
              {contextUsage && <div className="border-t border-zinc-100 dark:border-zinc-700/50 mb-2" />}
              <StatRow label={t("resultsPanel.promptTokens")} value={(tokenUsage?.prompt_tokens ?? contextUsage?.input_tokens)?.toLocaleString()} />
              <StatRow label={t("resultsPanel.completionTokens")} value={(tokenUsage?.completion_tokens ?? contextUsage?.output_tokens)?.toLocaleString()} />
              {/* Cumulative session totals — sourced from SessionTokens via the
                  context_usage WebSocket event. Distinct from the per-turn
                  Prompt / Completion rows above (which use the `last_` value
                  from the most recent LLM call). Rendered only when the runtime
                  has reported at least one LLM call for this session. */}
              <StatRow
                label={t("resultsPanel.totalInputTokens")}
                value={contextUsage?.total_input_tokens?.toLocaleString()}
              />
              <StatRow
                label={t("resultsPanel.totalOutputTokens")}
                value={contextUsage?.total_output_tokens?.toLocaleString()}
              />
              {/* 字符/token — kept next to the token rows above so all
                  token-related counters read as one block. A divider
                  below separates this token cluster from the runtime
                  / model / status fields that follow. */}
              <StatRow label={t("resultsPanel.labelCharactersPerToken")} value={modelRatio != null ? modelRatio.toFixed(2) : undefined} />
              {/* Divider — separates the token-count cluster above from
                  the runtime / model / status fields below. Uses `my-2`
                  (not `mb-2`) so the gap above and below the line is
                  symmetric — `StatRow` already has `py-1`, so an
                  asymmetric `mb-2` makes the line look glued to the
                  text above and far from the text below. */}
              <div className="my-2 border-t border-zinc-100 dark:border-zinc-700/50" />
              <StatRow label={t("resultsPanel.iterations")} value={iterations ? String(iterations) : undefined} />
              {sessionModel && (
                <StatRow label={t("resultsPanel.labelModel")} value={sessionModel} />
              )}
              {sessionProvider && (
                <StatRow label={t("resultsPanel.labelProvider")} value={sessionProvider} />
              )}
              {reasoningEffort != null && (
                <StatRow label={t("resultsPanel.labelThinkingLevel")} value={reasoningEffort.charAt(0).toUpperCase() + reasoningEffort.slice(1)} />
              )}
              <StatRow label={t("resultsPanel.labelTemperature")} value={temperature != null ? temperature.toFixed(2) : undefined} />
              <div className="flex justify-between py-1">
                <span className="text-zinc-500">{t("resultsPanel.sessionStatusLabel")}</span>
                <span className="flex items-center gap-1.5 text-zinc-700 dark:text-zinc-300">
                  <span
                    className={cn(
                      "inline-block h-2 w-2 rounded-full",
                      // ADR-049: 6-state color map. Each phase gets its own
                      // visual cue so the operator can distinguish "waiting"
                      // (gray), "streaming" (accent), "tool_executing"
                      // (blue), "waiting_approval" (yellow), "paused"
                      // (amber), and "idle" / unknown (zinc).
                      getProcessingPhase(sessionStatus) === "streaming" && "bg-[var(--color-accent)]",
                      getProcessingPhase(sessionStatus) === "waiting" && "bg-zinc-400 dark:bg-zinc-500",
                      getProcessingPhase(sessionStatus) === "tool_executing" && "bg-blue-400",
                      getProcessingPhase(sessionStatus) === "waiting_approval" && "bg-yellow-400",
                      getProcessingPhase(sessionStatus) === "paused" && "bg-amber-400",
                      (getProcessingPhase(sessionStatus) === "idle" || !sessionStatus) && "bg-zinc-300 dark:bg-zinc-600",
                    )}
                  />
                  {sessionStatus ? sessionStatus.status.replace(/_/g, " ") : "\u2014"}
                </span>
              </div>
            </div>
          </div>

          {/* Agent running status */}
          <div>
            <h3 className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
              {t("resultsPanel.agentStatus")}
            </h3>
            <div className="rounded-md bg-modal-surface p-3 text-xs">
              {selectedAgent ? (
                <>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.sessionStatusLabel")}</span>
                    <span className="flex items-center gap-1.5">
                      <span
                        className={cn(
                          "inline-block h-2 w-2 rounded-full",
                          selectedAgent.running ? "bg-[var(--color-accent)]" : "bg-zinc-300 dark:bg-zinc-600",
                        )}
                      />
                      <span className="text-zinc-700 dark:text-zinc-300">
                        {selectedAgent.running ? t("resultsPanel.running") : t("resultsPanel.stopped")}
                      </span>
                    </span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.agent")}</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{selectedAgent.name}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.version")}</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{selectedAgent.version}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.activeSessions")}</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{openSessionCount}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.totalSessions")}</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{totalSessionCount}</span>
                  </div>
                  {/* Divider — separates the identity / session-count
                      cluster above from the agent-scoped token totals
                      below. Mirrors the divider style used in the
                      Session Status panel above the prompt/completion
                      token rows. Uses `my-2` so the gap above and
                      below the line is symmetric (the surrounding rows
                      use `py-1`, so an asymmetric `mb-2` would make the
                      line look glued to the row above). */}
                  <div className="my-2 border-t border-zinc-100 dark:border-zinc-700/50" />
                  {/* ADR-028: agent-scoped cumulative totals across every LLM
                      call made by this Runtime process for this agent. These
                      are agent-level (not session-level) figures, so they
                      live in the Agent Status panel rather than the session
                      context panel above. Precedence:
                        1. live `context_usage` WebSocket push
                           (contextUsage?.agent_total_input_tokens) — most
                           authoritative, updated on every LLM call;
                        2. fallback stashed by
                           `agentStore.agents[id].agentTokenTotals`,
                           refreshed on every session-list fetch — covers
                           the gap before the first LLM call lands, and
                           remains usable even when no session is active. */}
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.agentTotalInputTokens")}</span>
                    <span className="font-mono text-zinc-700 dark:text-zinc-300">
                      {(
                        contextUsage?.agent_total_input_tokens ??
                        agentTokenTotals?.input
                      )?.toLocaleString() ?? "—"}
                    </span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.agentTotalOutputTokens")}</span>
                    <span className="font-mono text-zinc-700 dark:text-zinc-300">
                      {(
                        contextUsage?.agent_total_output_tokens ??
                        agentTokenTotals?.output
                      )?.toLocaleString() ?? "—"}
                    </span>
                  </div>
                  {/* ADR-066: agent-level cumulative cache totals.  These are
                      agent-scoped (across every LLM call for this agent), so
                      they live in the Agent Status panel.  The session-level
                      real-time cache hit rate lives in the Session Status
                      block above — it is NOT duplicated here.  Precedence:
                        1. live `context_usage` WebSocket push
                           (contextUsage?.agent_total_cache_read_tokens) —
                           most authoritative, updated on every LLM call;
                        2. fallback stashed by
                           `agentStore.agents[id].agentTokenTotals`,
                           refreshed on every session-list fetch. */}
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">{t("resultsPanel.agentTotalCacheReadTokens")}</span>
                    <span className="font-mono text-zinc-700 dark:text-zinc-300">
                      {(
                        contextUsage?.agent_total_cache_read_tokens ??
                        agentTokenTotals?.cacheRead
                      )?.toLocaleString() ?? "—"}
                    </span>
                  </div>
                  {/* ADR-066 §6: cache-write is an Anthropic-only concept.
                      OpenAI Chat Completions has no cache-write event
                      (caching is automatic and not surfaced as a per-
                      call write token), and the Runtime hard-wires this
                      counter to 0 for OpenAI-protocol providers. Showing
                      "0 累计缓存写入" next to a live cache-READ value
                      reads as a bug — better to hide the row entirely
                      for non-Anthropic sessions. */}
                  {cacheProtocol === "anthropic" && (
                    <div className="flex justify-between py-1">
                      <span className="text-zinc-500">{t("resultsPanel.agentTotalCacheWriteTokens")}</span>
                      <span className="font-mono text-zinc-700 dark:text-zinc-300">
                        {(
                          contextUsage?.agent_total_cache_write_tokens ??
                          agentTokenTotals?.cacheWrite
                        )?.toLocaleString() ?? "—"}
                      </span>
                    </div>
                  )}
                </>
              ) : (
                <div className="py-1 text-zinc-400 dark:text-zinc-500">{t("resultsPanel.noAgentSelected")}</div>
              )}
            </div>
          </div>
        </div>
      )}

      {/* ── Memory tab content — CSS visibility preserves component state across tab switches ── */}
      <div
        className="flex min-h-0 flex-1 flex-col overflow-hidden"
        style={{ display: activeTab === "memory" ? "flex" : "none" }}
      >
        <MemoryPanel />
      </div>

      {/* ── Setup tab content ─────────────────────────────────────── */}
      {activeTab === "setup" && (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
          <AgentSetupTab />
        </div>
      )}

      {/* ── Tools tab content ─────────────────────────────────────── */}
      {activeTab === "tools" && <ToolsTab />}

      {/* ── Workspace tab content ─────────────────────────────────── */}
      <div
        className="flex min-h-0 flex-1 flex-col overflow-hidden"
        style={{ display: activeTab === "workspace" ? "flex" : "none" }}
      >
        <WorkspaceExplorer />
      </div>
    </div>

  );
}

function formatTokenCount(n: number): string {
  // M uses 2 decimals (= 10K granularity) so e.g. 2.71M and 2.78M
  // don't both collapse to "2.7M"; that was previously impossible
  // to distinguish in the cache-vs-input pair of the Session Status
  // panel because the gap between them was under 100K.
  // K keeps 1 decimal (100-token granularity) — at K scale that
  // resolution is still finer than any visual difference the eye
  // can pick out next to a M-scale neighbour, and bumping it would
  // just widen the column without adding real signal.
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

function StatRow({ label, value }: { label: string; value?: string }) {
  return (
    <div className="flex items-center justify-between gap-2 py-1">
      <span className="shrink-0 text-zinc-500">{label}</span>
      <span
        className="min-w-0 truncate font-mono text-zinc-700 dark:text-zinc-300"
        title={value}
      >
        {value ?? "\u2014"}
      </span>
    </div>
  );
}
