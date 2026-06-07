import { useState, useRef, useEffect, useCallback, useMemo } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useFileEditorStore, type OpenFile } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { cn } from "../../lib/utils";
import { X, Save, Loader2, FileText } from "lucide-react";
import Editor, { type OnMount } from "@monaco-editor/react";
import { ScrollableTabBar } from "../common/ScrollableTabBar";
import { TabItem } from "../common/tab";

export function FileEditorPanel({ width }: { width: number }) {
    const { t } = useTranslation();
    const openFiles = useFileEditorStore((s) => s.openFiles);
    const activeFileId = useFileEditorStore((s) => s.activeFileId);
    const setActiveFile = useFileEditorStore((s) => s.setActiveFile);
    const updateContent = useFileEditorStore((s) => s.updateContent);
    const saveFile = useFileEditorStore((s) => s.saveFile);
    const closeFile = useFileEditorStore((s) => s.closeFile);

    const theme = useSettingsStore((s) => s.theme);
    const [closingFileId, setClosingFileId] = useState<string | null>(null);
    const editorRef = useRef<Parameters<OnMount>[0] | null>(null);
    const [cursor, setCursor] = useState({ line: 1, column: 1 });
    const [selectedCount, setSelectedCount] = useState(0);

    const activeFile = openFiles.find((f) => f.id === activeFileId) ?? null;

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

    const handleEditorMount: OnMount = useCallback((editor, _monaco) => {
        editorRef.current = editor;
        // Track cursor position + selection
        editor.onDidChangeCursorPosition((e) => {
            setCursor({ line: e.position.lineNumber, column: e.position.column });
            // Sync selection count
            const sel = editor.getSelection();
            if (sel && !sel.isEmpty()) {
                const model = editor.getModel();
                if (model) {
                    setSelectedCount(model.getValueInRange(sel).length);
                    return;
                }
            }
            setSelectedCount(0);
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
    }, [saveFile]);

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
        closeFile(file.id);
    }, [closeFile]);

    const confirmClose = useCallback(() => {
        if (!closingFileId) return;
        closeFile(closingFileId, true);
        setClosingFileId(null);
    }, [closingFileId, closeFile]);

    return (
        <div
            className="flex flex-col border-l border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900"
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
                        return (
                            <TabItem
                                key={file.id}
                                data-file-id={file.id}
                                onClick={() => setActiveFile(file.id)}
                                active={isActive}
                                title={file.relPath}
                            >
                                {/* Dirty indicator / loading */}
                                {file.loading ? (
                                    <Loader2 className="h-3 w-3 shrink-0 animate-spin text-zinc-400" />
                                ) : file.dirty ? (
                                    <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
                                ) : null}
                                {/* File name */}
                                <span className="min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]">
                                    {file.fileName}
                                </span>
                                {/* Close button */}
                                <button
                                    onClick={(e) => handleClose(e, file)}
                                    className={cn(
                                        "shrink-0 rounded p-0.5 transition-opacity",
                                        isActive
                                            ? "opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600"
                                            : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-600",
                                    )}
                                    title="Close"
                                >
                                    <X className="h-3 w-3" />
                                </button>
                            </TabItem>
                        );
                    })}
                </ScrollableTabBar>

                {/* Save button */}
                {activeFile && !activeFile.loading && (
                    <button
                        onClick={() => activeFile.dirty && void saveFile(activeFile.id)}
                        disabled={!activeFile.dirty || activeFile.saving}
                        className={cn(
                            "flex items-center justify-center rounded p-1 transition-colors shrink-0",
                            activeFile.dirty
                                ? "text-[var(--color-accent)] hover:bg-zinc-200 dark:hover:bg-zinc-700"
                                : "text-zinc-300 dark:text-zinc-600 cursor-default",
                        )}
                        title="Save (Ctrl+S)"
                    >
                        {activeFile.saving ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                            <Save className="h-3.5 w-3.5" />
                        )}
                    </button>
                )}
            </div>

            {/* Editor area */}
            <div className="flex-1 overflow-hidden">
                {!activeFile ? (
                    <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
                        {t("fileEditor.emptyState")}
                    </div>
                ) : activeFile.loading ? (
                    <div className="flex h-full items-center justify-center gap-2 text-xs text-zinc-400">
                        <Loader2 className="h-4 w-4 animate-spin" />
                        Loading...
                    </div>
                ) : (
                    <Editor
                        key={activeFile.id}
                        value={activeFile.content}
                        language={activeFile.language}
                        theme={resolvedMonacoTheme}
                        onChange={handleEditorChange}
                        onMount={handleEditorMount}
                        options={{
                            minimap: { enabled: false },
                            fontSize: 13,
                            lineNumbers: "on",
                            scrollBeyondLastLine: false,
                            wordWrap: "on",
                            tabSize: 2,
                            renderWhitespace: "selection",
                            padding: { top: 8 },
                            automaticLayout: true,
                            readOnly: false,
                        }}
                    />
                )}
            </div>

            {/* Status bar */}
            {activeFile && !activeFile.loading && (
                <div className="flex items-center justify-between border-t border-zinc-200 bg-zinc-100 px-3 h-5 text-[11px] text-zinc-500 select-none dark:border-zinc-800 dark:bg-zinc-800 dark:text-zinc-400">
                    <span className="uppercase">{activeFile.language || "plain text"}</span>
                    <span>Ln {cursor.line}, Col {cursor.column}{selectedCount > 0 ? ` (${selectedCount} selected)` : ""}</span>
                </div>
            )}

            {/* Close confirmation dialog */}
            {closingFileId && (
                <div
                    className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50"
                    onClick={() => setClosingFileId(null)}
                >
                    <div
                        className="mx-4 w-full max-w-sm rounded-xl border border-zinc-200 bg-white p-5 shadow-xl dark:border-zinc-700 dark:bg-zinc-800"
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
                                className="rounded-lg btn-solid px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.cancel")}
                            </button>
                            <button
                                onClick={confirmClose}
                                className="rounded-lg btn-accent px-3 py-1.5 text-xs"
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
