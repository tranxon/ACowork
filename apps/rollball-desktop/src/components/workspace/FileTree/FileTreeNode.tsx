import { memo, useCallback, useState, useRef, useEffect } from "react";
import { createPortal } from "react-dom";
import { ChevronRight, FilePlus, FolderPlus, MessageSquarePlus, Trash2, Copy, ClipboardPaste } from "lucide-react";
import { cn } from "../../../lib/utils";
import { getFileIcon } from "./fileIcons";
import { SetiIcon } from "../../common/SetiIcon";
import { useChatStore } from "../../../stores/chatStore";
import { useWorkspaceStore } from "../../../stores/workspaceStore";
import type { TreeEntry } from "../../../stores/workspaceStore";

// Lazy-load Tauri dialog to avoid import error in browser dev mode
let _dialogModule: typeof import("@tauri-apps/plugin-dialog") | null = null;
async function getTauriDialog() {
  if (!_dialogModule) {
    _dialogModule = await import("@tauri-apps/plugin-dialog");
  }
  return _dialogModule;
}

interface FileTreeNodeProps {
  entry: TreeEntry;
  depth: number;
  relPath: string;
  isExpanded: boolean;
  isLoading: boolean;
  isSelected: boolean;
  onToggle: (relPath: string) => void;
  onSelect: (entry: TreeEntry, relPath: string) => void;
  onDoubleClick?: (entry: TreeEntry, relPath: string) => void;
  onContextNewItem?: (type: "file" | "dir", parentPath: string) => void;
  onDelete?: (relPath: string, isDir: boolean) => void;
  onCopy?: (relPath: string, isDir: boolean) => void;
  onPaste?: (parentPath: string) => void;
}

