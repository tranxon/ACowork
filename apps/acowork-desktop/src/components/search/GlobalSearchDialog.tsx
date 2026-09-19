/**
 * GlobalSearchDialog — ADR-081 application-wide search (Ctrl+Shift+F).
 *
 * Layout (3-zone dialog matching CloneDialog / PublishWizard chrome):
 *
 *   ┌─ Header  border-b  🔍 全局搜索                          ─┐
 *   ├─ Zone 1  border-b  [AgentPickerChip]│  🔍 input  [Aa][\b]─┤
 *   │                       (avatar only)   (search row)        │
 *   ├─ Zone 2  border-b?  [文件][git][doc][project][对话][记忆] ┤
 *   │                       ─────                               │
 *   │                       result rows (flex-1)                │
 *   ├─ Zone 3 (if hit focused)  PreviewPane                     ┤
 *   ├─ Footer border-t                              [关闭]      ┤
 *   └────────────────────────────────────────────────────────────┘
 *
 * Interaction:
 *   - Single click / Arrow keys → focused hit → preview pane renders
 *   - Double click / Enter       → activate the hit (open file, etc.)
 *   - Escape                    → close dialog
 *
 * Data ownership follows ADR-081 D1: per-agent sources (file/git) are
 * queried on the owning Agent Runtime via the Gateway reverse proxy;
 * doc/pm are global sources queried via their Gateway reverse proxies.
 *
 * - 文件 tab  : GET /api/agents/{id}/workspaces/search (ripgrep) → openFile
 * - git tab   : GET /api/agents/{id}/git/log + CommitPicker-style
 *               client-side subject/author/hash filter → virtual log file
 * - 文档 tab  : GET /api/doc/search (doc process) → DocEditor
 * - 项目 tab  : GET /api/pm/search (pm process) → ProjectBoard / TaskDetailDrawer
 */

import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import { BookOpen, KanbanSquare, MessageSquare, Search } from "lucide-react";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useAgentStore } from "../../stores/agentStore";
import { useSearchStore } from "../../stores/searchStore";
import { useGitStore, type GitCommitDto } from "../../stores/gitStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useMemoryStore } from "../../stores/memoryStore";
import { useChatStore } from "../../stores/chatStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { searchDocs } from "../../lib/doc-api";
import { searchPm, type PmSearchHit } from "../../lib/pm-api";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "../workspace/FileTree/fileIcons";
import { log } from "../../lib/logger";
import { useTranslation } from "../../i18n/useTranslation";
import { AgentPickerChip } from "./AgentPickerChip";
import { PreviewPane } from "./PreviewPane";

/* ─── Types ─────────────────────────────────────────────────────────── */

export type Tab = "file" | "git" | "doc" | "project" | "conversation" | "memory";

export interface SearchMatch {
    file: string;
    line: number;
    column: number;
    text: string;
}

export interface SearchResponse {
    matches: SearchMatch[];
    totalMatches: number;
    truncated: boolean;
}

/** ADR-081 §4.1 Runtime `/search` aggregate hit (Gateway reverse proxy). */
export interface GlobalSearchHit {
    scope: string;
    title: string;
    snippet: string;
    score: number;
    payload: {
        node_id?: number;
        node_type?: string;
        sub_type?: string | null;
        file?: string;
        line?: number;
        hash?: string;
        session_id?: string;
        message_index?: number;
        role?: string;
    };
}

export interface GlobalSearchResponse {
    hits: GlobalSearchHit[];
    scopes: Record<string, { status: string; count?: number }>;
    indexing: boolean;
}

/** ADR-081 D4 unified row. */
export interface SearchHit {
    type: Tab;
    id: string;
    title: string;
    snippet: string;
    /** Secondary line (path / commit meta). */
    sub: string;
}

/** File hits carry the ADR-081 D4 `meta` (locate info) on the row. */
export interface FileHit extends SearchHit {
    agentId: string;
    workspaceId?: string;
    path: string;
    line: number;
}

export interface DocHit extends SearchHit {
    docId: string;
}

export interface ProjectHit extends SearchHit {
    projectId: string;
    taskId?: string;
}

export interface MemoryHit extends SearchHit {
    nodeId: number;
    nodeType: string;
}

/** Conversation hits: one row per indexed message (ADR-081 §4.2). */
export interface ConversationHit extends SearchHit {
    sessionId: string;
    messageIndex: number;
    role: string;
}

export type RowHit =
    | FileHit
    | DocHit
    | ProjectHit
    | MemoryHit
    | ConversationHit
    | SearchHit;

export interface GlobalSearchDialogProps {
    /** Switch the app-level view (projects/docs/chat) when a hit is opened. */
    onNavigate: (view: "projects" | "docs" | "chat") => void;
}

