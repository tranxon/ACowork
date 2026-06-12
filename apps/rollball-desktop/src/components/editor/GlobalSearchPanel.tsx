/**
 * GlobalSearchPanel — VS Code Ctrl+Shift+F style content search widget.
 *
 * Visual design is identical to GoToFilePalette (Monaco QuickInput clone):
 *   - 60% width, centered in editor area
 *   - Same dark/light theme colors
 *   - Search input + optional file filter input
 *   - Results list showing file path, line number, and matching text
 *   - Same typography (13px, line-height 22px)
 *
 * Search modes:
 *   - "symbol" (default when LSP ready): uses LSP workspace/symbol for
 *     code-aware semantic search — finds functions, classes, variables, etc.
 *   - "text": Gateway ripgrep-based full-text search across all files.
 *   - Toggle button switches between modes; only visible when LSP is ready.
 */

import { useState, useEffect, useRef, useMemo, useCallback } from "react";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "../workspace/FileTree/fileIcons";
import type { MonacoLanguageClient } from "monaco-languageclient";
import type { LspStatus } from "../../hooks/useLspClient";

/* ─── Types ─────────────────────────────────────────────────────────── */

interface SearchMatch {
    file: string;
    line: number;
    column: number;
    text: string;
}

interface SearchResponse {
    matches: SearchMatch[];
    totalMatches: number;
    truncated: boolean;
}

/** LSP workspace/symbol result converted for display. */
interface LspSymbolResult {
    /** Symbol name (e.g. function name, class name) */
    name: string;
    /** Symbol kind as a short label: "fn", "class", "var", "iface", etc. */
    kindLabel: string;
    /** Relative file path within workspace */
    file: string;
    /** 1-based line number */
    line: number;
    /** 1-based column number */
    column: number;
    /** Container name (e.g. parent class), if any */
    containerName?: string;
}

/** Search mode: LSP semantic symbols or full-text grep. */
type SearchMode = "symbol" | "text";

interface GlobalSearchPanelProps {
    agentId: string;
    workspaceId: string;
    onClose: () => void;
    /** LSP client for the active language (null if not connected). */
    lspClient?: MonacoLanguageClient | null;
    /** Current LSP connection status. */
    lspStatus?: LspStatus;
    /** Absolute workspace root path (needed for URI → relPath conversion). */
    workspaceRoot?: string;
}

/* ─── VS Code Theme Colors (same as GoToFilePalette) ─────────────────── */

const darkTheme = {
    widgetBg: "#252526",
    inputBg: "#3C3C3C",
    inputFg: "#CCCCCC",
    inputBorder: "transparent",
    inputFocusBorder: "#007FD4",
    inputPlaceholder: "#969696",
    listFocusBg: "#04395E",
    listFocusFg: "#FFFFFF",
    listHoverBg: "#2A2D2E",
    highlight: "#2AAAFF",
    description: "rgba(204,204,204,0.7)",
    countBg: "#4D4D4D",
    countFg: "#FFFFFF",
    shadow: "rgba(0,0,0,0.36) 0 0 8px 2px",
};

const lightTheme = {
    widgetBg: "#F3F3F3",
    inputBg: "#FFFFFF",
    inputFg: "#616161",
    inputBorder: "#CECECE",
    inputFocusBorder: "#0090F1",
    inputPlaceholder: "#767676",
    listFocusBg: "#0060C0",
    listFocusFg: "#FFFFFF",
    listHoverBg: "#E8E8E8",
    highlight: "#0066BF",
    description: "#616161",
    countBg: "#C4C4C4",
    countFg: "#616161",
    shadow: "rgba(0,0,0,0.16) 0 2px 8px",
};

/* ─── Main Component ────────────────────────────────────────────────── */

/** LSP SymbolKind numeric constants (from LSP spec). */
const SYMBOL_KIND_LABEL: Record<number, string> = {
    1: "file", 2: "mod", 3: "ns", 4: "pkg",
    5: "class", 6: "method", 7: "prop", 8: "field",
    9: "ctor", 10: "enum", 11: "iface", 12: "func",
    13: "var", 14: "const", 15: "string", 16: "num",
    17: "bool", 18: "array", 19: "obj", 20: "key",
    21: "null", 22: "enumMbr", 23: "struct", 24: "event",
    25: "op", 26: "tparam",
};

