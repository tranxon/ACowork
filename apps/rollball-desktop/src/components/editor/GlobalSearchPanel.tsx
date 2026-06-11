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
 * Data source: calls Gateway search API (ignore crate = ripgrep backend).
 */

import { useState, useEffect, useRef, useMemo, useCallback } from "react";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "../workspace/FileTree/fileIcons";

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

interface GlobalSearchPanelProps {
    agentId: string;
    workspaceId: string;
    onClose: () => void;
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

export function GlobalSearchPanel({ agentId, workspaceId, onClose }: GlobalSearchPanelProps) {
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
        if (!q.trim() || !agentId) {
            setMatches([]);
            setTotalMatches(0);
            setTruncated(false);
            setSearched(false);
            return;
        }
        setLoading(true);
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
            const resp = await fetch(url);
            if (!resp.ok) {
                console.error("[GlobalSearch] search failed:", resp.status);
                setMatches([]);
                setTotalMatches(0);
                setSearched(true);
                setLoading(false);
                return;
            }
            const data = (await resp.json()) as SearchResponse;
            setMatches(data.matches);
            setTotalMatches(data.totalMatches);
            setTruncated(data.truncated);
            setSearched(true);
        } catch (e) {
            console.error("[GlobalSearch] search error:", e);
            setMatches([]);
            setSearched(true);
        } finally {
            setLoading(false);
        }
    }, [agentId, workspaceId, fileFilter, caseSensitive, wholeWord]);

    /* ── Auto-focus ──────────────────────────────────────────────────── */

    useEffect(() => {
        requestAnimationFrame(() => inputRef.current?.focus());
    }, []);

    /* ── Debounced search on query change ────────────────────────────── */

    useEffect(() => {
        const timer = setTimeout(() => doSearch(query), 200);
        return () => clearTimeout(timer);
    }, [query, doSearch]);

    /* Re-search immediately when toggles change (if already searched) */

    useEffect(() => {
        if (query.trim() && searched) {
            doSearch(query);
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [caseSensitive, wholeWord]);

    /* ── Reset focus when matches change ─────────────────────────────── */

    useEffect(() => {
        setFocusedIdx(0);
    }, [matches.length]);

    /* ── Scroll focused item into view ───────────────────────────────── */

    useEffect(() => {
        const el = itemRefs.current[focusedIdx];
        el?.scrollIntoView({ block: "nearest" });
    }, [focusedIdx]);

    /* ── Group matches by file ───────────────────────────────────────── */

    const grouped = useMemo(() => {
        const map = new Map<string, SearchMatch[]>();
        for (const m of matches) {
            const arr = map.get(m.file) || [];
            arr.push(m);
            map.set(m.file, arr);
        }
        return map;
    }, [matches]);

    /* ── Flatten for keyboard nav (file header + matches) ────────────── */

    type NavItem =
        | { kind: "file"; file: string; matchCount: number }
        | { kind: "match"; match: SearchMatch };

    const flatItems = useMemo(() => {
        const items: NavItem[] = [];
        for (const [file, ms] of grouped) {
            items.push({ kind: "file", file, matchCount: ms.length });
            for (const m of ms) {
                items.push({ kind: "match", match: m });
            }
        }
        return items;
    }, [grouped]);

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
                    } else {
                        // File header — expand/collapse (or open first match)
                        const fileMatches = grouped.get(item.file);
                        if (fileMatches && fileMatches.length > 0) {
                            openFileAtLine(fileMatches[0].file, fileMatches[0].line);
                        }
                    }
                    break;
                }
            }
        },
        [flatItems, focusedIdx, grouped, openFileAtLine, onClose],
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
            // Enter in input triggers first search
            if (e.key === "Enter" && query.trim()) {
                e.preventDefault();
                doSearch(query);
            } else if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                onClose();
            } else {
                handleKeyDown(e);
            }
        },
        [query, doSearch, onClose, handleKeyDown],
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
                            placeholder="Search across all files (regex)..."
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
                        {/* Toggle buttons + count badge */}
                        <div style={{
                            position: "absolute", right: 4, display: "flex", alignItems: "center",
                            height: 30, gap: 0,
                        }}>
                            {/* Match Case (Aa) */}
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
                            {/* Match Whole Word (ab) */}
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
                    ) : !searched ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            Type a search query and press Enter
                        </div>
                    ) : matches.length === 0 ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            No results found{truncated ? " (results truncated)" : ""}
                        </div>
                    ) : (
                        /* Render file groups with match rows */
                        Array.from(grouped.entries()).map(([file, fileMatches]) => {
                            const fileName = file.split("/").pop() || file;
                            const dirPath = file.includes("/") ? file.slice(0, file.lastIndexOf("/")) : "";
                            return (
                                <div key={file}>
                                    {/* File header */}
                                    <div style={{
                                        display: "flex",
                                        alignItems: "center",
                                        padding: "2px 10px 0 10px",
                                        height: 22,
                                        color: colors.description,
                                        fontSize: 11,
                                        fontWeight: 600,
                                    }}>
                                        <SetiIcon {...getFileIcon(fileName)} size={16} />
                                        <span style={{ marginLeft: 5, color: colors.highlight, fontWeight: 700 }}>
                                            {fileName}
                                        </span>
                                        {dirPath && (
                                            <span style={{ marginLeft: 6, opacity: 0.7 }}>
                                                {dirPath}
                                            </span>
                                        )}
                                        <span style={{ marginLeft: "auto", opacity: 0.6 }}>
                                            {fileMatches.length}
                                        </span>
                                    </div>
                                    {/* Match rows */}
                                    {fileMatches.map((m) => {
                                        const globalIdx = flatItems.findIndex(
                                            (it) => it.kind === "match" && it.match === m,
                                        );
                                        const focused = globalIdx === focusedIdx;
                                        return (
                                            <div
                                                key={`${file}:${m.line}`}
                                                ref={(el) => { itemRefs.current[globalIdx] = el; }}
                                                onMouseEnter={() => setFocusedIdx(globalIdx)}
                                                onClick={() => openFileAtLine(m.file, m.line)}
                                                style={{
                                                    display: "flex",
                                                    alignItems: "center",
                                                    padding: "0 6px",
                                                    height: 22,
                                                    cursor: "pointer",
                                                    backgroundColor: focused ? colors.listFocusBg : "transparent",
                                                    color: focused ? colors.listFocusFg : colors.inputFg,
                                                    borderRadius: 3,
                                                    margin: "0 6px",
                                                }}
                                            >
                                                {/* Line number */}
                                                <span style={{
                                                    minWidth: 36,
                                                    textAlign: "right",
                                                    fontSize: 11,
                                                    opacity: focused ? 0.9 : 0.6,
                                                    marginRight: 6,
                                                    flexShrink: 0,
                                                }}>
                                                    {m.line}
                                                </span>
                                                {/* Match text */}
                                                <span style={{
                                                    overflow: "hidden",
                                                    textOverflow: "ellipsis",
                                                    whiteSpace: "nowrap",
                                                    fontSize: 12,
                                                }}>
                                                    {m.text}
                                                </span>
                                            </div>
                                        );
                                    })}
                                </div>
                            );
                        })
                    )}
                    {truncated && matches.length > 0 && (
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