/* ─── Constants ─────────────────────────────────────────────────────── */

/** Lazy commits beyond the newest page can't be hit by the client-side
 *  filter — same ceiling CommitPicker accepts per page. Pagination chrome
 *  inside the dialog is a later phase (P0-2+). */
export const GIT_LOG_LIMIT = 100;
const FILE_MAX_RESULTS = 50;
const GLOBAL_SOURCE_LIMIT = 20;

/* ─── Pure helpers (unit-tested) ────────────────────────────────────── */

/** Case-insensitive keyword highlighting for result rows (ADR-081 4.5). */
function highlightParts(text: string, query: string): { text: string; hit: boolean }[] {
    const q = query.trim();
    if (!q) return [{ text, hit: false }];
    const lower = text.toLowerCase();
    const needle = q.toLowerCase();
    const parts: { text: string; hit: boolean }[] = [];
    let i = 0;
    let idx = lower.indexOf(needle, i);
    while (idx !== -1 && parts.length < 20) {
        if (idx > i) parts.push({ text: text.slice(i, idx), hit: false });
        parts.push({ text: text.slice(idx, idx + q.length), hit: true });
        i = idx + q.length;
        idx = lower.indexOf(needle, i);
    }
    if (i < text.length) parts.push({ text: text.slice(i), hit: false });
    return parts.length ? parts : [{ text, hit: false }];
}

function Highlight({ text, query }: { text: string; query: string }) {
    return (
        <>
            {highlightParts(text, query).map((p, i) =>
                p.hit ? (
                    <mark
                        key={i}
                        className="rounded-sm bg-[var(--color-accent)]/25 px-0.5 text-[var(--color-accent)]"
                    >
                        {p.text}
                    </mark>
                ) : (
                    <span key={i}>{p.text}</span>
                ),
            )}
        </>
    );
}

/** CommitPicker-compatible client-side commit filter (subject/author/hash). */
export function filterGitCommits(commits: GitCommitDto[], q: string): GitCommitDto[] {
    const term = q.trim().toLowerCase();
    if (!term) return commits;
    return commits.filter(
        (c) =>
            c.subject.toLowerCase().includes(term) ||
            c.author.toLowerCase().includes(term) ||
            c.shortHash.toLowerCase().startsWith(term) ||
            c.hash.toLowerCase().startsWith(term),
    );
}

export function toFileHits(agentId: string, workspaceId: string | undefined, matches: SearchMatch[]): FileHit[] {
    return matches.map((m) => {
        const fileName = m.file.split("/").pop() ?? m.file;
        return {
            type: "file",
            id: `${m.file}:${m.line}`,
            title: fileName,
            snippet: m.text,
            sub: m.file,
            agentId,
            workspaceId,
            path: m.file,
            line: m.line,
        };
    });
}

function toGitHits(commits: GitCommitDto[]): SearchHit[] {
    return commits.map((c) => ({
        type: "git",
        id: c.hash,
        title: c.subject,
        snippet: `${c.shortHash}  ${c.author}  ${c.date}`,
        sub: c.subject,
    }));
}

function toDocHits(hits: { doc_id: string; name: string; path: string; snippet: string }[]): DocHit[] {
    return hits.map((h) => ({
        type: "doc",
        id: h.doc_id,
        title: h.name,
        snippet: h.snippet,
        sub: h.path,
        docId: h.doc_id,
    }));
}

function toProjectHits(hits: PmSearchHit[]): ProjectHit[] {
    return hits.map((h) => ({
        type: "project",
        id: h.id,
        title: h.title,
        snippet: h.snippet,
        sub: h.kind === "task" ? `task · ${h.project_id}` : `project · ${h.project_id}`,
        projectId: h.project_id,
        taskId: h.task_id,
    }));
}

export function toMemoryHits(hits: GlobalSearchHit[]): MemoryHit[] {
    return hits.map((h) => ({
        type: "memory",
        id: String(h.payload.node_id ?? h.title),
        title: h.title,
        snippet: h.snippet,
        sub: h.payload.sub_type ? `${h.title} · ${h.payload.sub_type}` : h.title,
        nodeId: h.payload.node_id ?? 0,
        nodeType: h.title,
    }));
}

/** ADR-081 §4.2: Runtime `/search` conversation-scope hits → rows. */
export function toConversationHits(hits: GlobalSearchHit[]): ConversationHit[] {
    return hits
        .filter((h) => h.payload.session_id)
        .map((h) => ({
            type: "conversation",
            id: `${h.payload.session_id}:${h.payload.message_index ?? 0}`,
            title: h.payload.session_id!,
            snippet: h.snippet,
            sub: `${h.payload.role ?? ""} · #${h.payload.message_index ?? 0}`,
            sessionId: h.payload.session_id!,
            messageIndex: h.payload.message_index ?? 0,
            role: h.payload.role ?? "",
        }));
}

