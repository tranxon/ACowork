import { useState, useRef, useEffect, useCallback, useMemo } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "../../i18n/useTranslation";
import { useFileEditorStore, type OpenFile } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useLspClientPool, type LspStatus } from "../../hooks/useLspClientPool";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { X, Save, Loader2, FileText, CircleDot, Circle, Copy, Check, MessageSquarePlus, Play, AlertTriangle, Eye } from "lucide-react";
import Editor, { type OnMount } from "@monaco-editor/react";
import { ScrollableTabBar } from "../common/ScrollableTabBar";
import { TabItem } from "../common/tab";
import { registerLspProviders, disposeModelForFile, unpinPreviewModel } from "./lspProviders";
import { LspDocumentTracker } from "./LspDocumentTracker";
import { MarkdownPreviewView } from "./MarkdownPreviewView";
import type { IDisposable } from "monaco-editor";
import { GoToFilePalette } from "./GoToFilePalette";
import { GlobalSearchPanel } from "./GlobalSearchPanel";
import { Tooltip } from "../common/Tooltip";

// ── LSP Install Hints ─────────────────────────────────────────────────

const LSP_INSTALL_HINTS: Record<string, { name: string; command: string; url?: string }> = {
    rust: {
        name: "rust-analyzer",
        command: "rustup component add rust-analyzer",
        url: "https://rust-analyzer.github.io/",
    },
    python: {
        name: "python-lsp-server",
        command: "pip install python-lsp-server",
        url: "https://github.com/python-lsp/python-lsp-server",
    },
    typescript: {
        name: "typescript-language-server",
        command: "npm install -g typescript-language-server typescript",
        url: "https://github.com/typescript-language-server/typescript-language-server",
    },
    javascript: {
        name: "typescript-language-server",
        command: "npm install -g typescript-language-server typescript",
    },
    go: {
        name: "gopls",
        command: "go install golang.org/x/tools/gopls@latest",
        url: "https://pkg.go.dev/golang.org/x/tools/gopls",
    },
    cpp: {
        name: "clangd",
        command: "Windows: winget install LLVM.LLVM | Linux: apt install clangd | macOS: brew install llvm",
        url: "https://clangd.llvm.org/",
    },
    c: {
        name: "clangd",
        command: "Windows: winget install LLVM.LLVM | Linux: apt install clangd | macOS: brew install llvm",
    },
    java: {
        name: "jdtls (Eclipse JDT Language Server)",
        command: "Windows / Linux / macOS: Install VS Code Java Extension Pack, or download jdtls from https://download.eclipse.org/jdtls/",
        url: "https://github.com/eclipse-jdtls/eclipse.jdt.ls",
    },
    kotlin: {
        name: "kotlin-language-server",
        command: "Windows: Download from https://github.com/fwcd/kotlin-language-server/releases | Linux: Download from https://github.com/fwcd/kotlin-language-server/releases | macOS: brew install kotlin-language-server",
        url: "https://github.com/fwcd/kotlin-language-server",
    },
    swift: {
        name: "sourcekit-lsp",
        command: "Windows: Install Swift toolchain from https://swift.org/install | Linux: Included with Swift toolchain (https://swift.org/install) | macOS: Included with Xcode Command Line Tools",
        url: "https://github.com/swiftlang/sourcekit-lsp",
    },
    "objective-c": {
        name: "clangd / sourcekit-lsp",
        command: "Windows: winget install LLVM.LLVM | Linux: apt install clangd | macOS: Included with Xcode (sourcekit-lsp)",
        url: "https://clangd.llvm.org/",
    },
    dart: {
        name: "Dart Analysis Server",
        command: "Included with Dart SDK / Flutter SDK: dart pub global activate dart_language_server",
        url: "https://dart.dev/get-dart",
    },
    markdown: {
        name: "marksman",
        command: "Windows: winget install marksman | Linux: See https://github.com/artempyanykh/marksman | macOS: brew install marksman",
        url: "https://github.com/artempyanykh/marksman",
    },
    md: {
        name: "marksman",
        command: "Windows: winget install marksman | Linux: See https://github.com/artempyanykh/marksman | macOS: brew install marksman",
        url: "https://github.com/artempyanykh/marksman",
    },
    json: {
        name: "vscode-json-languageserver",
        command: "npm install -g vscode-json-languageserver",
    },
    yaml: {
        name: "yaml-language-server",
        command: "npm install -g yaml-language-server",
        url: "https://github.com/redhat-developer/yaml-language-server",
    },
    yml: {
        name: "yaml-language-server",
        command: "npm install -g yaml-language-server",
        url: "https://github.com/redhat-developer/yaml-language-server",
    },
    html: {
        name: "vscode-html-languageserver",
        command: "npm install -g vscode-html-languageserver",
    },
    css: {
        name: "vscode-css-languageserver",
        command: "npm install -g vscode-css-languageserver",
    },
    scss: {
        name: "vscode-css-languageserver",
        command: "npm install -g vscode-css-languageserver",
    },
    less: {
        name: "vscode-css-languageserver",
        command: "npm install -g vscode-css-languageserver",
    },
};

// ── LSP Status Indicator ──────────────────────────────────────────────

