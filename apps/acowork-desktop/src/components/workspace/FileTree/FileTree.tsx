import { useState, useCallback, useEffect, useMemo, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useWorkspaceStore, type TreeEntry } from "../../../stores/workspaceStore";
import { useAgentStore } from "../../../stores/agentStore";
import { useChatStore } from "../../../stores/chatStore";
import { useFileEditorStore } from "../../../stores/fileEditorStore";
import { FileTreeNode } from "./FileTreeNode";

/** Flattened tree node for virtualized rendering */
interface FlatNode {
    entry: TreeEntry;
    depth: number;
    relPath: string;
}

interface FileTreeProps {
    agentId: string;
    workspaceId: string;
    sessionId: string;
    onFileDoubleClick?: (entry: TreeEntry, relPath: string) => void;
    onContextNewItem?: (type: "file" | "dir", parentPath: string) => void;
    onDelete?: (relPath: string, isDir: boolean) => void;
    onCopy?: (relPath: string, isDir: boolean) => void;
    onPaste?: (parentPath: string) => void;
}

export function FileTree({ agentId, workspaceId, sessionId, onFileDoubleClick, onContextNewItem, onDelete, onCopy, onPaste }: FileTreeProps) {
    const [selectedPath, setSelectedPath] = useState<string | null>(null);
    const treeCache = useWorkspaceStore((s) => s.treeCache);
    const fetchTree = useWorkspaceStore((s) => s.fetchTree);
    const treeLoadingPaths = useWorkspaceStore((s) => s.treeLoadingPaths);
    const toggleTreeExpandedPath = useChatStore((s) => s.toggleTreeExpandedPath);
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

    /** Build cache key prefix: agentId:workspaceId (tree cache is NOT per-session) */
    const treeCachePrefix = `${agentId}:${workspaceId}`;

    // Expanded paths from the session — Zustand selector is reactive
    const expandedPathsArr = useChatStore((s) => {
        const ss = s.agentStates[agentId]?.sessionStates[sessionId];
        return ss?.treeExpandedPaths ?? [];
    });
    const expandedPaths = useMemo(() => new Set(expandedPathsArr), [expandedPathsArr]);

    // Compute set of directory paths that contain at least one open editor tab.
    // e.g. open file "src/components/Foo.tsx" → dirs: "src", "src/components"
    const openFiles = useFileEditorStore((s) => s.openFiles);
    const openFileDirSet = useMemo(() => {
        const dirs = new Set<string>();
        for (const f of openFiles) {
            const parts = f.relPath.split("/");
            for (let i = 0; i < parts.length - 1; i++) {
                dirs.add(parts.slice(0, i + 1).join("/"));
            }
        }
        return dirs;
    }, [openFiles]);

    // Reset state when agent or workspace changes
    useEffect(() => {
        setSelectedPath(null);
    }, [selectedAgentId, workspaceId]);

    // Fetch root when agent or workspace changes
    useEffect(() => {
        if (agentId) {
            fetchTree(agentId, workspaceId, "");
        }
    }, [agentId, workspaceId, fetchTree]);

    // Flatten the tree into a list respecting expanded state
    const flatNodes = useMemo<FlatNode[]>(() => {
        const result: FlatNode[] = [];

        function walk(relPath: string, depth: number) {
            const cacheKey = `${treeCachePrefix}:${relPath}`;
            const entries = treeCache[cacheKey];
            if (!entries) return;

            for (const entry of entries) {
                const childRelPath = relPath ? `${relPath}/${entry.name}` : entry.name;

                result.push({ entry, depth, relPath: childRelPath });

                if (entry.type === "directory" && expandedPaths.has(childRelPath)) {
                    walk(childRelPath, depth + 1);
                }
            }
        }

        walk("", 0);
        return result;
    }, [treeCachePrefix, treeCache, expandedPaths]);

    const handleToggle = useCallback(
        (relPath: string) => {
            const isCurrentlyExpanded = expandedPaths.has(relPath);
            toggleTreeExpandedPath(agentId, sessionId, relPath);
            // Lazy-load children when expanding
            if (!isCurrentlyExpanded && !treeCache[`${treeCachePrefix}:${relPath}`]) {
                fetchTree(agentId, workspaceId, relPath);
            }
        },
        [agentId, workspaceId, sessionId, treeCachePrefix, expandedPaths, treeCache, fetchTree, toggleTreeExpandedPath],
    );

    const handleSelect = useCallback((_entry: TreeEntry, relPath: string) => {
        setSelectedPath(relPath);
    }, []);

    // Virtual scrolling setup
    const scrollRef = useRef<HTMLDivElement | null>(null);
    const virtualizer = useVirtualizer({
        count: flatNodes.length,
        getScrollElement: () => scrollRef.current,
        estimateSize: () => 22,
        overscan: 20,
    });

    // Empty state
    if (flatNodes.length === 0) {
        const rootEntries = treeCache[`${treeCachePrefix}:`];
        if (!rootEntries) {
            return (
                <div className="flex items-center justify-center py-8 text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                    Loading...
                </div>
            );
        }
        if (rootEntries.length === 0) {
            return (
                <div className="flex flex-col items-center justify-center py-8 text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                    <span>Empty workspace</span>
                </div>
            );
        }
    }

    return (
        <div
            ref={scrollRef}
            className="flex-1 overflow-auto"
        >
            <div
                style={{
                    height: `${virtualizer.getTotalSize()}px`,
                    width: "fit-content",
                    minWidth: "100%",
                    position: "relative",
                }}
            >
                {virtualizer.getVirtualItems().map((virtualRow) => {
                    const node = flatNodes[virtualRow.index];
                    const isLoading = treeLoadingPaths.has(`${treeCachePrefix}:${node.relPath}`);

                    return (
                        <div
                            key={node.relPath}
                            style={{
                                position: "absolute",
                                top: 0,
                                left: 0,
                                minWidth: "100%",
                                width: "fit-content",
                                height: `${virtualRow.size}px`,
                                transform: `translateY(${virtualRow.start}px)`,
                            }}
                        >
                            <FileTreeNode
                                entry={node.entry}
                                depth={node.depth}
                                relPath={node.relPath}
                                isExpanded={expandedPaths.has(node.relPath)}
                                isLoading={isLoading}
                                isSelected={selectedPath === node.relPath}
                                hasOpenDescendant={openFileDirSet.has(node.relPath)}
                                onToggle={handleToggle}
                                onSelect={handleSelect}
                                onDoubleClick={onFileDoubleClick}
                                onContextNewItem={onContextNewItem}
                                onDelete={onDelete}
                                onCopy={onCopy}
                                onPaste={onPaste}
                            />
                        </div>
                    );
                })}
            </div>
        </div>
    );
}
