import { memo, useCallback, useState, useRef, useEffect } from "react";
import { createPortal } from "react-dom";
import { ChevronRight, FilePlus, FolderPlus, MessageSquarePlus, Trash2, Copy, ClipboardPaste, Eye, Check, Code, Pencil } from "lucide-react";
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

interface FileTreeNodeProps {
  entry: TreeEntry;
  depth: number;
  agentId: string;
  sessionId: string;
  relPath: string;
  absPath: string;
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
  /**
   * Rename the entry to `newName` (basename only — the parent path is
   * taken from the entry's current location). Resolves to `true` on
   * success, `false` on failure (caller surfaces an error toast).
   */
  onRename?: (relPath: string, newName: string, isDir: boolean) => Promise<boolean>;
  /** When this matches the node's `relPath`, the node swaps its name
   * span for an inline rename input. Owned by the parent so the same
   * machinery drives right-click "New File / Folder / Paste" (which
   * create the entry then enter rename) and right-click "Rename" on
   * an existing entry. */
  renameTarget?: string | null;
  /** Initial value seeded into the rename input when this node enters
   * rename mode. Updated only on transition into the rename state —
   * subsequent keystrokes live in local component state. */
  renameInitialValue?: string;
  /** Called when the user presses Esc, blurs without changes, or commits
   * an empty/no-op name. Parent should clear `renameTarget`. */
  onCancelRename?: (relPath: string) => void;
  /** Right-click "Rename" on this node — parent decides the target +
   * initial value (typically `entry.name`). */
  onRequestRename?: (relPath: string, initialName: string) => void;
  /* ── Pointer-based Drag & Drop ────────────────────────────────── */
  /** RelPath of the entry currently being dragged (or `null`). Set
   * by FileTree from the parent. When it matches this node's
   * `relPath`, the row renders at reduced opacity
   * (`.file-tree-row-drag-source`). */
  draggingRelPath?: string | null;
  /** RelPath of the directory the cursor is hovering. When it matches
   * this node AND the node is a directory, the row gets the accent
   * background (`.file-tree-row-drop-target`). */
  dropTarget?: string | null;
  /** pointerdown on a row - records the drag candidate. The parent's
   * global pointermove/pointerup listeners handle the actual drag. */
  onPointerDownTreeEntry?: (relPath: string, isDir: boolean, e: React.PointerEvent) => void;
}

