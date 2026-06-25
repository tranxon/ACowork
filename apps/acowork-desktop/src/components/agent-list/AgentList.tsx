import { useEffect, useState, useRef, useMemo } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useToast } from "../common/ToastProvider";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { AgentDetailDialog } from "./AgentDetailDialog";
import { CloneDialog } from "./CloneDialog";
import { PublishWizard } from "./PublishWizard";
import { CreateWizard } from "./CreateWizard";
import { AgentAvatar } from "../common/AgentAvatar";
import { Tooltip } from "../common/Tooltip";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { Play, Square, Trash2, Info, Copy, Plus, Search, Package, Sparkles, Bug } from "lucide-react";
import { StyledInput } from "../common/StyledInput";
import { open } from "@tauri-apps/plugin-dialog";
import type { CloneResponse } from "../../lib/types";
import { syncAgentUI } from "../../lib/agent-start";

interface AgentListProps {
  width?: number;
}

export function AgentList({ width }: AgentListProps) {
  const { t } = useTranslation();
  const isCollapsed = width !== undefined && width <= 80;
  const { selectedAgentId, loading, fetchAgents, selectAgent, startAgent, stopAgent, uninstallAgent, restartAgentInDebug } =
    useAgentStore();
  const agentsMap = useAgentStore((s) => s.agents);
  const agentsList = useMemo(() => Object.values(agentsMap).map((s) => s.meta), [agentsMap]);
  const fetchLatestSessionTitle = useAgentStore((s) => s.fetchLatestSessionTitle);
  const { addToast } = useToast();
  const [contextMenu, setContextMenu] = useState<{ agentId: string; x: number; y: number } | null>(null);
  const [installing, setInstalling] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const addMenuRef = useRef<HTMLDivElement>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [addMenuOpen, setAddMenuOpen] = useState(false);

  // Track agents currently waiting for the Runtime to become ready.
  // Reading this Set is the dedup gate for `handleStart` — see guard there.
  const [startingAgentIds, setStartingAgentIds] = useState<Set<string>>(new Set());

  /// Poll fetchAgents until the agent's `ready` flag becomes true, then connect WebSocket.
  /// Called after startAgent() succeeds. Max wait 15 seconds (30 × 500ms).
  const waitForAgentReady = async (agentId: string, devMode: boolean) => {
    setStartingAgentIds((prev) => new Set(prev).add(agentId));
    try {
      await syncAgentUI(agentId);
      addToast({ type: "success", message: devMode ? "Agent started in debug mode" : "Agent started" });
    } catch (e: any) {
      addToast({ type: "error", message: e?.message ?? String(e) });
    } finally {
      setStartingAgentIds((prev) => {
        const next = new Set(prev);
        next.delete(agentId);
        return next;
      });
    }
  };

  // Confirm dialog state
  const [confirmDialog, setConfirmDialog] = useState<{
    open: boolean;
    title: string;
    message: string;
    confirmLabel: string;
    destructive: boolean;
    onConfirm: () => void;
  }>({
    open: false,
    title: "",
    message: "",
    confirmLabel: "Confirm",
    destructive: false,
    onConfirm: () => { },
  });

  // Agent detail dialog state
  const [detailAgentId, setDetailAgentId] = useState<string | null>(null);

  // Clone dialog state
  const [cloneSource, setCloneSource] = useState<{ agentId: string; agentName: string } | null>(null);

  // Publish wizard state
  const [publishTarget, setPublishTarget] = useState<{ agentId: string; agentName: string } | null>(null);

  // Create wizard state
  const [showCreateWizard, setShowCreateWizard] = useState(false);

  useEffect(() => {
    fetchAgents();
    const interval = setInterval(fetchAgents, 30_000);
    return () => clearInterval(interval);
  }, [fetchAgents]);

  // Close context menu and add menu on click outside
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setContextMenu(null);
      }
      if (addMenuRef.current && !addMenuRef.current.contains(e.target as Node)) {
        setAddMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, []);

  // Fetch latest session title for each agent
  useEffect(() => {
    if (agentsList.length === 0) return;
    for (const agent of agentsList) {
      if (agentsMap[agent.agent_id]?.sessionTitle === undefined) {
        void fetchLatestSessionTitle(agent.agent_id);
      }
    }
  }, [agentsList, agentsMap, fetchLatestSessionTitle]);

  const handleInstall = async () => {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "Agent Package", extensions: ["agent"] }],
      });
      if (selected) {
        setInstalling(true);
        await useAgentStore.getState().installAgent(selected);
        addToast({ type: "success", message: "Agent installed successfully" });
        // Auto-select the newly installed agent
        await fetchAgents();
        const agentsNow = useAgentStore.getState().agents;
        const ids = Object.keys(agentsNow);
        if (ids.length > 0) {
          selectAgent(ids[ids.length - 1]);
        }
      }
    } catch (e) {
      addToast({ type: "error", message: `Failed to install agent: ${String(e)}` });
    } finally {
      setInstalling(false);
    }
  };

  const handleStart = async (agentId: string) => {
    // Dedup gate: prevent rapid double-fires (e.g. user double-clicks the list
    // item, or double-clicks and then triggers Start from the context menu).
    // The set is held until `waitForAgentReady` finishes, see below.
    if (startingAgentIds.has(agentId)) return;
    try {
      await startAgent(agentId);
      // Poll until ready, then connect WebSocket
      waitForAgentReady(agentId, false);
    } catch (e) {
      addToast({ type: "error", message: `Failed to start agent: ${String(e)}` });
    }
    setContextMenu(null);
  };

  const handleDebugStart = async (agentId: string) => {
    try {
      // Clean up any stale chat store state before starting debug mode.
      // Without this, a stopped agent may leave behind a stale wsMap entry
      // that prevents connectStream from creating a new WebSocket.
      const chatStore = useChatStore.getState();
      if (chatStore.wsMap[agentId]) {
        chatStore.disconnectStream(agentId);
      }
      await startAgent(agentId, true);
      // Poll until ready, then connect WebSocket
      waitForAgentReady(agentId, true);
    } catch (e) {
      addToast({ type: "error", message: `Failed to start debug agent: ${String(e)}` });
    }
    setContextMenu(null);
  };

  const handleRestartDebug = async (agentId: string) => {
    try {
      await restartAgentInDebug(agentId);
    } catch (e) {
      addToast({ type: "error", message: `Failed to restart in debug: ${String(e)}` });
    }
    setContextMenu(null);
  };

  const handleStop = async (agentId: string) => {
    const agent = agentsMap[agentId]?.meta;
    setConfirmDialog({
      open: true,
      title: "Stop Agent",
      message: t("agentList.stopConfirm", { agent: agent?.name ?? agentId }),
      confirmLabel: "Stop",
      destructive: true,
      onConfirm: async () => {
        setConfirmDialog((prev) => ({ ...prev, open: false }));
        try {
          await stopAgent(agentId);
          addToast({ type: "success", message: "Agent stopped" });
        } catch (e) {
          addToast({ type: "error", message: `Failed to stop agent: ${String(e)}` });
        }
      },
    });
    setContextMenu(null);
  };

  const handleUninstall = (agentId: string) => {
    // Block uninstalling System Agent
    if (agentId === "com.acowork.system") {
      addToast({ type: "warning", message: "System Agent cannot be uninstalled" });
      return;
    }
    const agent = agentsMap[agentId]?.meta;
    setConfirmDialog({
      open: true,
      title: "Uninstall Agent",
      message: t("agentList.uninstallConfirm", { agent: agent?.name ?? agentId }),
      confirmLabel: "Uninstall",
      destructive: true,
      onConfirm: async () => {
        setConfirmDialog((prev) => ({ ...prev, open: false }));
        try {
          await uninstallAgent(agentId);
          addToast({ type: "success", message: "Agent uninstalled" });
        } catch (e) {
          addToast({ type: "error", message: `Failed to uninstall agent: ${String(e)}` });
        }
      },
    });
    setContextMenu(null);
  };

  const handleContextMenu = (e: React.MouseEvent, agentId: string) => {
    e.preventDefault();
    setContextMenu({ agentId, x: e.clientX, y: e.clientY });
  };

  const contextAgent = contextMenu?.agentId ? agentsMap[contextMenu.agentId]?.meta : undefined;
  const filteredAgents = agentsList.filter((a) =>
    a.name.toLowerCase().includes(searchQuery.toLowerCase()),
  );

  return (
    <div
      className="flex flex-col bg-[#EEEEF0] dark:bg-[#2F2F30] rounded-lg"
      style={{ width: width ?? 240 }}
    >
      {/* Header — search input */}
      <div className={cn(isCollapsed ? "px-1.5 py-2" : "px-3 py-2")}>
        <div className="relative min-w-0 flex-1">
          <Search
            className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-zinc-500 dark:text-zinc-400"
          />
          <StyledInput
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder={isCollapsed ? "" : t("agentList.searchPlaceholder")}
            aria-label={t("agentList.searchPlaceholder")}
            className={cn(
              "rounded-md bg-[#D8D9DC] pl-7 dark:bg-[#3D3D3F] py-1.5",
              isCollapsed ? "min-w-0 pr-0" : "pr-2",
            )}
          />
        </div>
      </div>

      {/* Agent list */}
      <div className="flex-1 overflow-y-auto overflow-x-hidden" role="list" aria-label="Agent list">

        {loading && agentsList.length === 0 && (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          </div>
        )}

        {filteredAgents.map((agent, index) => {
          const sessionTitle = agentsMap[agent.agent_id]?.sessionTitle;

          return (
            <div
              key={agent.agent_id}
              className={cn(
                "relative flex cursor-pointer items-center rounded-md px-3 py-2.5 transition-colors duration-150",
                isCollapsed ? "gap-0" : "gap-3",
                selectedAgentId === agent.agent_id
                  ? "bg-[var(--color-accent)]/90 text-white"
                  : "hover:bg-[#E2E3E6] dark:hover:bg-[#38383A]",
                index < filteredAgents.length - 1 && (isCollapsed ? "after:absolute after:bottom-0 after:left-1 after:right-1 after:border-b after:border-[#C8C8C8]/40 dark:after:border-zinc-600/40" : "after:absolute after:bottom-0 after:left-1.5 after:right-1.5 after:border-b after:border-[#C8C8C8]/40 dark:after:border-zinc-600/40")
              )}
              onClick={() => selectAgent(agent.agent_id)}
              onDoubleClick={() => {
                // Convenience: double-click a stopped agent to start it.
                // Running/starting agents ignore this — use context menu for Stop.
                if (!agent.running && !startingAgentIds.has(agent.agent_id)) {
                  void handleStart(agent.agent_id);
                }
              }}
              title={agent.running ? undefined : t("agentList.doubleClickToStart")}
              onContextMenu={(e) => handleContextMenu(e, agent.agent_id)}
              role="listitem"
            >
              {/* Avatar */}
              <Tooltip
                content={isCollapsed ? (agentsMap[agent.agent_id]?.profile?.displayName ?? agent.display_name ?? agent.name) : ""}
                variant="plain"
                position="right"
                delayMs={0}
              >
                <AgentAvatar
                  agentId={agent.agent_id}
                  displayName={agent.display_name ?? agent.name}
                  avatarUrl={agent.avatar}
                  version={agent.version}
                  builtinAvatarId={agent.builtin_avatar}
                  size={40}
                  className={isCollapsed ? "mx-auto" : ""}
                />
              </Tooltip>

              {/* Content area — width-collapsed when sidebar is collapsed to preserve item height */}
              <div className={cn("min-w-0 overflow-hidden", isCollapsed ? "w-0" : "flex-1")}>
                  {/* Top row: name */}
                  <div className="flex items-center justify-between gap-2">
                    <div className="min-w-0 flex items-center gap-1.5">
                      <span className={cn("truncate font-medium", selectedAgentId === agent.agent_id ? "text-white" : agent.running ? "text-zinc-900 dark:text-zinc-100" : "text-zinc-400 dark:text-zinc-500")} style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>{agentsMap[agent.agent_id]?.profile?.displayName ?? agent.display_name ?? agent.name}</span>

                    </div>
                  </div>
                  {/* Bottom row: current session title.
                      * min-height + animate-pulse skeleton locks the row height so the agent
                      * name above does not jump when the async session title loads. */}
                  <div
                    className="mt-0.5 flex items-center"
                    style={{
                      minHeight: "calc(var(--ui-font-size, 0.875rem) * 0.85 * 1.5)",
                      fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)",
                    }}
                  >
                    {sessionTitle === undefined ? (
                      <span
                        aria-hidden
                        className={cn(
                          "block h-2.5 w-2/3 animate-pulse rounded",
                          selectedAgentId === agent.agent_id
                            ? "bg-white/40"
                            : "bg-zinc-300/60 dark:bg-zinc-600/60",
                        )}
                      />
                    ) : (
                      <span
                        className={cn(
                          "block truncate",
                          selectedAgentId === agent.agent_id
                            ? "text-white/70"
                            : "text-zinc-500 dark:text-zinc-400",
                        )}
                      >
                        {sessionTitle === null ? (
                          <span aria-label="agent sleeping" className="inline-flex items-baseline">
                            <span className="zzz-n">z</span>
                            <span className="zzz-n">z</span>
                            <span className="zzz-n">z</span>
                            <span className="zzz-n">z</span>
                            <span className="zzz-n">z</span>
                          </span>
                        ) : (sessionTitle || "Untitled session")}
                      </span>
                    )}
                  </div>
                </div>
            </div>
          );
        })}

        {filteredAgents.length === 0 && !loading && (
          <div className="px-3 py-8 text-center text-xs text-zinc-400 dark:text-zinc-500">
            {agentsList.length === 0 ? "No agents installed" : "No matching agents"}
          </div>
        )}
      </div>

      <div ref={addMenuRef} className="relative p-1.5">
        <button
          onClick={() => setAddMenuOpen(!addMenuOpen)}
          className="flex w-full items-center justify-center rounded-md bg-[#D8D9DC] px-0 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-600 transition-colors hover:bg-[#C8C9CC] dark:bg-[#3D3D3F] dark:text-zinc-300 dark:hover:bg-[#474749]"
          aria-label="Add agent"
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
        {addMenuOpen && (
          <div className="absolute bottom-full left-1 z-50 mb-1 w-max rounded-md border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
            <button
              onClick={() => {
                setAddMenuOpen(false);
                setShowCreateWizard(true);
              }}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
            >
              <Sparkles className="h-3.5 w-3.5" />
              Create Agent
            </button>
            <button
              onClick={() => {
                setAddMenuOpen(false);
                void handleInstall();
              }}
              disabled={installing}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
            >
              <Plus className="h-3.5 w-3.5" />
              Install Agent
            </button>
          </div>
        )}
      </div>

      {/* Context menu */}
      {contextMenu && (
        <div
          ref={menuRef}
          className="fixed z-50 min-w-[160px] rounded-md border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          {contextAgent && !contextAgent.running && (
            <>
              <button
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
                onClick={() => handleStart(contextMenu.agentId)}
              >
                <Play className="h-3.5 w-3.5" /> Start
              </button>
              <button
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-amber-600 transition-colors hover:bg-amber-50 dark:text-amber-400 dark:hover:bg-amber-900/20"
                onClick={() => handleDebugStart(contextMenu.agentId)}
              >
                <Bug className="h-3.5 w-3.5" /> Start in Debug
              </button>
            </>
          )}
          {contextAgent && contextAgent.running && (
            <>
              <button
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
                onClick={() => handleStop(contextMenu.agentId)}
              >
                <Square className="h-3.5 w-3.5" /> Stop
              </button>
              <button
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-amber-600 transition-colors hover:bg-amber-50 dark:text-amber-400 dark:hover:bg-amber-900/20"
                onClick={() => handleRestartDebug(contextMenu.agentId)}
              >
                <Bug className="h-3.5 w-3.5" /> Restart in Debug
              </button>
            </>
          )}
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
            onClick={() => {
              setDetailAgentId(contextMenu.agentId);
              setContextMenu(null);
            }}
          >
            <Info className="h-3.5 w-3.5" /> Details
          </button>
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
            onClick={() => {
              if (contextAgent) {
                setCloneSource({
                  agentId: contextAgent.agent_id,
                  agentName: contextAgent.display_name ?? contextAgent.name,
                });
                setContextMenu(null);
              }
            }}
          >
            <Copy className="h-3.5 w-3.5" /> Clone
          </button>
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
            onClick={() => {
              if (contextAgent) {
                setPublishTarget({
                  agentId: contextAgent.agent_id,
                  agentName: contextAgent.display_name ?? contextAgent.name,
                });
                setContextMenu(null);
              }
            }}
          >
            <Package className="h-3.5 w-3.5" /> Publish
          </button>

          {contextAgent && contextAgent.agent_id !== "com.acowork.system" && (
            <>
              <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
              <button
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
                onClick={() => handleUninstall(contextMenu.agentId)}
              >
                <Trash2 className="h-3.5 w-3.5" /> Uninstall
              </button>
            </>
          )}
        </div>
      )}

      {/* Confirm dialog */}
      <ConfirmDialog
        open={confirmDialog.open}
        title={confirmDialog.title}
        message={confirmDialog.message}
        confirmLabel={confirmDialog.confirmLabel}
        destructive={confirmDialog.destructive}
        onConfirm={confirmDialog.onConfirm}
        onCancel={() => setConfirmDialog((prev) => ({ ...prev, open: false }))}
      />

      {/* Agent detail dialog */}
      <AgentDetailDialog
        open={!!detailAgentId}
        agentId={detailAgentId}
        onClose={() => setDetailAgentId(null)}
      />

      {/* Clone dialog */}
      <CloneDialog
        open={!!cloneSource}
        agentId={cloneSource?.agentId ?? ""}
        agentName={cloneSource?.agentName ?? ""}
        onCloned={(result: CloneResponse) => {
          setCloneSource(null);
          addToast({ type: "success", message: `Agent cloned: ${result.agent_id}` });
          void fetchAgents().then(() => {
            selectAgent(result.agent_id);
          });
        }}
        onClose={() => setCloneSource(null)}
      />

      {/* Publish wizard */}
      <PublishWizard
        open={!!publishTarget}
        agentId={publishTarget?.agentId ?? ""}
        agentName={publishTarget?.agentName ?? ""}
        onClose={() => setPublishTarget(null)}
      />

      {/* Create wizard */}
      <CreateWizard
        open={showCreateWizard}
        onCreated={(agentId) => {
          setShowCreateWizard(false);
          addToast({ type: "success", message: `Agent created: ${agentId}` });
          void fetchAgents().then(() => {
            selectAgent(agentId);
          });
        }}
        onClose={() => setShowCreateWizard(false)}
      />
    </div>
  );
}
