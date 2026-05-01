import { useState, useEffect, useRef, useCallback } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import type { WorkspaceDir } from "../../stores/workspaceStore";
import { useToast } from "../common/ToastProvider";
import { ChevronDown, FolderOpen, FolderPlus, Trash2, Shield, ShieldOff, Search, Check } from "lucide-react";
import * as dialog from "@tauri-apps/plugin-dialog";
import { cn } from "../../lib/utils";

export function WorkspaceSelector() {
  const { selectedAgentId } = useAgentStore();
  const { gatewayUrl } = useSettingsStore();
  const { addToast } = useToast();
  const { workspaces, currentWorkspaceId, loading, fetchWorkspaces, setCurrentWorkspace } =
    useWorkspaceStore();
  const [open, setOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  // Load workspaces when agent changes
  useEffect(() => {
    if (!selectedAgentId) {
      useWorkspaceStore.getState().reset();
      return;
    }
    void fetchWorkspaces(selectedAgentId);
  }, [selectedAgentId, fetchWorkspaces]);

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

  const handleSelect = async (dir: WorkspaceDir) => {
    if (!selectedAgentId) return;
    await setCurrentWorkspace(selectedAgentId, dir.id);
    setOpen(false);
  };
  
  const handleBrowse = async () => {
    try {
      const selected = await dialog.open({ directory: true });
      if (selected && selectedAgentId) {
        const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            path: selected,
            alias: selected.split(/[\\/]/).filter(Boolean).pop() || undefined,
            access: "read-only",
          }),
        });
        if (response.ok) {
          addToast({ type: "success", message: "Workspace added" });
          void fetchWorkspaces(selectedAgentId);
        }
      }
    } catch {
      // User cancelled
    }
  };
  
  const handleDelete = useCallback(async (id: string, name: string) => {
    if (!selectedAgentId || deletingId) return;
    setDeletingId(id);
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces/${id}`, {
        method: "DELETE",
      });
      if (response.ok) {
        addToast({ type: "success", message: `Removed ${name}` });
        void fetchWorkspaces(selectedAgentId);
      } else {
        addToast({ type: "error", message: "Failed to remove workspace" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to remove workspace: ${String(error)}` });
    } finally {
      setDeletingId(null);
    }
    setConfirmDelete(null);
  }, [selectedAgentId, gatewayUrl, addToast, fetchWorkspaces, deletingId]);
  
  const handleToggleAccess = useCallback(async (dir: WorkspaceDir) => {
    if (!selectedAgentId) return;
    const newAccess = dir.access === "read-only" ? "read-write" : "read-only";
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces/${dir.id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ access: newAccess }),
      });
      if (response.ok) {
        addToast({ type: "success", message: `Access changed to ${newAccess === "read-write" ? "RW" : "RO"}` });
        void fetchWorkspaces(selectedAgentId);
      } else {
        addToast({ type: "error", message: "Failed to update access" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to update access: ${String(error)}` });
    }
  }, [selectedAgentId, gatewayUrl, addToast, fetchWorkspaces]);

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
          }}
          className={cn(
            "inline-flex items-center gap-1.5 rounded-lg px-2 py-1 text-xs transition-colors",
            "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200",
            open && "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100",
          )}
        >
          <FolderOpen size={14} />
          <span>
            {currentWorkspaceId
              ? (() => {
                  const w = workspaces.find((ws) => ws.id === currentWorkspaceId);
                  if (!w) return "Select Workspace";
                  return w.alias || w.path.split(/[\/\\]/).filter(Boolean).pop() || w.path;
                })()
              : "Select Workspace"}
          </span>
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

            {/* Add workspace button */}
            <button
              onClick={handleBrowse}
              className="mb-2 flex w-full items-center gap-2 rounded-md border border-dashed border-zinc-300 px-3 py-2 text-xs text-zinc-600 hover:border-zinc-400 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-400 dark:hover:border-zinc-500 dark:hover:bg-zinc-700/50"
            >
              <FolderPlus className="h-3.5 w-3.5" />
              Add Workspace
            </button>

            {/* Workspace list */}
            <div className="max-h-56 overflow-y-auto">
              {loading ? (
                <div className="py-4 text-center text-xs text-zinc-400">Loading...</div>
              ) : filteredWorkspaces.length === 0 ? (
                <div className="py-4 text-center text-xs text-zinc-400">
                  {searchQuery ? "No matching workspaces" : "No workspaces configured"}
                </div>
              ) : (
                <div className="space-y-0.5">
                  {filteredWorkspaces.map((dir) => {
                    const isCurrent = dir.id === currentWorkspaceId;
                    const isDeleting = confirmDelete === dir.id;
                    const displayName = dir.alias || dir.path.split(/[\\/]/).filter(Boolean).pop() || dir.path;
            
                    return (
                      <div
                        key={dir.id}
                        className="group flex items-center gap-2 rounded-md px-2 py-1.5 transition-colors hover:bg-zinc-50 dark:hover:bg-zinc-700/50"
                      >
                        {/* Select workspace button */}
                        <button
                          onClick={() => handleSelect(dir)}
                          className="flex flex-1 items-center gap-2 text-left"
                        >
                          <FolderOpen className="h-3.5 w-3.5 shrink-0 text-zinc-400" />
                          <div className="flex-1 truncate">
                            <div className="font-medium text-zinc-800 dark:text-zinc-200">
                              {displayName}
                            </div>
                            <div className="truncate text-[10px] text-zinc-500 dark:text-zinc-400">
                              {dir.path}
                            </div>
                          </div>
                          {isCurrent && (
                            <Check className="h-3.5 w-3.5 shrink-0 text-emerald-500" />
                          )}
                        </button>
            
                        {/* Action buttons: access toggle + delete */}
                        <div className="flex shrink-0 items-center gap-1">
                          {/* Access toggle button */}
                          <button
                            onClick={(e) => {
                              e.stopPropagation();
                              void handleToggleAccess(dir);
                            }}
                            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] transition-colors hover:bg-zinc-200 dark:hover:bg-zinc-600"
                            title={dir.access === "read-write" ? "Change to read-only" : "Change to read-write"}
                          >
                            {dir.access === "read-write" ? (
                              <>
                                <ShieldOff className="h-3 w-3 text-orange-600 dark:text-orange-400" />
                                <span className="font-medium text-orange-700 dark:text-orange-400">RW</span>
                              </>
                            ) : (
                              <>
                                <Shield className="h-3 w-3 text-zinc-500" />
                                <span className="font-medium text-zinc-600 dark:text-zinc-400">RO</span>
                              </>
                            )}
                          </button>
            
                          {/* Delete button */}
                          {isDeleting ? (
                            <div className="flex items-center gap-0.5">
                              <span className="text-[10px] text-zinc-500">?</span>
                              <button
                                onClick={(e) => {
                                  e.stopPropagation();
                                  void handleDelete(dir.id, displayName);
                                }}
                                disabled={deletingId !== null}
                                className="rounded bg-red-500 px-1.5 py-0.5 text-[10px] text-white hover:bg-red-600 disabled:opacity-50 disabled:cursor-not-allowed"
                              >
                                ✓
                              </button>
                              <button
                                onClick={(e) => {
                                  e.stopPropagation();
                                  setConfirmDelete(null);
                                }}
                                className="rounded px-1 py-0.5 text-[10px] text-zinc-600 hover:bg-zinc-200 dark:text-zinc-400 dark:hover:bg-zinc-600"
                              >
                                ✗
                              </button>
                            </div>
                          ) : (
                            <button
                              onClick={(e) => {
                                e.stopPropagation();
                                setConfirmDelete(dir.id);
                              }}
                              disabled={deletingId !== null}
                              className="rounded p-1 text-zinc-400 opacity-0 transition-all group-hover:opacity-100 hover:bg-red-50 hover:text-red-600 dark:hover:bg-red-900/20 dark:hover:text-red-400 disabled:opacity-50 disabled:cursor-not-allowed"
                              title="Remove workspace"
                            >
                              <Trash2 className="h-3 w-3" />
                            </button>
                          )}
                        </div>
                      </div>
                    );
                  })}
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
    </>
  );
}
