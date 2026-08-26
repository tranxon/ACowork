import { create } from "zustand";
import { useSettingsStore } from "./settingsStore";
import { DEFAULT_GATEWAY_URL } from "../lib/config";
import { log } from "../lib/logger";

/**
 * Check if a string looks like a valid HTTP/HTTPS URL.
 * Returns the normalized URL (lowercased scheme) or null if not a valid http(s) URL.
 */
function normalizeUrl(candidate: string): string | null {
    if (!candidate) return null;
    const trimmed = candidate.trim();
    try {
        const url = new URL(trimmed);
        if (url.protocol === "http:" || url.protocol === "https:") {
            return url.href;
        }
        return null;
    } catch {
        return null;
    }
}

/** Extension → Monaco language ID mapping */
const EXT_LANGUAGE_MAP: Record<string, string> = {
    rs: "rust",
    ts: "typescript",
    tsx: "typescript",
    js: "javascript",
    jsx: "javascript",
    json: "json",
    // Monaco ships no built-in TOML grammar; TOML syntax is close enough to
    // INI (comments, [sections], key = value) that we reuse "ini" for
    // highlighting. Must stay in lockstep with AgentSetupTab's
    // "TOML is mapped to ini language" when it opens shell_risk_rules.toml.
    toml: "ini",
    yaml: "yaml",
    yml: "yaml",
    md: "markdown",
    html: "html",
    htm: "html",
    css: "css",
    scss: "scss",
    less: "less",
    xml: "xml",
    sh: "shell",
    bash: "shell",
    zsh: "shell",
    ps1: "powershell",
    psm1: "powershell",
    psd1: "powershell",
    bat: "bat",
    cmd: "bat",
    py: "python",
    rb: "ruby",
    go: "go",
    java: "java",
    c: "c",
    h: "c",
    cpp: "cpp",
    cc: "cpp",
    cxx: "cpp",
    hpp: "cpp",
    cs: "csharp",
    swift: "swift",
    kt: "kotlin",
    kts: "kotlin",
    sql: "sql",
    graphql: "graphql",
    gql: "graphql",
    dockerfile: "dockerfile",
    ini: "ini",
    cfg: "ini",
    conf: "ini",
};

/** A file opened in the editor */
export interface OpenFile {
    /** Unique ID: `${agentId}:${workspaceId}:${relPath}` for files, `${agentId}:url:${url}` for URLs */
    id: string;
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
    /** Current content (may differ from originalContent when dirty) */
    content: string;
    /** Original content loaded from disk */
    originalContent: string;
    loading: boolean;
    saving: boolean;
    /** Monaco language ID (e.g. "typescript", "rust") */
    language: string;
    /** Whether the file has unsaved changes */
    dirty: boolean;
    /** Set when the last save attempt failed. Cleared on next successful save or on content edit. */
    saveError?: string;
    /** If set, editor should reveal this line (1-based) after mount */
    cursorLine?: number;
    /** "edit" = Monaco editor; "preview" = read-only Markdown render. */
    mode: "edit" | "preview";
    /** "file" = workspace file; "url" = external URL loaded in an iframe */
    kind: "file" | "url";
    /** The URL to load (only for kind === "url") */
    url?: string;
    /** MIME type from Gateway response (e.g. "image/png", "text/html") */
    mimeType?: string;
    // ── ADR-058: external-modification conflict tracking ──
    /** Disk mtime (RFC3339 string) as of the last read/save — used to
     *  distinguish real external writes from touch/chmod noise. */
    diskModified?: string;
    /** Disk size (bytes) as of the last read/save — pairs with diskModified. */
    diskSize?: number;
    /** Save-success moment (Desktop local clock, epoch ms). Used to
     *  suppress the echo of our own write coming back via MQTT. */
    lastSavedAtMs?: number;
    /** Set when the file was deleted on disk while the tab is dirty. */
    diskDeleted?: boolean;
    /** External-change conflict state ("modified" | "deleted"). */
    diskConflict?: "modified" | "deleted";
}

