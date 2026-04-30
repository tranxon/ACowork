import { useState, useEffect, useRef, useCallback } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useToast } from "../common/ToastProvider";
import { WorkspaceManager } from "./WorkspaceManager";
import { ChevronDown, FolderOpen, Plus, Settings2, Search } from "lucide-react";
import * as dialog from "@tauri-apps/plugin-dialog";
import { cn } from "../../lib/utils";

interface WorkspaceDir {
  id: string;
  path: string;
  alias?: string;
  access: "read-only" | "read-write";
  added_at: string;
}

export function WorkspaceSelector() {
  const { selectedAgentId } = useAgentStore();
  const { gatewayUrl } = useSettingsStore();
  const { addToast } = useToast();
  const [workspaces, setWorkspaces] = useState<WorkspaceDir[]>([]);
  const [loading, setLoading] = useState(false);
  const [open, setOpen] = useState(false);
  const [showManager, setShowManager] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const ref = useRef<HTMLDivElement>(null);

  const loadWorkspaces = useCallback(async () => {
    if (!selectedAgentId) return;
    setLoading(true);
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces`);
      if (response.ok) {
        const data = await response.json();
        setWorkspaces(data.workspaces || []);
      }
    } catch {
      // Silently fail — workspace is optional
    } finally {
      setLoading(false);
    }
  }, [gatewayUrl, selectedAgentId]);

  // Load workspaces when agent changes
  useEffect(() => {
    if (!selectedAgentId) {
      setWorkspaces([]);
      return;
    }
    void loadWorkspaces();
  }, [selectedAgentId, loadWorkspaces]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const handleBrowse = async () => {
    try {
      const selected = await dialog.open({ directory: true });
      if (selected && selectedAgentId) {
        // Direct add with read-only default
        const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            path: selected,
            alias: selected.split(/[\/\\]/).pop() || undefined,
            access: "read-only",
          }),
        });
        if (response.ok) {
          addToast({ type: "success", message: "Workspace added" });
          await loadWorkspaces();
        }
      }
    } catch {
      // User cancelled
    }
    setOpen(false);
  };

  const handleSelect = async (dir: WorkspaceDir) => {
    // Future: switch active workspace context
    setOpen(false);
    // TODO: emit workspace change event
  };

  const filteredWorkspaces = workspaces.filter((w) =>
    !searchQuery.trim() ||
    w.path.toLowerCase().includes(searchQuery.toLowerCase()) ||
    (w.alias && w.alias.toLowerCase().includes(searchQuery.toLowerCase())),
  );

  return (
    <>
      {/* Trigger button */}
      <div ref={ref} className="relative inline-block">
        <button
          type="button"
          onClick={() => {
            setOpen(!open);
            if (!open) void loadWorkspaces();
          }}
          className={cn(
            "inline-flex items-center gap-1.5 rounded-lg px-2 py-1 text-xs transition-colors",
            "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200",
            open && "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100",
          )}
        >
          <FolderOpen size={14} />
          <span>{workspaces.length > 0 ? workspaces[0].alias || workspaces[0].path.split(/[\/\\]/).pop() : "Workspace"}</span>
          <ChevronDown className="h-3 w-3 text-zinc-400" />
        </button>

        {/* Dropdown menu */}
        {open && (
          <div className="absolute bottom-full left-0 mb-2 w-80 rounded-lg border border-zinc-200 bg-white p-2 shadow-lg dark:border-zinc-700 dark:bg-zinc-800" style={{ zIndex: 100 }}>
            {/* Search */}
            <div className="relative mb-2">
              <Search className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-zinc-400" />
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search workspace..."
                className="w-full rounded-md border border-zinc-200 bg-white pl-7 pr-3 py-1.5 text-xs outline-none focus:border-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200"
              />
            </div>

            {/* Action buttons */}
            <div className="mb-2 flex gap-1">
              <button
                onClick={handleBrowse}
                className="flex flex-1 items-center gap-1.5 rounded-md border border-zinc-200 px-2 py-1.5 text-xs text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                <FolderOpen className="h-3.5 w-3.5" />
                Open Folder
              </button>
              <button
                onClick={() => {
                  setOpen(false);
                  setShowManager(true);
                }}
                className="flex flex-1 items-center gap-1.5 rounded-md border border-zinc-200 px-2 py-1.5 text-xs text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                <Settings2 className="h-3.5 w-3.5" />
                Manage
              </button>
            </div>

            {/* Workspace list */}
            <div className="max-h-48 overflow-y-auto">
              {loading ? (
                <div className="py-4 text-center text-xs text-zinc-400">Loading...</div>
              ) : filteredWorkspaces.length === 0 ? (
                <div className="py-4 text-center text-xs text-zinc-400">
                  {searchQuery ? "No matching workspaces" : "No workspaces configured"}
                </div>
              ) : (
                <div className="space-y-0.5">
                  {filteredWorkspaces.map((dir) => (
                    <button
                      key={dir.id}
                      onClick={() => handleSelect(dir)}
                      className="flex w-full items-start gap-2 rounded-md px-2 py-2 text-left text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                    >
                      <FolderOpen className="mt-0.5 h-3.5 w-3.5 shrink-0 text-zinc-400" />
                      <div className="flex-1 truncate">
                        <div className="font-medium text-zinc-800 dark:text-zinc-200">
                          {dir.alias || dir.path.split(/[\/\\]/).pop() || dir.path}
                        </div>
                        <div className="truncate text-[10px] text-zinc-500 dark:text-zinc-400">
                          {dir.path}
                        </div>
                      </div>
                      {dir.access === "read-write" && (
                        <span className="shrink-0 rounded bg-orange-100 px-1.5 py-0.5 text-[10px] font-medium text-orange-700 dark:bg-orange-900/30 dark:text-orange-400">
                          RW
                        </span>
                      )}
                    </button>
                  ))}
                </div>
              )}
            </div>

            {/* Footer stats */}
            {workspaces.length > 0 && (
              <div className="mt-2 border-t border-zinc-100 pt-2 text-[10px] text-zinc-400 dark:border-zinc-700">
                {workspaces.length} workspace{workspaces.length > 1 ? "s" : ""} ·{" "}
                {workspaces.filter((w) => w.access === "read-write").length} read-write
              </div>
            )}
          </div>
        )}
      </div>

      {/* Workspace Manager Dialog */}
      {showManager && selectedAgentId && (
        <WorkspaceManager
          agentId={selectedAgentId}
          onClose={() => {
            setShowManager(false);
            void loadWorkspaces();
          }}
        />
      )}
    </>
  );
}
