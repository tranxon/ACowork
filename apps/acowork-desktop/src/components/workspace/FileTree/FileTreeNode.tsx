import { memo, useCallback, useState, useRef, useEffect } from "react";
import { createPortal } from "react-dom";
import { ChevronRight, FilePlus, FolderPlus, MessageSquarePlus, Trash2, Copy, ClipboardPaste, Eye } from "lucide-react";
import { cn } from "../../../lib/utils";
import { getFileIcon } from "./fileIcons";
import { SetiIcon } from "../../common/SetiIcon";
import { useChatStore } from "../../../stores/chatStore";
import { useWorkspaceStore } from "../../../stores/workspaceStore";
import { useFileEditorStore } from "../../../stores/fileEditorStore";
import { useTranslation } from "../../../i18n/useTranslation";
import type { TreeEntry } from "../../../stores/workspaceStore";

// Lazy-load Tauri dialog to avoid import error in browser dev mode
let _dialogModule: typeof import("@tauri-apps/plugin-dialog") | null = null;
async function getTauriDialog() {
  if (!_dialogModule) {
    _dialogModule = await import("@tauri-apps/plugin-dialog");
  }
  return _dialogModule;
}

const CONTEXT_MENU_FONT_SIZE: React.CSSProperties = { fontSize: "var(--ui-font-size, 0.875rem)" };

interface FileTreeNodeProps {
  entry: TreeEntry;
  depth: number;
  agentId: string;
  sessionId: string;
  relPath: string;
  isExpanded: boolean;
  isLoading: boolean;
  isSelected: boolean;
  /** True when at least one open editor tab lives under this directory */
  hasOpenDescendant?: boolean;
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
  agentId,
  sessionId,
  relPath,
  isExpanded,
  isLoading,
  isSelected,
  hasOpenDescendant,
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
  const { t } = useTranslation();
  const openPreview = useFileEditorStore((s) => s.openPreview);
  // Preview is currently limited to Markdown files only.
  const isPreviewable = !isDir && entry.name.toLowerCase().endsWith(".md");

  // Context menu state
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const addAttachedContext = useChatStore((s) => s.addAttachedContext);

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
    addAttachedContext(agentId, sessionId, {
      id: `${agentId}:${relPath}`,
      type: isDir ? "directory" : "file",
      name: entry.name,
      relPath,
    });
    setContextMenu(null);
  }, [agentId, sessionId, isDir, relPath, entry.name, addAttachedContext]);

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

  const handlePreview = useCallback(() => {
    const workspaceId = useWorkspaceStore.getState().sessionWorkspaceMap[sessionId] ?? "__agent_home__";
    void openPreview(agentId, workspaceId, relPath);
    setContextMenu(null);
  }, [agentId, sessionId, relPath, openPreview]);

  return (
    <>
      <div
        className={cn(
          "flex cursor-pointer items-center gap-1 py-[2px] pr-3 hover:bg-zinc-100 dark:hover:bg-zinc-800",
          isSelected && "bg-[var(--color-accent)]/10",
        )}
        style={{ paddingLeft: `${depth * 16 + 8}px`, fontSize: "var(--ui-font-size, 0.875rem)" }}
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
          <span className="ml-auto text-zinc-400" style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.78)" }}>...</span>
        )}

        {/* Open-files dot indicator for directories (VS Code style) */}
        {isDir && hasOpenDescendant && (
          <span className="ml-auto h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
        )}
      </div>

      {/* Context menu portal — rendered to document.body to escape virtual list transform containment */}
      {contextMenu && createPortal(
        <div
          ref={menuRef}
          className="fixed z-[100] min-w-[160px] rounded-md border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          <button
            onClick={handleAddToChat}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            style={CONTEXT_MENU_FONT_SIZE}
          >
            <MessageSquarePlus className="h-3.5 w-3.5 text-zinc-400" />
            {t("workspace.contextMenu.addToChat")}
          </button>
          {isPreviewable && (
            <button
              onClick={handlePreview}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
              style={CONTEXT_MENU_FONT_SIZE}
            >
              <Eye className="h-3.5 w-3.5 text-zinc-400" />
              {t("workspace.contextMenu.preview")}
            </button>
          )}
          <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
          <button
            onClick={handleNewFile}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            style={CONTEXT_MENU_FONT_SIZE}
          >
            <FilePlus className="h-3.5 w-3.5 text-zinc-400" />
            {t("workspace.contextMenu.newFile")}
          </button>
          <button
            onClick={handleNewFolder}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            style={CONTEXT_MENU_FONT_SIZE}
          >
            <FolderPlus className="h-3.5 w-3.5 text-zinc-400" />
            {t("workspace.contextMenu.newFolder")}
          </button>
          <div className="my-1 border-t border-zinc-200 dark:border-zinc-700" />
          <button
            onClick={handleCopy}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            style={CONTEXT_MENU_FONT_SIZE}
          >
            <Copy className="h-3.5 w-3.5 text-zinc-400" />
            {t("workspace.contextMenu.copy")}
          </button>
          <button
            onClick={handlePaste}
            disabled={!useWorkspaceStore.getState().copiedEntry}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-zinc-700 hover:bg-zinc-100 disabled:opacity-40 disabled:cursor-not-allowed dark:text-zinc-300 dark:hover:bg-zinc-700"
            style={CONTEXT_MENU_FONT_SIZE}
          >
            <ClipboardPaste className="h-3.5 w-3.5 text-zinc-400" />
            {t("workspace.contextMenu.paste")}
          </button>
          <button
            onClick={handleDelete}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-900/20"
            style={CONTEXT_MENU_FONT_SIZE}
          >
            <Trash2 className="h-3.5 w-3.5 text-red-500" />
            {t("workspace.contextMenu.delete")}
          </button>
        </div>,
        document.body,
      )}
    </>
  );
});