interface FileEditorState {
    openFiles: OpenFile[];
    activeFileId: string | null;

    /** Open a file in edit mode (or activate if already open). Fetches content from Gateway.
     * @param line - Optional 1-based line number to reveal after opening */
    openFile: (agentId: string, workspaceId: string, relPath: string, line?: number) => Promise<void>;
    /** Open a file in read-only preview mode. Re-fetches content from disk.
     *  - If the file is already open in preview mode, just activates it.
     *  - If the file is open in edit mode, switches that tab to preview mode. */
    openPreview: (agentId: string, workspaceId: string, relPath: string) => Promise<void>;
    /** Open a file with pre-loaded content (skips Gateway fetch). Used by LSP cross-file navigation. */
    openFileWithContent: (agentId: string, workspaceId: string, relPath: string, content: string, language: string) => void;
    /** Open a URL in a new tab (rendered in an iframe). Does nothing if the URL is invalid. */
    openUrl: (agentId: string, url: string) => void;
    /** Close a file tab. Returns false if dirty (caller should confirm first). */
    closeFile: (fileId: string, force?: boolean) => boolean;
    /** Close all tabs except the one with `keepFileId`.
     *  Returns `false` if any non-kept file is dirty and `force` is not set. */
    closeOthers: (keepFileId: string, force?: boolean) => boolean;
    /** Set the active (focused) file */
    setActiveFile: (fileId: string) => void;
    /** Update file content (marks as dirty) */
    updateContent: (fileId: string, content: string) => void;
    /** Save file content to Gateway */
    saveFile: (fileId: string) => Promise<void>;
    /** Re-fetch file content from disk and replace both content and originalContent.
     *  Resets dirty=false. URL-preview tabs (kind === "url") are skipped.
     *  No-op while a refresh is already in flight for this file.
     *  `opts.skipIfDirty` (ADR-058 review M-3): skip instead of overwrite
     *  when the tab turned dirty while the fetch was in flight — used by
     *  fs-event-triggered silent reloads so user input is never clobbered. */
    refreshFile: (fileId: string, opts?: { skipIfDirty?: boolean }) => Promise<void>;
    /** ADR-058: clear the external-change conflict markers on a tab
     *  (the "Keep mine" answer to a disk-change toast). Purely local —
     *  the disk state itself is untouched. */
    clearDiskConflict: (fileId: string) => void;
    /** Close all open files. If `force` is false and any file is dirty,
     *  no files are closed and the function returns `false`. */
    closeAllFiles: (force?: boolean) => boolean;
}

function getGatewayUrl(): string {
    return useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
}

/**
 * Build the Gateway endpoint URL for a workspace file (read/write).
 * Centralised so all callers (openFile, openPreview, saveFile, refreshFile)
 * stay in sync on workspace_id handling and the `path` query param.
 */
function buildFileUrl(agentId: string, workspaceId: string, relPath: string): string {
    const baseUrl = getGatewayUrl();
    const params = new URLSearchParams();
    if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
    }
    params.set("path", relPath);
    return `${baseUrl}/api/agents/${agentId}/workspaces/file?${params.toString()}`;
}

/**
 * The shell risk rules file lives at `<work_dir>/config/shell_risk_rules.toml`,
 * which is NOT inside any workspace — so the generic workspace file API
 * (`/workspaces/file`) cannot write to it. The Runtime exposes a dedicated
 * endpoint `/agents/{id}/shell-risk-rules` that handles persistence and
 * live-reload of the in-memory rule cache.
 *
 * Detect this special file by its path so saveFile routes to the right URL.
 */
const SHELL_RISK_RULES_PATH = "config/shell_risk_rules.toml";
function isShellRiskRulesFile(workspaceId: string, relPath: string): boolean {
    return workspaceId === "__agent_home__" && relPath === SHELL_RISK_RULES_PATH;
}
function buildShellRiskRulesUrl(agentId: string): string {
    return `${getGatewayUrl()}/api/agents/${agentId}/shell-risk-rules`;
}