export const FileTreeNode = memo(function FileTreeNode({
  entry,
  depth,
  agentId,
  sessionId,
  relPath,
  absPath,
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
  onRename,
  renameTarget = null,
  renameInitialValue = "",
  onCancelRename,
  onRequestRename,
  draggingRelPath = null,
  dropTarget = null,
  onPointerDownTreeEntry,
}: FileTreeNodeProps) {
  const isDir = entry.type === "directory";
  const fileIcon = isDir ? null : getFileIcon(entry.name);
  const { t } = useTranslation();
  const openPreview = useFileEditorStore((s) => s.openPreview);
  // Preview is available for Markdown (.md), HTML (.html/.htm), and image files.
  const isPreviewable = !isDir && /\.(md|html?)$/i.test(entry.name);

  // Context menu state
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  // Inline rename state — controlled by the parent via `renameTarget`.
  // When `renameTarget === relPath` the name span is replaced by an
  // `<input>` seeded with `renameInitialValue`. Local `inputValue` owns
  // the keystroke buffer; `wasRenamingRef` makes sure we only reseed on
  // the false→true transition so the user can keep typing without
  // losing what they already entered.
  const isRenaming = renameTarget === relPath;
  const [inputValue, setInputValue] = useState("");
  const renameInputRef = useRef<HTMLInputElement | null>(null);
  const wasRenamingRef = useRef(false);

  // Reseed + focus + select-stem only on the false→true transition.
  // Dependency array intentionally omits `inputValue` — we don't want
  // to reset the user's typing on every parent re-render.
  useEffect(() => {
    if (isRenaming && !wasRenamingRef.current) {
      const seed = renameInitialValue || entry.name;
      setInputValue(seed);
      // Wait for the input to mount before focusing / selecting.
      const frame = requestAnimationFrame(() => {
        const input = renameInputRef.current;
        if (!input) return;
        input.focus();
        const dotIdx = seed.lastIndexOf(".");
        // Leading-dot filenames like ".gitignore" use the whole name as stem.
        if (dotIdx > 0) {
          input.setSelectionRange(0, dotIdx);
        } else {
          input.select();
        }
      });
      wasRenamingRef.current = true;
      return () => cancelAnimationFrame(frame);
    }
    if (!isRenaming) {
      wasRenamingRef.current = false;
    }
  }, [isRenaming, renameInitialValue, entry.name]);

  /** Right-click "Rename" on this entry — defer to parent so the rename
   * target lives in a single place alongside the "New File / Folder /
   * Paste" flows. */
  const beginRename = useCallback(() => {
    setContextMenu(null);
    onRequestRename?.(relPath, entry.name);
  }, [onRequestRename, relPath, entry.name]);

  const handleCommitRename = useCallback(() => {
    if (!isRenaming) return; // already closed by parent
    const trimmed = inputValue.trim();
    // Empty / unchanged → just close (no server roundtrip).
    if (!trimmed || trimmed === entry.name) {
      onCancelRename?.(relPath);
      return;
    }
    if (!onRename) {
      onCancelRename?.(relPath);
      return;
    }
    // Fire-and-forget: the parent handles the async `rename_item` call
    // and closes the rename input via `onCancelRename` once it has a
    // result (success or toast-on-failure both close immediately).
    onRename(relPath, trimmed, isDir);
  }, [isRenaming, inputValue, entry.name, onRename, onCancelRename, relPath, isDir]);

  const handleCancelRename = useCallback(() => {
    if (!isRenaming) return;
    onCancelRename?.(relPath);
  }, [isRenaming, onCancelRename, relPath]);

  const handleRenameKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleCommitRename();
      } else if (e.key === "Escape") {
        e.preventDefault();
        handleCancelRename();
      }
    },
    [handleCommitRename, handleCancelRename],
  );

  const handleRenameBlur = useCallback(() => {
    handleCommitRename();
  }, [handleCommitRename]);

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
      // Directories: select (for toolbar "create in selected dir" target)
      // AND toggle expansion in one click. Matches VS Code behavior
      // where clicking a folder name selects it and toggles expansion
      // simultaneously — the chevron click is just a shortcut for the
      // same action.
      onSelect(entry, relPath);
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
      absPath,
    });
    setContextMenu(null);
  }, [agentId, sessionId, isDir, relPath, entry.name, absPath, addAttachedContext]);

  const handleDelete = useCallback(async () => {
    const label = isDir ? `directory "${entry.name}"` : `file "${entry.name}"`;
    let confirmed = false;
    try {
      const { ask } = await getTauriDialog();
      confirmed = await ask(`Delete ${label}?\n\nThis action cannot be undone.`, {
        title: "Confirm Delete",
        kind: "warning",
        okLabel: t("fileTree.deleteConfirmOk"),
        cancelLabel: t("fileTree.deleteConfirmCancel"),
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

  const handleTogglePromptFile = useCallback(() => {
    const state = useWorkspaceStore.getState();
    const workspaceId = state.sessionWorkspaceMap[sessionId] ?? "__agent_home__";
    const workspace = state.workspaces.find((ws) => ws.id === workspaceId);
    const isActive = workspace?.prompt_file === entry.name;
    const newPromptFile = isActive ? null : entry.name;
    void state.setPromptFile(agentId, workspaceId, newPromptFile);
    setContextMenu(null);
  }, [agentId, sessionId, entry.name]);

  // Check if this file qualifies as a prompt file (CLAUDE.md / AGENTS.md)
  const isPromptFile = !isDir && /^(CLAUDE|AGENTS)\.md$/i.test(entry.name);
  const workspaceId = useWorkspaceStore((s) => s.sessionWorkspaceMap[sessionId] ?? "__agent_home__");
  const workspace = useWorkspaceStore((s) => s.workspaces.find((ws) => ws.id === workspaceId));
  const isActivePromptFile = workspace?.prompt_file === entry.name;

  // DnD visibility state — kept as locals because they're pure
  // derivations of parent-supplied props; recomputing inside JSX would
  // re-allocate className strings every render.
  const isDragSource = draggingRelPath === relPath;
  // Drop highlight: only directories can be drop targets, and the row
  // must currently be the active drop target per parent's state.
  // (Parent's `onDragOverTreeEntry` already rejected self/ancestor
  // drags and files, so the class application here is safe.)
  const isDropTarget = isDir && dropTarget === relPath;

  return (
    <>
      <div
        className={cn(
          "file-tree-row flex cursor-pointer items-center gap-1 py-[0.2em] pr-3 hover:bg-zinc-100 dark:hover:bg-zinc-800 select-none",
          isSelected && "bg-[var(--color-accent)]/10",
          isDragSource && "file-tree-row-drag-source",
          isDropTarget && "file-tree-row-drop-target",
        )}
        style={{ paddingLeft: `${depth * 16 + 8}px`, fontSize: "var(--ui-font-size, 0.875rem)" }}
        /* DnD: data attributes identify this row to the parent's global
         * pointermove/pointerup handlers via `elementFromPoint`.
         * `onPointerDown` records the drag candidate; the parent
         * decides when to actually start dragging (after a 5px
         * threshold). No HTML5 DnD events are used. */
        data-rel-path={relPath}
        data-is-dir={String(isDir)}
        onPointerDown={(e) => {
          if (!onPointerDownTreeEntry) return;
          onPointerDownTreeEntry(relPath, isDir, e);
        }}
        onClick={handleClick}
        onDoubleClick={handleDoubleClick}
        onContextMenu={handleContextMenu}
        title={relPath}
      >
        {/* Icon — chevron for dirs, file-type for files; both occupy same 16px slot so names align */}
        <span className="flex shrink-0 items-center justify-center" style={{ height: "1.15em", width: "1.15em" }}>
          {isDir ? (
            <ChevronRight
              className={cn(
                "h-[0.8em] w-[0.8em] text-zinc-400 transition-transform duration-150",
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

        {/* Name — no truncation; horizontal scrollbar on parent handles overflow.
            When `isRenaming`, swap the span for an inline input. */}
        {isRenaming ? (
          <input
            ref={renameInputRef}
            type="text"
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            onKeyDown={handleRenameKeyDown}
            onBlur={handleRenameBlur}
            onClick={(e) => e.stopPropagation()}
            onDoubleClick={(e) => e.stopPropagation()}
            onContextMenu={(e) => e.stopPropagation()}
            className="min-w-0 flex-1 rounded-sm border border-[var(--color-accent)] bg-modal-surface px-1 text-zinc-700 outline-none dark:bg-zinc-900 dark:text-zinc-300"
            style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
          />
        ) : (
          <span className="whitespace-nowrap text-zinc-700 dark:text-zinc-400">{entry.name}</span>
        )}

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
          className="context-menu"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          <button
            type="button"
            onClick={handleAddToChat}
            className="context-menu-item"
          >
            <MessageSquarePlus className="context-menu-item__icon" />
            {t("workspace.contextMenu.addToChat")}
          </button>
          {isPreviewable && (
            <button
              type="button"
              onClick={handlePreview}
              className="context-menu-item"
            >
              <Eye className="context-menu-item__icon" />
              {t("workspace.contextMenu.preview")}
            </button>
          )}
          {isPromptFile && (
            <button
              type="button"
              onClick={handleTogglePromptFile}
              className="context-menu-item"
            >
              {isActivePromptFile ? (
                <Check className="context-menu-item__icon" style={{ color: "#22c55e" }} />
              ) : (
                <Code className="context-menu-item__icon" />
              )}
              {isActivePromptFile ? "取消注入上下文" : "注入上下文"}
            </button>
          )}
          <div className="context-menu-divider" />
          <button
            type="button"
            onClick={handleNewFile}
            className="context-menu-item"
          >
            <FilePlus className="context-menu-item__icon" />
            {t("workspace.contextMenu.newFile")}
          </button>
          <button
            type="button"
            onClick={handleNewFolder}
            className="context-menu-item"
          >
            <FolderPlus className="context-menu-item__icon" />
            {t("workspace.contextMenu.newFolder")}
          </button>
          <div className="context-menu-divider" />
          <button
            type="button"
            onClick={handleCopy}
            className="context-menu-item"
          >
            <Copy className="context-menu-item__icon" />
            {t("workspace.contextMenu.copy")}
          </button>
          <button
            type="button"
            onClick={handlePaste}
            disabled={!useWorkspaceStore.getState().copiedEntry}
            className="context-menu-item"
          >
            <ClipboardPaste className="context-menu-item__icon" />
            {t("workspace.contextMenu.paste")}
          </button>
          {onRename && (
            <button
              type="button"
              onClick={beginRename}
              className="context-menu-item"
            >
              <Pencil className="context-menu-item__icon" />
              {t("workspace.contextMenu.rename")}
            </button>
          )}
          <button
            type="button"
            onClick={handleDelete}
            className="context-menu-item context-menu-item--danger"
          >
            <Trash2 className="context-menu-item__icon" />
            {t("workspace.contextMenu.delete")}
          </button>
        </div>,
        document.body,
      )}
    </>
  );
});