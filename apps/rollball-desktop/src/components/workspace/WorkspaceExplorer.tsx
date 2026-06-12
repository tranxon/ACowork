import { useState, useCallback, useRef, useEffect } from "react";
import { RefreshCw, FolderOpen, FilePlus, FolderPlus } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { FileTree } from "./FileTree/FileTree";
import { WorkspaceSelector } from "./WorkspaceSelector";
import type { TreeEntry } from "../../stores/workspaceStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";

export function WorkspaceExplorer() {
    const { t } = useTranslation();
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
    const agents = useAgentStore((s) => s.agents);
    const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);
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
        void openFile(selectedAgentId, currentWorkspaceId, relPath);
    }, [selectedAgentId, currentWorkspaceId, openFile]);

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
                    <span className="text-[10px] font-medium text-[var(--color-accent)] shrink-0">
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

            {/* File tree (normal mode, no search filtering) */}
            {selectedAgentId && activeSessionId && (
                <FileTree
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