function detectLanguage(fileName: string): string {
    const ext = fileName.split(".").pop()?.toLowerCase() || "";
    // Handle special filenames without extension
    const baseName = fileName.toLowerCase();
    if (baseName === "dockerfile") return "dockerfile";
    if (baseName === "makefile") return "makefile";
    if (baseName === ".gitignore" || baseName === ".editorconfig") return "ini";
    return EXT_LANGUAGE_MAP[ext] || "plaintext";
}

export const useFileEditorStore = create<FileEditorState>((set, get) => ({
    openFiles: [],
    activeFileId: null,

    openFile: async (agentId: string, workspaceId: string, relPath: string, line?: number) => {
        const fileId = `${agentId}:${workspaceId}:${relPath}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open — activate, switch to edit mode if it was preview,
            // and jump to line if specified
            set({
                activeFileId: fileId,
                openFiles: get().openFiles.map((f) =>
                    f.id === fileId
                        ? {
                            ...f,
                            mode: "edit",
                            ...(line !== undefined ? { cursorLine: line } : {}),
                        }
                        : f,
                ),
            });
            return;
        }

        const fileName = relPath.split("/").pop() || relPath;
        const language = detectLanguage(fileName);

        // Add placeholder file
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath,
            fileName,
            content: "",
            originalContent: "",
            loading: true,
            saving: false,
            language,
            dirty: false,
            mode: "edit",
            ...(line !== undefined ? { cursorLine: line } : {}),
            kind: "file",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));

        // Fetch content from Gateway
        try {
            const url = buildFileUrl(agentId, workspaceId, relPath);
            const resp = await fetch(url);
            if (!resp.ok) {
                log.error("[FileEditorStore] read_file failed:", resp.status);
                // Remove the file on error
                set((state) => ({
                    openFiles: state.openFiles.filter((f) => f.id !== fileId),
                    activeFileId: state.activeFileId === fileId ? null : state.activeFileId,
                }));
                return;
            }
            const data = (await resp.json()) as { content: string; size: number; mimeType: string; modified?: string };
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId
                        ? { ...f, content: data.content, originalContent: data.content, loading: false, mimeType: data.mimeType,
                            // ADR-058: cache disk metadata for conflict checks
                            diskModified: data.modified, diskSize: data.size }
                        : f,
                ),
            }));
        } catch (e) {
            log.error("[FileEditorStore] openFile error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, loading: false } : f,
                ),
            }));
        }
    },

    openPreview: async (agentId: string, workspaceId: string, relPath: string) => {
        const fileId = `${agentId}:${workspaceId}:${relPath}`;
        const fileName = relPath.split("/").pop() || relPath;

        // If already open — just activate and switch to preview mode
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            set({
                activeFileId: fileId,
                openFiles: get().openFiles.map((f) =>
                    f.id === fileId ? { ...f, mode: "preview" } : f,
                ),
            });
            return;
        }

        // Add a preview-mode placeholder
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath,
            fileName,
            content: "",
            originalContent: "",
            loading: true,
            saving: false,
            language: "markdown",
            dirty: false,
            mode: "preview",
            kind: "file",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));

        // Fetch content from Gateway
        try {
            const url = buildFileUrl(agentId, workspaceId, relPath);
            const resp = await fetch(url);
            if (!resp.ok) {
                log.error("[FileEditorStore] openPreview read failed:", resp.status);
                set((state) => ({
                    openFiles: state.openFiles.filter((f) => f.id !== fileId),
                    activeFileId: state.activeFileId === fileId ? null : state.activeFileId,
                }));
                return;
            }
            const data = (await resp.json()) as { content: string; size: number; mimeType: string; modified?: string };
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId
                        ? { ...f, content: data.content, originalContent: data.content, loading: false, mimeType: data.mimeType,
                            // ADR-058: cache disk metadata for conflict checks
                            diskModified: data.modified, diskSize: data.size }
                        : f,
                ),
            }));
        } catch (e) {
            log.error("[FileEditorStore] openPreview error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, loading: false } : f,
                ),
            }));
        }
    },

    openFileWithContent: (agentId: string, workspaceId: string, relPath: string, content: string, language: string) => {
        const fileId = `${agentId}:${workspaceId}:${relPath}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open, just activate
            set({ activeFileId: fileId });
            return;
        }

        const fileName = relPath.split("/").pop() || relPath;
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath,
            fileName,
            content,
            originalContent: content,
            loading: false, // Already have content, no need to fetch
            saving: false,
            language,
            dirty: false,
            mode: "edit",
            kind: "file",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));
    },

    closeFile: (fileId: string, force?: boolean) => {
        const file = get().openFiles.find((f) => f.id === fileId);
        if (!file) return true;
        if (file.dirty && !force) return false;

        set((state) => {
            const nextFiles = state.openFiles.filter((f) => f.id !== fileId);
            let nextActive = state.activeFileId;
            if (state.activeFileId === fileId) {
                // Activate adjacent tab or null
                const idx = state.openFiles.findIndex((f) => f.id === fileId);
                nextActive = nextFiles.length > 0
                    ? nextFiles[Math.min(idx, nextFiles.length - 1)].id
                    : null;
            }
            return { openFiles: nextFiles, activeFileId: nextActive };
        });
        return true;
    },

    closeOthers: (keepFileId: string, force?: boolean) => {
        const state = get();
        if (!state.openFiles.some((f) => f.id === keepFileId)) return true;
        if (!force) {
            const hasDirty = state.openFiles.some(
                (f) => f.id !== keepFileId && f.dirty,
            );
            if (hasDirty) return false;
        }
        set({
            openFiles: state.openFiles.filter((f) => f.id === keepFileId),
            // Keep the requested tab active; if it wasn't the active one,
            // promote it so the surviving single tab is clearly focused.
            activeFileId: keepFileId,
        });
        return true;
    },

    setActiveFile: (fileId: string) => {
        set({ activeFileId: fileId });
    },

    updateContent: (fileId: string, content: string) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId
                    ? {
                        ...f,
                        content,
                        dirty: content !== f.originalContent,
                        // Clear stale save error — user is editing to recover.
                        saveError: undefined,
                    }
                    : f,
            ),
        }));
    },

    saveFile: async (fileId: string) => {
        const file = get().openFiles.find((f) => f.id === fileId);
        if (!file || file.saving) return;

        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId ? { ...f, saving: true, saveError: undefined } : f,
            ),
        }));

        try {
            // Route shell-risk-rules to the dedicated endpoint (the file
            // is not under any workspace, so /workspaces/file can't write it).
            const isRiskRules = isShellRiskRulesFile(file.workspaceId, file.relPath);
            const url = isRiskRules
                ? buildShellRiskRulesUrl(file.agentId)
                : buildFileUrl(file.agentId, file.workspaceId, file.relPath);
            const resp = await fetch(url, {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ content: file.content }),
            });
            if (!resp.ok) {
                // Surface the error body so the editor toast can show a
                // useful message (e.g. TOML parse error from Runtime).
                const errText = await resp.text().catch(() => "");
                const saveError = errText || `HTTP ${resp.status}`;
                log.error("[FileEditorStore] saveFile failed:", resp.status, errText);
                set((state) => ({
                    openFiles: state.openFiles.map((f) =>
                        f.id === fileId ? { ...f, saving: false, saveError } : f,
                    ),
                }));
                return;
            }
            // ADR-058: parse the post-write disk metadata echoed by the
            // Runtime (modified RFC3339 + size) and stamp lastSavedAtMs
            // (Desktop local clock) for echo suppression in the
            // workspace fs-changed listener.
            const saved = (await resp.json().catch(() => ({}))) as {
                modified?: string;
                size?: number;
            };
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId
                        ? {
                            ...f,
                            saving: false,
                            originalContent: f.content,
                            dirty: false,
                            saveError: undefined,
                            ...(saved.modified !== undefined ? { diskModified: saved.modified } : {}),
                            ...(saved.size !== undefined ? { diskSize: saved.size } : {}),
                            lastSavedAtMs: Date.now(),
                            diskConflict: undefined,
                            diskDeleted: undefined,
                        }
                        : f,
                ),
            }));
        } catch (e) {
            log.error("[FileEditorStore] saveFile error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, saving: false, saveError: String(e) } : f,
                ),
            }));
        }
    },

    refreshFile: async (fileId: string, opts?: { skipIfDirty?: boolean }) => {
        const file = get().openFiles.find((f) => f.id === fileId);
        if (!file) return;
        // Only workspace files have a Gateway file endpoint to refresh against.
        if (file.kind !== "file") return;
        // Skip if a load is already running (avoids interleaved responses
        // clobbering the in-flight state).
        if (file.loading) return;

        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId ? { ...f, loading: true } : f,
            ),
        }));

        try {
            const url = buildFileUrl(file.agentId, file.workspaceId, file.relPath);
            const resp = await fetch(url);
            if (!resp.ok) {
                log.error("[FileEditorStore] refreshFile failed:", resp.status);
                set((state) => ({
                    openFiles: state.openFiles.map((f) =>
                        f.id === fileId ? { ...f, loading: false } : f,
                    ),
                }));
                return;
            }
            const data = (await resp.json()) as { content: string; size: number; mimeType: string; modified?: string };
            set((state) => ({
                openFiles: state.openFiles.map((f) => {
                    if (f.id !== fileId) return f;
                    // ADR-058 (review M-3): a reload triggered by an fs
                    // event must never clobber edits the user started
                    // while the fetch was in flight — skip instead of
                    // overwriting when the tab turned dirty meanwhile.
                    if (opts?.skipIfDirty && f.dirty) return { ...f, loading: false };
                    return {
                        ...f,
                        content: data.content,
                        originalContent: data.content,
                        mimeType: data.mimeType,
                        loading: false,
                        // Resetting both content and originalContent means dirty must be false.
                        dirty: false,
                        // ADR-058: refresh resolves any pending disk
                        // conflict (the reload is now authoritative)
                        // and re-caches disk metadata.
                        diskModified: data.modified,
                        diskSize: data.size,
                        diskConflict: undefined,
                        diskDeleted: undefined,
                    };
                }),
            }));
        } catch (e) {
            log.error("[FileEditorStore] refreshFile error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, loading: false } : f,
                ),
            }));
        }
    },

    clearDiskConflict: (fileId: string) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId ? { ...f, diskConflict: undefined, diskDeleted: undefined } : f,
            ),
        }));
    },

    closeAllFiles: (force?: boolean) => {
        const state = get();
        if (!force && state.openFiles.some((f) => f.dirty)) return false;
        set({ openFiles: [], activeFileId: null });
        return true;
    },

    openUrl: (agentId: string, url: string) => {
        const normalized = normalizeUrl(url);
        if (!normalized) {
            log.warn("[FileEditorStore] openUrl — skipping invalid URL:", url);
            return;
        }

        // Use the URL itself as a unique file ID (scoped to agent)
        const fileId = `${agentId}:url:${normalized}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open — just activate it
            set({ activeFileId: fileId });
            return;
        }

        const fileName = new URL(normalized).hostname; // e.g. "example.com"
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId: "",
            relPath: normalized,
            fileName,
            content: "",
            originalContent: "",
            loading: false,
            saving: false,
            language: "plaintext",
            dirty: false,
            mode: "preview",
            kind: "url",
            url: normalized,
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));
    },
}));
