import { useState, useEffect, useRef, useCallback } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useDebugStore } from "../../stores/debugStore";
import type { ChatMessage, SessionStatus } from "../../lib/types";
import { cn } from "../../lib/utils";
import {
  Bug,
  WifiOff,
  Loader,
  Play,
  Pause,
  StepForward,
  Square,
  RefreshCw,
  RotateCcw,
} from "lucide-react";
import { AgentSetupTab } from "./AgentSetupTab";
import { MemoryPanel } from "../memory/MemoryPanel";
import { WorkspaceExplorer } from "../workspace/WorkspaceExplorer";
import { ControlButton, StateLabel, SnapshotNode } from "../debug/DebugPanel";
import { isGatewayLocal } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { TabButton } from "../common/tab";
import { ScrollableTabBar } from "../common/ScrollableTabBar";

interface ResultsPanelProps {
  onCollapse: () => void;
  isDebugMode?: boolean;
  onResizeStart?: (e: React.MouseEvent) => void;
}

type PanelTab = "debug" | "status" | "setup" | "memory" | "workspace";

// Stable empty array reference to avoid Zustand selector infinite loop
const EMPTY_MESSAGES: ChatMessage[] = [];

export function ResultsPanel({ width, isDebugMode = false, onResizeStart }: ResultsPanelProps & { width: number }) {
  const { agents, selectedAgentId } = useAgentStore();
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
  const sessionStatus: SessionStatus | null = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.sessionStatus ?? null;
  });
  const openSessionCount = useChatStore((s) => {
    if (!selectedAgentId) return 0;
    const agent = s.agentStates[selectedAgentId];
    return agent?.openSessionIds?.length ?? 0;
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
  const [activeTab, setActiveTab] = useState<PanelTab>(isDebugMode ? "debug" : "workspace");

  // ── Debug store (always called, conditionally used) ──────────────
  const {
    connected,
    connecting,
    debugAgentId,
    iteration,
    phase,
    debugState,
    promptTokens,
    completionTokens,
    snapshots,
    sectionCache,
    hasPendingPatches,
    connect,
    disconnect,
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
  const autoConnectAttempted = useRef(false);
  const prevAgentId = useRef<string | null>(null);

  // Debug section expansion / editing state
  const [expandedSections, setExpandedSections] = useState<Set<string>>(new Set());
  const [loadedSections, setLoadedSections] = useState<Set<string>>(new Set());
  const [editingSection, setEditingSection] = useState<{
    iteration: number;
    section: string;
    original: string;
    current: string;
  } | null>(null);

  // Selected agent info
  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Count iterations (number of assistant messages)
  const iterations = messages.filter((m) => m.type === "assistant").length;

  // ── Debug auto-connect effect ────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode || !selectedAgentId) return;

    // Debug WebSocket is a direct Desktop ↔ Runtime connection (127.0.0.1:19878).
    // In remote mode (Desktop on a different machine than Gateway/Runtime),
    // this connection cannot be established. Skip silently.
    if (!isGatewayLocal()) return;

    const agentChanged = selectedAgentId !== prevAgentId.current;

    if (selectedAgent?.dev_mode && selectedAgent.running) {
      if (agentChanged || !connected || debugAgentId !== selectedAgentId) {
        connect(selectedAgentId, selectedAgent?.debug_port);
      }
      autoConnectAttempted.current = true;
    }

    if (agentChanged) {
      prevAgentId.current = selectedAgentId;
    }
  }, [isDebugMode, selectedAgentId, selectedAgent?.dev_mode, selectedAgent?.running, connected, debugAgentId, connect]);

  // ── Debug disconnect effect ──────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode) return;
    if (connected && selectedAgent && (!selectedAgent.dev_mode || !selectedAgent.running)) {
      disconnect();
    }
  }, [isDebugMode, selectedAgent?.dev_mode, selectedAgent?.running, connected, disconnect]);

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
            getSection(iteration, section);
            setLoadedSections((l) => new Set(l).add(key));
          }
        }
        return next;
      });
    },
    [getSection, loadedSections]
  );

  // ── Switch to debug tab when entering debug mode ─────────────────
  const prevIsDebugMode = useRef(isDebugMode);
  useEffect(() => {
    if (isDebugMode && !prevIsDebugMode.current) {
      setActiveTab("debug");
    }
    prevIsDebugMode.current = isDebugMode;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isDebugMode]);

  // ── Switch to status tab when agent stops ────────────────────────
  const prevRunning = useRef(selectedAgent?.running);
  useEffect(() => {
    const isRunning = selectedAgent?.running ?? false;
    const wasRunning = prevRunning.current;
    if (!isRunning && wasRunning !== false && (activeTab === "memory" || activeTab === "setup")) {
      setActiveTab("status");
    }
    prevRunning.current = isRunning;
  }, [selectedAgent?.running, activeTab]);

  return (
    <div className="relative flex flex-col border-l border-zinc-200 bg-[#fafafa] dark:border-zinc-800 dark:bg-zinc-900" style={{ width }}>
      {/* Resize handle overlay — invisible, sits on the border-l */}
      <div
        className="absolute left-0 top-0 bottom-0 w-2 cursor-col-resize z-10 group"
        onMouseDown={onResizeStart}
      >
        <div className="absolute inset-y-0 left-0 w-2 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors" />
      </div>
      {/* Header with tabs */}
      <div className="border-b border-zinc-200 pt-px dark:border-zinc-800">
        <div className="flex items-center px-1 pt-1">
          <ScrollableTabBar>
            <TabButton
              className="px-[var(--tab-px)] py-[var(--tab-py)] text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]"
              active={activeTab === "workspace"}
              onClick={() => setActiveTab("workspace")}
            >
              {t("resultsPanel.workspace")}
            </TabButton>
            {isDebugMode && (
              <TabButton
                className="px-[var(--tab-px)] py-[var(--tab-py)] text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]"
                active={activeTab === "debug"}
                onClick={() => setActiveTab("debug")}
              >
                {connected ? (
                  <span className="flex items-center gap-1.5">
                    <Bug className="h-3.5 w-3.5 text-amber-600" />
                    {t("resultsPanel.debug")}
                  </span>
                ) : (
                  <span className="flex items-center gap-1.5">
                    {connecting ? (
                      <Loader className="h-3.5 w-3.5 animate-spin" />
                    ) : (
                      <WifiOff className="h-3.5 w-3.5" />
                    )}
                    {t("resultsPanel.debug")}
                  </span>
                )}
              </TabButton>
            )}
            <TabButton
              className="px-[var(--tab-px)] py-[var(--tab-py)] text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]"
              active={activeTab === "status"}
              onClick={() => setActiveTab("status")}
            >
              {t("resultsPanel.status")}
            </TabButton>
            {selectedAgent?.running && (
              <TabButton
                className="px-[var(--tab-px)] py-[var(--tab-py)] text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]"
                active={activeTab === "memory"}
                onClick={() => setActiveTab("memory")}
              >
                {t("resultsPanel.memory")}
              </TabButton>
            )}
            {selectedAgent?.running && (
              <TabButton
                className="px-[var(--tab-px)] py-[var(--tab-py)] text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]"
                active={activeTab === "setup"}
                onClick={() => setActiveTab("setup")}
              >
                {t("resultsPanel.setup")}
              </TabButton>
            )}
          </ScrollableTabBar>
        </div>
      </div>

      {/* ── Debug tab content ─────────────────────────────────────── */}
      {activeTab === "debug" && isDebugMode && (
        <>
          {!isGatewayLocal() ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
              <WifiOff className="h-5 w-5" />
              <span className="text-center text-xs">
                {t("resultsPanel.debugUnavailableRemote")}
              </span>
              <span className="text-center text-xs text-zinc-400">
                {t("resultsPanel.debugRemoteDesc")}
              </span>
            </div>
          ) : !connected ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
              {connecting ? (
                <>
                  <Loader className="h-5 w-5 animate-spin" />
                  <span>{t("resultsPanel.connectingDebug")}</span>
                </>
              ) : (
                <>
                  <WifiOff className="h-5 w-5" />
                  <span className="text-center">
                    {selectedAgent?.running && selectedAgent?.dev_mode
                      ? t("resultsPanel.debugConnectionLost")
                      : selectedAgent?.running
                        ? t("resultsPanel.agentNotDebugMode")
                        : t("resultsPanel.noAgentDebug")}
                  </span>
                </>
              )}
            </div>
          ) : (
            <div className="flex-1 overflow-y-auto p-3 space-y-3">
              {/* ── Controls card ──────────────────────────────────── */}
              <div className="rounded-lg border border-zinc-200 bg-white p-2 dark:border-zinc-700 dark:bg-zinc-800">
                <div className="flex items-center gap-1">
                  <ControlButton
                    onClick={debugState === "Paused" ? resume : debugState === "Stopped" ? restart : pauseDebug}
                    title={
                      debugState === "Paused"
                        ? "Resume (F5)"
                        : debugState === "Stopped"
                          ? "Restart"
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
                    onClick={() => step("iteration")}
                    title="Step (F10)"
                    disabled={debugState === "Stopped"}
                  >
                    <StepForward className="h-3.5 w-3.5" />
                  </ControlButton>
                  <ControlButton
                    onClick={stop}
                    title="Stop"
                    disabled={debugState === "Stopped"}
                  >
                    <Square className="h-3.5 w-3.5" />
                  </ControlButton>
                  <ControlButton onClick={restart} title="Restart" disabled={!debugAgentId}>
                    <RefreshCw className="h-3.5 w-3.5" />
                  </ControlButton>
                  {hasPendingPatches && (
                    <>
                      <div className="mx-1 h-4 w-px bg-zinc-200 dark:bg-zinc-700" />
                      <ControlButton
                        onClick={() => reExecute().catch(console.error)}
                        title="Re-execute with patches"
                        active
                      >
                        <RotateCcw className="h-3.5 w-3.5" />
                      </ControlButton>
                    </>
                  )}
                </div>
              </div>

              {/* ── State card ─────────────────────────────────────── */}
              <div className="rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-700 dark:bg-zinc-800">
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

              {/* ── Context snapshots card ─────────────────────────── */}
              <div className="rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-700 dark:bg-zinc-800">
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
                      patchContext(patches).catch(console.error);
                      setEditingSection(null);
                    }}
                    onEditChange={(content) =>
                      setEditingSection((prev) => (prev ? { ...prev, current: content } : null))
                    }
                    onRewind={(iter) => rewind(iter).catch(console.error)}
                    getSection={getSection}
                  />
                ))}
              </div>
            </div>
          )}
        </>
      )}

      {/* ── Status tab content ───────────────────────────────────── */}
      {activeTab === "status" && (
        <div className="flex-1 overflow-y-auto p-3">
          {/* Token statistics */}
          <div className="mb-4">
            <h3 className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
              {t("resultsPanel.sessionStatus")}
            </h3>
            <div className="rounded-md bg-white p-3 text-xs dark:bg-zinc-800">
              {/* Context usage progress bar */}
              {contextUsage ? (
                <div className="mb-3">
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-zinc-500">{t("resultsPanel.contextUsage")}</span>
                    <span className="font-mono font-medium" style={{ color: "var(--color-accent)" }}>
                      {contextUsage.usage_percent}%
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
              {/* Divider */}
              {contextUsage && <div className="border-t border-zinc-100 dark:border-zinc-700/50 mb-2" />}
              <StatRow label={t("resultsPanel.promptTokens")} value={(tokenUsage?.prompt_tokens ?? contextUsage?.input_tokens)?.toLocaleString()} />
              <StatRow label={t("resultsPanel.completionTokens")} value={(tokenUsage?.completion_tokens ?? contextUsage?.output_tokens)?.toLocaleString()} />
              <StatRow label={t("resultsPanel.totalTokens")} value={(tokenUsage?.total_tokens ?? contextUsage?.total_tokens)?.toLocaleString()} />
              <StatRow label={t("resultsPanel.iterations")} value={iterations ? String(iterations) : undefined} />
              <div className="flex justify-between py-1">
                <span className="text-zinc-500">{t("resultsPanel.sessionStatusLabel")}</span>
                <span className="flex items-center gap-1.5 text-zinc-700 dark:text-zinc-300">
                  <span
                    className={cn(
                      "inline-block h-2 w-2 rounded-full",
                      sessionStatus?.status === "streaming" && "bg-[var(--color-accent)]",
                      sessionStatus?.status === "idle" && "bg-zinc-300 dark:bg-zinc-600",
                      sessionStatus?.status === "paused" && "bg-amber-400",
                      sessionStatus?.status === "waiting_approval" && "bg-yellow-400",
                      !sessionStatus && "bg-zinc-300 dark:bg-zinc-600",
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
            <div className="rounded-md bg-white p-3 text-xs dark:bg-zinc-800">
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
                    <span className="text-zinc-500">{t("resultsPanel.sessions")}</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{openSessionCount}</span>
                  </div>
                </>
              ) : (
                <div className="py-1 text-zinc-400 dark:text-zinc-500">{t("resultsPanel.noAgentSelected")}</div>
              )}
            </div>
          </div>
        </div>
      )}

      {/* ── Memory tab content ────────────────────────────────────── */}
      {activeTab === "memory" && <MemoryPanel />}

      {/* ── Setup tab content ─────────────────────────────────────── */}
      {activeTab === "setup" && <AgentSetupTab />}

      {/* ── Workspace tab content ─────────────────────────────────── */}
      {activeTab === "workspace" && <WorkspaceExplorer />}
    </div>
  );
}

function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

function StatRow({ label, value }: { label: string; value?: string }) {
  return (
    <div className="flex justify-between py-1">
      <span className="text-zinc-500">{label}</span>
      <span className="font-mono text-zinc-700 dark:text-zinc-300">{value ?? "\u2014"}</span>
    </div>
  );
}