function LspIndicator({ status, statusMessage, language }: { status: LspStatus; statusMessage: string; language: string }) {
    const { t } = useTranslation();
    const [showPopover, setShowPopover] = useState(false);
    const [copied, setCopied] = useState(false);
    const [installing, setInstalling] = useState(false);
    const [installResult, setInstallResult] = useState<{ success: boolean; text: string } | null>(null);
    const popoverRef = useRef<HTMLDivElement>(null);

    const isUnavailable = status === "disconnected" || status === "error";
    const hint = LSP_INSTALL_HINTS[language];

    // Close popover on outside click or Escape
    useEffect(() => {
        if (!showPopover) return;

        const handleClickOutside = (e: MouseEvent) => {
            if (popoverRef.current && !popoverRef.current.contains(e.target as Node)) {
                setShowPopover(false);
            }
        };
        const handleEscape = (e: KeyboardEvent) => {
            if (e.key === "Escape") setShowPopover(false);
        };

        document.addEventListener("mousedown", handleClickOutside);
        document.addEventListener("keydown", handleEscape);
        return () => {
            document.removeEventListener("mousedown", handleClickOutside);
            document.removeEventListener("keydown", handleEscape);
        };
    }, [showPopover]);

    const handleClick = () => {
        if (isUnavailable && hint) {
            setShowPopover((v) => !v);
        }
    };

    const copyToClipboard = () => {
        if (!hint) return;
        void navigator.clipboard.writeText(hint.command).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        });
    };

    const runInstall = async () => {
        if (!language || installing) return;
        setInstalling(true);
        setInstallResult(null);
        try {
            const gatewayUrl = getGatewayUrl();
            const resp = await fetch(`${gatewayUrl}/api/lsp/install/${encodeURIComponent(language)}`, {
                method: "POST",
            });
            const data = await resp.json();
            if (data.success) {
                setInstallResult({
                    success: true,
                    text: data.stdout || "Installation completed. Restart Gateway to apply.",
                });
            } else {
                // Show stderr first, then stdout, then error field, then fallback
                const detail = data.stderr || data.stdout || data.error || `Install failed (exit code: ${data.exit_code})`;
                setInstallResult({
                    success: false,
                    text: detail,
                });
            }
        } catch (err: any) {
            setInstallResult({
                success: false,
                text: err?.message || "Failed to run install script",
            });
        } finally {
            setInstalling(false);
        }
    };

    // Render the status text
    let content: React.ReactNode;
    if (status === "disconnected") {
        content = (
            <span className="flex items-center gap-1 text-[10px] text-zinc-400 dark:text-zinc-500">
                <Circle className="h-2 w-2" />
                <span>{language} LSP unavailable</span>
            </span>
        );
    } else if (status === "connecting") {
        content = (
            <span className="flex items-center gap-1 text-[10px] text-zinc-400">
                <Circle className="h-2 w-2 animate-pulse" />
                <span>{language} LSP connecting...</span>
            </span>
        );
    } else if (status === "indexing") {
        content = (
            <span className="flex items-center gap-1 text-[10px] text-amber-500 dark:text-amber-400">
                <Circle className="h-2 w-2 animate-pulse" />
                <span>{statusMessage ? `${language} ${statusMessage}` : `${language} LSP indexing...`}</span>
            </span>
        );
    } else if (status === "connected") {
        // Handshake done, but indexing has not started/finished yet —
        // hover/definition results may be incomplete.
        content = (
            <span className="flex items-center gap-1 text-[10px] text-emerald-500/70 dark:text-emerald-400/70">
                <Circle className="h-2 w-2" />
                <span>{language} LSP connected</span>
            </span>
        );
    } else if (status === "ready") {
        content = (
            <span className="flex items-center gap-1 text-[10px] text-emerald-600 dark:text-emerald-400">
                <CircleDot className="h-2 w-2" />
                <span>{language} LSP ready</span>
            </span>
        );
    } else {
        // error
        const tooltip = statusMessage || "unknown error";
        content = (
            <Tooltip content={tooltip} variant="plain">
                <span className="flex items-center gap-1 text-[10px] text-amber-500">
                    <Circle className="h-2 w-2" />
                    <span>{language} LSP unavailable</span>
                </span>
            </Tooltip>
        );
    }

    return (
        <div className="relative" ref={popoverRef}>
            <button
                type="button"
                onClick={handleClick}
                className={cn(
                    "flex items-center",
                    isUnavailable && hint ? "cursor-pointer hover:opacity-80" : "cursor-default",
                )}
            >
                {content}
            </button>

            {/* Install hint popover */}
            {showPopover && hint && (
                <div className="absolute bottom-full left-0 z-50 mb-1 w-72 rounded-md border border-zinc-200 bg-white p-3 shadow-lg dark:border-zinc-700 dark:bg-zinc-800 text-xs">
                    <div className="font-medium text-zinc-700 dark:text-zinc-200 mb-1.5">
                        Install {hint.name}
                    </div>
                    <div className="flex items-center gap-1.5 rounded bg-zinc-100 dark:bg-zinc-900 px-2 py-1.5 font-mono text-[11px]">
                        <span className="flex-1 select-all break-all text-zinc-700 dark:text-zinc-300">
                            {hint.command}
                        </span>
                        <Tooltip content={t("fileEditor.copy")} variant="plain">
                            <button
                                type="button"
                                onClick={copyToClipboard}
                                className="shrink-0 rounded p-0.5 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-200 transition-colors"
                            >
                                {copied ? <Check className="h-3 w-3 text-emerald-500" /> : <Copy className="h-3 w-3" />}
                            </button>
                        </Tooltip>
                    </div>

                    {/* Install button */}
                    <button
                        type="button"
                        onClick={runInstall}
                        disabled={installing}
                        className={cn(
                            "mt-2 flex w-full items-center justify-center gap-1.5 rounded px-3 py-1.5 text-[11px] font-medium transition-colors",
                            installing
                                ? "bg-zinc-200 text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400 cursor-not-allowed"
                                : "bg-[var(--color-accent)] text-white hover:opacity-90",
                        )}
                    >
                        {installing ? (
                            <>
                                <Loader2 className="h-3 w-3 animate-spin" />
                                Installing...
                            </>
                        ) : (
                            <>
                                <Play className="h-3 w-3" />
                                Run Install Script
                            </>
                        )}
                    </button>

                    {/* Install result */}
                    {installResult && (
                        <div
                            className={cn(
                                "mt-2 rounded px-2 py-1.5 text-[11px] leading-relaxed",
                                installResult.success
                                    ? "bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400"
                                    : "bg-amber-50 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400",
                            )}
                        >
                            <div className="flex items-start gap-1">
                                {installResult.success ? (
                                    <Check className="h-3 w-3 mt-0.5 shrink-0" />
                                ) : (
                                    <AlertTriangle className="h-3 w-3 mt-0.5 shrink-0" />
                                )}
                                <span className="whitespace-pre-wrap break-all">{installResult.text}</span>
                            </div>
                        </div>
                    )}

                    {hint.url && (
                        <a
                            href={hint.url}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="mt-2 inline-block text-[var(--color-accent)] hover:underline text-[11px]"
                        >
                            Documentation →
                        </a>
                    )}
                </div>
            )}
        </div>
    );
}

