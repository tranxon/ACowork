import { useEffect, useState, useRef } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useToast } from "../common/ToastProvider";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { AgentDetailDialog } from "./AgentDetailDialog";
import { CloneDialog } from "./CloneDialog";
import { PublishWizard } from "./PublishWizard";
import { CreateWizard } from "./CreateWizard";
import { AgentAvatar } from "../common/AgentAvatar";
import { cn } from "../../lib/utils";
import { Play, Square, Trash2, Info, Copy, Plus, Search, Package, Sparkles, Bug } from "lucide-react";
import { open } from "@tauri-apps/plugin-dialog";
import { useSessionStore } from "../../stores/sessionStore";
import { useAgentProfileStore } from "../../stores/agentProfileStore";
import type { CloneResponse } from "../../lib/types";

interface AgentListProps {
  width?: number;
}

export function AgentList({ width }: AgentListProps) {
  const { agents, selectedAgentId, loading, fetchAgents, selectAgent, startAgent, stopAgent, uninstallAgent } =
    useAgentStore();
  const sessionTitles = useSessionStore((s) => s.sessionTitles);
  const agentProfiles = useAgentProfileStore((s) => s.profiles);
  const fetchLatestSessionTitle = useSessionStore((s) => s.fetchLatestSessionTitle);
  const { addToast } = useToast();
  const [contextMenu, setContextMenu] = useState<{ agentId: string; x: number; y: number } | null>(null);
  const [installing, setInstalling] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const addMenuRef = useRef<HTMLDivElement>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [addMenuOpen, setAddMenuOpen] = useState(false);

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
    onConfirm: () => {},
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
    if (agents.length === 0) return;
    for (const agent of agents) {
      if (sessionTitles[agent.agent_id] === undefined) {
        void fetchLatestSessionTitle(agent.agent_id);
      }
    }
  }, [agents, sessionTitles, fetchLatestSessionTitle]);

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
        const agents = useAgentStore.getState().agents;
        if (agents.length > 0) {
          selectAgent(agents[agents.length - 1].agent_id);
        }
      }
    } catch (e) {
      addToast({ type: "error", message: `Failed to install agent: ${String(e)}` });
    } finally {
      setInstalling(false);
    }
  };

  const handleStart = async (agentId: string) => {
    try {
      await startAgent(agentId);
      addToast({ type: "success", message: "Agent started" });
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
      addToast({ type: "success", message: "Agent started in debug mode" });
    } catch (e) {
      addToast({ type: "error", message: `Failed to start debug agent: ${String(e)}` });
    }
    setContextMenu(null);
  };

  const handleRestartDebug = async (agentId: string) => {
    try {
      // Synchronously disconnect the WebSocket and clear chat store state
      // BEFORE stopping the agent.  This prevents a lingering sending=true
      // (set by a previous sendMessage) from surviving the restart when
      // the new connectStream WebSocket creation fails because the Gateway
      // is not yet ready.
      const chatStore = useChatStore.getState();
      chatStore.disconnectStream(agentId);

      await stopAgent(agentId);
      await startAgent(agentId, true);
      addToast({ type: "success", message: "Agent restarted in debug mode" });
    } catch (e) {
      addToast({ type: "error", message: `Failed to restart in debug: ${String(e)}` });
    }
    setContextMenu(null);
  };

  const handleStop = async (agentId: string) => {
    const agent = agents.find((a) => a.agent_id === agentId);
    setConfirmDialog({
      open: true,
      title: "Stop Agent",
      message: `确定要停止 ${agent?.name ?? agentId} 吗？当前对话状态将保留。`,
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
    if (agentId === "com.rollball.system") {
      addToast({ type: "warning", message: "System Agent cannot be uninstalled" });
      return;
    }
    const agent = agents.find((a) => a.agent_id === agentId);
    setConfirmDialog({
      open: true,
      title: "Uninstall Agent",
      message: `确定要卸载 ${agent?.name ?? agentId} 吗？此操作不可撤销。`,
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

  const contextAgent = agents.find((a) => a.agent_id === contextMenu?.agentId);
  const filteredAgents = agents.filter((a) =>
    a.name.toLowerCase().includes(searchQuery.toLowerCase()),
  );

  return (
    <div
      className="flex flex-col bg-[#EEEEF0] dark:bg-[#2F2F30]"
      style={{ width: width ?? 240 }}
    >
      {/* Header */}
      <div className="bg-[#EEEEF0] px-3 py-2 dark:bg-[#2F2F30]">
        <div className="flex items-center gap-2">
          {/* Search input */}
          <div className="relative flex-1">
            <Search className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-zinc-500 dark:text-zinc-400" />
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search agents..."
              className="w-full rounded-md bg-[#D8D9DC] py-1.5 pl-7 pr-2 text-xs text-zinc-800 placeholder-zinc-500 outline-none dark:bg-[#3D3D3F] dark:text-zinc-200 dark:placeholder-zinc-400"
            />
          </div>
          {/* Add button */}
          <div ref={addMenuRef} className="relative">
            <button
              onClick={() => setAddMenuOpen(!addMenuOpen)}
              className="flex items-center justify-center rounded-md p-1.5 transition-colors hover:bg-[#D8D9DC] dark:hover:bg-[#3D3D3F]"
            >
              <Plus className="h-4 w-4 text-zinc-600 dark:text-zinc-300" />
            </button>
            {/* Add menu dropdown */}
            {addMenuOpen && (
              <div className="absolute right-0 top-full z-50 mt-1 w-max rounded-lg border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
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
        </div>
      </div>

      {/* Agent list */}
      <div className="flex-1 overflow-y-auto bg-[#EEEEF0] dark:bg-[#2F2F30]" role="list" aria-label="Agent list">
        <div className="border-t border-[#C8C8C8]/40 dark:border-zinc-600/40" />

        {loading && agents.length === 0 && (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          </div>
        )}

        {filteredAgents.map((agent, index) => {
          const sessionTitle = sessionTitles[agent.agent_id];
          
          return (
            <div
              key={agent.agent_id}
              className={cn(
                "flex cursor-pointer items-start gap-3 px-3 py-2.5 transition-colors duration-150",
                selectedAgentId === agent.agent_id
                  ? "bg-[#D8D9DC] dark:bg-[#3D3D3F]"
                  : "hover:bg-[#E2E3E6] dark:hover:bg-[#38383A]",
                index < filteredAgents.length - 1 && "border-b border-[#C8C8C8]/40 dark:border-zinc-600/40"
              )}
              onClick={() => selectAgent(agent.agent_id)}
              onContextMenu={(e) => handleContextMenu(e, agent.agent_id)}
              role="listitem"
            >
              {/* Avatar */}
              <AgentAvatar
                agentId={agent.agent_id}
                displayName={agent.display_name ?? agent.name}
                avatarUrl={agent.avatar}
                iconId={agentProfiles[agent.agent_id]?.avatarIconId}
                size={40}
                className="mt-0.5"
              />

              {/* Content area */}
              <div className="min-w-0 flex-1">
                {/* Top row: name */}
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0 flex items-center gap-1.5">
                    <span className="truncate font-medium text-zinc-900 dark:text-zinc-100" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>{agentProfiles[agent.agent_id]?.displayName ?? agent.display_name ?? agent.name}</span>

                  </div>
                </div>
                {/* Bottom row: current session title */}
                <div className="mt-0.5 truncate" style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}>
                  <span className="text-zinc-500 dark:text-zinc-400">
                    {sessionTitle === undefined ? "" : (sessionTitle === null ? "No session" : (sessionTitle || "Untitled session"))}
                  </span>
                </div>
              </div>
            </div>
          );
        })}

        {filteredAgents.length === 0 && !loading && (
          <div className="px-3 py-8 text-center text-xs text-zinc-400 dark:text-zinc-500">
            {agents.length === 0 ? "No agents installed" : "No matching agents"}
          </div>
        )}
      </div>

      {/* Context menu */}
      {contextMenu && (
        <div
          ref={menuRef}
          className="fixed z-50 min-w-[160px] rounded-lg border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
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

          <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
            onClick={() => handleUninstall(contextMenu.agentId)}
          >
            <Trash2 className="h-3.5 w-3.5" /> Uninstall
          </button>
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
