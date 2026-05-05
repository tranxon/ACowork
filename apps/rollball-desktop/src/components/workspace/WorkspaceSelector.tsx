import { useState, useEffect, useRef, useCallback } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import type { WorkspaceDir } from "../../stores/workspaceStore";
import { useToast } from "../common/ToastProvider";
import { ChevronDown, FolderOpen, FolderPlus, Trash2, Shield, ShieldOff } from "lucide-react";
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
            "inline-flex items-center gap-1 rounded-lg px-2 py-1 text-xs transition-colors",
            "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200",
            open && "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100",
          )}
        >
          <FolderOpen size={14} />
          <span className="max-w-[120px] truncate">
            {currentWorkspaceId
              ? (() => {
                  const w = workspaces.find((ws) => ws.id === currentWorkspaceId);
                  if (!w) return "Workspace";
                  const name = w.alias || w.path.split(/[\/\\]/).filter(Boolean).pop() || w.path;
                  return name.length > 24 ? name.slice(0, 24) + "..." : name;
                })()
              : "Workspace"}
          </span>
          <ChevronDown className="h-3 w-3 text-zinc-400" />
        </button>

        {/* Dropdown menu */}
        {open && (
          <div className="absolute bottom-full left-0 mb-2 w-80 rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-800" style={{ zIndex: 100 }}>
            {/* Workspace list */}
            <div className="max-h-56 overflow-y-auto py-1">
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
                        className={cn("group flex items-center gap-2 px-2 py-1.5 transition-colors hover:bg-zinc-50 dark:hover:bg-zinc-700/50",
                          isCurrent && "bg-[#D8D9DC] dark:bg-[#3D3D3F]",
                        )}
                      >
                        {/* Select workspace button */}
                        <button
                          onClick={() => handleSelect(dir)}
                          className="flex min-w-0 flex-1 items-center gap-2 text-left"
                        >
                          <FolderOpen className="h-3.5 w-3.5 shrink-0 text-zinc-400" />
                          <div className="min-w-0 flex-1">
                            <div className={cn("truncate text-xs", isCurrent ? "font-medium text-blue-600 dark:text-blue-400" : "font-medium text-zinc-800 dark:text-zinc-200")}>
                              {displayName}
                            </div>
                            <div className="truncate text-[10px] text-zinc-500 dark:text-zinc-400" title={dir.path}>
                              {dir.path}
                            </div>
                          </div>
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

            {/* Divider */}
            <div className="border-t border-zinc-200 dark:border-zinc-700" />

            {/* Add workspace button */}
            <div className="p-2">
              <button
                onClick={handleBrowse}
                className="mx-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
              >
                <FolderPlus className="h-3.5 w-3.5" />
                Add Workspace
              </button>
            </div>
          </div>
        )}
      </div>
    </>
  );
}