export function FileEditorPanel({ width }: { width: number }) {
    const { t } = useTranslation();
    const openFiles = useFileEditorStore((s) => s.openFiles);
    const activeFileId = useFileEditorStore((s) => s.activeFileId);
    const setActiveFile = useFileEditorStore((s) => s.setActiveFile);
    const updateContent = useFileEditorStore((s) => s.updateContent);
    const saveFile = useFileEditorStore((s) => s.saveFile);
    const closeFile = useFileEditorStore((s) => s.closeFile);
    const closeOthers = useFileEditorStore((s) => s.closeOthers);
    const closeAllFiles = useFileEditorStore((s) => s.closeAllFiles);
    const addAttachedContext = useChatStore((s) => s.addAttachedContext);
    const getActiveSessionId = useChatStore((s) => s.getActiveSessionId);
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

    const theme = useSettingsStore((s) => s.theme);
    const fontSize = useSettingsStore((s) => s.fontSize);
    const [closingFileId, setClosingFileId] = useState<string | null>(null);
    // Tab right-click context menu (fileId, viewport position)
    const [tabContextMenu, setTabContextMenu] = useState<
        { fileId: string; x: number; y: number } | null
    >(null);
    const tabMenuRef = useRef<HTMLDivElement>(null);
    // Batch close confirmation (Close Others / Close All with dirty files)
    const [batchCloseRequest, setBatchCloseRequest] = useState<
        { kind: "others" | "all"; fileIds: string[]; dirtyCount: number; keepFileId?: string } | null
    >(null);
    const [showGoToFile, setShowGoToFile] = useState(false);
    const [showGlobalSearch, setShowGlobalSearch] = useState(false);
    const editorRef = useRef<Parameters<OnMount>[0] | null>(null);
    const monacoRef = useRef<typeof import("monaco-editor") | null>(null);
    const [cursor, setCursor] = useState({ line: 1, column: 1 });
    const [selectedCount, setSelectedCount] = useState(0);
    // Selection range for "Add to Chat" floating button
    const [selectionRange, setSelectionRange] = useState<{ startLine: number; endLine: number } | null>(null);
    const [addToChatPos, setAddToChatPos] = useState<{ top: number; left: number } | null>(null);
    const lspProvidersRef = useRef<IDisposable | null>(null);
    const documentTrackerRef = useRef<LspDocumentTracker | null>(null);
    // Track the previous lspClient to detect reconnections
    const prevLspClientRef = useRef<typeof lspClient>(null);
    // When Monaco's peek widget navigates to a different file, we store the
    // target position here and apply it inside onDidChangeModel — the single
    // authoritative entry point for cross-file navigation. The Editor is no
    // longer keyed on activeFile.id, so model switching is synchronous and
    // we no longer need a separate useEffect-based fallback.
    const pendingNavigationRef = useRef<{ line: number; column: number; endLineNumber?: number; endColumn?: number } | null>(null);
    // Per-model view state cache (cursor / scroll / selection / folding).
    // Without per-file Editor remounts, undo history and scroll position would
    // bleed across files; we save/restore view state on model boundaries.
    const viewStatesRef = useRef<Map<string, unknown>>(new Map());
    // Guard to prevent overriding ICodeEditorService.openCodeEditor more than once
    // (it's a shared singleton service, not per-editor).
    const codeEditorOverriddenRef = useRef(false);

    const activeFile = openFiles.find((f) => f.id === activeFileId) ?? null;

    // Resolve workspace root for LSP URI mapping.
    // Monaco's Uri.parse() cannot handle Windows file URIs (file:///C:/...),
    // so we use relPath as the model path (producing file:///core/... which
    // Monaco accepts). The LSP layer then maps relative URIs to absolute ones
    // using the workspace root. See lspProviders.ts → toLspUri().
    const treeRoots = useWorkspaceStore((s) => s.treeRoots);
    const workspaceRoot = useMemo(() => {
        if (!activeFile) return undefined;
        const rootKey = `${activeFile.agentId}:${activeFile.workspaceId}`;
        return treeRoots[rootKey];
    }, [activeFile, treeRoots]);

    // Determine the active language for LSP — preview-mode files don't need LSP.
    const lspLanguage = activeFile && activeFile.mode === "edit" ? activeFile.language : null;

    // Compute the set of all languages open in EDIT tabs (for pool lifecycle).
    // Preview-mode tabs are excluded — they are read-only and don't need LSP.
    const openLanguages = useMemo(() => {
        const langs = new Set<string>();
        for (const file of openFiles) {
            if (file.mode === "edit" && file.language && !file.loading) langs.add(file.language);
        }
        return langs;
    }, [openFiles]);

    // LSP pool is enabled when there is at least one open language
    const lspEnabled = openLanguages.size > 0;

    // LSP client pool — maintains connections for all open languages,
    // disconnects only after a 30s grace period once a language's last file closes.
    const { activeStatus: lspStatus, activeStatusMessage: lspStatusMessage, activeClient: lspClient } = useLspClientPool(
        lspLanguage,
        openLanguages,
        activeFile?.agentId,
        activeFile?.workspaceId,
        lspEnabled,
        workspaceRoot
    );

    // Diagnostic logging — only when key inputs change, not on every render.
    useEffect(() => {
        console.log(
            "[LSP] FileEditorPanel — lspLanguage:", lspLanguage,
            "status:", lspStatus,
            "lspEnabled:", lspEnabled,
        );
    }, [lspLanguage, lspStatus, lspEnabled]);

    // Jump to cursorLine when search result navigates to a file.
    // Must handle two cases:
    //   1. File already active → same model, navigate directly.
    //   2. New file (loading) → model not yet switched; defer via
    //      pendingNavigationRef so onDidChangeModel applies it.
    useEffect(() => {
        const line = activeFile?.cursorLine;
        if (!line) return;

        const ed = editorRef.current;
        let sameModel = false;
        if (ed) {
            const model = ed.getModel();
            if (model) {
                const modelPath = model.uri.path.replace(/^\/+/, "");
                sameModel = modelPath === activeFile!.relPath;
            }
        }

        if (sameModel) {
            // Editor already has the right model — navigate immediately.
            ed!.revealLineInCenter(line);
            ed!.setPosition({ lineNumber: line, column: 1 });
        } else {
            // Model hasn't switched yet (or editor not mounted) —
            // defer to onDidChangeModel for correct model.
            pendingNavigationRef.current = { line, column: 1 };
        }

        // Clear cursorLine so re-renders don't re-jump
        useFileEditorStore.setState((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === activeFile!.id ? { ...f, cursorLine: undefined } : f,
            ),
        }));
    }, [activeFile?.id, activeFile?.cursorLine]);

    // Determine Monaco theme based on app theme
    const monacoTheme = useMemo(() => {
        if (theme === "dark") return "vs-dark";
        if (theme === "light") return "vs";
        // system: check DOM
        return document.documentElement.classList.contains("dark") ? "vs-dark" : "vs";
    }, [theme]);

    // System theme change listener
    const [systemDark, setSystemDark] = useState(() =>
        document.documentElement.classList.contains("dark")
    );
    useEffect(() => {
        if (theme !== "system") return;
        const mq = window.matchMedia("(prefers-color-scheme: dark)");
        const handler = () => setSystemDark(mq.matches);
        mq.addEventListener("change", handler);
        return () => mq.removeEventListener("change", handler);
    }, [theme]);

    const resolvedMonacoTheme = theme === "system"
        ? (systemDark ? "vs-dark" : "vs")
        : monacoTheme;

    // Compute Monaco editor font size in pixels from global fontSize (rem-based).
    // Root font size is 16px by browser default, so fontSize rem * 16 = px.
    const editorFontSize = useMemo(() => Math.round(fontSize * 16), [fontSize]);

    const handleEditorMount: OnMount = useCallback((editor, monaco) => {
        editorRef.current = editor;
        monacoRef.current = monaco;
        // Track cursor position + selection + "Add to Chat" button position
        editor.onDidChangeCursorSelection((e) => {
            setCursor({ line: e.selection.positionLineNumber, column: e.selection.positionColumn });
            // Sync selection count and "Add to Chat" button
            const sel = e.selection;
            if (sel && !sel.isEmpty()) {
                const model = editor.getModel();
                if (model) {
                    setSelectedCount(model.getValueInRange(sel).length);
                    setSelectionRange({
                        startLine: sel.startLineNumber,
                        endLine: sel.endLineNumber,
                    });
                    // Position the floating button below selection end, clamped to container
                    requestAnimationFrame(() => {
                        const ed = editorRef.current;
                        if (!ed) return;
                        const endPos = ed.getScrolledVisiblePosition({
                            lineNumber: sel.endLineNumber,
                            column: sel.endColumn,
                        });
                        if (endPos) {
                            // Button is ~120px wide; clamp left so it stays inside the editor
                            const btnWidth = 120;
                            const containerWidth = ed.getLayoutInfo().width;
                            const left = Math.max(8, Math.min(endPos.left + 20, containerWidth - btnWidth));
                            setAddToChatPos({ top: endPos.top + 18, left });
                        }
                    });
                    return;
                }
            }
            setSelectedCount(0);
            setSelectionRange(null);
            setAddToChatPos(null);
        });

        // Handle model switches — the authoritative lifecycle hook for both
        // tab switches (driven by `path` prop change) and LSP peek-widget
        // cross-file navigation (driven by ICodeEditorService.openCodeEditor).
        // The Editor instance is no longer recreated per file, so this fires
        // synchronously when @monaco-editor/react calls editor.setModel().
        editor.onDidChangeModel(() => {
            const newModel = editor.getModel();
            if (!newModel) {
                console.log("[LSP] onDidChangeModel — model is null");
                return;
            }

            // Only process file:// URIs — ignore inmemory://, output://, etc.
            const scheme = newModel.uri.scheme;
            if (scheme !== 'file') {
                console.log("[LSP] onDidChangeModel — ignoring non-file model:", newModel.uri.toString());
                return;
            }

            // The model's URI path is the relative path (e.g. "core/runtime/src/foo.rs")
            const relPath = newModel.uri.path.replace(/^\/+/, "");
            console.log("[LSP] onDidChangeModel — new relPath:", relPath, "uri:", newModel.uri.toString());

            // Restore previously saved view state for this model unless a
            // pending navigation will override it below.
            if (!pendingNavigationRef.current) {
                const savedState = viewStatesRef.current.get(relPath);
                if (savedState) {
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                    editor.restoreViewState(savedState as any);
                }
            }

            // Apply pending cross-file navigation (takes priority over restored view state).
            // Deferred via requestAnimationFrame so that @monaco-editor/react's internal
            // viewState restoration (which runs AFTER onDidChangeModel) does not override
            // our navigation target.
            if (pendingNavigationRef.current) {
                const nav = pendingNavigationRef.current;
                pendingNavigationRef.current = null;
                requestAnimationFrame(() => {
                    const ed = editorRef.current;
                    if (!ed) return;
                    const currentModel = ed.getModel();
                    const lineCount = currentModel ? currentModel.getLineCount() : nav.line;
                    const safeLine = Math.min(Math.max(nav.line, 1), lineCount);
                    ed.setPosition({ lineNumber: safeLine, column: nav.column });
                    ed.revealLineInCenter(safeLine);
                    if (nav.endColumn !== undefined) {
                        ed.setSelection({
                            startLineNumber: safeLine,
                            startColumn: nav.column,
                            endLineNumber: nav.endLineNumber ?? safeLine,
                            endColumn: nav.endColumn,
                        });
                        ed.revealRangeInCenter({
                            startLineNumber: safeLine,
                            startColumn: nav.column,
                            endLineNumber: nav.endLineNumber ?? safeLine,
                            endColumn: nav.endColumn,
                        });
                    }
                    console.log(`[LSP] onDidChangeModel — deferred navigation applied to line: ${safeLine}`);
                });
            }

            // Sync store active file when the model switch was triggered by
            // Monaco internals (peek widget) rather than React state. If the
            // store's active file already matches relPath, this is a no-op.
            const store = useFileEditorStore.getState();
            const activeFile = store.openFiles.find((f) => f.id === store.activeFileId);
            if (activeFile && activeFile.relPath === relPath) {
                console.log("[LSP] onDidChangeModel — same file as active, skipping store sync");
                return;
            }

            const existingFile = store.openFiles.find((f) => f.relPath === relPath);
            if (existingFile) {
                console.log("[LSP] onDidChangeModel — activating existing tab:", existingFile.id);
                store.setActiveFile(existingFile.id);
                return;
            }

            // The file isn't open — it must be a model created by ensureModelsForUris
            // for LSP cross-file reference preview. Open it via the store, which
            // re-uses the existing model content (already fetched).
            if (activeFile) {
                console.log("[LSP] onDidChangeModel — cross-file navigation, opening:", relPath);
                void store.openFile(activeFile.agentId, activeFile.workspaceId, relPath);
            }
        });

        // Ctrl+S / Cmd+S to save
        editor.addCommand(
            // eslint-disable-next-line no-bitwise
            2048 | 49, // KeyMod.CtrlCmd | KeyCode.KeyS
            () => {
                const currentId = useFileEditorStore.getState().activeFileId;
                if (currentId) void saveFile(currentId);
            },
        );

        // Ctrl+P / Cmd+P — Go to File (Monaco QuickInput-style palette).
        // Monaco standalone has no built-in "Go to File" provider and
        // IQuickInputService is not accessible from the editor's local DI
        // container, so we render a custom React component that replicates
        // the QuickInput visual style (same colors, typography, layout).
        // KeyCode.KeyP = 46 in monaco-editor 0.55.x (NOT 80).
        editor.addCommand(
            // eslint-disable-next-line no-bitwise
            2048 | 46, // KeyMod.CtrlCmd | KeyCode.KeyP
            () => {
                setShowGoToFile(true);
            },
        );

        // Ctrl+Shift+P / Cmd+Shift+P — Command Palette (Go to File).
        // In VS Code this opens the Command Palette; here we reuse the
        // GoToFilePalette since a dedicated command palette doesn't exist yet.
        // Without this, the event bubbles to the browser and triggers Print.
        // KeyCode.KeyP = 46 in monaco-editor 0.55.x.
        editor.addAction({
            id: "acowork.commandPalette",
            label: "Command Palette",
            keybindings: [
                // eslint-disable-next-line no-bitwise
                monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyP,
            ],
            run: () => {
                setShowGoToFile(true);
            },
        });

        // Ctrl+Shift+F / Cmd+Shift+F — Search in files (ripgrep backend).
        // Same visual style as GoToFilePalette.
        // KeyCode.KeyF = 33 in monaco-editor 0.55.x.
        // Use addAction (not addCommand) to ensure the keybinding overrides
        // any built-in Monaco action that may silently consume the event.
        editor.addAction({
            id: "acowork.globalSearch",
            label: "Search in Files",
            keybindings: [
                // eslint-disable-next-line no-bitwise
                monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyF,
            ],
            run: () => {
                console.log("[GlobalSearch] addAction fired — opening panel");
                setShowGlobalSearch(true);
            },
        });

        // ── Override ICodeEditorService.openCodeEditor ───────────────
        // In Monaco standalone, the default ICodeEditorService.openCodeEditor()
        // can only navigate within the same file. For cross-file navigation
        // (from LSP peek widgets like definition/references), it returns null.
        // We override it to detect cross-file navigation and switch the
        // active file in the store, which causes the editor to remount via
        // key={activeFile.id} with the target file loaded.
        if (!codeEditorOverriddenRef.current) {
            // Diagnostic: inspect what internal services are available
            const editorAny = editor as any;
            const svcKeys = Object.keys(editorAny).filter(k => k.toLowerCase().includes("service") || k.toLowerCase().includes("codeeditor"));
            // Use console.warn so it stands out in the console
            console.warn("[LSP] ═══ Editor internal service keys:", svcKeys);
            console.warn("[LSP] ═══ _codeEditorService:", !!editorAny._codeEditorService,
                "openCodeEditor:", !!editorAny._codeEditorService?.openCodeEditor);
            console.warn("[LSP] ═══ _instantiationService:", !!editorAny._instantiationService);

            let codeEditorSvc = editorAny._codeEditorService;

            // Fallback: try to get ICodeEditorService via _instantiationService
            if (!codeEditorSvc && editorAny._instantiationService) {
                try {
                    const instSvc = editorAny._instantiationService;
                    // Try common service access patterns
                    if (typeof instSvc.invokeFunction === "function") {
                        codeEditorSvc = instSvc.invokeFunction((accessor: any) => {
                            // Try known service IDs
                            for (const id of ["codeEditorService", "ICodeEditorService", "codeEditor"]) {
                                try { return accessor.get(id); } catch { /* skip */ }
                            }
                            return null;
                        });
                        console.log("[LSP] _instantiationService lookup result:", !!codeEditorSvc);
                    }
                } catch (e) {
                    console.warn("[LSP] _instantiationService lookup failed:", e);
                }
            }

            if (codeEditorSvc?.openCodeEditor) {
                const originalOpenCodeEditor = codeEditorSvc.openCodeEditor.bind(codeEditorSvc);
                codeEditorSvc.openCodeEditor = async (
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                    input: any,
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                    source: any
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                ): Promise<any> => {
                    console.log("[LSP] openCodeEditor — input.resource:", input?.resource?.toString(),
                        "selection:", JSON.stringify(input?.options?.selection));

                    // Try default behavior first (same-file navigation)
                    const result = await originalOpenCodeEditor(input, source);
                    if (result) {
                        // Same-file navigation — Monaco's default handler returned
                        // an editor, but it may not correctly apply position/selection
                        // for subsequent navigations within the same file. We must
                        // explicitly apply the selection to ensure the cursor moves.
                        const selection = input?.options?.selection;
                        if (selection && editorRef.current) {
                            const pos = { lineNumber: selection.startLineNumber, column: selection.startColumn };
                            editorRef.current.setPosition(pos);
                            editorRef.current.revealLineInCenter(pos.lineNumber);
                            // If selection has an end range, set the full selection
                            if (selection.endLineNumber && selection.endColumn) {
                                editorRef.current.setSelection({
                                    startLineNumber: selection.startLineNumber,
                                    startColumn: selection.startColumn,
                                    endLineNumber: selection.endLineNumber,
                                    endColumn: selection.endColumn,
                                });
                            }
                            console.log(`[LSP] openCodeEditor — same-file nav, applied selection to line: ${pos.lineNumber}`);
                        } else {
                            console.log("[LSP] openCodeEditor — default handled it (same file, no selection to apply)");
                        }
                        return result;
                    }

                    // Cross-file navigation: the default service couldn't handle it
                    const targetUri = input?.resource;
                    const selection = input?.options?.selection;
                    if (!targetUri) {
                        console.warn("[LSP] openCodeEditor — no target URI, giving up");
                        return null;
                    }

                    // Not a file URI — let Monaco handle natively (inmemory://, output://, etc.)
                    if (targetUri.scheme !== 'file') {
                        console.log("[LSP] openCodeEditor — ignoring non-file URI:", targetUri.toString());
                        return null;
                    }

                    // Extract relPath from model URI (e.g. file:///core/.../foo.rs → core/.../foo.rs)
                    const relPath = targetUri.path.replace(/^\/+/, "");
                    console.log("[LSP] openCodeEditor — cross-file navigation to:", relPath);

                    // Check if the target file is already the active file — in this
                    // case setActiveFile() won't change the model, so onDidChangeModel
                    // won't fire and pendingNavigationRef won't be consumed. We must
                    // apply the position directly.
                    const store = useFileEditorStore.getState();
                    const currentActiveFile = store.openFiles.find((f) => f.id === store.activeFileId);
                    if (currentActiveFile && currentActiveFile.relPath === relPath) {
                        // Target is already the active file — defer position application
                        // to avoid being overridden by any internal Monaco state restore.
                        if (selection) {
                            const sel = selection;
                            requestAnimationFrame(() => {
                                const ed = editorRef.current;
                                if (!ed) return;
                                const pos = { lineNumber: sel.startLineNumber, column: sel.startColumn };
                                ed.setPosition(pos);
                                ed.revealLineInCenter(pos.lineNumber);
                                if (sel.endLineNumber && sel.endColumn) {
                                    ed.setSelection({
                                        startLineNumber: sel.startLineNumber,
                                        startColumn: sel.startColumn,
                                        endLineNumber: sel.endLineNumber,
                                        endColumn: sel.endColumn,
                                    });
                                    ed.revealRangeInCenter({
                                        startLineNumber: sel.startLineNumber,
                                        startColumn: sel.startColumn,
                                        endLineNumber: sel.endLineNumber,
                                        endColumn: sel.endColumn,
                                    });
                                }
                                console.log(`[LSP] openCodeEditor — deferred navigation applied (same file) to line: ${pos.lineNumber}`);
                            });
                        }
                        return editorRef.current ?? null;
                    }

                    // Store target position for applying after model switch
                    if (selection) {
                        pendingNavigationRef.current = {
                            line: selection.startLineNumber,
                            column: selection.startColumn,
                            endLineNumber: selection.endLineNumber,
                            endColumn: selection.endColumn,
                        };
                    }

                    // Switch to the target file
                    const existingFile = store.openFiles.find((f) => f.relPath === relPath);

                    if (existingFile) {
                        console.log("[LSP] openCodeEditor — activating existing tab:", existingFile.id);
                        store.setActiveFile(existingFile.id);
                    } else {
                        if (currentActiveFile) {
                            // Check if a Monaco model already exists for this file
                            // (created by ensureModelsForUris). If so, reuse its
                            // content to avoid a second fetch and ensure the line
                            // numbers match the reference locations.
                            const monacoInst = monacoRef.current;
                            const targetMonacoUri = monacoInst?.Uri.parse(relPath);
                            const existingModel = targetMonacoUri
                                ? monacoInst!.editor.getModel(targetMonacoUri)
                                : null;

                            if (existingModel && monacoInst) {
                                const content = existingModel.getValue();
                                const lang = existingModel.getLanguageId();
                                console.log("[LSP] openCodeEditor — reusing model content, lines:", content.split("\n").length);
                                store.openFileWithContent(
                                    currentActiveFile.agentId, currentActiveFile.workspaceId,
                                    relPath, content, lang
                                );
                            } else {
                                console.log("[LSP] openCodeEditor — opening new file (fetch):", relPath);
                                void store.openFile(currentActiveFile.agentId, currentActiveFile.workspaceId, relPath);
                            }
                        }
                    }

                    return null; // We handled navigation via React state
                };
                codeEditorOverriddenRef.current = true;
                console.warn("[LSP] ═══ ICodeEditorService.openCodeEditor OVERRIDDEN — cross-file navigation enabled");
            } else {
                console.warn("[LSP] ═══ Could not access _codeEditorService — cross-file navigation won't work");
            }
        }

        // Note: handleEditorMount only runs once now (Editor is no longer keyed
        // by activeFile.id), so all listeners and the openCodeEditor override
        // above are registered exactly once for the lifetime of this panel.
    }, [saveFile]);

    // ── Save view state before model switch ──────────────────────────────
    // The cleanup of this effect fires during React's effect-cleanup phase,
    // BEFORE @monaco-editor/react's setup effect calls editor.setModel() with
    // the new path. At that moment the editor still has the previous model
    // bound, so saveViewState() captures the outgoing file's cursor/scroll/
    // selection state. We restore it inside onDidChangeModel when the model
    // switches back.
    const activeReadyRelPath = activeFile && !activeFile.loading ? activeFile.relPath : undefined;
    useEffect(() => {
        return () => {
            if (editorRef.current && activeReadyRelPath) {
                const state = editorRef.current.saveViewState();
                if (state) {
                    viewStatesRef.current.set(activeReadyRelPath, state);
                }
            }
        };
    }, [activeReadyRelPath]);

    // ── Document Tracker lifecycle (bound to workspaceRoot) ────────────
    useEffect(() => {
        if (workspaceRoot) {
            documentTrackerRef.current = new LspDocumentTracker(workspaceRoot);
        }
        return () => {
            documentTrackerRef.current?.dispose(lspClient ?? null);
            documentTrackerRef.current = null;
        };
    }, [workspaceRoot]);

    // ── Track open documents via LspDocumentTracker ──────────────────────
    // When the editor mounts a new file (tab switch or cross-file navigation),
    // notify the LSP server via the tracker. Also handles LSP client
    // reconnection by re-opening all previously tracked documents.
    useEffect(() => {
        if (!lspClient || !workspaceRoot || !activeFile || activeFile.loading) return;
        if (!monacoRef.current) return;
        const tracker = documentTrackerRef.current;
        if (!tracker) return;

        // Detect LSP client reconnection — re-open all tracked documents
        if (prevLspClientRef.current !== null && prevLspClientRef.current !== lspClient) {
            console.log("[LSP] DocumentTracker: client reconnected, re-opening all tracked docs");
            tracker.reopenAll(lspClient, monacoRef.current);
        }
        prevLspClientRef.current = lspClient;

        // Track the current active model as open
        const relPath = activeFile.relPath;
        const monacoUri = monacoRef.current.Uri.parse(relPath);
        const model = monacoRef.current.editor.getModel(monacoUri);
        if (model) {
            tracker.trackOpen(lspClient, model);
        }
    }, [activeFile, lspClient, workspaceRoot]);

    // ── Unpin opened tabs from the preview-model LRU pool ───────────────
    // Any file currently shown in a tab must not be LRU-evicted by
    // ensureModelsForUris peek-widget activity. Unpin them here so the
    // pool only tracks transient preview models.
    useEffect(() => {
        const monacoInst = monacoRef.current;
        if (!monacoInst) return;
        for (const f of openFiles) {
            const uriStr = monacoInst.Uri.parse(f.relPath).toString();
            unpinPreviewModel(uriStr);
        }
    }, [openFiles]);

    // ── LSP providers registration ──────────────────────────────────────

    useEffect(() => {
        // Unregister previous providers
        if (lspProvidersRef.current) {
            lspProvidersRef.current.dispose();
            lspProvidersRef.current = null;
        }

        // Register providers when both monaco and LSP client are ready
        if (monacoRef.current && lspClient && lspLanguage && workspaceRoot && activeFile) {
            try {
                console.log("[LSP] Registering providers for:", lspLanguage, "client:", !!lspClient);
                lspProvidersRef.current = registerLspProviders(monacoRef.current, {
                    client: lspClient,
                    language: lspLanguage,
                    workspaceRoot,
                    agentId: activeFile.agentId,
                    workspaceId: activeFile.workspaceId,
                });
            } catch (err) {
                console.warn("[LSP] Failed to register providers:", err);
            }
        } else {
            console.log("[LSP] Skipping provider registration — monaco:", !!monacoRef.current, "client:", !!lspClient, "language:", lspLanguage);
        }

        return () => {
            if (lspProvidersRef.current) {
                lspProvidersRef.current.dispose();
                lspProvidersRef.current = null;
            }
        };
    }, [lspClient, lspLanguage]);

    /** Add selected lines to chat context */
    const handleAddSelectionToChat = useCallback(() => {
        const agentId = selectedAgentId;
        if (!agentId || !activeFile || !selectionRange) return;
        const sessionId = getActiveSessionId(agentId);
        if (!sessionId) return;

        const { startLine, endLine } = selectionRange;
        const lineLabel = startLine === endLine ? `L${startLine}` : `L${startLine}-L${endLine}`;
        // Resolve absolute path from tree root
        const treeRoots = useWorkspaceStore.getState().treeRoots;
        const workspaceRoot = treeRoots[`${agentId}:${activeFile.workspaceId}`] ?? "";
        const absPath = workspaceRoot ? `${workspaceRoot}/${activeFile.relPath}` : activeFile.relPath;
        addAttachedContext(agentId, sessionId, {
            id: `${agentId}:${activeFile.relPath}:${startLine}:${endLine}`,
            type: "selection",
            name: `${activeFile.relPath.split("/").pop() || activeFile.relPath} ${lineLabel}`,
            absPath,
            startLine,
            endLine,
        });
    }, [selectedAgentId, activeFile, selectionRange, getActiveSessionId, addAttachedContext]);

    const handleEditorChange = useCallback((value: string | undefined) => {
        if (value === undefined) return;
        const currentId = useFileEditorStore.getState().activeFileId;
        if (currentId) updateContent(currentId, value);
    }, [updateContent]);

    const handleClose = useCallback((e: React.MouseEvent, file: OpenFile) => {
        e.stopPropagation();
        if (file.dirty) {
            setClosingFileId(file.id);
            return;
        }
        // Send didClose before removing from store
        if (lspClient && monacoRef.current && documentTrackerRef.current) {
            const monacoUri = monacoRef.current.Uri.parse(file.relPath);
            const model = monacoRef.current.editor.getModel(monacoUri);
            if (model) {
                documentTrackerRef.current.trackClose(lspClient, model);
            }
        }
        closeFile(file.id);
        // Dispose Monaco model if no other tab still references the same file
        if (monacoRef.current) {
            const remaining = useFileEditorStore.getState().openFiles;
            const stillReferenced = remaining.some(
                (f) => f.id !== file.id && f.relPath === file.relPath
            );
            if (!stillReferenced) {
                disposeModelForFile(monacoRef.current, file.relPath);
            }
        }
    }, [closeFile, lspClient]);

    const confirmClose = useCallback(() => {
        if (!closingFileId) return;
        const closingFile = openFiles.find((f) => f.id === closingFileId);
        // Send didClose before discarding
        if (lspClient && monacoRef.current && documentTrackerRef.current && closingFile) {
            const monacoUri = monacoRef.current.Uri.parse(closingFile.relPath);
            const model = monacoRef.current.editor.getModel(monacoUri);
            if (model) {
                documentTrackerRef.current.trackClose(lspClient, model);
            }
        }
        closeFile(closingFileId, true);
        setClosingFileId(null);
        // Dispose Monaco model if no other tab still references the same file
        if (monacoRef.current && closingFile) {
            const remaining = useFileEditorStore.getState().openFiles;
            const stillReferenced = remaining.some(
                (f) => f.id !== closingFile.id && f.relPath === closingFile.relPath
            );
            if (!stillReferenced) {
                disposeModelForFile(monacoRef.current, closingFile.relPath);
            }
        }
    }, [closingFileId, closeFile, lspClient, openFiles]);

    // ── Tab right-click menu (VSCode-style: Close / Close Others / Close All) ──

    // Close the menu on outside click or Escape
    useEffect(() => {
        if (!tabContextMenu) return;
        const handleClickOutside = (e: MouseEvent) => {
            if (tabMenuRef.current && !tabMenuRef.current.contains(e.target as Node)) {
                setTabContextMenu(null);
            }
        };
        const handleEscape = (e: KeyboardEvent) => {
            if (e.key === "Escape") setTabContextMenu(null);
        };
        document.addEventListener("mousedown", handleClickOutside);
        document.addEventListener("keydown", handleEscape);
        return () => {
            document.removeEventListener("mousedown", handleClickOutside);
            document.removeEventListener("keydown", handleEscape);
        };
    }, [tabContextMenu]);

    const handleTabContextMenu = useCallback((e: React.MouseEvent, file: OpenFile) => {
        e.preventDefault();
        e.stopPropagation();
        setTabContextMenu({ fileId: file.id, x: e.clientX, y: e.clientY });
    }, []);

    /**
     * Send didClose to LSP and dispose Monaco models for a set of files.
     * Store mutations are the caller's responsibility (single- or batch-close).
     * This is the shared cleanup path for "Close", "Close Others", and "Close All".
     */
    const cleanupClosedFiles = useCallback((files: OpenFile[]) => {
        if (files.length === 0) return;
        // 1. Notify LSP server via didClose for every tracked model
        if (lspClient && monacoRef.current && documentTrackerRef.current) {
            for (const file of files) {
                const monacoUri = monacoRef.current.Uri.parse(file.relPath);
                const model = monacoRef.current.editor.getModel(monacoUri);
                if (model) {
                    documentTrackerRef.current.trackClose(lspClient, model);
                }
            }
        }
        // 2. Dispose Monaco models that are no longer referenced by any surviving tab
        if (monacoRef.current) {
            const remaining = useFileEditorStore.getState().openFiles;
            for (const file of files) {
                const stillReferenced = remaining.some(
                    (f) => f.id !== file.id && f.relPath === file.relPath,
                );
                if (!stillReferenced) {
                    disposeModelForFile(monacoRef.current, file.relPath);
                }
            }
        }
    }, [lspClient]);

    const handleCloseTab = useCallback((file: OpenFile) => {
        setTabContextMenu(null);
        if (file.dirty) {
            // Reuse the single-file unsaved-changes dialog
            setClosingFileId(file.id);
            return;
        }
        cleanupClosedFiles([file]);
        closeFile(file.id);
    }, [cleanupClosedFiles, closeFile]);

    const handleCloseOthers = useCallback((file: OpenFile) => {
        setTabContextMenu(null);
        const others = openFiles.filter((f) => f.id !== file.id);
        if (others.length === 0) return;
        // Activate the kept tab first so the surviving tab is the focused one
        // (the store will also do this, but doing it here keeps the visual
        // transition smoother and avoids intermediate focus shifts).
        setActiveFile(file.id);
        const dirtyCount = others.filter((f) => f.dirty).length;
        if (dirtyCount > 0) {
            setBatchCloseRequest({
                kind: "others",
                fileIds: others.map((f) => f.id),
                dirtyCount,
                keepFileId: file.id,
            });
            return;
        }
        cleanupClosedFiles(others);
        closeOthers(file.id);
    }, [openFiles, cleanupClosedFiles, closeOthers, setActiveFile]);

    const handleCloseAll = useCallback(() => {
        setTabContextMenu(null);
        if (openFiles.length === 0) return;
        const dirtyCount = openFiles.filter((f) => f.dirty).length;
        if (dirtyCount > 0) {
            setBatchCloseRequest({
                kind: "all",
                fileIds: openFiles.map((f) => f.id),
                dirtyCount,
            });
            return;
        }
        cleanupClosedFiles(openFiles);
        closeAllFiles();
    }, [openFiles, cleanupClosedFiles, closeAllFiles]);

    const confirmBatchClose = useCallback(() => {
        if (!batchCloseRequest) return;
        // Re-resolve the actual OpenFile objects from the latest store state
        // (in case files were closed by some other path while the dialog was open).
        const live = useFileEditorStore.getState().openFiles;
        const filesToClean = batchCloseRequest.fileIds
            .map((id) => live.find((f) => f.id === id))
            .filter((f): f is OpenFile => f !== undefined);
        if (filesToClean.length > 0) {
            cleanupClosedFiles(filesToClean);
        }
        if (batchCloseRequest.kind === "all") {
            closeAllFiles(true);
        } else if (batchCloseRequest.kind === "others" && batchCloseRequest.keepFileId) {
            closeOthers(batchCloseRequest.keepFileId, true);
        }
        setBatchCloseRequest(null);
    }, [batchCloseRequest, cleanupClosedFiles, closeAllFiles, closeOthers]);

    return (
        <div
            className="relative flex flex-col bg-[#FAFAFA] dark:border-zinc-800 dark:bg-zinc-900 rounded-lg overflow-hidden"
            style={{ width }}
        >
            {/* Tab bar */}
            <div className="flex items-center bg-[#FAFAFA] dark:bg-zinc-900 select-none px-0.5 gap-0.5 mt-[5px] border-b border-zinc-200 dark:border-zinc-800">
                <ScrollableTabBar
                    activeItemSelector={activeFileId ? `[data-file-id="${activeFileId}"]` : undefined}
                    activeItemId={activeFileId ?? undefined}
                >
                    {openFiles.map((file) => {
                        const isActive = file.id === activeFileId;
                        const isPreview = file.mode === "preview";
                        return (
                            <Tooltip
                                content={isPreview ? `${file.relPath} · ${t("fileEditor.previewBadge")}` : file.relPath}
                                variant="plain"
                                key={file.id}
                            >
                                <TabItem
                                    data-file-id={file.id}
                                    onClick={() => setActiveFile(file.id)}
                                    onContextMenu={(e) => handleTabContextMenu(e, file)}
                                    active={isActive}
                                >
                                    {/* Dirty indicator / loading / preview badge */}
                                    {file.loading ? (
                                        <Loader2 className="h-3 w-3 shrink-0 animate-spin text-zinc-400" />
                                    ) : isPreview ? (
                                        <Eye className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
                                    ) : file.dirty ? (
                                        <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
                                    ) : null}
                                    {/* File name */}
                                    <span className="min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]">
                                        {file.fileName}
                                    </span>
                                    {/* Close button */}
                                    <Tooltip content={t("fileEditor.close")} variant="plain">
                                        <button
                                            onClick={(e) => handleClose(e, file)}
                                            className={cn(
                                                "shrink-0 rounded p-0.5 transition-opacity",
                                                isActive
                                                    ? "opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600"
                                                    : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-600",
                                            )}
                                        >
                                            <X className="h-3 w-3" />
                                        </button>
                                    </Tooltip>
                                </TabItem>
                            </Tooltip>
                        );
                    })}
                </ScrollableTabBar>

                {/* Save button — only for editable files in edit mode */}
                {activeFile && !activeFile.loading && activeFile.mode === "edit" && (
                    <Tooltip content={t("fileEditor.save")} variant="plain">
                        <button
                            onClick={() => activeFile.dirty && void saveFile(activeFile.id)}
                            disabled={!activeFile.dirty || activeFile.saving}
                            className={cn(
                                "flex items-center justify-center rounded p-1 transition-colors shrink-0",
                                activeFile.dirty
                                    ? "text-[var(--color-accent)] hover:bg-zinc-200 dark:hover:bg-zinc-700"
                                    : "text-zinc-300 dark:text-zinc-600 cursor-default",
                            )}
                        >
                            {activeFile.saving ? (
                                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                            ) : (
                                <Save className="h-3.5 w-3.5" />
                            )}
                        </button>
                    </Tooltip>
                )}
            </div>

            {/* Editor area — Editor is mounted whenever there is at least one
                open file. Switching tabs changes `path` (and therefore the
                Monaco model) without recreating the Editor instance, so LSP
                cross-file navigation no longer races with editor remounts. */}
            <div className="relative flex-1 overflow-hidden">
                {!activeFile ? (
                    <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
                        {t("fileEditor.emptyState")}
                    </div>
                ) : activeFile.mode === "preview" ? (
                    <MarkdownPreviewView file={activeFile} />
                ) : (
                    <>
                        <Editor
                            path={activeReadyRelPath}
                            value={activeFile && !activeFile.loading ? activeFile.content : undefined}
                            language={activeFile && !activeFile.loading ? activeFile.language : undefined}
                            theme={resolvedMonacoTheme}
                            onChange={handleEditorChange}
                            onMount={handleEditorMount}
                            keepCurrentModel
                            options={{
                                minimap: { enabled: false },
                                fontSize: editorFontSize,
                                lineNumbers: "on",
                                scrollBeyondLastLine: false,
                                wordWrap: "off",
                                tabSize: 2,
                                renderWhitespace: "selection",
                                padding: { top: 8 },
                                automaticLayout: true,
                                readOnly: false,
                            }}
                        />
                        {/* Floating "Add to Chat" button near selection end */}
                        {selectionRange && addToChatPos && selectedAgentId && (
                            <button
                                onClick={handleAddSelectionToChat}
                                className="absolute z-30 flex items-center gap-1.5 rounded-md px-2 py-1 text-xs font-medium text-white shadow-md transition-colors"
                                style={{
                                    top: addToChatPos.top,
                                    left: addToChatPos.left,
                                    backgroundColor: "var(--color-accent)",
                                    borderColor: "color-mix(in srgb, var(--color-accent) 70%, black)",
                                    borderWidth: 1,
                                    borderStyle: "solid",
                                }}
                                onMouseEnter={(e) => { e.currentTarget.style.filter = "brightness(0.88)"; }}
                                onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
                            >
                                <MessageSquarePlus className="h-3.5 w-3.5" />
                                Add to Chat
                            </button>
                        )}
                        {activeFile.loading && (
                            <div className="absolute inset-0 flex items-center justify-center gap-2 bg-[#FAFAFA]/80 text-xs text-zinc-400 dark:bg-zinc-900/80">
                                <Loader2 className="h-4 w-4 animate-spin" />
                                Loading...
                            </div>
                        )}
                    </>
                )}
            </div>

            {/* Status bar — only for editable files */}
            {activeFile && !activeFile.loading && activeFile.mode === "edit" && (
                <div className="flex items-center justify-between gap-2 border-t border-zinc-200 bg-[#FAFAFA] px-3 h-5 text-[11px] text-zinc-500 select-none dark:border-zinc-800 dark:bg-zinc-900 dark:text-zinc-400">
                    <span className="uppercase truncate min-w-0">{activeFile.language || "plain text"}</span>
                    {lspEnabled && lspLanguage && (
                        <div className="shrink-0">
                            <LspIndicator status={lspStatus} statusMessage={lspStatusMessage} language={lspLanguage} />
                        </div>
                    )}
                    <span className="truncate min-w-0 text-right">Ln {cursor.line}, Col {cursor.column}{selectedCount > 0 ? ` (${selectedCount} selected)` : ""}</span>
                </div>
            )}

            {/* Close confirmation dialog */}
            {closingFileId && (
                <div
                    className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50"
                    onClick={() => setClosingFileId(null)}
                >
                    <div
                        className="mx-4 w-full max-w-sm rounded-md border border-zinc-200 bg-white p-5 shadow-xl dark:border-zinc-700 dark:bg-zinc-800"
                        onClick={(e) => e.stopPropagation()}
                    >
                        <div className="flex items-start gap-3">
                            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-amber-100 dark:bg-amber-900/30">
                                <FileText className="h-5 w-5 text-amber-600 dark:text-amber-400" />
                            </div>
                            <div className="flex-1">
                                <h3 className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
                                    {t("fileEditor.unsavedChanges")}
                                </h3>
                                <p className="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
                                    {t("fileEditor.saveChanges")}
                                </p>
                            </div>
                        </div>
                        <div className="mt-4 flex justify-end gap-2">
                            <button
                                onClick={() => setClosingFileId(null)}
                                className="rounded btn-solid px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.cancel")}
                            </button>
                            <button
                                onClick={confirmClose}
                                className="rounded btn-accent px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.discard")}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Go to File palette (Ctrl+P) */}
            {showGoToFile && activeFile && (
                <GoToFilePalette
                    agentId={activeFile.agentId}
                    workspaceId={activeFile.workspaceId}
                    onClose={() => {
                        setShowGoToFile(false);
                        editorRef.current?.focus();
                    }}
                />
            )}

            {/* Global Search panel (Ctrl+Shift+F) */}
            {showGlobalSearch && activeFile && (
                <GlobalSearchPanel
                    agentId={activeFile.agentId}
                    workspaceId={activeFile.workspaceId}
                    lspClient={lspClient}
                    lspStatus={lspStatus}
                    workspaceRoot={workspaceRoot}
                    onClose={() => {
                        setShowGlobalSearch(false);
                        editorRef.current?.focus();
                    }}
                />
            )}

            {/* Tab right-click context menu (VSCode-style) */}
            {tabContextMenu && createPortal(
                <div
                    ref={tabMenuRef}
                    className="fixed z-[100] min-w-[160px] rounded-md border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
                    style={{ left: tabContextMenu.x, top: tabContextMenu.y }}
                    onContextMenu={(e) => e.preventDefault()}
                >
                    {(() => {
                        const target = openFiles.find((f) => f.id === tabContextMenu.fileId);
                        if (!target) return null;
                        const canCloseOthers = openFiles.length > 1;
                        const canCloseAll = openFiles.length > 0;
                        const baseItem =
                            "flex w-full items-center px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700";
                        const disabledItem =
                            "disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:disabled:hover:bg-transparent";
                        return (
                            <>
                                <button
                                    type="button"
                                    onClick={() => handleCloseTab(target)}
                                    className={baseItem}
                                >
                                    {t("fileEditor.close")}
                                </button>
                                <button
                                    type="button"
                                    onClick={() => canCloseOthers && handleCloseOthers(target)}
                                    disabled={!canCloseOthers}
                                    className={cn(baseItem, disabledItem)}
                                >
                                    {t("fileEditor.closeOthers")}
                                </button>
                                <button
                                    type="button"
                                    onClick={() => canCloseAll && handleCloseAll()}
                                    disabled={!canCloseAll}
                                    className={cn(baseItem, disabledItem)}
                                >
                                    {t("fileEditor.closeAll")}
                                </button>
                            </>
                        );
                    })()}
                </div>,
                document.body,
            )}

            {/* Batch close confirmation (Close Others / Close All with dirty files) */}
            {batchCloseRequest && (
                <div
                    className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50"
                    onClick={() => setBatchCloseRequest(null)}
                >
                    <div
                        className="mx-4 w-full max-w-sm rounded-md border border-zinc-200 bg-white p-5 shadow-xl dark:border-zinc-700 dark:bg-zinc-800"
                        onClick={(e) => e.stopPropagation()}
                    >
                        <div className="flex items-start gap-3">
                            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-amber-100 dark:bg-amber-900/30">
                                <FileText className="h-5 w-5 text-amber-600 dark:text-amber-400" />
                            </div>
                            <div className="flex-1">
                                <h3 className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
                                    {t("fileEditor.unsavedChanges")}
                                </h3>
                                <p className="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
                                    {batchCloseRequest.kind === "all"
                                        ? t("fileEditor.batchCloseAllConfirm", {
                                            count: batchCloseRequest.dirtyCount,
                                        })
                                        : t("fileEditor.batchCloseOthersConfirm", {
                                            count: batchCloseRequest.dirtyCount,
                                        })}
                                </p>
                            </div>
                        </div>
                        <div className="mt-4 flex justify-end gap-2">
                            <button
                                onClick={() => setBatchCloseRequest(null)}
                                className="rounded btn-solid px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.cancel")}
                            </button>
                            <button
                                onClick={confirmBatchClose}
                                className="rounded btn-accent px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.discard")}
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}
