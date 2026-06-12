/**
 * Multi-language LSP Client Pool hook.
 *
 * Maintains a pool of MonacoLanguageClient instances — one per language —
 * keyed by language id. Clients persist across active-file switches so that
 * opening another file in an already-connected language is instant.
 *
 * Lifecycle rules:
 *   - When a new language appears in `openLanguages`, a WebSocket and
 *     LanguageClient are created for it.
 *   - When a language disappears from `openLanguages` (all files of that
 *     language are closed), a 30-second grace timer starts. If the language
 *     reappears within the timer, the timer is cancelled and the existing
 *     client continues to be used. Otherwise the client is shut down and
 *     evicted from the pool.
 *   - On unmount, every client and WebSocket is torn down synchronously.
 *
 * The connection logic intentionally mirrors the single-language
 * `useLspClient` hook (WebSocket adapter, VS Code API init, manual didOpen
 * on initialize, rust-analyzer progress tracking) so behaviour stays
 * consistent.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { WebSocketMessageReader, WebSocketMessageWriter } from "vscode-ws-jsonrpc";
import {
    type MessageTransports,
    type LanguageClientOptions,
    State,
} from "vscode-languageclient/browser";
import { MonacoLanguageClient } from "monaco-languageclient";
import {
    type LspStatus,
    adaptWebSocket,
    buildAbsoluteUri,
    buildLspWsUrl,
    ensureVscodeApiInitialized,
} from "./useLspClient";

// ── Types ──────────────────────────────────────────────────────────────

export type { LspStatus };

export interface LspClientEntry {
    client: MonacoLanguageClient;
    status: LspStatus;
    statusMessage: string;
}

export interface LspClientPoolResult {
    /** Get the LSP client for a specific language (null if not connected). */
    getClient: (language: string) => MonacoLanguageClient | null;
    /** Get status info for a specific language. */
    getStatus: (language: string) => { status: LspStatus; statusMessage: string };
    /** Status for the currently active language (convenience). */
    activeStatus: LspStatus;
    /** Status message for the currently active language (convenience). */
    activeStatusMessage: string;
    /** The client for the active language (convenience). */
    activeClient: MonacoLanguageClient | null;
    /**
     * All currently tracked languages and their statuses, for status-bar
     * style displays that want to show every active LSP server at once
     * (e.g. "rust: ready", "python: indexing 42%").
     *
     * Each entry corresponds to a language that has at least one open
     * file or is still within its disconnect grace period. The map is
     * a fresh instance on every status change so React reference
     * equality checks correctly trigger re-renders.
     */
    allStatuses: Map<string, { status: LspStatus; statusMessage: string }>;
}

/** Internal per-language state. Lives in a ref Map. */
interface ClientState {
    language: string;
    ws: WebSocket | null;
    client: MonacoLanguageClient | null;
    status: LspStatus;
    statusMessage: string;
    handshakeDone: boolean;
    connecting: boolean;
    /** Set to true to ignore late async results after teardown begins. */
    cancelled: boolean;
    /** Pending eviction timer; non-null while in the 30s grace period. */
    disconnectTimer: ReturnType<typeof setTimeout> | null;
    /** Active workDoneProgress tokens (rust-analyzer indexing, etc.). */
    activeProgressTokens: Set<string | number>;
    /** True when the WebSocket dropped post-handshake; effect will retry. */
    reconnectNeeded: boolean;
    /** Connection params used for the current attempt — guards re-entry. */
    paramsKey: string;
    /** Fallback timer: if still "connected" after 5s with no progress → ready. */
    readyTimeoutId: ReturnType<typeof setTimeout> | null;
    /**
     * Debounce timer for indexing→ready transitions. rust-analyzer starts
     * through several sequential phases (Roots Scanned → Fetching →
     * cachePriming → flycheck) with brief gaps between them; without a
     * debounce the status would flicker ready↔indexing during each gap.
     */
    readyDebounceId: ReturnType<typeof setTimeout> | null;
}