function symbolKindLabel(kind: number): string {
    return SYMBOL_KIND_LABEL[kind] ?? `kind(${kind})`;
}

export function GlobalSearchPanel({
    agentId, workspaceId, onClose, lspClient, workspaceRoot,
}: GlobalSearchPanelProps) {
    const [query, setQuery] = useState("");
    const [fileFilter, setFileFilter] = useState("");
    const [focusedIdx, setFocusedIdx] = useState(0);
    const [loading, setLoading] = useState(false);
    const [matches, setMatches] = useState<SearchMatch[]>([]);
    const [totalMatches, setTotalMatches] = useState(0);
    const [truncated, setTruncated] = useState(false);
    const [searched, setSearched] = useState(false);
    const [caseSensitive, setCaseSensitive] = useState(false);
    const [wholeWord, setWholeWord] = useState(false);
    const [inputFocused, setInputFocused] = useState(false);
    const [error, setError] = useState<string | null>(null);

    // LSP symbol search results (only populated in "symbol" mode)
    const [lspSymbolResults, setLspSymbolResults] = useState<LspSymbolResult[]>([]);

    // Search mode — default to symbol when LSP client is available
    const [searchMode, setSearchMode] = useState<SearchMode>(
        () => (lspClient != null ? "symbol" : "text"),
    );

    const abortRef = useRef<AbortController | null>(null);
    const symbolAbortRef = useRef<AbortController | null>(null);

    const inputRef = useRef<HTMLInputElement>(null);
    const itemRefs = useRef<(HTMLDivElement | null)[]>([]);

    const theme = useSettingsStore((s) => s.theme);
    const isDark = useMemo(() => {
        if (theme === "dark") return true;
        if (theme === "light") return false;
        return document.documentElement.classList.contains("dark");
    }, [theme]);
    const colors = isDark ? darkTheme : lightTheme;

    /* ── Search API ─────────────────────────────────────────────────── */

    const doSearch = useCallback(async (q: string) => {
        // Cancel any in-flight request
        if (abortRef.current) {
            abortRef.current.abort();
        }

        if (!q.trim() || !agentId) {
            setMatches([]);
            setTotalMatches(0);
            setTruncated(false);
            setSearched(false);
            setError(null);
            return;
        }

        const controller = new AbortController();
        abortRef.current = controller;
        setLoading(true);
        setError(null);

        try {
            const baseUrl = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
            const params = new URLSearchParams();
            params.set("q", q);
            if (workspaceId && workspaceId !== "__agent_home__") {
                params.set("workspace_id", workspaceId);
            }
            if (fileFilter.trim()) {
                params.set("include", fileFilter.trim());
            }
            if (caseSensitive) {
                params.set("case_sensitive", "true");
            }
            if (wholeWord) {
                params.set("whole_word", "true");
            }
            params.set("max_results", "200");
            const url = `${baseUrl}/api/agents/${agentId}/workspaces/search?${params.toString()}`;

            // 30-second timeout — abort slow searches so the UI doesn't hang
            const timeoutId = setTimeout(() => controller.abort(), 30_000);
            const resp = await fetch(url, { signal: controller.signal });
            clearTimeout(timeoutId);

            if (!resp.ok) {
                console.error("[GlobalSearch] search failed:", resp.status);
                setMatches([]);
                setTotalMatches(0);
                setSearched(true);
                setError(`Server error (${resp.status})`);
                setLoading(false);
                return;
            }
            const data = (await resp.json()) as SearchResponse;
            setMatches(data.matches);
            setTotalMatches(data.totalMatches);
            setTruncated(data.truncated);
            setSearched(true);
            setError(null);
        } catch (e: any) {
            if (e?.name === "AbortError") {
                console.log("[GlobalSearch] search aborted (timeout or new query)");
                setError("Search timed out — try a more specific query or file filter");
            } else {
                console.error("[GlobalSearch] search error:", e);
                setError("Search failed — check Gateway connection");
            }
            setMatches([]);
            setSearched(true);
        } finally {
            setLoading(false);
        }
    }, [agentId, workspaceId, fileFilter, caseSensitive, wholeWord]);

    /* ── LSP workspace/symbol search ─────────────────────────────────── */

    // 30-second timeout — jdtls can be slow for large Java projects
    const SYMBOL_SEARCH_TIMEOUT_MS = 30_000;

    const doSymbolSearch = useCallback(async (q: string) => {
        if (symbolAbortRef.current) {
            symbolAbortRef.current.abort();
        }
        if (!q.trim() || !lspClient || !workspaceRoot) {
            setLspSymbolResults([]);
            setTotalMatches(0);
            setTruncated(false);
            setSearched(false);
            setError(null);
            return;
        }

        const controller = new AbortController();
        symbolAbortRef.current = controller;
        setLoading(true);
        setError(null);

        // Track timeout so we can clear it on success/abort.
        let timeoutId: ReturnType<typeof setTimeout> | undefined;

        try {
            // LSP workspace/symbol returns SymbolInformation[]
            const raw = (await Promise.race([
                lspClient.sendRequest("workspace/symbol", {
                    query: q,
                }),
                new Promise<never>((_, reject) => {
                    timeoutId = setTimeout(() => {
                        controller.abort();
                        reject(new DOMException("Symbol search timed out", "AbortError"));
                    }, SYMBOL_SEARCH_TIMEOUT_MS);
                }),
            ])) as any[];

            clearTimeout(timeoutId);
            if (controller.signal.aborted) return;

            if (!raw || raw.length === 0) {
                setLspSymbolResults([]);
                setTotalMatches(0);
                setSearched(true);
                setLoading(false);
                return;
            }

            // Convert LSP SymbolInformation → LspSymbolResult
            const results: LspSymbolResult[] = [];
            const seen = new Set<string>();
            for (const si of raw) {
                // Resolve URI to relative path
                let uri: string = si.location?.uri ?? "";
                let relPath = uri;
                if (uri.startsWith("file://")) {
                    let filePath = uri.slice("file://".length);
                    // Decode percent-encoded characters
                    try { filePath = decodeURIComponent(filePath); } catch { /* keep as-is */ }
                    // On Windows, strip leading slash from /C:/...
                    if (/^\/[A-Za-z]:/.test(filePath)) {
                        filePath = filePath.slice(1);
                    }
                    // Convert to relative path
                    const root = workspaceRoot.replace(/\\/g, "/");
                    const fpNorm = filePath.replace(/\\/g, "/");
                    if (fpNorm.toLowerCase().startsWith(root.toLowerCase())) {
                        relPath = fpNorm.slice(root.length).replace(/^\//, "");
                    } else {
                        relPath = fpNorm;
                    }
                }

                // Deduplicate: same (name + container + file)
                const dedupKey = `${si.name}\0${si.containerName ?? ""}\0${relPath}`;
                if (seen.has(dedupKey)) continue;
                seen.add(dedupKey);

                results.push({
                    name: si.name,
                    kindLabel: symbolKindLabel(si.kind),
                    file: relPath,
                    line: (si.location?.range?.start?.line ?? 0) + 1, // 0-based → 1-based
                    column: (si.location?.range?.start?.character ?? 0) + 1,
                    containerName: si.containerName,
                });
            }

            results.sort((a, b) => {
                // Group by file, then by line
                const fc = a.file.localeCompare(b.file);
                if (fc !== 0) return fc;
                return a.line - b.line;
            });

            setLspSymbolResults(results);
            setTotalMatches(results.length);
            setTruncated(false);
            setSearched(true);
            setError(null);
            // Also keep text matches empty so grouped/flatItems references are clean
            setMatches([]);
        } catch (e: any) {
            clearTimeout(timeoutId);
            const isAbort = e?.name === "AbortError" || e?.message?.includes("canceled") || e?.message?.includes("timed out");
            // LSP servers that don't support workspace/symbol (e.g. pylsp)
            // return MethodNotFound (-32601). This is NOT an error — the
            // server simply doesn't implement this capability. Fall back to
            // text search gracefully instead of showing an error.
            const isMethodNotFound = e?.code === -32601 || (e?.message && e?.message.includes("method not found")) || (e?.message && e?.message.includes("MethodNotFound"));
            if (isAbort) {
                console.log("[GlobalSearch] symbol search timed out — falling back to text search");
                setSearchMode("text");
                setError(null);
                doSearch(query);
                return; // let doSearch manage loading state
            } else if (isMethodNotFound) {
                console.log(`[GlobalSearch] LSP server does not support workspace/symbol — falling back to text search`);
                setSearchMode("text");
                setError(null);
                doSearch(query);
                return; // let doSearch manage loading state
            } else {
                console.error("[GlobalSearch] symbol search error:", e);
                setError(`Symbol search failed: ${e?.message ?? String(e)}`);
            }
            setLspSymbolResults([]);
            setTotalMatches(0);
            setSearched(true);
        }
        setLoading(false);
    }, [lspClient, workspaceRoot]);

    // Track whether user has explicitly toggled mode — if not, auto-switch
    // to symbol when LSP becomes available.
    const userToggledRef = useRef(false);

    /* ── Auto-switch mode when LSP status changes ────────────────────── */

    useEffect(() => {
        if (lspClient != null && !userToggledRef.current && searchMode === "text") {
            // LSP just became ready — auto-switch to symbol mode for code-aware search
            setSearchMode("symbol");
            setError(null);
            // Trigger immediate symbol search with current query
            if (query.trim()) {
                doSymbolSearch(query);
            }
        } else if (lspClient == null && searchMode === "symbol") {
            // LSP disconnected — fall back to text
            setSearchMode("text");
            setError(null);
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [lspClient, searchMode, query, doSymbolSearch]);

    /* ── Trigger search based on mode ────────────────────────────────── */

    // Debounced search on query change — routes to correct mode
    useEffect(() => {
        const timer = setTimeout(() => {
            if (searchMode === "symbol") {
                doSymbolSearch(query);
            } else {
                doSearch(query);
            }
        }, 200);
        return () => clearTimeout(timer);
    }, [query, searchMode, searchMode === "symbol" ? doSymbolSearch : doSearch]);

    /* Re-search immediately when toggles change (if already searched) — text mode only */
    useEffect(() => {
        if (searchMode === "text" && query.trim() && searched) {
            doSearch(query);
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [caseSensitive, wholeWord]);

    // Reset error when switching modes
    const handleModeToggle = useCallback((mode: SearchMode) => {
        userToggledRef.current = true;
        setSearchMode(mode);
        setError(null);
        // Trigger immediate search with current query
        if (query.trim()) {
            if (mode === "symbol") {
                doSymbolSearch(query);
            } else {
                doSearch(query);
            }
        }
    }, [query, doSearch, doSymbolSearch]);

    /* ── Auto-focus ──────────────────────────────────────────────────── */

    useEffect(() => {
        requestAnimationFrame(() => inputRef.current?.focus());
    }, []);

    /* ── Reset focus when results change ─────────────────────────────── */

    useEffect(() => {
        setFocusedIdx(0);
    }, [matches.length]);

    /* ── Scroll focused item into view ───────────────────────────────── */

    useEffect(() => {
        const el = itemRefs.current[focusedIdx];
        el?.scrollIntoView({ block: "nearest" });
    }, [focusedIdx]);

    /* ── Group matches/symbols by file ───────────────────────────────── */

    /** Union nav item: text match, symbol result, or file header. */
    type NavItem =
        | { kind: "file"; file: string; matchCount: number }
        | { kind: "match"; match: SearchMatch }
        | { kind: "symbol"; symbol: LspSymbolResult };

    const grouped = useMemo(() => {
        if (searchMode === "symbol") {
            const map = new Map<string, LspSymbolResult[]>();
            for (const s of lspSymbolResults) {
                const arr = map.get(s.file) || [];
                arr.push(s);
                map.set(s.file, arr);
            }
            return map;
        }
        const map = new Map<string, SearchMatch[]>();
        for (const m of matches) {
            const arr = map.get(m.file) || [];
            arr.push(m);
            map.set(m.file, arr);
        }
        return map;
    }, [searchMode, matches, lspSymbolResults]);

    const flatItems = useMemo(() => {
        const items: NavItem[] = [];
        if (searchMode === "symbol") {
            for (const [file, syms] of grouped) {
                const symsArr = syms as LspSymbolResult[];
                items.push({ kind: "file", file, matchCount: symsArr.length });
                for (const s of symsArr) {
                    items.push({ kind: "symbol", symbol: s });
                }
            }
        } else {
            for (const [file, ms] of grouped) {
                const msArr = ms as SearchMatch[];
                items.push({ kind: "file", file, matchCount: msArr.length });
                for (const m of msArr) {
                    items.push({ kind: "match", match: m });
                }
            }
        }
        return items;
    }, [grouped, searchMode]);

    /* ── Open file at match line ─────────────────────────────────────── */

    const openFileAtLine = useCallback((file: string, line: number) => {
        void useFileEditorStore.getState().openFile(agentId, workspaceId, file, line);
        onClose();
    }, [agentId, workspaceId, onClose]);

    /* ── Keyboard handler ────────────────────────────────────────────── */

    const handleKeyDown = useCallback(
        (e: React.KeyboardEvent) => {
            switch (e.key) {
                case "Escape":
                    e.preventDefault();
                    e.stopPropagation();
                    onClose();
                    break;
                case "ArrowDown":
                    e.preventDefault();
                    setFocusedIdx((i) => Math.min(i + 1, flatItems.length - 1));
                    break;
                case "ArrowUp":
                    e.preventDefault();
                    setFocusedIdx((i) => Math.max(i - 1, 0));
                    break;
                case "Enter": {
                    e.preventDefault();
                    const item = flatItems[focusedIdx];
                    if (!item) break;
                    if (item.kind === "match") {
                        openFileAtLine(item.match.file, item.match.line);
                    } else if (item.kind === "symbol") {
                        openFileAtLine(item.symbol.file, item.symbol.line);
                    } else {
                        // File header — open first item in the group
                        const firstInGroup = flatItems.slice(focusedIdx + 1).find(
                            (it) => it.kind === "match" || it.kind === "symbol",
                        );
                        if (firstInGroup) {
                            if (firstInGroup.kind === "match") {
                                openFileAtLine(firstInGroup.match.file, firstInGroup.match.line);
                            } else if (firstInGroup.kind === "symbol") {
                                openFileAtLine(firstInGroup.symbol.file, firstInGroup.symbol.line);
                            }
                        }
                    }
                    break;
                }
            }
        },
        [flatItems, focusedIdx, openFileAtLine, onClose],
    );

    /* ── Scrollbar CSS ───────────────────────────────────────────────── */

    /* ── Scrollbar CSS — inherits --ui-scrollbar-* tokens from globals.css */
    const scrollStyle = `
        .gs-list::-webkit-scrollbar { width: var(--ui-scrollbar-size); }
        .gs-list::-webkit-scrollbar-track { background: transparent; }
        .gs-list::-webkit-scrollbar-thumb {
            background: var(--ui-scrollbar-thumb);
            border-radius: 3px;
        }
        .gs-list::-webkit-scrollbar-thumb:hover {
            background: var(--ui-scrollbar-thumb-hover);
        }
    `;

    const inputKeyDown = useCallback(
        (e: React.KeyboardEvent) => {
            if (e.key === "Enter" && query.trim()) {
                e.preventDefault();
                if (searchMode === "symbol") {
                    doSymbolSearch(query);
                } else {
                    doSearch(query);
                }
            } else if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                onClose();
            } else {
                handleKeyDown(e);
            }
        },
        [query, searchMode, doSearch, doSymbolSearch, onClose, handleKeyDown],
    );

    return (
        <>
            <style>{scrollStyle}</style>
            {/* Backdrop */}
            <div
                style={{ position: "absolute", inset: 0, zIndex: 2549 }}
                onMouseDown={(e) => { e.stopPropagation(); onClose(); }}
            />
            {/* Widget */}
            <div
                style={{
                    position: "absolute",
                    top: 40,
                    left: "50%",
                    transform: "translateX(-50%)",
                    width: "60%",
                    zIndex: 2550,
                    backgroundColor: colors.widgetBg,
                    borderRadius: 6,
                    boxShadow: colors.shadow,
                    fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
                    fontSize: 13,
                    overflow: "hidden",
                    userSelect: "none",
                }}
                onKeyDown={handleKeyDown}
            >
                {/* ── Search input ──────────────────────────────────────── */}
                <div style={{ padding: "6px 6px 0 6px" }}>
                    <div style={{ position: "relative", display: "flex", alignItems: "center" }}>
                        <svg width="16" height="16" viewBox="0 0 16 16"
                            style={{ position: "absolute", left: 8, flexShrink: 0, pointerEvents: "none" }}>
                            <circle cx="6.5" cy="6.5" r="4.5" stroke={colors.description} strokeWidth="1.2" fill="none" />
                            <line x1="10" y1="10" x2="14" y2="14" stroke={colors.description} strokeWidth="1.2" />
                        </svg>
                        <input
                            ref={inputRef}
                            type="text"
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            onKeyDown={inputKeyDown}
                            placeholder={
                                searchMode === "symbol"
                                    ? "Search symbols (functions, classes, variables)..."
                                    : "Search across all files (regex)..."
                            }
                            style={{
                                flexGrow: 1,
                                height: 30,
                                padding: "0 0 0 28px",
                                backgroundColor: colors.inputBg,
                                color: colors.inputFg,
                                border: `1px solid ${colors.inputBorder}`,
                                borderRadius: 2,
                                outline: inputFocused ? `2px solid ${colors.inputFocusBorder}` : "none",
                                outlineOffset: inputFocused ? "-2px" : "initial",
                                fontSize: 13,
                                lineHeight: "28px",
                            }}
                            onFocus={() => setInputFocused(true)}
                            onBlur={() => setInputFocused(false)}
                        />
                        {/* Mode toggle + case/word toggles + count badge */}
                        <div style={{
                            position: "absolute", right: 4, display: "flex", alignItems: "center",
                            height: 30, gap: 0,
                        }}>
                            {/* Symbol / Text mode toggle — only visible when LSP is available */}
                            {lspClient != null && (
                                <button
                                    onClick={(e) => {
                                        e.preventDefault();
                                        handleModeToggle(searchMode === "symbol" ? "text" : "symbol");
                                    }}
                                    onMouseDown={(e) => e.preventDefault()}
                                    title={searchMode === "symbol"
                                        ? "Switch to text search (grep)"
                                        : "Switch to symbol search (LSP)"}
                                    style={{
                                        height: 20,
                                        display: "flex", alignItems: "center", justifyContent: "center",
                                        padding: "0 5px",
                                        border: `1px solid ${colors.inputFocusBorder}`,
                                        borderRadius: 3,
                                        backgroundColor: isDark ? "rgba(0,127,212,0.3)" : "rgba(0,144,241,0.15)",
                                        color: colors.highlight,
                                        fontSize: 10, fontWeight: 700,
                                        cursor: "pointer",
                                        lineHeight: 1,
                                        fontFamily: "inherit",
                                        marginRight: 4,
                                    }}
                                >
                                    {searchMode === "symbol" ? "#" : "Aa"}
                                </button>
                            )}
                            {/* Match Case (Aa) — text mode only */}
                            {searchMode === "text" && (
                                <button
                                    onClick={(e) => { e.preventDefault(); setCaseSensitive((p) => !p); }}
                                    onMouseDown={(e) => e.preventDefault()}

                                    style={{
                                        width: 20, height: 20,
                                        display: "flex", alignItems: "center", justifyContent: "center",
                                        border: caseSensitive
                                            ? `1px solid ${colors.inputFocusBorder}`
                                            : "1px solid transparent",
                                        borderRadius: 3,
                                        backgroundColor: caseSensitive
                                            ? isDark ? "rgba(0,127,212,0.3)" : "rgba(0,144,241,0.15)"
                                            : "transparent",
                                        color: caseSensitive ? colors.highlight : colors.description,
                                        fontSize: 11, fontWeight: 600,
                                        cursor: "pointer",
                                        padding: 0, lineHeight: 1,
                                        fontFamily: "inherit",
                                    }}
                                >
                                    Aa
                                </button>
                            )}
                            {/* Match Whole Word (ab) — text mode only */}
                            {searchMode === "text" && (
                                <button
                                    onClick={(e) => { e.preventDefault(); setWholeWord((p) => !p); }}
                                    onMouseDown={(e) => e.preventDefault()}
                                    style={{
                                        width: 20, height: 20,
                                        display: "flex", alignItems: "center", justifyContent: "center",
                                        border: wholeWord
                                            ? `1px solid ${colors.inputFocusBorder}`
                                            : "1px solid transparent",
                                        borderRadius: 3,
                                        backgroundColor: wholeWord
                                            ? isDark ? "rgba(0,127,212,0.3)" : "rgba(0,144,241,0.15)"
                                            : "transparent",
                                        color: wholeWord ? colors.highlight : colors.description,
                                        fontSize: 11, fontWeight: 600,
                                        cursor: "pointer",
                                        padding: 0, lineHeight: 1,
                                        fontFamily: "inherit",
                                    }}
                                >
                                    ab
                                </button>
                            )}
                            {/* Result count */}
                            {searched && !loading && (
                                <span style={{
                                    padding: "2px 4px", borderRadius: 2,
                                    fontSize: 11, lineHeight: "normal",
                                    backgroundColor: colors.countBg, color: colors.countFg,
                                    marginLeft: 4,
                                }}>
                                    {totalMatches}
                                </span>
                            )}
                        </div>
                    </div>
                    {/* File filter (collapsed row) */}
                    <div style={{ padding: "4px 0 2px 0" }}>
                        <input
                            type="text"
                            value={fileFilter}
                            onChange={(e) => setFileFilter(e.target.value)}
                            placeholder="files to include (e.g. *.rs,*.toml)"
                            style={{
                                width: "100%",
                                height: 24,
                                padding: "0 8px",
                                backgroundColor: colors.inputBg,
                                color: colors.inputFg,
                                border: `1px solid ${colors.inputBorder}`,
                                borderRadius: 2,
                                fontSize: 11,
                                outline: "none",
                            }}
                        />
                    </div>
                </div>

                {/* ── Results list ──────────────────────────────────────── */}
                <div
                    className="gs-list"
                    style={{ maxHeight: 20 * 22, overflowY: "auto", lineHeight: "22px", paddingBottom: 5 }}
                >
                    {loading ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            Searching...
                        </div>
                    ) : error ? (
                        <div style={{ padding: "8px 12px", color: "#E74856", fontSize: 12 }}>
                            {error}
                        </div>
                    ) : !searched ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            Type a search query and press Enter
                        </div>
                    ) : (searchMode === "symbol" ? lspSymbolResults.length === 0 : matches.length === 0) ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            No results found{truncated ? " (results truncated)" : ""}
                        </div>
                    ) : (
                        /* Render file groups — mode-aware */
                        flatItems.map((item, idx) => {
                            if (item.kind === "file") {
                                const fileName = item.file.split("/").pop() || item.file;
                                const dirPath = item.file.includes("/") ? item.file.slice(0, item.file.lastIndexOf("/")) : "";
                                return (
                                    <div key={`file-${item.file}`}>
                                        <div style={{
                                            display: "flex", alignItems: "center",
                                            padding: "2px 10px 0 10px", height: 22,
                                            color: colors.description, fontSize: 11, fontWeight: 600,
                                        }}>
                                            <SetiIcon {...getFileIcon(fileName)} size={16} />
                                            <span style={{ marginLeft: 5, color: colors.highlight, fontWeight: 700 }}>
                                                {fileName}
                                            </span>
                                            {dirPath && (
                                                <span style={{ marginLeft: 6, opacity: 0.7 }}>{dirPath}</span>
                                            )}
                                            <span style={{ marginLeft: "auto", opacity: 0.6 }}>
                                                {item.matchCount}
                                            </span>
                                        </div>
                                    </div>
                                );
                            }
                            const focused = idx === focusedIdx;
                            if (item.kind === "match") {
                                const m = item.match;
                                return (
                                    <div
                                        key={`${m.file}:${m.line}`}
                                        ref={(el) => { itemRefs.current[idx] = el; }}
                                        onMouseEnter={() => setFocusedIdx(idx)}
                                        onClick={() => openFileAtLine(m.file, m.line)}
                                        style={{
                                            display: "flex", alignItems: "center",
                                            padding: "0 6px", height: 22,
                                            cursor: "pointer",
                                            backgroundColor: focused ? colors.listFocusBg : "transparent",
                                            color: focused ? colors.listFocusFg : colors.inputFg,
                                            borderRadius: 3, margin: "0 6px",
                                        }}
                                    >
                                        <span style={{
                                            minWidth: 36, textAlign: "right", fontSize: 11,
                                            opacity: focused ? 0.9 : 0.6, marginRight: 6, flexShrink: 0,
                                        }}>
                                            {m.line}
                                        </span>
                                        <span style={{
                                            overflow: "hidden", textOverflow: "ellipsis",
                                            whiteSpace: "nowrap", fontSize: 12,
                                        }}>
                                            {m.text}
                                        </span>
                                    </div>
                                );
                            }
                            // Symbol result rendering
                            const s = item.symbol;
                            return (
                                <div
                                    key={`${s.file}:${s.line}:${s.name}`}
                                    ref={(el) => { itemRefs.current[idx] = el; }}
                                    onMouseEnter={() => setFocusedIdx(idx)}
                                    onClick={() => openFileAtLine(s.file, s.line)}
                                    style={{
                                        display: "flex", alignItems: "center",
                                        padding: "0 6px", height: 22,
                                        cursor: "pointer",
                                        backgroundColor: focused ? colors.listFocusBg : "transparent",
                                        color: focused ? colors.listFocusFg : colors.inputFg,
                                        borderRadius: 3, margin: "0 6px",
                                    }}
                                >
                                    {/* Symbol kind badge */}
                                    <span style={{
                                        minWidth: 42, fontSize: 10, fontWeight: 600,
                                        color: colors.highlight, opacity: focused ? 1 : 0.8,
                                        marginRight: 6, flexShrink: 0, textAlign: "right",
                                    }}>
                                        {s.kindLabel}
                                    </span>
                                    {/* Symbol name */}
                                    <span style={{
                                        fontWeight: 600, fontSize: 12,
                                        overflow: "hidden", textOverflow: "ellipsis",
                                        whiteSpace: "nowrap", flexShrink: 1,
                                    }}>
                                        {s.name}
                                    </span>
                                    {/* Container name */}
                                    {s.containerName && (
                                        <span style={{
                                            marginLeft: 4, fontSize: 11, opacity: 0.6,
                                            overflow: "hidden", textOverflow: "ellipsis",
                                            whiteSpace: "nowrap", flexShrink: 1,
                                        }}>
                                            {s.containerName}
                                        </span>
                                    )}
                                </div>
                            );
                        })
                    )}
                    {truncated && (searchMode === "symbol" ? lspSymbolResults.length > 0 : matches.length > 0) && (
                        <div style={{ padding: "4px 12px", color: colors.description, fontSize: 11, fontStyle: "italic" }}>
                            Results truncated — {totalMatches} total matches
                        </div>
                    )}
                </div>

                {/* ── Footer ────────────────────────────────────────────── */}
                <div style={{
                    padding: "2px 10px 4px 10px",
                    fontSize: 10,
                    color: colors.description,
                    borderTop: `1px solid ${isDark ? "#3C3C3C" : "#E0E0E0"}`,
                    display: "flex",
                    gap: 12,
                }}>
                    <span>↑↓ navigate</span>
                    <span>Enter open</span>
                    <span>Esc close</span>
                </div>
            </div>
        </>
    );
}