/* ─── Component ─────────────────────────────────────────────────────── */

export function GlobalSearchDialog({ onNavigate }: GlobalSearchDialogProps) {
    const { t } = useTranslation();
    const open = useSearchStore((s) => s.open);
    const closeDialog = useSearchStore((s) => s.closeDialog);

    const [query, setQuery] = useState("");
    const [tab, setTab] = useState<Tab>("file");
    const [agentId, setAgentId] = useState<string>("");
    const [caseSensitive, setCaseSensitive] = useState(false);
    const [wholeWord, setWholeWord] = useState(false);

    const [fileHits, setFileHits] = useState<FileHit[]>([]);
    const [fileTotal, setFileTotal] = useState(0);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const [commits, setCommits] = useState<GitCommitDto[]>([]);
    const [gitLoaded, setGitLoaded] = useState(false);
    const [gitError, setGitError] = useState<string | null>(null);

    const [docHits, setDocHits] = useState<DocHit[]>([]);
    const [docLoading, setDocLoading] = useState(false);
    const [docError, setDocError] = useState<string | null>(null);

    const [projHits, setProjHits] = useState<ProjectHit[]>([]);
    const [projLoading, setProjLoading] = useState(false);
    const [projError, setProjError] = useState<string | null>(null);

    const [memoryHits, setMemoryHits] = useState<MemoryHit[]>([]);
    const [memoryLoading, setMemoryLoading] = useState(false);
    const [memoryError, setMemoryError] = useState<string | null>(null);

    const [conversationHits, setConversationHits] = useState<ConversationHit[]>([]);
    const [conversationLoading, setConversationLoading] = useState(false);
    const [conversationError, setConversationError] = useState<string | null>(null);

    const [focusedIdx, setFocusedIdx] = useState(0);
    const inputRef = useRef<HTMLInputElement>(null);
    const abortRef = useRef<AbortController | null>(null);
    const seqRef = useRef(0);

    const agents = useAgentStore((s) => s.agents);
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
    const agentList = useMemo(
        () =>
            Object.values(agents).filter((a) => a.meta.alive),
        [agents],
    );

    // Default agent scope = currently selected agent (P0 single-agent).
    useEffect(() => {
        if (open && !agentId) {
            setAgentId(selectedAgentId ?? agentList[0]?.meta.instance_id ?? "");
        }
    }, [open, agentId, selectedAgentId, agentList]);

    // Workspace scope = the active session's selected workspace — the same
    // source as the right-side file tree / git banner (WorkspaceExplorer).
    // NOT the open editor file: a global search with no file open would
    // otherwise fall back to the agent home instead of the user's workspace.
    // "__agent_home__" is normalised to `undefined` so the file/git requests
    // omit `workspace_id` and the Runtime resolves the agent home itself.
    const activeSessionId = useChatStore((s) =>
        agentId ? s.getActiveSessionId(agentId) : null,
    );
    const workspaceId = useWorkspaceStore((s) => {
        if (!activeSessionId) return undefined;
        const ws = s.sessionWorkspaceMap[activeSessionId];
        return !ws || ws === "__agent_home__" ? undefined : ws;
    });

    /* ── Reset when opened ──────────────────────────────────────────── */
    useEffect(() => {
        if (open) {
            setQuery("");
            setFileHits([]);
            setFileTotal(0);
            setError(null);
            setGitLoaded(false);
            setCommits([]);
            setGitError(null);
            setDocHits([]);
            setDocError(null);
            setProjHits([]);
            setProjError(null);
            setMemoryHits([]);
            setMemoryError(null);
            setConversationHits([]);
            setConversationError(null);
            setFocusedIdx(0);
            requestAnimationFrame(() => inputRef.current?.focus());
        } else {
            abortRef.current?.abort();
            seqRef.current += 1;
        }
    }, [open]);

    /* ── File tab: ripgrep via Runtime (Gateway reverse proxy) ──────── */
    const searchFiles = useCallback(
        async (q: string) => {
            if (abortRef.current) abortRef.current.abort();
            if (!q.trim() || !agentId) {
                setFileHits([]);
                setFileTotal(0);
                setLoading(false);
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
                if (workspaceId) params.set("workspace_id", workspaceId);
                if (caseSensitive) params.set("case_sensitive", "true");
                if (wholeWord) params.set("whole_word", "true");
                params.set("max_results", String(FILE_MAX_RESULTS));
                const url = `${baseUrl}/api/agents/${agentId}/workspaces/search?${params.toString()}`;

                const timeoutId = setTimeout(() => controller.abort(), 30_000);
                const resp = await fetch(url, { signal: controller.signal });
                clearTimeout(timeoutId);
                if (!resp.ok) {
                    setError(`Server error (${resp.status})`);
                    setFileHits([]);
                    setFileTotal(0);
                    return;
                }
                const data = (await resp.json()) as SearchResponse;
                setFileHits(toFileHits(agentId, workspaceId, data.matches));
                setFileTotal(data.totalMatches);
            } catch (e: unknown) {
                if ((e as Error)?.name === "AbortError") {
                    setError(t("globalSearch.errorFileTimeout"));
                } else {
                    log.error("[GlobalSearch] file search error:", e);
                    setError(t("globalSearch.errorFileFailed"));
                }
                setFileHits([]);
                setFileTotal(0);
            } finally {
                setLoading(false);
            }
        },
        [agentId, workspaceId, caseSensitive, wholeWord, t],
    );

    /* ── Doc tab: doc process /search (Gateway reverse proxy) ───────── */
    const searchDocsTab = useCallback(async (q: string) => {
        const seq = ++seqRef.current;
        const controller = new AbortController();
        abortRef.current = controller;
        setDocLoading(true);
        setDocError(null);
        try {
            const hits = await searchDocs(q, GLOBAL_SOURCE_LIMIT, controller.signal);
            if (seq === seqRef.current) setDocHits(toDocHits(hits));
        } catch (e: unknown) {
            if ((e as Error)?.name !== "AbortError" && seq === seqRef.current) {
                setDocError(t("globalSearch.errorDocService"));
                log.error("[GlobalSearch] doc search error:", e);
            }
        } finally {
            if (seq === seqRef.current) setDocLoading(false);
        }
    }, [t]);

    /* ── Project tab: pm process /search (Gateway reverse proxy) ────── */
    const searchProjectTab = useCallback(async (q: string) => {
        const seq = ++seqRef.current;
        const controller = new AbortController();
        abortRef.current = controller;
        setProjLoading(true);
        setProjError(null);
        try {
            const hits = await searchPm(q, GLOBAL_SOURCE_LIMIT, controller.signal);
            if (seq === seqRef.current) setProjHits(toProjectHits(hits));
        } catch (e: unknown) {
            if ((e as Error)?.name !== "AbortError" && seq === seqRef.current) {
                setProjError(t("globalSearch.errorProjectService"));
                log.error("[GlobalSearch] project search error:", e);
            }
        } finally {
            if (seq === seqRef.current) setProjLoading(false);
        }
    }, [t]);

    /* ── Memory tab: Runtime /search?scopes=memory (ADR-081 P1-1) ───── */
    const searchMemoryTab = useCallback(
        async (q: string) => {
            const seq = ++seqRef.current;
            const controller = new AbortController();
            abortRef.current = controller;
            setMemoryLoading(true);
            setMemoryError(null);
            try {
                const baseUrl = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
                const params = new URLSearchParams();
                params.set("q", q);
                params.set("scopes", "memory");
                params.set("mode", "hybrid");
                params.set("limit", String(GLOBAL_SOURCE_LIMIT));
                const url = `${baseUrl}/api/agents/${agentId}/search?${params.toString()}`;
                const timeoutId = setTimeout(() => controller.abort(), 30_000);
                const resp = await fetch(url, { signal: controller.signal });
                clearTimeout(timeoutId);
                if (!resp.ok) throw new Error(`status ${resp.status}`);
                const data = (await resp.json()) as GlobalSearchResponse;
                if (seq === seqRef.current) setMemoryHits(toMemoryHits(data.hits ?? []));
            } catch (e: unknown) {
                if ((e as Error)?.name !== "AbortError" && seq === seqRef.current) {
                    setMemoryError(t("globalSearch.errorMemory"));
                    log.error("[GlobalSearch] memory search error:", e);
                    setMemoryHits([]);
                }
            } finally {
                if (seq === seqRef.current) setMemoryLoading(false);
            }
        },
        [agentId, t],
    );

    /* ── Conversation tab: Runtime /search?scopes=conversation (P1-2) ─ */
    const searchConversationTab = useCallback(
        async (q: string) => {
            const seq = ++seqRef.current;
            const controller = new AbortController();
            abortRef.current = controller;
            setConversationLoading(true);
            setConversationError(null);
            try {
                const baseUrl = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
                const params = new URLSearchParams();
                params.set("q", q);
                params.set("scopes", "conversation");
                params.set("mode", "hybrid");
                params.set("limit", String(GLOBAL_SOURCE_LIMIT));
                const url = `${baseUrl}/api/agents/${agentId}/search?${params.toString()}`;
                const timeoutId = setTimeout(() => controller.abort(), 30_000);
                const resp = await fetch(url, { signal: controller.signal });
                clearTimeout(timeoutId);
                if (!resp.ok) throw new Error(`status ${resp.status}`);
                const data = (await resp.json()) as GlobalSearchResponse;
                if (seq === seqRef.current) setConversationHits(toConversationHits(data.hits ?? []));
            } catch (e: unknown) {
                if ((e as Error)?.name !== "AbortError" && seq === seqRef.current) {
                    setConversationError(t("globalSearch.errorConversation"));
                    log.error("[GlobalSearch] conversation search error:", e);
                    setConversationHits([]);
                }
            } finally {
                if (seq === seqRef.current) setConversationLoading(false);
            }
        },
        [agentId, t],
    );

    /* ── Git tab: fetch log on demand (no debounce — list is small) ─── */
    useEffect(() => {
        if (!open || tab !== "git" || gitLoaded || !agentId) return;
        let cancelled = false;
        setGitError(null);
        void useGitStore
            .getState()
            // Repo-wide history (empty path = no `-- <path>` filter), same
            // as the workspace git banner's CommitPicker. Global search is
            // not scoped to the open file.
            .fetchLog(agentId, workspaceId ?? "", "", GIT_LOG_LIMIT, 0)
            .then((resp) => {
                if (cancelled) return;
                setCommits(resp.commits);
                setGitLoaded(true);
            })
            .catch((err: unknown) => {
                if (cancelled) return;
                log.error("[GlobalSearch] git log error:", err);
                setGitError(t("globalSearch.errorGit"));
                setGitLoaded(true);
            });
        return () => {
            cancelled = true;
        };
    }, [open, tab, gitLoaded, agentId, workspaceId, t]);

    const gitHits = useMemo(() => toGitHits(filterGitCommits(commits, query)), [commits, query]);

    /* ── Debounced search (300ms) on query change ───────────────────── */
    useEffect(() => {
        if (!open || tab === "git") return;
        if (!query.trim()) {
            if (tab === "doc") setDocHits([]);
            if (tab === "project") setProjHits([]);
            if (tab === "memory") setMemoryHits([]);
            if (tab === "conversation") setConversationHits([]);
            return;
        }
        const timer = setTimeout(() => {
            if (tab === "file") void searchFiles(query);
            else if (tab === "doc") void searchDocsTab(query);
            else if (tab === "project") void searchProjectTab(query);
            else if (tab === "memory") void searchMemoryTab(query);
            else if (tab === "conversation") void searchConversationTab(query);
        }, 300);
        return () => clearTimeout(timer);
    }, [open, tab, query, searchFiles, searchDocsTab, searchProjectTab, searchMemoryTab, searchConversationTab]);

    /* ── Reset focus when results change ────────────────────────────── */
    const hits: RowHit[] =
        tab === "file" ? fileHits
        : tab === "git" ? gitHits
        : tab === "doc" ? docHits
        : tab === "project" ? projHits
        : tab === "conversation" ? conversationHits
        : memoryHits;
    useEffect(() => {
        setFocusedIdx(0);
    }, [hits.length, tab]);

    /* ── Actions ────────────────────────────────────────────────────── */
    const openFileHit = useCallback(
        (h: FileHit) => {
            void useFileEditorStore
                .getState()
                .openFile(h.agentId, h.workspaceId ?? "", h.path, h.line);
            closeDialog();
        },
        [closeDialog],
    );

    const openGitHit = useCallback(() => {
        const st = useFileEditorStore.getState();
        st.openVirtualFile({
            agentId,
            workspaceId: workspaceId ?? "",
            kind: "log",
            relPath: "",
            content:
                commits.map((c) => `${c.shortHash}  ${c.author}  ${c.date}\n    ${c.subject}`).join("\n\n") ||
                "No commits",
            language: "plaintext",
            loadedCommits: commits,
            displayedLimit: commits.length,
            reachedEnd: commits.length < GIT_LOG_LIMIT,
        });
        closeDialog();
    }, [agentId, workspaceId, commits, closeDialog]);

    const openDocHit = useCallback(
        (h: DocHit) => {
            void useDocEditorStore.getState().requestOpen(h.docId);
            onNavigate("docs");
            closeDialog();
        },
        [onNavigate, closeDialog],
    );

    const openProjectHit = useCallback(
        (h: ProjectHit) => {
            const pst = usePmProjectStore.getState();
            const ensure = () => {
                if (!pst.projects.some((p) => p.id === h.projectId)) {
                    return pst.loadProjects({ silent: false }).then(() => {
                        usePmProjectStore.getState().selectProject(h.projectId);
                    });
                }
                pst.selectProject(h.projectId);
                return Promise.resolve();
            };
            void ensure().then(() => {
                if (h.taskId) {
                    void usePmTaskDetailStore.getState().openTask(h.taskId);
                }
                onNavigate("projects");
            });
            closeDialog();
        },
        [onNavigate, closeDialog],
    );

    const openMemoryHit = useCallback(
        (h: MemoryHit) => {
            // Kept for the double-click path. The single-click preview path
            // routes through PreviewPane which calls fetchNode directly.
            void useMemoryStore.getState().fetchNode(agentId, h.nodeId).then(() => undefined);
        },
        [agentId],
    );

    const openConversationHit = useCallback(
        (h: ConversationHit) => {
            // Open the chat view at the hit session and scroll to the
            // exact message (ADR-081 §4.2): locateMessage aligns the
            // load window, then ChatPanel scrolls + highlights.
            onNavigate("chat");
            void useChatStore.getState().locateMessage(agentId, h.sessionId, h.messageIndex);
            closeDialog();
        },
        [agentId, onNavigate, closeDialog],
    );

    const activate = useCallback(
        (idx: number) => {
            const h = hits[idx];
            if (!h) return;
            if (h.type === "file") openFileHit(h as FileHit);
            else if (h.type === "git") openGitHit();
            else if (h.type === "doc") openDocHit(h as DocHit);
            else if (h.type === "project") openProjectHit(h as ProjectHit);
            else if (h.type === "conversation") openConversationHit(h as ConversationHit);
            else openMemoryHit(h as MemoryHit);
        },
        [hits, openFileHit, openGitHit, openDocHit, openProjectHit, openConversationHit, openMemoryHit],
    );

    const onKeyDown = useCallback(
        (e: React.KeyboardEvent) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                closeDialog();
            } else if (e.key === "ArrowDown") {
                e.preventDefault();
                setFocusedIdx((i) => Math.min(i + 1, hits.length - 1));
            } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setFocusedIdx((i) => Math.max(i - 1, 0));
            } else if (e.key === "Enter") {
                e.preventDefault();
                activate(focusedIdx);
            }
        },
        [hits.length, focusedIdx, activate, closeDialog],
    );

    if (!open) return null;

    const TABS: { key: Tab; label: string }[] = [
        { key: "file", label: t("globalSearch.tabs.file") },
        { key: "git", label: t("globalSearch.tabs.git") },
        { key: "doc", label: t("globalSearch.tabs.doc") },
        { key: "project", label: t("globalSearch.tabs.project") },
        { key: "conversation", label: t("globalSearch.tabs.conversation") },
        { key: "memory", label: t("globalSearch.tabs.memory") },
    ];

    const focusedHit = hits[focusedIdx] ?? null;
    const focusedGit = focusedHit?.type === "git"
        ? commits.find((c) => c.hash === focusedHit.id)
        : undefined;

    return (
        <div
            className="fixed inset-0 z-50 flex items-start justify-center bg-modal-overlay pt-[10vh]"
            onMouseDown={() => closeDialog()}
        >
            <div
                className="relative z-10 flex max-h-[70vh] w-full max-w-3xl flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface text-text shadow-xl"
                onMouseDown={(e) => e.stopPropagation()}
                onKeyDown={onKeyDown}
            >
                {/* ── Header ──────────────────────────────────────────── */}
                <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-5 py-3">
                    <Search className="h-5 w-5 text-text-tertiary" />
                    <h2 className="text-sm font-semibold">{t("globalSearch.title")}</h2>
                </div>

                {/* ── Zone 1: agent picker + search input ─────────────── */}
                <div className="flex shrink-0 items-stretch border-b border-border-divider">
                    <AgentPickerChip
                        agents={agentList.map((a) => a.meta)}
                        value={agentId}
                        onChange={(next) => {
                            setAgentId(next);
                            setGitLoaded(false);
                            setCommits([]);
                            setFileHits([]);
                            setMemoryHits([]);
                            setConversationHits([]);
                        }}
                    />
                    <div className="flex flex-1 items-center gap-1.5 border-l border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                        <Search className="h-3 w-3 shrink-0 text-text-tertiary" />
                        <input
                            ref={inputRef}
                            type="text"
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            placeholder={t("globalSearch.searchInputPlaceholder")}
                            className="flex-1 bg-transparent text-xs text-text-secondary outline-none placeholder:text-text-tertiary"
                        />
                        {tab === "file" && (
                            <div className="flex items-center gap-1">
                                <ToggleBtn
                                    title={t("globalSearch.toggleCaseSensitive")}
                                    active={caseSensitive}
                                    onClick={() => setCaseSensitive((p) => !p)}
                                >
                                    Aa
                                </ToggleBtn>
                                <ToggleBtn
                                    title={t("globalSearch.toggleWholeWord")}
                                    active={wholeWord}
                                    onClick={() => setWholeWord((p) => !p)}
                                >
                                    \b
                                </ToggleBtn>
                            </div>
                        )}
                    </div>
                </div>

                {/* ── Zone 2: tabs + results list ────────────────────── */}
                {/* `min-h-[180px]` keeps the results region visible even
                    before the user has typed anything — without it Zone 2
                    shrinks to its content (an empty placeholder) and the
                    dialog jumps taller the moment a query produces rows.
                    `flex-1` lets it grow up to the 70vh dialog cap once
                    Zone 3 has claimed its fixed 140–300px. */}
                <div className="flex min-h-[180px] flex-1 flex-col overflow-hidden border-b border-border-divider">
                    <div className="flex shrink-0 gap-1 border-b border-border-divider px-2 pt-1">
                        {TABS.map((tabDef) => (
                            <button
                                key={tabDef.key}
                                data-testid={`tab-${tabDef.key}`}
                                onClick={() => setTab(tabDef.key)}
                                className={`rounded-t px-3 py-1.5 text-xs font-medium transition-colors ${
                                    tab === tabDef.key
                                        ? "bg-zinc-100 text-text dark:bg-zinc-700/60"
                                        : "text-text-secondary hover:bg-zinc-50 dark:hover:bg-zinc-700/40"
                                }`}
                            >
                                {tabDef.label}
                            </button>
                        ))}
                    </div>

                    <div className="flex-1 overflow-y-auto">
                        {tab === "file" && loading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "file" && error && !loading && <RowInfo>{error}</RowInfo>}
                        {tab === "file" && !loading && !error && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.fileNoMatch")}</RowInfo>
                        )}
                        {tab === "file" && !query.trim() && <RowInfo>{t("globalSearch.fileEmptyHint")}</RowInfo>}
                        {tab === "file" && fileTotal > 0 && !loading && (
                            <div className="px-3 pt-1 text-[11px] text-text-tertiary">
                                {t("globalSearch.matchCount", { total: fileTotal, shown: hits.length })}
                            </div>
                        )}

                        {tab === "git" && gitError && <RowInfo>{gitError}</RowInfo>}
                        {tab === "git" && !gitError && gitLoaded && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.gitNoMatch")}</RowInfo>
                        )}
                        {tab === "git" && !gitLoaded && !gitError && (
                            <RowInfo>{t("globalSearch.loadingGitHistory")}</RowInfo>
                        )}

                        {tab === "doc" && docLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "doc" && docError && !docLoading && <RowInfo>{docError}</RowInfo>}
                        {tab === "doc" && !docLoading && !docError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.docNoMatch")}</RowInfo>
                        )}
                        {tab === "doc" && !query.trim() && <RowInfo>{t("globalSearch.docEmptyHint")}</RowInfo>}

                        {tab === "project" && projLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "project" && projError && !projLoading && <RowInfo>{projError}</RowInfo>}
                        {tab === "project" && !projLoading && !projError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.projectNoMatch")}</RowInfo>
                        )}
                        {tab === "project" && !projLoading && !projError && !query.trim() && (
                            <RowInfo>{t("globalSearch.projectEmptyHint")}</RowInfo>
                        )}

                        {tab === "memory" && memoryLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "memory" && memoryError && !memoryLoading && <RowInfo>{memoryError}</RowInfo>}
                        {tab === "memory" && !memoryLoading && !memoryError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.memoryNoMatch")}</RowInfo>
                        )}
                        {tab === "memory" && !memoryLoading && !memoryError && !query.trim() && (
                            <RowInfo>{t("globalSearch.memoryEmptyHint")}</RowInfo>
                        )}

                        {tab === "conversation" && conversationLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "conversation" && conversationError && !conversationLoading && <RowInfo>{conversationError}</RowInfo>}
                        {tab === "conversation" && !conversationLoading && !conversationError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.conversationNoMatch")}</RowInfo>
                        )}
                        {tab === "conversation" && !conversationLoading && !conversationError && !query.trim() && (
                            <RowInfo>{t("globalSearch.conversationEmptyHint")}</RowInfo>
                        )}

                        <ul data-testid="result-list">
                            {hits.map((h, i) => (
                                <li
                                    key={h.id}
                                    data-testid={`result-row-${h.type}`}
                                    onMouseDown={(e) => {
                                        // Single click → preview.
                                        e.preventDefault();
                                        setFocusedIdx(i);
                                    }}
                                    onDoubleClick={(e) => {
                                        e.preventDefault();
                                        e.stopPropagation();
                                        activate(i);
                                    }}
                                    className={`flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs ${
                                        i === focusedIdx
                                            ? "bg-zinc-100 text-text dark:bg-zinc-700/60"
                                            : "text-text-secondary hover:bg-zinc-50 dark:hover:bg-zinc-700/40"
                                    }`}
                                >
                                    <HitIcon type={h.type} title={h.title} focused={i === focusedIdx} />
                                    <div className="min-w-0 flex-1">
                                        <div className="truncate font-medium">
                                            <Highlight text={h.title} query={query} />
                                        </div>
                                        <div className="flex items-center gap-2 truncate text-[11px] text-text-tertiary">
                                            <span className="truncate">
                                                <Highlight text={localizeSub(h, t)} query={query} />
                                            </span>
                                            <span className="truncate font-mono">
                                                <Highlight text={h.snippet} query={query} />
                                            </span>
                                        </div>
                                    </div>
                                </li>
                            ))}
                        </ul>
                    </div>
                </div>

                {/* ── Zone 3: preview pane (always rendered for stable layout) ── */}
                <div
                    data-testid="preview-pane"
                    className="flex h-[35vh] max-h-[300px] min-h-[140px] shrink-0 flex-col overflow-hidden"
                >
                    <PreviewPane
                        hit={focusedHit}
                        tab={tab}
                        agentId={agentId}
                        workspaceId={workspaceId}
                        gitCommit={focusedGit}
                    />
                </div>

                {/* ── Footer ──────────────────────────────────────────── */}
                <div className="flex shrink-0 justify-end border-t border-border-divider px-5 py-3">
                    <button
                        type="button"
                        onClick={() => closeDialog()}
                        className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
                    >
                        {t("globalSearch.close")}
                    </button>
                </div>
            </div>
        </div>
    );
}

