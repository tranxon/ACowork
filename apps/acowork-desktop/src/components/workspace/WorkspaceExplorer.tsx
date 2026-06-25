import { useState, useCallback, useRef, useEffect } from "react";
import { Search, RefreshCw, FolderOpen, FilePlus, FolderPlus, X } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore, type TreeEntry } from "../../stores/workspaceStore";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { FileTree } from "./FileTree/FileTree";
import { WorkspaceSelector } from "./WorkspaceSelector";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "./FileTree/fileIcons";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { cn } from "../../lib/utils";

/** Abbreviate a file path from the left: "…parent/filename.ext" */
function abbreviatePath(path: string): string {
    const maxLen = 38;
    if (path.length <= maxLen) return path;
    const parts = path.split("/");
    const filename = parts[parts.length - 1];
    for (let i = parts.length - 2; i >= 1; i--) {
        const abbreviated = `…${parts.slice(i).join("/")}`;
        if (abbreviated.length <= maxLen) return abbreviated;
    }
    return `…${filename}`;
}

export function WorkspaceExplorer() {
    const { t } = useTranslation();
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
    const fontSize = useSettingsStore((s) => s.fontSize);
    const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);
    const invalidateTreeCache = useWorkspaceStore((s) => s.invalidateTreeCache);
    const fetchTree = useWorkspaceStore((s) => s.fetchTree);
    const sessionWorkspaceMap = useWorkspaceStore((s) => s.sessionWorkspaceMap);
    const createFile = useWorkspaceStore((s) => s.createFile);
    const createDir = useWorkspaceStore((s) => s.createDir);
    const deleteFile = useWorkspaceStore((s) => s.deleteFile);
    const deleteDir = useWorkspaceStore((s) => s.deleteDir);
    const copyItem = useWorkspaceStore((s) => s.copyItem);
    const setCopiedEntry = useWorkspaceStore((s) => s.setCopiedEntry);
    const openFile = useFileEditorStore((s) => s.openFile);
    const openPreview = useFileEditorStore((s) => s.openPreview);

    // Get the current workspace ID for the active session
    const activeSessionId = useChatStore((s) =>
        selectedAgentId ? s.getActiveSessionId(selectedAgentId) : null,
    );
    const currentWorkspaceId = activeSessionId
        ? (sessionWorkspaceMap[activeSessionId] ?? "__agent_home__")
        : "__agent_home__";

    const [newItemPrompt, setNewItemPrompt] = useState<{ type: "file" | "dir"; parentPath: string } | null>(null);
    const [newItemName, setNewItemName] = useState("");
    const promptInputRef = useRef<HTMLInputElement | null>(null);

    // Auto-focus the prompt input when it appears
    useEffect(() => {
        if (newItemPrompt && promptInputRef.current) {
            promptInputRef.current.focus();
        }
    }, [newItemPrompt]);

    /* ── Search box state (Ctrl+P-style file search above the file tree) ── */
    const [searchQuery, setSearchQuery] = useState("");
    const [searchFocused, setSearchFocused] = useState(false);
    const [focusedIdx, setFocusedIdx] = useState(0);
    const [searchLoading, setSearchLoading] = useState(false);
    const [searchResults, setSearchResults] = useState<
        Array<{ name: string; relPath: string; dir: string; score: number }>
    >([]);
    const searchInputRef = useRef<HTMLInputElement>(null);

    // Server-side filename search — one request per debounced query.
    // Previous in-flight requests are cancelled via AbortController so
    // the latest keystroke always wins, even on a slow network.
    useEffect(() => {
        if (!searchQuery.trim() || !selectedAgentId) {
            setSearchResults([]);
            setSearchLoading(false);
            return;
        }
        const controller = new AbortController();
        setSearchLoading(true);
        const timer = setTimeout(() => {
            void useWorkspaceStore
                .getState()
                .findFiles(
                    selectedAgentId,
                    currentWorkspaceId,
                    searchQuery,
                    50,
                    controller.signal,
                )
                .then((resp) => {
                    if (controller.signal.aborted) return;
                    if (!resp) {
                        setSearchResults([]);
                        setSearchLoading(false);
                        return;
                    }
                    const rows = resp.matches.map((m) => {
                        const slash = m.relPath.lastIndexOf("/");
                        return {
                            name: m.name,
                            relPath: m.relPath,
                            dir: slash >= 0 ? m.relPath.slice(0, slash) : "",
                            score: m.score,
                        };
                    });
                    setSearchResults(rows);
                    setSearchLoading(false);
                });
        }, 150);
        return () => {
            clearTimeout(timer);
            controller.abort();
        };
    }, [searchQuery, selectedAgentId, currentWorkspaceId]);

    const matchingFiles = searchQuery ? searchResults : [];

    // Clamp focused index when the result list changes
    useEffect(() => {
        setFocusedIdx((i) => (i >= matchingFiles.length ? 0 : i));
    }, [searchQuery, matchingFiles.length]);

    const handleSearchSelect = useCallback((relPath: string) => {
        if (!selectedAgentId) return;
        // Image extensions open in preview (mirrors handleFileDoubleClick)
        if (/\.(jpg|jpeg|png|gif|webp|svg)$/i.test(relPath)) {
            void openPreview(selectedAgentId, currentWorkspaceId, relPath);
        } else {
            void openFile(selectedAgentId, currentWorkspaceId, relPath);
        }
        setSearchQuery("");
        setSearchFocused(false);
        searchInputRef.current?.blur();
    }, [selectedAgentId, currentWorkspaceId, openFile, openPreview]);

    const handleSearchKeyDown = useCallback((e: React.KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Escape") {
            e.preventDefault();
            setSearchQuery("");
            setSearchFocused(false);
            searchInputRef.current?.blur();
        } else if (e.key === "ArrowDown") {
            e.preventDefault();
            setFocusedIdx((i) => Math.min(i + 1, matchingFiles.length - 1));
        } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setFocusedIdx((i) => Math.max(i - 1, 0));
        } else if (e.key === "Enter") {
            e.preventDefault();
            const item = matchingFiles[focusedIdx];
            if (item) handleSearchSelect(item.relPath);
        }
    }, [matchingFiles, focusedIdx, handleSearchSelect]);

    const showDropdown = searchFocused && searchQuery.trim().length > 0;

    const handleRefresh = useCallback(() => {
        if (!selectedAgentId) return;
        invalidateTreeCache(selectedAgentId);
        fetchTree(selectedAgentId, currentWorkspaceId, "");
    }, [selectedAgentId, currentWorkspaceId, invalidateTreeCache, fetchTree]);

    const handleNewFile = useCallback(() => {
        console.log("[WorkspaceExplorer] handleNewFile clicked, agent:", selectedAgentId, "workspace:", currentWorkspaceId);
        setNewItemName("");
        setNewItemPrompt({ type: "file", parentPath: "" });
    }, [selectedAgentId, currentWorkspaceId]);

    const handleNewFolder = useCallback(() => {
        console.log("[WorkspaceExplorer] handleNewFolder clicked");
        setNewItemName("");
        setNewItemPrompt({ type: "dir", parentPath: "" });
    }, []);

    const cancelPrompt = useCallback(() => {
        setNewItemPrompt(null);
        setNewItemName("");
    }, []);

    const handlePromptSubmit = useCallback(async () => {
        if (!selectedAgentId || !newItemPrompt) return;
        const name = newItemName.trim();
        if (!name) return;

        const relPath = newItemPrompt.parentPath ? `${newItemPrompt.parentPath}/${name}` : name;

        console.log("[WorkspaceExplorer] Creating", newItemPrompt.type, "at", relPath, "workspace:", currentWorkspaceId);

        let ok: boolean;
        if (newItemPrompt.type === "file") {
            ok = await createFile(selectedAgentId, currentWorkspaceId, relPath);
        } else {
            ok = await createDir(selectedAgentId, currentWorkspaceId, relPath);
        }

        console.log("[WorkspaceExplorer] Create result:", ok);

        if (ok) {
            // Re-fetch only the parent directory — fetchTree overwrites its cache entry,
            // so we don't need to invalidate everything (which would blank the tree).
            if (newItemPrompt.parentPath) {
                fetchTree(selectedAgentId, currentWorkspaceId, newItemPrompt.parentPath);
            } else {
                fetchTree(selectedAgentId, currentWorkspaceId, "");
            }
        }

        setNewItemPrompt(null);
        setNewItemName("");
    }, [selectedAgentId, currentWorkspaceId, newItemPrompt, newItemName, createFile, createDir, fetchTree]);

    const handlePromptKeyDown = useCallback((e: React.KeyboardEvent) => {
        console.log("[WorkspaceExplorer] keyDown:", e.key, "newItemName:", newItemName);
        if (e.key === "Escape") {
            cancelPrompt();
        } else if (e.key === "Enter") {
            e.preventDefault();
            handlePromptSubmit();
        }
    }, [handlePromptSubmit, cancelPrompt, newItemName]);

    const handleFileDoubleClick = useCallback((_entry: TreeEntry, relPath: string) => {
        if (!selectedAgentId) return;
        // Images open in preview; everything else opens in editor (source code)
        if (/\.(jpg|jpeg|png|gif|webp|svg)$/i.test(relPath)) {
            void openPreview(selectedAgentId, currentWorkspaceId, relPath);
        } else {
            void openFile(selectedAgentId, currentWorkspaceId, relPath);
        }
    }, [selectedAgentId, currentWorkspaceId, openFile, openPreview]);

    /** Called from FileTree context menu to create item at a specific path */
    const handleContextNewItem = useCallback((type: "file" | "dir", parentPath: string) => {
        setNewItemName("");
        setNewItemPrompt({ type, parentPath });
    }, []);

    const handleDelete = useCallback(async (relPath: string, isDir: boolean) => {
        if (!selectedAgentId) return;
        const ok = isDir
            ? await deleteDir(selectedAgentId, currentWorkspaceId, relPath)
            : await deleteFile(selectedAgentId, currentWorkspaceId, relPath);
        if (ok) {
            // Re-fetch parent directory
            const parentPath = relPath.substring(0, relPath.lastIndexOf("/"));
            if (parentPath) {
                fetchTree(selectedAgentId, currentWorkspaceId, parentPath);
            } else {
                fetchTree(selectedAgentId, currentWorkspaceId, "");
            }
        }
    }, [selectedAgentId, currentWorkspaceId, deleteFile, deleteDir, fetchTree]);

    const handleCopy = useCallback((relPath: string, isDir: boolean) => {
        if (!selectedAgentId) return;
        setCopiedEntry({
            agentId: selectedAgentId,
            workspaceId: currentWorkspaceId,
            path: relPath,
            type: isDir ? "directory" : "file",
        });
    }, [selectedAgentId, currentWorkspaceId, setCopiedEntry]);

    const handlePaste = useCallback(async (parentPath: string) => {
        if (!selectedAgentId) return;
        const entry = useWorkspaceStore.getState().copiedEntry;
        if (!entry || entry.agentId !== selectedAgentId || entry.workspaceId !== currentWorkspaceId) return;

        const name = entry.path.split("/").pop() || entry.path;
        // Generate a unique name to avoid "Destination already exists":
        // "aaa.txt" → "aaa copy.txt", "bbbb" → "bbbb copy"
        const dotIdx = name.lastIndexOf(".");
        const uniqueName = dotIdx > 0
            ? `${name.slice(0, dotIdx)} copy${name.slice(dotIdx)}`
            : `${name} copy`;
        const dest = parentPath ? `${parentPath}/${uniqueName}` : uniqueName;

        const ok = await copyItem(selectedAgentId, currentWorkspaceId, entry.path, dest);
        setCopiedEntry(null); // clear clipboard after paste (one-shot)
        if (ok) {
            fetchTree(selectedAgentId, currentWorkspaceId, parentPath || "");
        }
    }, [selectedAgentId, currentWorkspaceId, copyItem, fetchTree, setCopiedEntry]);

    if (!selectedAgent?.running) {
        return (
            <div className="flex flex-1 flex-col items-center justify-center gap-2 p-6 text-xs text-zinc-500 dark:text-zinc-400">
                <FolderOpen className="h-6 w-6" />
                <span>{t("workspace.explorer.agentNotRunning")}</span>
            </div>
        );
    }

    return (
        <div className="flex flex-1 flex-col overflow-hidden">
            {/* Workspace selector + action buttons */}
            <div className="flex items-center gap-0.5 border-b border-zinc-200 px-1.5 py-1.5 dark:border-zinc-800">
                <WorkspaceSelector dropDirection="down" />
                <div className="ml-auto flex items-center gap-0.5">
                    <Tooltip content={t("workspace.newFile")} variant="plain">
                        <button
                            onClick={handleNewFile}
                            className="rounded p-1 text-zinc-400 hover:bg-zinc-100 hover:text-[var(--color-accent)] dark:hover:bg-zinc-800"
                        >
                            <FilePlus className="h-3.5 w-3.5" />
                        </button>
                    </Tooltip>
                    <Tooltip content={t("workspace.newFolder")} variant="plain">
                        <button
                            onClick={handleNewFolder}
                            className="rounded p-1 text-zinc-400 hover:bg-zinc-100 hover:text-yellow-600 dark:hover:bg-zinc-800 dark:hover:text-yellow-400"
                        >
                            <FolderPlus className="h-3.5 w-3.5" />
                        </button>
                    </Tooltip>
                    <button
                        onClick={handleRefresh}
                        className="rounded p-0.5 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-800 dark:hover:text-zinc-300"

                    >
                        <RefreshCw className="h-3 w-3" />
                    </button>
                </div>
            </div>

            {/* Inline name prompt for new file/directory */}
            {newItemPrompt && (
                <div className="flex items-center gap-1.5 border-b border-[var(--color-accent)]/30 bg-[var(--color-accent)]/10 px-3 py-1.5">
                    <span className="font-medium text-[10px] text-[var(--color-accent)] shrink-0">
                        {newItemPrompt.type === "file" ? "New file:" : "New folder:"}
                    </span>
                    <input
                        ref={promptInputRef}
                        type="text"
                        value={newItemName}
                        onChange={(e) => setNewItemName(e.target.value)}
                        onKeyDown={handlePromptKeyDown}
                        placeholder={newItemPrompt.type === "file" ? "filename.ext" : "folder-name"}
                        className="flex-1 bg-transparent text-xs text-zinc-700 outline-none placeholder:text-zinc-400 dark:text-zinc-300 dark:placeholder:text-zinc-500"
                    />
                </div>
            )}

            {/* Search box with file-search dropdown (Ctrl+P-style) */}
            <div className="relative border-b border-zinc-200 dark:border-zinc-800">
                <div className="flex items-center gap-1.5 px-3 py-1">
                    <Search className="h-3 w-3 shrink-0 text-zinc-400" />
                    <input
                        ref={searchInputRef}
                        type="text"
                        value={searchQuery}
                        onChange={(e) => setSearchQuery(e.target.value)}
                        onFocus={() => setSearchFocused(true)}
                        onBlur={() => setTimeout(() => setSearchFocused(false), 150)}
                        onKeyDown={handleSearchKeyDown}
                        placeholder={t("workspace.explorer.searchPlaceholder")}
                        className="flex-1 bg-transparent text-xs text-zinc-700 outline-none placeholder:text-zinc-400 dark:text-zinc-400 dark:placeholder:text-zinc-500"
                    />
                    {searchQuery && (
                        <button
                            onClick={() => {
                                setSearchQuery("");
                                searchInputRef.current?.focus();
                            }}
                            className="text-[10px] text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
                            title="Clear search"
                        >
                            <X className="h-3 w-3" />
                        </button>
                    )}
                </div>

                {/* Dropdown results */}
                {showDropdown && (
                    <div className="absolute left-0 right-0 top-full z-50 mt-1 rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
                        {/* Header with count */}
                        <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-1.5 text-[11px] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400">
                            {searchLoading ? (
                                <span>Searching…</span>
                            ) : matchingFiles.length > 0 ? (
                                <span>{matchingFiles.length} matching file{matchingFiles.length === 1 ? "" : "s"}</span>
                            ) : (
                                <span>No matching files</span>
                            )}
                        </div>

                        {/* Items */}
                        <div className="max-h-80 overflow-y-auto py-1">
                            {matchingFiles.length === 0 ? (
                                <div className="px-3 py-4 text-center text-xs text-zinc-400 dark:text-zinc-500">
                                    {searchLoading ? "Searching workspace…" : "No matching files"}
                                </div>
                            ) : (
                                matchingFiles.map((f, idx) => {
                                    const focused = idx === focusedIdx;
                                    return (
                                        <div
                                            key={f.relPath}
                                            onMouseDown={(e) => e.preventDefault()}
                                            onMouseEnter={() => setFocusedIdx(idx)}
                                            onClick={() => handleSearchSelect(f.relPath)}
                                            className={cn(
                                                "group flex items-center gap-2 px-3 py-1.5 transition-colors cursor-pointer",
                                                focused
                                                    ? "bg-[var(--color-accent)]/10 text-zinc-900 dark:text-zinc-100"
                                                    : "hover:bg-zinc-50 dark:hover:bg-zinc-700/50",
                                            )}
                                        >
                                            <div className="h-3.5 w-3.5 shrink-0 flex items-center justify-center">
                                                <SetiIcon {...getFileIcon(f.name)} size={14} />
                                            </div>
                                            <span className="shrink-0 text-xs text-zinc-700 dark:text-zinc-300">
                                                {f.name}
                                            </span>
                                            <span className="min-w-0 truncate text-[10px] text-zinc-400 dark:text-zinc-500 ml-3">
                                                {abbreviatePath(f.dir)}
                                            </span>
                                        </div>
                                    );
                                })
                            )}
                        </div>
                    </div>
                )}
            </div>

            {/* File tree (normal mode, no search filtering) */}
            {selectedAgentId && activeSessionId && (
                <FileTree
                    key={fontSize}
                    agentId={selectedAgentId}
                    workspaceId={currentWorkspaceId}
                    sessionId={activeSessionId}
                    onFileDoubleClick={handleFileDoubleClick}
                    onContextNewItem={handleContextNewItem}
                    onDelete={handleDelete}
                    onCopy={handleCopy}
                    onPaste={handlePaste}
                />
            )}
        </div>
    );
}