/** Public per-language snapshot used to drive React re-renders. */
interface PublicEntry {
    status: LspStatus;
    statusMessage: string;
    client: MonacoLanguageClient | null;
}

const DISCONNECT_GRACE_MS = 30_000;
const START_TIMEOUT_MS = 30_000;
const EMPTY_ENTRY: PublicEntry = { status: "disconnected", statusMessage: "", client: null };

// ── Helpers ────────────────────────────────────────────────────────────

/** Language-specific LSP install hints shown in error messages. */
const LSP_INSTALL_HINTS: Record<string, string> = {
    typescript: "npm install -g typescript-language-server typescript",
    javascript: "npm install -g typescript-language-server typescript",
    ts: "npm install -g typescript-language-server typescript",
    js: "npm install -g typescript-language-server typescript",
    rust: "rustup component add rust-analyzer",
    python: "pip install python-lsp-server",
    go: "go install golang.org/x/tools/gopls@latest",
    json: "npm install -g vscode-json-languageserver",
    yaml: "npm install -g yaml-language-server",
    yml: "npm install -g yaml-language-server",
    html: "npm install -g vscode-html-languageserver",
    css: "npm install -g vscode-css-languageserver",
    scss: "npm install -g vscode-css-languageserver",
    less: "npm install -g vscode-css-languageserver",
    markdown: "Install marksman: https://github.com/artempyanykh/marksman",
    md: "Install marksman: https://github.com/artempyanykh/marksman",
};

function formatLspError(language: string, reason: string): string {
    const hint = LSP_INSTALL_HINTS[language.toLowerCase()];
    if (hint) {
        return `${reason}. Install: ${hint}`;
    }
    return reason;
}

// ── Hook ───────────────────────────────────────────────────────────────

