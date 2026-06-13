import { useState, useEffect, useCallback } from "react";
import { ChevronRight, ChevronDown, Folder, FolderOpen, HardDrive } from "lucide-react";
import { useSettingsStore } from "../../stores/settingsStore";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";

interface FsBrowseEntry {
    name: string;
    type: string;
    path: string;
    size?: number;
    childrenCount?: number;
}

interface FsBrowseResponse {
    path: string;
    entries: FsBrowseEntry[];
}

interface RemoteFolderPickerProps {
    /** Called when user selects a directory path */
    onSelect: (path: string) => void;
    /** Called when user cancels */
    onCancel: () => void;
}

export function RemoteFolderPicker({ onSelect, onCancel }: RemoteFolderPickerProps) {
    const { t } = useTranslation();
    const { gatewayUrl } = useSettingsStore();
    const baseUrl = gatewayUrl || DEFAULT_GATEWAY_URL;

    // Navigation history (breadcrumb)
    const [breadcrumbs, setBreadcrumbs] = useState<string[]>([]);
    const [currentPath, setCurrentPath] = useState<string>("");
    const [entries, setEntries] = useState<FsBrowseEntry[]>([]);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [selectedPath, setSelectedPath] = useState<string | null>(null);

    // Expanded directories (for inline expansion)
    const [expandedDirs, setExpandedDirs] = useState<Set<string>>(new Set());
    const [expandedEntries, setExpandedEntries] = useState<Map<string, FsBrowseEntry[]>>(new Map());

    const fetchEntries = useCallback(async (path: string) => {
        setLoading(true);
        setError(null);
        try {
            const resp = await fetch(`${baseUrl}/api/fs/browse?path=${encodeURIComponent(path)}`);
            if (!resp.ok) {
                const err = await resp.json().catch(() => null);
                setError(err?.error || `Failed to browse: ${resp.status}`);
                return;
            }
            const data: FsBrowseResponse = await resp.json();
            setEntries(data.entries);
            setCurrentPath(data.path || path);
        } catch (e) {
            setError(String(e));
        } finally {
            setLoading(false);
        }
    }, [baseUrl]);

    // Load root on mount
    useEffect(() => {
        void fetchEntries("");
    }, [fetchEntries]);

    const navigateTo = useCallback(async (path: string) => {
        setExpandedDirs(new Set());
        setExpandedEntries(new Map());
        setSelectedPath(null);

        // Build breadcrumbs
        if (path === "" || path === "/") {
            setBreadcrumbs([]);
        } else {
            const parts = path.split("/").filter(Boolean);
            // Handle Windows paths like C:/Users/...
            const crumbs: string[] = [];
            if (path.match(/^\w:\//)) {
                // Windows drive root
                crumbs.push(path.slice(0, 3).replace("/", ":/") + "/");
            }
            // Build incremental paths
            let accumulated = path.match(/^\w:\//) ? path.slice(0, 3) : "";
            for (const part of parts) {
                accumulated = accumulated ? `${accumulated}/${part}` : `/${part}`;
                crumbs.push(accumulated);
            }
            setBreadcrumbs(crumbs);
        }

        await fetchEntries(path);
    }, [fetchEntries]);

    const handleExpand = useCallback(async (entry: FsBrowseEntry) => {
        if (entry.type !== "directory") return;

        const newExpanded = new Set(expandedDirs);
        if (newExpanded.has(entry.path)) {
            newExpanded.delete(entry.path);
            setExpandedDirs(newExpanded);
            return;
        }
        newExpanded.add(entry.path);
        setExpandedDirs(newExpanded);

        // Fetch sub-entries if not cached
        if (!expandedEntries.has(entry.path)) {
            try {
                const resp = await fetch(`${baseUrl}/api/fs/browse?path=${encodeURIComponent(entry.path)}`);
                if (resp.ok) {
                    const data: FsBrowseResponse = await resp.json();
                    setExpandedEntries(new Map(expandedEntries).set(entry.path, data.entries));
                }
            } catch {
                // ignore
            }
        }
    }, [baseUrl, expandedDirs, expandedEntries]);

    const handleConfirm = () => {
        if (selectedPath) {
            onSelect(selectedPath);
        }
    };

    const handleSelectDir = (entry: FsBrowseEntry) => {
        if (entry.type === "directory") {
            setSelectedPath(entry.path);
        }
    };

    const handleDoubleClick = (entry: FsBrowseEntry) => {
        if (entry.type === "directory") {
            void navigateTo(entry.path);
        }
    };

    // Render a directory entry row
    const renderEntry = (entry: FsBrowseEntry, depth: number = 0) => {
        const isExpanded = expandedDirs.has(entry.path);
        const isSelected = selectedPath === entry.path;
        const isDir = entry.type === "directory";
        const hasChildren = isDir && (entry.childrenCount ?? 0) > 0;

        return (
            <div key={entry.path}>
                <div
                    className={cn(
                        "flex items-center gap-1 px-2 py-1 cursor-pointer transition-colors text-xs",
                        isSelected
                            ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
                            : "hover:bg-zinc-100 dark:hover:bg-zinc-700/50 text-zinc-700 dark:text-zinc-300",
                    )}
                    style={{ paddingLeft: `${depth * 16 + 8}px` }}
                    onClick={() => handleSelectDir(entry)}
                    onDoubleClick={() => handleDoubleClick(entry)}
                >
                    {isDir ? (
                        <button
                            onClick={(e) => { e.stopPropagation(); void handleExpand(entry); }}
                            className="flex items-center"
                        >
                            {hasChildren ? (
                                isExpanded ? <ChevronDown className="h-3 w-3 shrink-0" /> : <ChevronRight className="h-3 w-3 shrink-0" />
                            ) : (
                                <span className="w-3" />
                            )}
                        </button>
                    ) : (
                        <span className="w-3" />
                    )}
                    {isDir ? (
                        isExpanded ? <FolderOpen className="h-3.5 w-3.5 shrink-0 text-zinc-400" /> : <Folder className="h-3.5 w-3.5 shrink-0 text-zinc-400" />
                    ) : null}
                    <span className="truncate min-w-0 flex-1">{entry.name}</span>
                    {!isDir && entry.size != null && (
                        <span className="text-[10px] text-zinc-400 shrink-0">
                            {entry.size < 1024 ? `${entry.size} B`
                                : entry.size < 1024 * 1024 ? `${(entry.size / 1024).toFixed(1)} KB`
                                    : `${(entry.size / (1024 * 1024)).toFixed(1)} MB`}
                        </span>
                    )}
                </div>
                {/* Expanded children */}
                {isExpanded && expandedEntries.has(entry.path) && (
                    expandedEntries.get(entry.path)?.map((child) => renderEntry(child, depth + 1))
                )}
            </div>
        );
    };

    return (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
            <div className="w-full max-w-lg rounded-lg bg-white shadow-xl dark:bg-zinc-900 flex flex-col max-h-[80vh]">
                {/* Header */}
                <div className="flex items-center justify-between border-b border-zinc-200 px-4 py-3 dark:border-zinc-700">
                    <h3 className="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
                        {t("workspace.remoteBrowseTitle")}
                    </h3>
                    <button onClick={onCancel} className="rounded-md p-1 text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800">
                        ✕
                    </button>
                </div>

                {/* Breadcrumb navigation */}
                {breadcrumbs.length > 0 && (
                    <div className="flex items-center gap-1 px-4 py-2 border-b border-zinc-100 dark:border-zinc-800 overflow-x-auto text-[10px]">
                        <button
                            onClick={() => void navigateTo("")}
                            className="flex items-center gap-0.5 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
                        >
                            <HardDrive className="h-3 w-3" />
                        </button>
                        {breadcrumbs.map((crumb, i) => (
                            <span key={crumb} className="flex items-center gap-1">
                                <span className="text-zinc-400">/</span>
                                <button
                                    onClick={() => void navigateTo(crumb)}
                                    className={cn(
                                        "truncate hover:text-zinc-600 dark:hover:text-zinc-300",
                                        i === breadcrumbs.length - 1 ? "text-zinc-700 dark:text-zinc-200 font-medium" : "text-zinc-400",
                                    )}
                                >
                                    {crumb.split("/").filter(Boolean).pop() || crumb}
                                </button>
                            </span>
                        ))}
                    </div>
                )}

                {/* Directory tree */}
                <div className="flex-1 overflow-y-auto min-h-0 py-1">
                    {loading ? (
                        <div className="flex items-center justify-center py-8 text-xs text-zinc-400">
                            {t("workspace.explorer.loading")}
                        </div>
                    ) : error ? (
                        <div className="flex flex-col items-center justify-center py-8 text-xs text-zinc-400">
                            <span className="text-red-500 mb-2">{error}</span>
                            <button
                                onClick={() => void fetchEntries(currentPath)}
                                className="rounded-md px-3 py-1 text-xs bg-zinc-100 hover:bg-zinc-200 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                            >
                                {t("common.retry")}
                            </button>
                        </div>
                    ) : entries.length === 0 ? (
                        <div className="flex items-center justify-center py-8 text-xs text-zinc-400">
                            {t("workspace.remoteBrowseEmpty")}
                        </div>
                    ) : (
                        entries.map((entry) => renderEntry(entry))
                    )}
                </div>

                {/* Selected path display + action buttons */}
                <div className="border-t border-zinc-200 dark:border-zinc-700 px-4 py-3">
                    {selectedPath && (
                        <div className="mb-2 text-xs text-zinc-500 dark:text-zinc-400 truncate">
                            {t("workspace.remoteBrowseSelected")}: <span className="font-mono text-zinc-700 dark:text-zinc-300">{selectedPath}</span>
                        </div>
                    )}
                    <div className="flex items-center justify-end gap-2">
                        <button
                            onClick={onCancel}
                            className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
                        >
                            {t("common.cancel")}
                        </button>
                        <button
                            onClick={handleConfirm}
                            disabled={!selectedPath}
                            className="rounded-md px-3 py-1.5 text-xs font-medium text-white disabled:opacity-50 disabled:cursor-not-allowed"
                            style={{ backgroundColor: "var(--color-accent)" }}
                        >
                            {t("workspace.remoteBrowseSelect")}
                        </button>
                    </div>
                </div>
            </div>
        </div>
    );
}