export const FileTreeNode = memo(function FileTreeNode({
  entry,
  depth,
  relPath,
  isExpanded,
  isLoading,
  isSelected,
  onToggle,
  onSelect,
  onDoubleClick,
  onContextNewItem,
  onDelete,
  onCopy,
  onPaste,
}: FileTreeNodeProps) {
  const isDir = entry.type === "directory";
  const fileIcon = isDir ? null : getFileIcon(entry.name);

  // Context menu state
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const addAttachedContext = useChatStore((s) => s.addAttachedContext);
  const getActiveSessionId = useChatStore((s) => s.getActiveSessionId);

  // Close context menu on click outside or Escape
  useEffect(() => {
    if (!contextMenu) return;
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setContextMenu(null);
      }
    };
    const keyHandler = (e: KeyboardEvent) => {
      if (e.key === "Escape") setContextMenu(null);
    };
    document.addEventListener("mousedown", handler);
    document.addEventListener("keydown", keyHandler);
    return () => {
      document.removeEventListener("mousedown", handler);
      document.removeEventListener("keydown", keyHandler);
    };
  }, [contextMenu]);

  const handleClick = useCallback(() => {
    if (isDir) {
      onToggle(relPath);
    } else {
      onSelect(entry, relPath);
    }
  }, [isDir, onToggle, onSelect, relPath, entry]);

  const handleDoubleClick = useCallback(() => {
    if (!isDir && onDoubleClick) {
      onDoubleClick(entry, relPath);
    }
  }, [isDir, onDoubleClick, entry, relPath]);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setContextMenu({ x: e.clientX, y: e.clientY });
  }, []);

  const handleNewFile = useCallback(() => {
    const parentPath = isDir ? relPath : relPath.substring(0, relPath.lastIndexOf("/"));
    onContextNewItem?.("file", parentPath);
    setContextMenu(null);
  }, [isDir, relPath, onContextNewItem]);

  const handleNewFolder = useCallback(() => {
    const parentPath = isDir ? relPath : relPath.substring(0, relPath.lastIndexOf("/"));
    onContextNewItem?.("dir", parentPath);
    setContextMenu(null);
  }, [isDir, relPath, onContextNewItem]);

  const handleAddToChat = useCallback(() => {
    const agentId = useChatStore.getState().currentAgentId;
    if (!agentId) return;
    const sessionId = getActiveSessionId(agentId);
    if (!sessionId) return;
    addAttachedContext(agentId, sessionId, {
      id: `${agentId}:${relPath}`,
      type: isDir ? "directory" : "file",
      name: entry.name,
      relPath,
    });
    setContextMenu(null);
  }, [isDir, relPath, entry.name, addAttachedContext, getActiveSessionId]);

  const handleDelete = useCallback(async () => {
    const label = isDir ? `directory "${entry.name}"` : `file "${entry.name}"`;
    let confirmed = false;
    try {
      const { ask } = await getTauriDialog();
      confirmed = await ask(`Delete ${label}?\n\nThis action cannot be undone.`, {
        title: "Confirm Delete",
        kind: "warning",
        okLabel: "Delete",
        cancelLabel: "Cancel",
      });
    } catch {
      // Fallback for non-Tauri environments (e.g. browser dev)
      confirmed = window.confirm(`Delete ${label}?\n\nThis action cannot be undone.`);
    }
    if (confirmed) {
      onDelete?.(relPath, isDir);
    }
    setContextMenu(null);
  }, [isDir, relPath, entry.name, onDelete]);

  const handleCopy = useCallback(() => {
    onCopy?.(relPath, isDir);
    setContextMenu(null);
  }, [isDir, relPath, onCopy]);

  const handlePaste = useCallback(() => {
    const parentPath = isDir ? relPath : relPath.substring(0, relPath.lastIndexOf("/"));
    onPaste?.(parentPath);
    setContextMenu(null);
  }, [isDir, relPath, onPaste]);

  return (
    <>
      <div
        className={cn(
          "flex cursor-pointer items-center gap-1 py-[2px] pr-3 text-xs hover:bg-zinc-100 dark:hover:bg-zinc-800",
          isSelected && "bg-[var(--color-accent)]/10",
        )}
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
        onClick={handleClick}
        onDoubleClick={handleDoubleClick}
        onContextMenu={handleContextMenu}
        title={relPath}
      >
        {/* Icon — chevron for dirs, file-type for files; both occupy same 16px slot so names align */}
        <span className="flex h-4 w-4 shrink-0 items-center justify-center">
          {isDir ? (
            <ChevronRight
              className={cn(
                "h-3 w-3 text-zinc-400 transition-transform duration-150",
                isExpanded && "rotate-90",
              )}
            />
          ) : fileIcon ? (
            <SetiIcon
              name={fileIcon.name}
              size={14}
            />
          ) : null}
        </span>

        {/* Name — no truncation; horizontal scrollbar on parent handles overflow */}
        <span className="whitespace-nowrap text-zinc-700 dark:text-zinc-400">{entry.name}</span>

        {/* Loading indicator for directories being fetched */}
        {isLoading && isDir && isExpanded && (
          <span className="ml-auto text-[10px] text-zinc-400">...</span>
        )}

        {/* Children count badge for collapsed directories */}
        {isDir && !isExpanded && entry.childrenCount !== undefined && entry.childrenCount > 0 && (
          <span className="ml-auto text-[10px] text-zinc-400">{entry.childrenCount}</span>
        )}
      </div>

      {/* Context menu portal — rendered to document.body to escape virtual list transform containment */}
      {contextMenu && createPortal(
        <div
          ref={menuRef}
          className="fixed z-[100] min-w-[160px] rounded-lg border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          <button
            onClick={handleNewFile}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            <FilePlus className="h-3.5 w-3.5 text-zinc-400" />
            New File
          </button>
          <button
            onClick={handleNewFolder}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            <FolderPlus className="h-3.5 w-3.5 text-zinc-400" />
            New Folder
          </button>
          <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
          <button
            onClick={handleCopy}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            <Copy className="h-3.5 w-3.5 text-zinc-400" />
            Copy
          </button>
          <button
            onClick={handlePaste}
            disabled={!useWorkspaceStore.getState().copiedEntry}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 disabled:opacity-40 disabled:cursor-not-allowed dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            <ClipboardPaste className="h-3.5 w-3.5 text-zinc-400" />
            Paste
          </button>
          <button
            onClick={handleDelete}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-900/20"
          >
            <Trash2 className="h-3.5 w-3.5 text-red-500" />
            Delete
          </button>
          <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
          <button
            onClick={handleAddToChat}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            <MessageSquarePlus className="h-3.5 w-3.5 text-zinc-400" />
            Add to Chat
          </button>
        </div>,
        document.body,
      )}
    </>
  );
});
