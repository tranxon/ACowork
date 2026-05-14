import { useEffect, useRef, useState, useCallback } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useDebugStore } from "../../stores/debugStore";
import { cn } from "../../lib/utils";
import {
  Play,
  Pause,
  StepForward,
  Square,
  RefreshCw,
  Bug,
  ChevronDown,
  ChevronRight,
  Loader,
  Wifi,
  WifiOff,
  Rewind,
  RotateCcw,
  Edit3,
  Check,
  X,
} from "lucide-react";

interface SectionContentType {
  content: string;
  hash: string;
  token_count: number;
}

export type { SectionContentType };

export const SECTION_LABELS: Record<string, string> = {
  system_prompt: "System Prompt",
  workspace_context: "Workspace Context",
  environment: "Environment",
  tool_definitions: "Tool Definitions",
  skill_instructions: "Skill Instructions",
  retrieved_memory: "Retrieved Memory",
  identity_context: "Identity Context",
};

export const SECTION_ORDER = [
  "system_prompt",
  "workspace_context",
  "environment",
  "tool_definitions",
  "skill_instructions",
  "retrieved_memory",
  "identity_context",
];

export function formatBytes(bytes: number): string {
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

// ── Component ──────────────────────────────────────────────────────────

export function DebugPanel({ width = 320 }: { width?: number }) {
  const { agents, selectedAgentId } = useAgentStore();
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

  // Track which sections are expanded: key = `${iteration}:${section}`
  const [expandedSections, setExpandedSections] = useState<Set<string>>(new Set());
  // Track which sections have content loaded
  const [loadedSections, setLoadedSections] = useState<Set<string>>(new Set());
  // Inline editing state: key = `${iteration}:${section}` → edited text
  const [editingSection, setEditingSection] = useState<{
    iteration: number;
    section: string;
    original: string;
    current: string;
  } | null>(null);
  const autoConnectAttempted = useRef(false);
  const prevAgentId = useRef<string | null>(null);

  // Determine if the selected agent is in debug mode
  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Auto-connect when a debug-mode agent is selected or becomes active
  useEffect(() => {
    if (!selectedAgentId) return;

    const agentChanged = selectedAgentId !== prevAgentId.current;

    if (selectedAgent?.dev_mode && selectedAgent.running) {
      // Connect when: agent switched, or agent just entered debug mode, or not yet connected
      if (agentChanged || !connected || debugAgentId !== selectedAgentId) {
        connect(selectedAgentId, selectedAgent?.debug_port);
      }
      autoConnectAttempted.current = true;
    }

    if (agentChanged) {
      prevAgentId.current = selectedAgentId;
    }
  }, [selectedAgentId, selectedAgent?.dev_mode, selectedAgent?.running, connected, debugAgentId, connect]);

  // Disconnect when selected agent is no longer in debug mode
  useEffect(() => {
    if (connected && selectedAgent && (!selectedAgent.dev_mode || !selectedAgent.running)) {
      disconnect();
    }
  }, [selectedAgent?.dev_mode, selectedAgent?.running, connected, disconnect]);

  // Poll state when connected
  useEffect(() => {
    if (!connected) return;
    const interval = setInterval(() => {
      getState().catch(() => {});
    }, 1000);
    return () => clearInterval(interval);
  }, [connected, getState]);

  const toggleSection = useCallback(
    async (iteration: number, section: string) => {
      const key = `${iteration}:${section}`;
      setExpandedSections((prev) => {
        const next = new Set(prev);
        if (next.has(key)) {
          next.delete(key);
        } else {
          next.add(key);
          // Trigger lazy load
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

  // ── Not connected fallback ───────────────────────────────────────────

  if (!connected) {
    return (
      <div className="flex flex-col items-center justify-center gap-3 border-l border-zinc-200 bg-zinc-50 p-6 text-sm text-zinc-500 dark:border-zinc-800 dark:bg-zinc-900 dark:text-zinc-400" style={{ width }}>
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
    );
  }

  return (
    <div className="flex flex-col border-l border-zinc-200 bg-zinc-50 dark:border-zinc-800 dark:bg-zinc-900" style={{ width }}>
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <div className="flex items-center gap-2 text-sm font-medium text-zinc-700 dark:text-zinc-300">
          <Bug className="h-4 w-4 text-amber-600" />
          <span>Debug</span>
          <Wifi className="h-3 w-3 text-emerald-500" />
        </div>

      </div>

      {/* Control bar */}
      <div className="flex items-center gap-1 border-b border-zinc-200 px-2 py-1.5 dark:border-zinc-800">
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

      {/* State display */}
      <div className="border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <div className="grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
          <StateLabel label="Iteration" value={`#${iteration}`} />
          <StateLabel label="Phase" value={phase} highlight />
          <StateLabel label="Tokens" value={`${promptTokens + completionTokens}`} />
          <StateLabel
            label="State"
            value={debugState}
            highlight={debugState !== "Running" && debugState !== "Stepping"}
          />
        </div>
      </div>

      {/* Context snapshot tree */}
      <div className="flex-1 overflow-y-auto">
        <div className="px-2 py-1 text-xs font-medium text-zinc-500 dark:text-zinc-400">
          Context Snapshots ({snapshots.length})
        </div>
        {snapshots.length === 0 && (
          <div className="px-3 py-4 text-center text-xs text-zinc-400">
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
  );
}

// ── Sub-components ─────────────────────────────────────────────────────

export function ControlButton({
  children,
  onClick,
  title,
  active,
  disabled,
}: {
  children: React.ReactNode;
  onClick: () => void;
  title: string;
  active?: boolean;
  disabled?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      title={title}
      disabled={disabled}
      className={cn(
        "rounded p-1.5 transition-colors",
        disabled
          ? "cursor-not-allowed text-zinc-300 dark:text-zinc-600"
          : active
            ? "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
            : "text-zinc-500 hover:bg-zinc-200 hover:text-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-300"
      )}
    >
      {children}
    </button>
  );
}

export function StateLabel({
  label,
  value,
  highlight,
}: {
  label: string;
  value: string;
  highlight?: boolean;
}) {
  return (
    <>
      <span className="text-zinc-400 dark:text-zinc-500">{label}</span>
      <span
        className={cn(
          "text-right font-mono",
          highlight ? "text-amber-600 dark:text-amber-400" : "text-zinc-700 dark:text-zinc-300"
        )}
      >
        {value}
      </span>
    </>
  );
}

export function SnapshotNode({
  snapshot,
  expandedSections,
  sectionCache,
  editingSection,
  onToggleSection,
  onStartEdit,
  onCancelEdit,
  onSaveEdit,
  onEditChange,
  onRewind,
  getSection,
}: {
  snapshot: {
    iteration: number;
    built_at: string;
    sections: Record<string, { size_bytes: number; token_estimate: number; hash: string }>;
    total_token_estimate: number;
    phase: string;
  };
  expandedSections: Set<string>;
  sectionCache: Map<string, { content: string; hash: string; token_count: number }>;
  editingSection: { iteration: number; section: string; original: string; current: string } | null;
  onToggleSection: (section: string) => void;
  onStartEdit: (section: string, original: string) => void;
  onCancelEdit: () => void;
  onSaveEdit: (section: string, content: string) => void;
  onEditChange: (content: string) => void;
  onRewind: (iteration: number) => void;
  getSection: (iteration: number, section: string) => Promise<SectionContentType | null>;
}) {
  const [collapsed, setCollapsed] = useState(true);

  return (
    <div className="border-b border-zinc-100 dark:border-zinc-800">
      {/* Iteration header */}
      <button
        onClick={() => setCollapsed(!collapsed)}
        className="flex w-full items-center gap-1.5 px-3 py-2 text-left transition-colors hover:bg-zinc-100 dark:hover:bg-zinc-800"
      >
        {collapsed ? (
          <ChevronRight className="h-3 w-3 shrink-0 text-zinc-400" />
        ) : (
          <ChevronDown className="h-3 w-3 shrink-0 text-zinc-400" />
        )}
        <span className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
          Iteration #{snapshot.iteration}
        </span>
        <span className="ml-auto text-[10px] text-zinc-400">
          ~{snapshot.total_token_estimate} tok
        </span>
        <button
          onClick={(e) => {
            e.stopPropagation();
            onRewind(snapshot.iteration);
          }}
          title={`Rewind to iteration ${snapshot.iteration}`}
          className="ml-1 rounded p-0.5 text-zinc-400 transition-colors hover:bg-amber-100 hover:text-amber-600 dark:hover:bg-amber-900/30 dark:hover:text-amber-400"
        >
          <Rewind className="h-3 w-3" />
        </button>
      </button>

      {/* Sections */}
      {!collapsed && (
        <div className="pb-1">
          {SECTION_ORDER.map((sectionKey) => {
            const section = snapshot.sections[sectionKey];
            if (!section) return null;

            const cacheKey = `${snapshot.iteration}:${sectionKey}`;
            const isExpanded = expandedSections.has(cacheKey);
            const cachedContent = sectionCache.get(cacheKey);

            return (
              <div key={sectionKey}>
                {/* Section header */}
                <div className="flex w-full items-center gap-1.5 py-1 pl-8 pr-3 text-left transition-colors hover:bg-zinc-100 dark:hover:bg-zinc-800">
                  <button
                    onClick={() => onToggleSection(sectionKey)}
                    className="flex flex-1 items-center gap-1.5"
                  >
                    {isExpanded ? (
                      <ChevronDown className="h-2.5 w-2.5 shrink-0 text-zinc-400" />
                    ) : (
                      <ChevronRight className="h-2.5 w-2.5 shrink-0 text-zinc-400" />
                    )}
                    <span className="text-[11px] text-zinc-600 dark:text-zinc-400">
                      {SECTION_LABELS[sectionKey] ?? sectionKey}
                    </span>
                    <span className="ml-auto text-[10px] text-zinc-400">
                      {formatBytes(section.size_bytes)} / ~{section.token_estimate} tok
                    </span>
                  </button>
                  {/* Edit button — opens inline editor with the section's full content */}
                  <button
                    onClick={async () => {
                      const cacheKey = `${snapshot.iteration}:${sectionKey}`;
                      const cached = sectionCache.get(cacheKey);
                      if (cached) {
                        onStartEdit(sectionKey, cached.content);
                      } else {
                        // Lazy-load the content first
                        const loaded = await getSection(snapshot.iteration, sectionKey);
                        if (loaded) {
                          onStartEdit(sectionKey, loaded.content);
                        }
                      }
                    }}
                    title="Edit section"
                    className="rounded p-0.5 text-zinc-400 transition-colors hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                  >
                    <Edit3 className="h-2.5 w-2.5" />
                  </button>
                </div>

                {/* Section content (lazy-loaded or inline-editing) */}
                {isExpanded && (
                  <div className="mx-3 mb-1 rounded border border-zinc-200 bg-zinc-100/50 px-2 py-1.5 dark:border-zinc-700 dark:bg-zinc-800/50">
                    {/* Inline editing mode */}
                    {editingSection &&
                    editingSection.iteration === snapshot.iteration &&
                    editingSection.section === sectionKey ? (
                      <div className="flex flex-col gap-1.5">
                        <textarea
                          value={editingSection.current}
                          onChange={(e) => onEditChange(e.target.value)}
                          className="max-h-48 min-h-[80px] w-full resize-y rounded border border-amber-300 bg-white px-2 py-1 font-mono text-[10px] leading-relaxed text-zinc-700 outline-none focus:ring-1 focus:ring-amber-400 dark:border-amber-600 dark:bg-zinc-800 dark:text-zinc-300"
                          autoFocus
                        />
                        <div className="flex items-center gap-1">
                          <button
                            onClick={() => onSaveEdit(sectionKey, editingSection.current)}
                            className="flex items-center gap-0.5 rounded bg-amber-500 px-2 py-0.5 text-[10px] text-white transition-colors hover:bg-amber-600"
                          >
                            <Check className="h-2.5 w-2.5" />
                            Apply Patch
                          </button>
                          <button
                            onClick={onCancelEdit}
                            className="flex items-center gap-0.5 rounded px-2 py-0.5 text-[10px] text-zinc-500 transition-colors hover:bg-zinc-200 dark:text-zinc-400 dark:hover:bg-zinc-700"
                          >
                            <X className="h-2.5 w-2.5" />
                            Cancel
                          </button>
                        </div>
                      </div>
                    ) : cachedContent ? (
                      <>
                        <div className="mb-1 flex items-center gap-2 text-[10px] text-zinc-400">
                          <span>{cachedContent.token_count} tokens</span>
                          <span className="font-mono">{cachedContent.hash.slice(0, 8)}</span>
                        </div>
                        <pre className="max-h-32 overflow-y-auto whitespace-pre-wrap text-[10px] leading-relaxed text-zinc-600 dark:text-zinc-400">
                          {cachedContent.content.slice(0, 2000)}
                          {cachedContent.content.length > 2000 && (
                            <span className="text-zinc-400">... (truncated)</span>
                          )}
                        </pre>
                      </>
                    ) : (
                      <div className="flex items-center gap-1.5 text-[10px] text-zinc-400">
                        <Loader className="h-2.5 w-2.5 animate-spin" />
                        Loading section...
                      </div>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
