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
import { ControlButton, StateLabel, SnapshotNode } from "../debug/DebugPanel";
import { isGatewayLocal } from "../../lib/config";

interface ResultsPanelProps {
  onCollapse: () => void;
  isDebugMode?: boolean;
}

type PanelTab = "debug" | "status" | "setup" | "memory";

// Stable empty array reference to avoid Zustand selector infinite loop
const EMPTY_MESSAGES: ChatMessage[] = [];

export function ResultsPanel({ width, isDebugMode = false }: ResultsPanelProps & { width: number }) {
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
  const messages = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[agent.activeSessionId]?.messages ?? EMPTY_MESSAGES;
  });
  const [activeTab, setActiveTab] = useState<PanelTab>(isDebugMode ? "debug" : "status");

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
    getState,
    getSection,
    rewind,
    reExecute,
    patchContext,
  } = useDebugStore();
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

  // ── Debug poll state effect ──────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode || !connected) return;
    const interval = setInterval(() => {
      getState().catch(() => {});
    }, 1000);
    return () => clearInterval(interval);
  }, [isDebugMode, connected, getState]);

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
    <div className="flex flex-col border-l border-zinc-200 bg-zinc-50 transition-[width] duration-250 ease-in-out dark:border-zinc-800 dark:bg-zinc-900" style={{ width }}>
      {/* Header with tabs */}
      <div className="border-b border-zinc-200 dark:border-zinc-800">
        <div className="flex items-center px-3 pt-2">
          <div className="flex gap-0">
            {isDebugMode && (
              <TabButton
                active={activeTab === "debug"}
                onClick={() => setActiveTab("debug")}
              >
                {connected ? (
                  <span className="flex items-center gap-1.5">
                    <Bug className="h-3.5 w-3.5 text-amber-600" />
                    Debug
                  </span>
                ) : (
                  <span className="flex items-center gap-1.5">
                    {connecting ? (
                      <Loader className="h-3.5 w-3.5 animate-spin" />
                    ) : (
                      <WifiOff className="h-3.5 w-3.5" />
                    )}
                    Debug
                  </span>
                )}
              </TabButton>
            )}
            <TabButton
              active={activeTab === "status"}
              onClick={() => setActiveTab("status")}
            >
              Status
            </TabButton>
            {selectedAgent?.running && (
              <TabButton
                active={activeTab === "memory"}
                onClick={() => setActiveTab("memory")}
              >
                Memory
              </TabButton>
            )}
            {selectedAgent?.running && (
              <TabButton
                active={activeTab === "setup"}
                onClick={() => setActiveTab("setup")}
              >
                Setup
              </TabButton>
            )}
          </div>
        </div>
      </div>

      {/* ── Debug tab content ─────────────────────────────────────── */}
      {activeTab === "debug" && isDebugMode && (
        <>
          {!isGatewayLocal() ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
              <WifiOff className="h-5 w-5" />
              <span className="text-center text-xs">
                Debug unavailable in remote mode
              </span>
              <span className="text-center text-xs text-zinc-400">
                Debug requires a direct local connection to the Agent Runtime.
                The Desktop App and Gateway must run on the same machine.
              </span>
            </div>
          ) : !connected ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-sm text-zinc-500 dark:text-zinc-400">
              {connecting ? (
                <>
                  <Loader className="h-5 w-5 animate-spin" />
                  <span>Connecting to debug server...</span>
                </>
              ) : (
                <>
                  <WifiOff className="h-5 w-5" />
                  <span className="text-center">
                    {selectedAgent?.running && selectedAgent?.dev_mode
                      ? "Debug connection lost"
                      : selectedAgent?.running
                        ? "Agent is not in debug mode.\nUse Start in Debug to enable."
                        : "No agent in debug mode"}
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
                  <ControlButton onClick={restart} title="Restart">
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
                  <StateLabel label="Iteration" value={`#${iteration}`} />
                  <StateLabel label="Phase" value={phase} highlight />
                  <StateLabel label="Tokens" value={`${promptTokens + completionTokens}`} />
                  <StateLabel
                    label="Status"
                    value={debugState}
                    highlight={debugState !== "Running" && debugState !== "Stepping"}
                  />
                </div>
              </div>

              {/* ── Context snapshots card ─────────────────────────── */}
              <div className="rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-700 dark:bg-zinc-800">
                <div className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
                  Context Snapshots ({snapshots.length})
                </div>
                {snapshots.length === 0 && (
                  <div className="py-3 text-center text-xs text-zinc-400">
                    No context snapshots yet.
                    <br />
                    Send a message to the agent to generate snapshots.
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
              Session Stats
            </h3>
            <div className="rounded-md bg-white p-3 text-xs dark:bg-zinc-800">
              {/* Context usage progress bar */}
              {contextUsage ? (
                <div className="mb-3">
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-zinc-500">Context Usage</span>
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
                    <span>{formatTokenCount(contextUsage.total_tokens)} used</span>
                    <span>{formatTokenCount(contextUsage.usable_context)} / {formatTokenCount(contextUsage.context_window)} available</span>
                  </div>
                </div>
              ) : (
                <div className="mb-3 text-zinc-400 dark:text-zinc-500 italic">No context data yet</div>
              )}
              {/* Divider */}
              {contextUsage && <div className="border-t border-zinc-100 dark:border-zinc-700/50 mb-2" />}
              <StatRow label="Prompt tokens" value={(tokenUsage?.prompt_tokens ?? contextUsage?.input_tokens)?.toLocaleString()} />
              <StatRow label="Completion tokens" value={(tokenUsage?.completion_tokens ?? contextUsage?.output_tokens)?.toLocaleString()} />
              <StatRow label="Total tokens" value={(tokenUsage?.total_tokens ?? contextUsage?.total_tokens)?.toLocaleString()} />
              <StatRow label="Iterations" value={iterations ? String(iterations) : undefined} />
              <div className="flex justify-between py-1">
                <span className="text-zinc-500">Session Status</span>
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
              Agent Status
            </h3>
            <div className="rounded-md bg-white p-3 text-xs dark:bg-zinc-800">
              {selectedAgent ? (
                <>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">Status</span>
                    <span className="flex items-center gap-1.5">
                      <span
                        className={cn(
                          "inline-block h-2 w-2 rounded-full",
                          selectedAgent.running ? "bg-[var(--color-accent)]" : "bg-zinc-300 dark:bg-zinc-600",
                        )}
                      />
                      <span className="text-zinc-700 dark:text-zinc-300">
                        {selectedAgent.running ? "Running" : "Stopped"}
                      </span>
                    </span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">Agent</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{selectedAgent.name}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-zinc-500">Version</span>
                    <span className="text-zinc-700 dark:text-zinc-300">{selectedAgent.version}</span>
                  </div>
                </>
              ) : (
                <div className="py-1 text-zinc-400 dark:text-zinc-500">No agent selected</div>
              )}
            </div>
          </div>
        </div>
      )}

      {/* ── Memory tab content ────────────────────────────────────── */}
      {activeTab === "memory" && <MemoryPanel />}

      {/* ── Setup tab content ─────────────────────────────────────── */}
      {activeTab === "setup" && <AgentSetupTab />}
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

function TabButton({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "border-b-2 px-3 py-2 text-xs font-medium transition-colors",
        active
          ? "border-[var(--color-accent)] text-[var(--color-accent)] dark:border-[var(--color-accent)] dark:text-[var(--color-accent)]"
          : "border-transparent text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300",
      )}
    >
      {children}
    </button>
  );
}