export function useLspClientPool(
    activeLanguage: string | null,
    openLanguages: Set<string>,
    agentId: string | undefined,
    workspaceId: string | undefined,
    enabled: boolean,
    workspaceRoot?: string
): LspClientPoolResult {
    // Public snapshot used for re-renders. Keyed by language id.
    const [snapshot, setSnapshot] = useState<Map<string, PublicEntry>>(() => new Map());
    // Internal mutable per-language state. Keyed by language id.
    const statesRef = useRef<Map<string, ClientState>>(new Map());

    /** Push the latest mutable state of one language into the public snapshot. */
    const publish = useCallback((language: string) => {
        const st = statesRef.current.get(language);
        setSnapshot((prev) => {
            const next = new Map(prev);
            if (!st) {
                next.delete(language);
            } else {
                next.set(language, {
                    status: st.status,
                    statusMessage: st.statusMessage,
                    client: st.client,
                });
            }
            return next;
        });
    }, []);

    /** Tear down a single language's client + socket. Removes from the map. */
    const evict = useCallback(
        (language: string) => {
            const st = statesRef.current.get(language);
            if (!st) return;
            console.log("[LSP] pool evict —", language);
            st.cancelled = true;
            if (st.readyTimeoutId) {
                clearTimeout(st.readyTimeoutId);
                st.readyTimeoutId = null;
            }
            if (st.readyDebounceId) {
                clearTimeout(st.readyDebounceId);
                st.readyDebounceId = null;
            }
            if (st.disconnectTimer) {
                clearTimeout(st.disconnectTimer);
                st.disconnectTimer = null;
            }
            const c = st.client;
            if (c) {
                // Do NOT call c.stop() — it triggers global StandaloneServices
                // shutdown, which is a non-reversible singleton state.  Just
                // close the WebSocket below; the transport teardown is enough.
                // Null the ref so GC can collect it.
                st.client = null;
            }
            const ws = st.ws;
            if (ws) {
                try {
                    ws.close();
                } catch {
                    // ignore
                }
            }
            statesRef.current.delete(language);
            setSnapshot((prev) => {
                if (!prev.has(language)) return prev;
                const next = new Map(prev);
                next.delete(language);
                return next;
            });
        },
        []
    );

    /** Establish a connection for a single language. Idempotent per state. */
    const connectLanguage = useCallback(
        async (language: string) => {
            if (!enabled || !workspaceRoot) return;

            let st = statesRef.current.get(language);
            if (!st) {
                st = {
                    language,
                    ws: null,
                    client: null,
                    status: "disconnected",
                    statusMessage: "",
                    handshakeDone: false,
                    connecting: false,
                    cancelled: false,
                    disconnectTimer: null,
                    activeProgressTokens: new Set(),
                    reconnectNeeded: false,
                    paramsKey: "",
                    readyTimeoutId: null,
                    readyDebounceId: null,
                };
                statesRef.current.set(language, st);
            }

            // Cancel a pending eviction — language is back in use.
            if (st.disconnectTimer) {
                console.log("[LSP] pool cancel disconnect timer —", language);
                clearTimeout(st.disconnectTimer);
                st.disconnectTimer = null;
            }

            const paramsKey = `${language}|${agentId ?? ""}|${workspaceId ?? ""}|${workspaceRoot ?? ""}`;

            // Already connected with the same params and no reconnect requested.
            if (
                paramsKey === st.paramsKey &&
                st.handshakeDone &&
                !st.reconnectNeeded
            ) {
                return;
            }

            // Concurrent connect guard.
            if (st.connecting) {
                console.log("[LSP] pool connect skipped (already connecting) —", language);
                return;
            }

            st.connecting = true;
            st.cancelled = false;
            st.reconnectNeeded = false;
            st.paramsKey = paramsKey;
            st.status = "connecting";
            st.statusMessage = "";
            publish(language);

            // Drop stale client ref without calling stop() — stop()
            // triggers global StandaloneServices shutdown which is a
            // non-reversible singleton; closing the WS below is enough.
            if (st.client) {
                st.client = null;
            }
            if (st.ws) {
                try {
                    st.ws.close();
                } catch {
                    // ignore
                }
                st.ws = null;
            }
            st.handshakeDone = false;

            const t0 = performance.now();
            const wsUrl = buildLspWsUrl(language, agentId, workspaceId);
            console.log("[LSP] pool connecting —", language, "url:", wsUrl);

            let ws: WebSocket;
            try {
                ws = new WebSocket(wsUrl);
            } catch (err) {
                if (st.cancelled) return;
                console.error("[LSP] pool ws ctor failed —", language, err);
                st.status = "error";
                st.statusMessage = formatLspError(language, `Failed to connect: ${err}`);
                st.connecting = false;
                publish(language);
                return;
            }
            st.ws = ws;

            // Wait for socket open.
            const openPromise = new Promise<void>((resolve, reject) => {
                ws.onopen = () => {
                    if (st!.cancelled) {
                        ws.close();
                        resolve();
                        return;
                    }
                    resolve();
                };
                ws.onerror = () => reject(new Error("WebSocket connection failed"));
                ws.onclose = (e) =>
                    reject(
                        new Error(
                            `Connection closed (${e.code})${e.reason ? ": " + e.reason : ""}`
                        )
                    );
            });

            try {
                await openPromise;
                if (st.cancelled) return;
                const t1 = performance.now();
                console.log("[LSP] pool ws opened —", language, `elapsed: ${Math.round(t1 - t0)}ms`);

                await ensureVscodeApiInitialized();
                if (st.cancelled) return;
                const t2 = performance.now();
                console.log("[LSP] pool vscode api ready —", language, `elapsed: ${Math.round(t2 - t1)}ms`);

                const socket = adaptWebSocket(ws);
                const reader = new WebSocketMessageReader(socket);
                const writer = new WebSocketMessageWriter(socket);
                const messageTransports: MessageTransports = { reader, writer };

                // Empty documentSelector — the editor manages didOpen/didChange
                // manually with absolute URIs (see useLspClient comments).
                const monaco = await import("monaco-editor");
                const wsRoot = workspaceRoot!.replace(/\\/g, "/");
                const rootFolderUri = monaco.Uri.file(wsRoot);
                const clientOptions: LanguageClientOptions = {
                    documentSelector: [],
                    workspaceFolder: { uri: rootFolderUri, name: "workspace", index: 0 },
                    // jdtls requires initializationOptions to import projects.
                    // Without extendedClientCapabilities.gradleBuildFileSupport,
                    // jdtls won't import Gradle/Maven projects, causing
                    // workspace/symbol to hang indefinitely.
                    // Ref: VS Code Java extension, nvim-jdtls setup.lua
                    initializationOptions: {
                        workspaceFolders: [wsRoot],
                        settings: {
                            java: {
                                import: {
                                    gradle: { enabled: true, wrapper: { enabled: true } },
                                    maven: { enabled: true },
                                },
                            },
                        },
                        extendedClientCapabilities: {
                            gradleBuildFileSupport: true,
                            classFileContentsSupport: true,
                            clientDocumentSymbolProvider: true,
                        },
                    },
                };

                const t3 = performance.now();
                console.log("[LSP] pool transports ready —", language, `elapsed: ${Math.round(t3 - t2)}ms`);

                const lspClient = new MonacoLanguageClient({
                    name: `${language} LSP`,
                    clientOptions,
                    messageTransports,
                });
                const t4 = performance.now();
                console.log("[LSP] pool MonacoLanguageClient created —", language, `elapsed: ${Math.round(t4 - t3)}ms`);

                lspClient.onDidChangeState((e) => {
                    console.log(
                        "[LSP] pool client state —",
                        language,
                        State[e.oldState],
                        "→",
                        State[e.newState]
                    );
                    // Guard: only act if the state ref still points to the
                    // same ClientState that created this listener.  If the
                    // pool was evicted and a fresh connectLanguage() call
                    // created a new ClientState, this callback belongs to
                    // the old (zombie) client and must be ignored.
                    const cur = statesRef.current.get(language);
                    if (cur !== st) {
                        console.log(
                            "[LSP] pool state change ignored (stale client) —",
                            language
                        );
                        return;
                    }
                    if (e.newState === State.Stopped && !st!.cancelled) {
                        st!.status = "disconnected";
                        st!.statusMessage = "";
                        publish(language);
                    }
                });

                // Post-open handlers: trigger reconnect if the socket drops
                // while the language is still in active use.
                ws.onclose = (e) => {
                    if (st!.cancelled) return;
                    console.warn(
                        "[LSP] pool ws closed after open —",
                        language,
                        "code:",
                        e.code,
                        "reason:",
                        e.reason
                    );
                    st!.handshakeDone = false;
                    st!.connecting = false;
                    st!.paramsKey = "";
                    st!.client = null;
                    st!.ws = null;
                    st!.reconnectNeeded = true;
                    st!.status = "error";
                    st!.statusMessage = formatLspError(
                        language,
                        `Connection lost (${e.code})`
                    );
                    publish(language);
                };
                ws.onerror = (ev) => {
                    if (!st!.cancelled) {
                        console.error("[LSP] pool ws error after open —", language, ev);
                    }
                };

                console.log("[LSP] pool calling lspClient.start() —", language);

                // Start the LSP handshake with a timeout and retry for
                // "Shutdown already requested" races (see evict comment).
                let attempt = 0;
                let startResult: "ok" | "timeout";
                // eslint-disable-next-line no-constant-condition
                while (true) {
                    // eslint-disable-next-line prefer-const
                    let timeoutId: undefined | ReturnType<typeof setTimeout> = undefined;
                    attempt++;
                    try {
                        startResult = await Promise.race([
                            lspClient.start().then(() => {
                                clearTimeout(timeoutId);
                                return "ok" as const;
                            }),
                            new Promise<"timeout">((resolve) => {
                                timeoutId = setTimeout(() => resolve("timeout"), START_TIMEOUT_MS);
                            }),
                        ]);
                        break; // success
                    } catch (err: any) {
                        if (timeoutId !== undefined) clearTimeout(timeoutId);
                        const msg = String(err?.message ?? err ?? "");
                        if (
                            msg.includes("Shutdown already requested") &&
                            attempt < 5
                        ) {
                            console.warn(
                                "[LSP] pool start retry —",
                                language,
                                "attempt",
                                attempt,
                                "reason:",
                                msg
                            );
                            await new Promise((r) => setTimeout(r, 600 * attempt));
                            continue;
                        }
                        throw err;
                    }
                }

                if (st.cancelled) return;

                if (startResult === "timeout") {
                    console.error("[LSP] pool start timeout —", language, `total elapsed: ${Math.round(performance.now() - t0)}ms`);
                    st.connecting = false;
                    st.client = null;
                    st.status = "error";
                    st.statusMessage = formatLspError(
                        language,
                        `Initialize timed out (${START_TIMEOUT_MS / 1000}s). Check Gateway logs for LSP errors.`
                    );
                    try {
                        lspClient.stop();
                    } catch {
                        // ignore
                    }
                    publish(language);
                    return;
                }

                const t5 = performance.now();
                console.log("[LSP] pool client started —", language, `start() elapsed: ${Math.round(t5 - t4)}ms`, `total elapsed: ${Math.round(t5 - t0)}ms`);

                // Manually send didOpen for all currently-open models of this
                // language using their absolute filesystem URI.
                if (workspaceRoot) {
                    try {
                        const monaco = await import("monaco-editor");
                        const models = monaco.editor.getModels();
                        for (const model of models) {
                            if (model.getLanguageId() !== language) continue;
                            const absUri = buildAbsoluteUri(
                                workspaceRoot,
                                model.uri.path.replace(/^\/+/, "")
                            );
                            try {
                                lspClient.sendNotification("textDocument/didOpen", {
                                    textDocument: {
                                        uri: absUri,
                                        languageId: model.getLanguageId(),
                                        version: 0,
                                        text: model.getValue(),
                                    },
                                });
                            } catch (err) {
                                console.warn(
                                    "[LSP] pool manual didOpen failed —",
                                    language,
                                    err
                                );
                            }
                        }
                    } catch (err) {
                        console.warn("[LSP] pool monaco import failed —", language, err);
                    }
                }

                // Send workspace/didChangeConfiguration after handshake.
                // jdtls (Eclipse JDT LS) requires this notification with Java
                // settings to trigger project import (Gradle/Maven).
                // Empty settings only cause per-file analysis (Validate + Publish
                // Diagnostics); proper settings trigger full project import.
                try {
                    lspClient.sendNotification("workspace/didChangeConfiguration", {
                        settings: {
                            java: {
                                import: {
                                    gradle: { enabled: true, wrapper: { enabled: true } },
                                    maven: { enabled: true },
                                },
                            },
                        },
                    });
                    console.log("[LSP] pool sent workspace/didChangeConfiguration —", language);
                } catch (err) {
                    console.warn("[LSP] pool didChangeConfiguration failed —", language, err);
                }

                st.handshakeDone = true;
                st.connecting = false;
                st.client = lspClient;
                st.status = "connected";
                st.statusMessage = language;
                publish(language);
                const t6 = performance.now();
                console.log("[LSP] pool handshake complete —", language, `didOpen+publish elapsed: ${Math.round(t6 - t5)}ms`, `total elapsed: ${Math.round(t6 - t0)}ms`);

                // Fallback for servers that never emit workDoneProgress:
                // if still "connected" after 5s with no indexing, assume ready.
                st.readyTimeoutId = setTimeout(() => {
                    st!.readyTimeoutId = null;
                    if (st!.cancelled) return;
                    if (st!.status === "connected") {
                        st!.status = "ready";
                        st!.statusMessage = language;
                        publish(language);
                    }
                }, 5000);

                // ── rust-analyzer / generic workDoneProgress tracking ──
                lspClient.onNotification(
                    "window/workDoneProgress/create",
                    (params: any) => {
                        const token = params?.token;
                        if (token != null) st!.activeProgressTokens.add(token);
                    }
                );
                lspClient.onNotification("$/progress" as any, (params: any) => {
                    console.log("[LSP] $/progress received —", params?.value?.kind, params?.value?.title || "", "token:", params?.token);
                    if (st!.cancelled) return;
                    const token = params?.token;
                    const kind = params?.value?.kind;
                    const title = params?.value?.title || "";

                    if (kind === "begin") {
                        if (token != null) st!.activeProgressTokens.add(token);
                        // A new phase started — cancel any pending ready debounce
                        // so the status stays in "indexing" through the
                        // inter-phase gap rather than flipping to ready and back.
                        if (st!.readyDebounceId != null) {
                            console.log("[LSP] $/progress — debounce cancelled, new phase started");
                            clearTimeout(st!.readyDebounceId);
                            st!.readyDebounceId = null;
                        }
                        // Indexing started — cancel the ready-fallback timer.
                        if (st!.readyTimeoutId != null) {
                            clearTimeout(st!.readyTimeoutId);
                            st!.readyTimeoutId = null;
                        }
                        st!.status = "indexing";
                        st!.statusMessage = title || `${language} analyzing`;
                        publish(language);
                    } else if (kind === "report") {
                        const percentage = params?.value?.percentage;
                        if (percentage != null) {
                            st!.statusMessage = `${title || "analyzing"} ${Math.round(percentage)}%`;
                            publish(language);
                        }
                    } else if (kind === "end") {
                        if (token != null) st!.activeProgressTokens.delete(token);
                        // Only mark ready when every active progress has ended.
                        // Debounce 1.5s: rust-analyzer has multiple sequential
                        // phases with brief gaps; declaring ready immediately
                        // would cause flicker when the next phase begins.
                        if (st!.activeProgressTokens.size === 0) {
                            console.log("[LSP] $/progress — all tokens cleared, debouncing ready (1.5s)");
                            if (st!.readyDebounceId != null) {
                                clearTimeout(st!.readyDebounceId);
                            }
                            st!.readyDebounceId = setTimeout(() => {
                                st!.readyDebounceId = null;
                                if (st!.cancelled) return;
                                st!.status = "ready";
                                st!.statusMessage = language;
                                publish(language);
                            }, 1500);
                        }
                    }
                });
            } catch (err) {
                if (st.cancelled) return;
                console.error("[LSP] pool connect failed —", language, err);
                st.client = null;
                st.status = "error";
                st.statusMessage = formatLspError(language, String(err));
                st.connecting = false;
                publish(language);
            }
        },
        [agentId, workspaceId, enabled, workspaceRoot, publish]
    );

    /** Schedule (or refresh) a delayed eviction for a language. */
    const scheduleEviction = useCallback(
        (language: string) => {
            const st = statesRef.current.get(language);
            if (!st) return;
            if (st.disconnectTimer) {
                clearTimeout(st.disconnectTimer);
            }
            console.log(
                "[LSP] pool schedule disconnect —",
                language,
                `${DISCONNECT_GRACE_MS / 1000}s`
            );
            st.disconnectTimer = setTimeout(() => {
                st.disconnectTimer = null;
                evict(language);
            }, DISCONNECT_GRACE_MS);
        },
        [evict]
    );

    // Reconcile the pool with the requested set of `openLanguages` whenever
    // the inputs change. New languages connect; vanished languages start
    // their grace timer; reappearing ones cancel theirs.
    //
    // `openLanguages` is depended on via its serialised contents to avoid
    // re-running on identical Set instances.
    const openLanguagesKey = useMemo(
        () => Array.from(openLanguages).sort().join(","),
        [openLanguages]
    );

    useEffect(() => {
        if (!enabled || !workspaceRoot) {
            // Disabled / no workspace → drain the pool entirely.
            for (const lang of Array.from(statesRef.current.keys())) {
                evict(lang);
            }
            return;
        }

        const wanted = new Set(openLanguages);

        // 1. Add / refresh: ensure every wanted language has a live client.
        for (const lang of wanted) {
            const st = statesRef.current.get(lang);
            if (!st) {
                void connectLanguage(lang);
            } else {
                // Cancel pending eviction; trigger reconnect if the socket
                // was dropped while idle.
                if (st.disconnectTimer) {
                    clearTimeout(st.disconnectTimer);
                    st.disconnectTimer = null;
                }
                if (st.reconnectNeeded || (!st.handshakeDone && !st.connecting)) {
                    void connectLanguage(lang);
                }
            }
        }

        // 2. Remove: schedule eviction for languages no longer wanted.
        for (const lang of Array.from(statesRef.current.keys())) {
            if (!wanted.has(lang)) {
                const st = statesRef.current.get(lang);
                if (st && !st.disconnectTimer) {
                    scheduleEviction(lang);
                }
            }
        }
    }, [
        openLanguagesKey,
        agentId,
        workspaceId,
        enabled,
        workspaceRoot,
        connectLanguage,
        scheduleEviction,
        evict,
        openLanguages,
    ]);

    // Final teardown on unmount — close every socket and stop every client.
    useEffect(() => {
        return () => {
            console.log("[LSP] pool unmount — tearing down all clients");
            for (const lang of Array.from(statesRef.current.keys())) {
                const st = statesRef.current.get(lang);
                if (st?.disconnectTimer) {
                    clearTimeout(st.disconnectTimer);
                    st.disconnectTimer = null;
                }
                evict(lang);
            }
        };
        // Run only on unmount.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    // ── Public API ─────────────────────────────────────────────────────

    const getClient = useCallback(
        (language: string): MonacoLanguageClient | null => {
            return snapshot.get(language)?.client ?? null;
        },
        [snapshot]
    );

    const getStatus = useCallback(
        (language: string): { status: LspStatus; statusMessage: string } => {
            const entry = snapshot.get(language) ?? EMPTY_ENTRY;
            return { status: entry.status, statusMessage: entry.statusMessage };
        },
        [snapshot]
    );

    const activeEntry = activeLanguage ? snapshot.get(activeLanguage) ?? EMPTY_ENTRY : EMPTY_ENTRY;

    // Project the snapshot down to status-only entries for status-bar
    // consumers that don't need the heavy client reference.
    const allStatuses = useMemo(() => {
        const out = new Map<string, { status: LspStatus; statusMessage: string }>();
        for (const [lang, entry] of snapshot) {
            out.set(lang, { status: entry.status, statusMessage: entry.statusMessage });
        }
        return out;
    }, [snapshot]);

    return {
        getClient,
        getStatus,
        activeStatus: activeEntry.status,
        activeStatusMessage: activeEntry.statusMessage,
        activeClient: activeEntry.client,
        allStatuses,
    };
}

// ── Backward-compatible single-language wrapper ────────────────────────

/**
 * Backward-compatible wrapper that returns the client for the active
 * language only. Drop-in replacement for the original `useLspClient` hook
 * when callers do not yet need multi-language support.
 */
export function useLspClientCompat(
    language: string | null,
    agentId: string | undefined,
    workspaceId: string | undefined,
    enabled: boolean,
    workspaceRoot?: string
): { status: LspStatus; statusMessage: string; client: MonacoLanguageClient | null } {
    const openLanguages = useMemo<Set<string>>(
        () => (language ? new Set([language]) : new Set<string>()),
        [language]
    );
    const pool = useLspClientPool(
        language,
        openLanguages,
        agentId,
        workspaceId,
        enabled,
        workspaceRoot
    );
    return {
        status: pool.activeStatus,
        statusMessage: pool.activeStatusMessage,
        client: pool.activeClient,
    };
}
