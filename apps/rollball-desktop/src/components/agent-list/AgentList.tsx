import { useEffect, useState, useRef } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useToast } from "../common/ToastProvider";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { AgentDetailDialog } from "./AgentDetailDialog";
import { cn } from "../../lib/utils";
import { Play, Square, Trash2, Info, Copy, Plus } from "lucide-react";
import { open } from "@tauri-apps/plugin-dialog";
import { useSessionStore } from "../../stores/sessionStore";

interface AgentListProps {
  width?: number;
}

export function AgentList({ width }: AgentListProps) {
  const { agents, selectedAgentId, loading, fetchAgents, selectAgent, startAgent, stopAgent, uninstallAgent } =
    useAgentStore();
  const sessionTitles = useSessionStore((s) => s.sessionTitles);
  const fetchLatestSessionTitle = useSessionStore((s) => s.fetchLatestSessionTitle);
  const { addToast } = useToast();
  const [contextMenu, setContextMenu] = useState<{ agentId: string; x: number; y: number } | null>(null);
  const [installing, setInstalling] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

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

  useEffect(() => {
    fetchAgents();
    const interval = setInterval(fetchAgents, 30_000);
    return () => clearInterval(interval);
  }, [fetchAgents]);

  // Close context menu on click outside
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setContextMenu(null);
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

  return (
    <div
      className="flex flex-col bg-[#EEEEF0] dark:bg-[#2F2F30]"
      style={{ width: width ?? 240 }}
    >
      {/* Header - 微信联系人列表风格 */}
      <div className="flex items-center justify-between bg-[#EEEEF0] px-4 py-3 dark:bg-[#2F2F30]">
        <span className="text-xs font-medium uppercase tracking-wider text-zinc-600 dark:text-zinc-400">
          Agents
        </span>
      </div>

      {/* Agent list - 微信联系人列表风格 */}
      <div className="flex-1 overflow-y-auto bg-[#EEEEF0] py-1 dark:bg-[#2F2F30]" role="list" aria-label="Agent list">
        {loading && agents.length === 0 && (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          </div>
        )}

        {agents.map((agent) => {
          const isSystem = agent.agent_id === "com.rollball.system";
          const sessionTitle = sessionTitles[agent.agent_id];
          
          return (
            <div
              key={agent.agent_id}
              className={cn(
                "flex cursor-pointer items-start gap-3 px-3 py-2.5 transition-colors duration-150",
                selectedAgentId === agent.agent_id
                  ? "bg-[#D8D9DC] dark:bg-[#3D3D3F]"
                  : "hover:bg-[#E2E3E6] dark:hover:bg-[#38383A]",
              )}
              onClick={() => selectAgent(agent.agent_id)}
              onContextMenu={(e) => handleContextMenu(e, agent.agent_id)}
              role="listitem"
            >
              {/* Avatar */}
              <div className={cn(
                "flex h-10 w-10 shrink-0 items-center justify-center rounded-full text-sm font-semibold mt-0.5",
                isSystem 
                  ? "bg-gradient-to-br from-blue-500 to-blue-600 text-white"
                  : "bg-gradient-to-br from-zinc-400 to-zinc-500 text-white dark:from-zinc-500 dark:to-zinc-600"
              )}>
                {agent.name.charAt(0).toUpperCase()}
              </div>

              {/* Content area */}
              <div className="min-w-0 flex-1">
                {/* Top row: name + system badge */}
                <div className="flex items-center justify-between gap-2">
                  <div className="flex items-center gap-1.5 min-w-0">
                    <span className="truncate text-xs font-medium text-zinc-900 dark:text-zinc-100">{agent.name}</span>
                    {isSystem && (
                      <span className="shrink-0 rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-700 dark:bg-blue-900/50 dark:text-blue-300">System</span>
                    )}
                  </div>
                </div>
                {/* Bottom row: current session title */}
                <div className="mt-0.5 truncate text-xs">
                  <span className="text-zinc-500 dark:text-zinc-400">
                    {sessionTitle === undefined ? "" : (sessionTitle === null ? "No session" : (sessionTitle || "Untitled session"))}
                  </span>
                </div>
              </div>
            </div>
          );
        })}

        {agents.length === 0 && !loading && (
          <div className="px-3 py-8 text-center text-xs text-zinc-400 dark:text-zinc-500">
            No agents installed
          </div>
        )}
      </div>

      {/* Bottom action area - 微信联系人列表风格 */}
      <div className="bg-[#EEEEF0] px-3 pt-1 pb-3 dark:bg-[#2F2F30]">
        <button
          onClick={handleInstall}
          disabled={installing}
          className="flex w-full items-center justify-center gap-1.5 rounded-md bg-[#E2E3E6] px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-[#D8D9DC] disabled:opacity-50 dark:bg-[#38383A] dark:text-zinc-300 dark:hover:bg-[#3D3D3F]"
        >
          {installing ? (
            <>
              <div className="h-3 w-3 animate-spin rounded-full border border-zinc-400 border-t-zinc-700" />
              Installing...
            </>
          ) : (
            <>
              <Plus className="h-3.5 w-3.5" />
              Install Agent
            </>
          )}
        </button>
      </div>

      {/* Context menu */}
      {contextMenu && (
        <div
          ref={menuRef}
          className="fixed z-50 min-w-[160px] rounded-md border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          {contextAgent && !contextAgent.running && (
            <button
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-700"
              onClick={() => handleStart(contextMenu.agentId)}
            >
              <Play className="h-3.5 w-3.5" /> Start
            </button>
          )}
          {contextAgent && contextAgent.running && (
            <button
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950"
              onClick={() => handleStop(contextMenu.agentId)}
            >
              <Square className="h-3.5 w-3.5" /> Stop
            </button>
          )}
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-700"
            onClick={() => {
              setDetailAgentId(contextMenu.agentId);
              setContextMenu(null);
            }}
          >
            <Info className="h-3.5 w-3.5" /> Details
          </button>
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-700"
            disabled
            title="Available in S4"
          >
            <Copy className="h-3.5 w-3.5" /> Clone
          </button>
          <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950"
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
    </div>
  );
}