/* ─── Helpers used inside the component ─────────────────────────────── */

/** ADR-081 D4: row secondary line is raw (kind, role) so the renderer
 *  applies the locale-aware label here. */
function localizeSub(h: RowHit, t: (k: string) => string): string {
    if (h.type === "project") {
        const ph = h as ProjectHit;
        const isTask = ph.sub.startsWith("task");
        const label = t(isTask ? "globalSearch.subTask" : "globalSearch.subProject");
        return `${label} · ${ph.projectId}`;
    }
    if (h.type === "conversation") {
        const ch = h as ConversationHit;
        const isAssistant = ch.role === "assistant";
        const label = t(isAssistant ? "globalSearch.subAssistant" : "globalSearch.subUser");
        return `${label} · #${ch.messageIndex}`;
    }
    return h.sub;
}

function HitIcon({ type, title, focused }: { type: Tab; title: string; focused: boolean }) {
    const cls = `h-4 w-4 shrink-0 ${focused ? "text-text" : "text-text-tertiary"}`;
    if (type === "file") {
        return <SetiIcon {...getFileIcon(title)} size={16} className={cls} />;
    }
    if (type === "doc") return <BookOpen className={cls} />;
    if (type === "project") return <KanbanSquare className={cls} />;
    if (type === "conversation") return <MessageSquare className={cls} />;
    if (type === "memory") {
        return (
            <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" className={cls}>
                <ellipse cx="8" cy="5" rx="5" ry="3" />
                <path d="M3 5v4c0 1.7 2.2 3 5 3s5-1.3 5-3V5" />
                <path d="M3 9v2c0 1.7 2.2 3 5 3s5-1.3 5-3V9" />
            </svg>
        );
    }
    // git
    return <GitCommitRowIcon focused={focused} />;
}

function GitCommitRowIcon({ focused }: { focused: boolean }) {
    const cls = `h-4 w-4 shrink-0 ${focused ? "text-text" : "text-text-tertiary"}`;
    return <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" className={cls}>
        <circle cx="8" cy="4" r="2" />
        <line x1="8" y1="6" x2="8" y2="14" />
        <circle cx="8" cy="12" r="2" />
    </svg>;
}

function RowInfo({ children }: { children: React.ReactNode }) {
    return (
        <div className="px-3 py-3 text-center text-xs text-text-tertiary">{children}</div>
    );
}

function ToggleBtn({
    title,
    active,
    onClick,
    children,
}: {
    title: string;
    active: boolean;
    onClick: () => void;
    children: React.ReactNode;
}) {
    return (
        <button
            type="button"
            title={title}
            onMouseDown={(e) => e.preventDefault()}
            onClick={onClick}
            className={`flex h-6 min-w-[1.5rem] items-center justify-center rounded px-1.5 text-[11px] font-medium transition-colors ${
                active
                    ? "bg-[var(--color-accent)]/20 text-[var(--color-accent)]"
                    : "text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700/50"
            }`}
        >
            {children}
        </button>
    );
}

