/**
 * LSP Client hook for Monaco Editor integration.
 *
 * Connects to Gateway's LSP relay WebSocket endpoint and creates
 * a MonacoLanguageClient for a specific language. The Gateway
 * transparently forwards LSP JSON-RPC messages to the actual
 * language server process.
 *
 * IMPORTANT: Monaco's Uri.parse() cannot handle Windows file URIs
 * (file:///C:/...), so models use relative paths as their URI
 * (producing file:///core/...). This hook uses documentSelector: []
 * to prevent MonacoLanguageClient from auto-tracking documents
 * (which would send didOpen with the wrong URI), and manually
 * sends textDocument/didOpen/didChange/didClose with the correct
 * absolute URI constructed from the workspace root.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { WebSocketMessageReader, WebSocketMessageWriter } from "vscode-ws-jsonrpc";
import type { IWebSocket } from "vscode-ws-jsonrpc";
import {
    type MessageTransports,
    type LanguageClientOptions,
    State,
} from "vscode-languageclient/browser";
import { MonacoLanguageClient } from "monaco-languageclient";
import { MonacoVscodeApiWrapper } from "monaco-languageclient/vscodeApiWrapper";
import type { MonacoVscodeApiConfig } from "monaco-languageclient/vscodeApiWrapper";
import { getGatewayUrl } from "../lib/config";

// ── Types ──────────────────────────────────────────────────────────────

export type LspStatus = "disconnected" | "connecting" | "connected" | "indexing" | "ready" | "error";

export interface LspClientState {
    /** Current connection status */
    status: LspStatus;
    /** Human-readable status message (e.g. server name, error reason) */
    statusMessage: string;
    /** The MonacoLanguageClient instance (null if not connected) */
    client: MonacoLanguageClient | null;
}

// ── VS Code API initialization ────────────────────────────────────────
// MonacoLanguageClient v10+ requires VS Code API services to be initialized
// before construction. This must happen exactly once per application lifecycle.

let vscodeApiInitPromise: Promise<void> | null = null;
let vscodeApiInitDone = false;

/**
 * Initialize VS Code API services (required by MonacoLanguageClient).
 * Called lazily on first LSP connection attempt; runs only once.
 */
export async function ensureVscodeApiInitialized(): Promise<void> {
    if (vscodeApiInitDone) return;
    if (!vscodeApiInitPromise) {
        console.log("[LSP] Initializing VS Code API services (first time)...");
        const t0 = performance.now();
        vscodeApiInitPromise = (async () => {
            try {
                const config: MonacoVscodeApiConfig = {
                    $type: "classic",
                    viewsConfig: { $type: "EditorService" },
                };
                const wrapper = new MonacoVscodeApiWrapper(config);
                await wrapper.start({ caller: "useLspClient" });
                vscodeApiInitDone = true;
                console.log("[LSP] VS Code API services initialized successfully", `elapsed: ${Math.round(performance.now() - t0)}ms`);
            } catch (err) {
                vscodeApiInitPromise = null; // allow retry
                console.error("[LSP] VS Code API initialization failed:", err);
                throw err;
            }
        })();
    }
    await vscodeApiInitPromise;
}

// ── WebSocket adapter ──────────────────────────────────────────────────

/**
 * Adapt a browser WebSocket to vscode-ws-jsonrpc's IWebSocket interface.
 */
export function adaptWebSocket(ws: WebSocket): IWebSocket {
    const listeners: Array<() => void> = [];
    return {
        send(content: string): void {
            ws.send(content);
        },
        onMessage(cb: (data: unknown) => void): void {
            const handler = (e: MessageEvent) => cb(e.data);
            ws.addEventListener("message", handler);
            listeners.push(() => ws.removeEventListener("message", handler));
        },
        onError(cb: (reason: unknown) => void): void {
            const handler = () => cb(undefined);
            ws.addEventListener("error", handler);
            listeners.push(() => ws.removeEventListener("error", handler));
        },
        onClose(cb: (code: number, reason: string) => void): void {
            const handler = (e: CloseEvent) => cb(e.code, e.reason);
            ws.addEventListener("close", handler);
            listeners.push(() => ws.removeEventListener("close", handler));
        },
        dispose(): void {
            for (const remove of listeners) remove();
            listeners.length = 0;
        },
    };
}

// ── WebSocket URL builder ──────────────────────────────────────────────

/** Build the Gateway LSP WebSocket URL from an HTTP Gateway URL */
export function buildLspWsUrl(language: string, agentId?: string, workspaceId?: string): string {
    const httpUrl = getGatewayUrl();
    const wsUrl = httpUrl.replace(/^http/, "ws");
    let url = `${wsUrl}/lsp/${encodeURIComponent(language)}`;
    const params = new URLSearchParams();
    if (agentId) params.set("agent_id", agentId);
    if (workspaceId) params.set("workspace_id", workspaceId);
    const qs = params.toString();
    const result = qs ? `${url}?${qs}` : url;
    console.log("[LSP] buildLspWsUrl — httpUrl:", httpUrl, "→ wsUrl:", wsUrl, "→ result:", result);
    return result;
}

// ── Absolute URI builder ───────────────────────────────────────────────

/**
 * Build an absolute LSP file URI from a workspace root and relative path.
 *
 * Handles the Windows extended-length path prefix (\\?\) that
 * std::fs::canonicalize() produces on Windows. The prefix is stripped
 * because it produces invalid file URIs like "file:////?/C:/...".
 *
 * @param workspaceRoot - Absolute workspace root (e.g. "F:\\work\\tranxon\\rollball-ai"
 *   or possibly "\\\\?\\F:\\work\\tranxon\\rollball-ai" on Windows)
 * @param relPath - Relative file path (e.g. "core/rollball-runtime/src/main.rs")
 * @returns LSP-compatible file URI (e.g. "file:///F:/work/tranxon/rollball-ai/core/...")
 */
export function buildAbsoluteUri(workspaceRoot: string, relPath: string): string {
    let root = workspaceRoot.replace(/\\/g, "/");
    // Strip Windows extended-length path prefix: //?/ or /?/ or \\?\
    root = root.replace(/^\/\/\?\//, "").replace(/^\/\?\//, "");
    const rel = relPath.replace(/\\/g, "/");
    const absPath = `${root}/${rel}`;
    // file:///F:/work/... on Windows, file:///home/... on Unix
    if (/^[A-Za-z]:/.test(absPath)) {
        return `file:///${absPath}`;
    }
    return `file://${absPath}`;
}

// ── Hook ───────────────────────────────────────────────────────────────

/**
 * Create and manage an LSP client for a given language.
 *
 * The client connects when `enabled` is true and `language` is specified.
 * Only one client is active at a time; changing parameters disconnects
 * the old client and creates a new one.
 *
 * @param workspaceRoot - Absolute path to the workspace root directory,
 *   used to construct absolute file URIs for LSP document syncing.
 */
export function useLspClient(
    language: string | null,
    agentId: string | undefined,
    workspaceId: string | undefined,
    enabled: boolean,
    workspaceRoot?: string
): LspClientState {
    const [status, setStatus] = useState<LspStatus>("disconnected");
    const [statusMessage, setStatusMessage] = useState("");
    const [client, setClient] = useState<MonacoLanguageClient | null>(null);
    // Mirror of `status` for use inside async callbacks/timeouts where reading
    // the closed-over state value would be stale.
    const statusRef = useRef<LspStatus>("disconnected");
    useEffect(() => {
        statusRef.current = status;
    }, [status]);
    const wsRef = useRef<WebSocket | null>(null);
    // Ref to the active LSP client for cleanup without triggering React re-renders.
    // We use this instead of setClient(null) during connect() to avoid the race:
    //   setClient(null) → re-render → effect cleanup → disconnect() → kill in-progress connection
    const clientRef = useRef<MonacoLanguageClient | null>(null);
    // Track whether the LSP handshake completed, so onclose doesn't
    // overwrite an "error" status set by onerror or start() rejection.
    const handshakeDoneRef = useRef(false);
    // Track the last connected params to avoid reconnecting when
    // effect re-runs due to state changes (e.g. setClient triggering re-render)
    const lastParamsRef = useRef<string>("");
    // Track whether a connection attempt is currently in progress
    // to prevent concurrent connect() calls caused by React re-renders
    // during the async connect flow (e.g. setClient(null) from disconnect
    // triggers re-render → effect re-runs → tries to connect again)
    const connectingRef = useRef(false);
    // Set to true when the connection drops unexpectedly (ws.onclose after open).
    // The effect checks this and triggers a reconnect on the next re-render.
    const reconnectNeededRef = useRef(false);

    const disconnect = useCallback(() => {
        lastParamsRef.current = "";
        // Clean up existing client
        // Stop the client via ref (avoids React state update) then clear state
        const c = clientRef.current;
        if (c) {
            try {
                c.stop();
            } catch {
                // ignore cleanup errors
            }
            clientRef.current = null;
        }
        setClient(null);
        if (wsRef.current) {
            try {
                wsRef.current.close();
            } catch {
                // ignore
            }
            wsRef.current = null;
        }
        handshakeDoneRef.current = false;
        setStatus("disconnected");
        setStatusMessage("");
    }, []);

    useEffect(() => {
        // ── DIAGNOSTIC: log every effect run with dep values ──
        console.log("[LSP] effect run —", {
            enabled, language, agentId, workspaceId, workspaceRoot,
            lastParams: lastParamsRef.current,
            handshakeDone: handshakeDoneRef.current,
            connecting: connectingRef.current,
            clientRefExists: !!clientRef.current,
            reconnectNeeded: reconnectNeededRef.current,
        });

        // Don't connect if disabled, no language, or workspaceRoot not yet available.
        if (!enabled || !language || !workspaceRoot) {
            console.log("[LSP] effect — early return (missing deps), disconnecting");
            disconnect();
            lastParamsRef.current = "";
            return;
        }

        // Avoid reconnecting when effect re-runs with same params
        // (e.g. setClient → re-render → effect re-runs)
        const paramsKey = `${language}|${agentId ?? ""}|${workspaceId ?? ""}`;
        if (paramsKey === lastParamsRef.current && handshakeDoneRef.current) {
            // BUT: if the connection was lost and a reconnect was requested,
            // we should reconnect even with the same paramsKey.
            if (reconnectNeededRef.current) {
                console.log("[LSP] effect — reconnectNeededRef is true, forcing reconnect");
                reconnectNeededRef.current = false;
                // Fall through to connect
            } else {
                console.log("[LSP] effect — same paramsKey, handshake done, skipping reconnect");
                return;
            }
        }
        console.log("[LSP] effect — proceeding to connect. paramsKey:", paramsKey, "lastParams:", lastParamsRef.current, "handshakeDone:", handshakeDoneRef.current);

        // Prevent concurrent connect() calls. During the async connect flow,
        // setClient(null) from disconnect() triggers a React re-render which
        // causes the effect to re-run. Without this guard, a second connect()
        // would start before the first one completes, leading to timeouts.
        if (connectingRef.current) {
            console.log("[LSP] effect — connectingRef is true, skipping (concurrent guard)");
            return;
        }

        lastParamsRef.current = paramsKey;
        connectingRef.current = true;

        let cancelled = false;
        // Fallback timer: some LSP servers never emit workDoneProgress
        // (e.g. ts-server). If no indexing begin arrives within 5s after
        // the handshake, treat the client as fully ready.
        let readyTimeoutId: ReturnType<typeof setTimeout> | null = null;
        // Debounce timer for indexing→ready transitions. rust-analyzer starts
        // through several sequential phases (Roots Scanned → Fetching →
        // cachePriming → flycheck) with brief gaps between them; an immediate
        // ready flip during a gap causes the status to flicker between
        // ready and indexing. We wait 1.5s after every progress token has
        // ended before declaring ready, so a new phase beginning within
        // the gap can cancel the transition.
        let readyDebounceId: ReturnType<typeof setTimeout> | null = null;

        async function connect() {
            // Clean up any existing client BEFORE starting a new one.
            // CRITICAL: Do NOT call setClient(null) or disconnect() here!
            // Those trigger React state updates → re-render → effect cleanup →
            // disconnect() → kills THIS in-progress connection → timeout.
            // Instead, stop the old client via the ref (no state update).
            const oldClient = clientRef.current;
            if (oldClient) {
                try { oldClient.stop(); } catch { /* ignore */ }
                clientRef.current = null;
            }
            if (wsRef.current) {
                try { wsRef.current.close(); } catch { /* ignore */ }
                wsRef.current = null;
            }
            handshakeDoneRef.current = false;

            const wsUrl = buildLspWsUrl(language!, agentId, workspaceId);
            console.log("[LSP] connect() called — language:", language, "agentId:", agentId, "workspaceId:", workspaceId);
            console.log("[LSP] WebSocket URL:", wsUrl);

            let ws: WebSocket;
            try {
                ws = new WebSocket(wsUrl);
                console.log("[LSP] WebSocket constructor OK — readyState:", ws.readyState);
            } catch (err) {
                if (!cancelled) {
                    console.error("[LSP] WebSocket constructor threw:", err);
                    setStatus("error");
                    setStatusMessage(`Failed to create WebSocket: ${err}`);
                }
                return;
            }

            wsRef.current = ws;

            // Use a promise to wait for WebSocket open, then create and start
            // the LSP client in the same async context.
            const openPromise = new Promise<void>((resolve, reject) => {
                ws.onopen = () => {
                    console.log("[LSP] ws.onopen fired — readyState:", ws.readyState);
                    if (cancelled) {
                        ws.close();
                        resolve();
                        return;
                    }
                    resolve();
                };
                ws.onerror = (e) => {
                    console.error("[LSP] ws.onerror fired — event:", e, "readyState:", ws.readyState);
                    reject(new Error("WebSocket connection failed"));
                };
                ws.onclose = (e) => {
                    console.log("[LSP] ws.onclose fired before open — code:", e.code, "reason:", e.reason, "wasClean:", e.wasClean);
                    reject(new Error(`Connection closed (${e.code})${e.reason ? ": " + e.reason : ""}`));
                };
            });

            try {
                await openPromise;
                if (cancelled) return;

                console.log("[LSP] WebSocket opened successfully for:", language);

                // Initialize VS Code API services if not done yet
                // (required by MonacoLanguageClient v10+)
                await ensureVscodeApiInitialized();
                if (cancelled) return;

                // Adapt browser WebSocket to vscode-ws-jsonrpc's IWebSocket
                const socket = adaptWebSocket(ws);

                const reader = new WebSocketMessageReader(socket);
                const writer = new WebSocketMessageWriter(socket);
                console.log("[LSP] WebSocketMessageReader/Writer created");

                const messageTransports: MessageTransports = {
                    reader,
                    writer,
                };

                // Use EMPTY documentSelector to prevent MonacoLanguageClient from
                // auto-tracking models. This avoids sending textDocument/didOpen
                // with the model's relative URI (file:///core/...) which the LSP
                // server would reject. We handle document sync manually instead.
                const monaco = await import("monaco-editor");
                const wsRoot = workspaceRoot!.replace(/\\/g, "/");
                const rootFolderUri = monaco.Uri.file(wsRoot);
                const clientOptions: LanguageClientOptions = {
                    documentSelector: [],
                    workspaceFolder: { uri: rootFolderUri, name: "workspace", index: 0 },
                };

                console.log("[LSP] Creating MonacoLanguageClient — name:", `${language!} LSP`, "documentSelector: [] (manual doc sync)");

                const lspClient = new MonacoLanguageClient({
                    name: `${language!} LSP`,
                    clientOptions,
                    messageTransports,
                });

                console.log("[LSP] MonacoLanguageClient created, calling start()...");

                lspClient.onDidChangeState((e) => {
                    console.log("[LSP] Client state changed:", State[e.oldState], "→", State[e.newState]);
                    if (e.newState === State.Stopped && !cancelled) {
                        setStatus("disconnected");
                        setStatusMessage("");
                    }
                });

                // Re-register onclose/onerror AFTER open (the first set was
                // consumed by openPromise). These handle post-connect failures.
                ws.onclose = (e) => {
                    if (!cancelled) {
                        console.warn("[LSP] ws.onclose after open — code:", e.code, "reason:", e.reason, "wasClean:", e.wasClean);
                        // Reset refs so the effect will reconnect on next run.
                        handshakeDoneRef.current = false;
                        connectingRef.current = false;
                        lastParamsRef.current = "";
                        clientRef.current = null;
                        setClient(null);
                        // Signal that a reconnect is needed. The effect will
                        // pick this up on the next re-render (which setClient(null)
                        // triggers) and attempt to reconnect.
                        reconnectNeededRef.current = true;
                        setStatus("error");
                        setStatusMessage(`Connection lost (${e.code})`);
                    }
                };

                ws.onerror = (e) => {
                    if (!cancelled) {
                        console.error("[LSP] ws.onerror after open — event:", e);
                    }
                };

                // Start the client (sends initialize + initialized)
                // Add a timeout so we don't hang forever if the server
                // never responds to the initialize request.
                const START_TIMEOUT_MS = 30_000;
                let timeoutId: ReturnType<typeof setTimeout>;
                const startResult = await Promise.race([
                    lspClient.start().then(() => {
                        clearTimeout(timeoutId);
                        console.log("[LSP] client.start() resolved successfully");
                        return "ok" as const;
                    }),
                    new Promise<"timeout">((resolve) => {
                        timeoutId = setTimeout(() => {
                            console.error("[LSP] client.start() timed out after", START_TIMEOUT_MS, "ms");
                            resolve("timeout");
                        }, START_TIMEOUT_MS);
                    }),
                ]);

                if (cancelled) return;

                if (startResult === "timeout") {
                    console.error("[LSP] Client start timed out after", START_TIMEOUT_MS, "ms");
                    connectingRef.current = false;
                    clientRef.current = null;
                    setStatus("error");
                    setStatusMessage(`Initialize timed out (${START_TIMEOUT_MS / 1000}s)`);
                    lspClient.stop();
                    setClient(null);
                    return;
                }

                console.log("[LSP] Client started successfully for:", language);

                // Manually send textDocument/didOpen for all open models
                // of this language with the correct absolute URI.
                // MonacoLanguageClient won't do this because documentSelector
                // is empty — it doesn't track any documents automatically.
                if (workspaceRoot) {
                    const monaco = await import("monaco-editor");
                    const models = monaco.editor.getModels();
                    for (const model of models) {
                        if (model.getLanguageId() === language) {
                            const absUri = buildAbsoluteUri(workspaceRoot, model.uri.path.replace(/^\/+/, ""));
                            console.log("[LSP] Manually sending didOpen — absUri:", absUri);
                            try {
                                // textDocument/didOpen is a notification (no response expected).
                                // Using sendNotification instead of sendRequest avoids waiting
                                // for a response that never comes.
                                lspClient.sendNotification("textDocument/didOpen", {
                                    textDocument: {
                                        uri: absUri,
                                        languageId: model.getLanguageId(),
                                        version: 0,
                                        text: model.getValue(),
                                    },
                                });
                            } catch (err) {
                                console.warn("[LSP] Manual didOpen failed:", err);
                            }
                        }
                    }
                }

                handshakeDoneRef.current = true;
                connectingRef.current = false;
                // Store client in ref FIRST (for cleanup without re-render),
                // then update React state so consuming components see it.
                clientRef.current = lspClient;
                setClient(lspClient);

                // ── Monitor rust-analyzer indexing progress ───────────
                // rust-analyzer sends workDoneProgress notifications:
                //   1. "window/workDoneProgress/create" — server requests a progress token
                //   2. "$/progress" with value.kind = "begin"|"report"|"end"
                // Register listeners BEFORE flipping the status to "connected"
                // so we never miss progress notifications that arrive
                // immediately after the handshake.
                let activeProgressTokens = new Set<string | number>();

                lspClient.onNotification("window/workDoneProgress/create", (params: any) => {
                    const token = params?.token;
                    if (token != null) {
                        activeProgressTokens.add(token);
                    }
                });

                lspClient.onNotification("$/progress" as any, (params: any) => {
                    console.log("[LSP] $/progress received —", params?.value?.kind, params?.value?.title || "", "token:", params?.token);
                    const token = params?.token;
                    const kind = params?.value?.kind;
                    const title = params?.value?.title || "";

                    if (kind === "begin") {
                        if (token != null) activeProgressTokens.add(token);
                        console.log("[LSP] Progress begin:", title);
                        // A new phase started — cancel any pending ready debounce
                        // so the status stays in "indexing" rather than flipping
                        // to ready and back during the inter-phase gap.
                        if (readyDebounceId != null) {
                            console.log("[LSP] $/progress — debounce cancelled, new phase started");
                            clearTimeout(readyDebounceId);
                            readyDebounceId = null;
                        }
                        // Indexing started — cancel the ready-fallback timer.
                        if (readyTimeoutId != null) {
                            clearTimeout(readyTimeoutId);
                            readyTimeoutId = null;
                        }
                        setStatus("indexing");
                        setStatusMessage(title || `${language} indexing`);
                    } else if (kind === "report") {
                        const percentage = params?.value?.percentage;
                        if (percentage != null) {
                            setStatusMessage(`${title || "analyzing"} ${Math.round(percentage)}%`);
                        }
                    } else if (kind === "end") {
                        if (token != null) activeProgressTokens.delete(token);
                        // Only mark ready when every active progress has ended.
                        // Debounce 1.5s: rust-analyzer has multiple sequential
                        // phases with brief gaps; declaring ready immediately
                        // would cause flicker when the next phase begins.
                        if (activeProgressTokens.size === 0) {
                            console.log("[LSP] $/progress — all tokens cleared, debouncing ready (1.5s)");
                            if (readyDebounceId != null) {
                                clearTimeout(readyDebounceId);
                            }
                            readyDebounceId = setTimeout(() => {
                                readyDebounceId = null;
                                if (cancelled) return;
                                setStatus("ready");
                                setStatusMessage(language!);
                            }, 1500);
                        }
                    }
                });

                // Now that progress listeners are wired up, mark the
                // handshake as connected (waiting for indexing to begin).
                setStatus("connected");
                setStatusMessage(language!);

                // Fallback for servers that never emit workDoneProgress:
                // if we're still in "connected" after 5s with no indexing
                // notification, assume the server is ready.
                readyTimeoutId = setTimeout(() => {
                    readyTimeoutId = null;
                    if (cancelled) return;
                    if (statusRef.current === "connected") {
                        setStatus("ready");
                    }
                }, 5000);
            } catch (err) {
                if (!cancelled) {
                    console.error("[LSP] Connection/start failed:", err);
                    console.error("[LSP] Error type:", typeof err, "String:", String(err));
                    if (err instanceof Error) {
                        console.error("[LSP] Error message:", err.message);
                        console.error("[LSP] Error stack:", err.stack);
                    }
                    clientRef.current = null;
                    setClient(null);
                    setStatus("error");
                    setStatusMessage(String(err));
                }
                connectingRef.current = false;
            }
        }

        connect();

        return () => {
            console.log("[LSP] effect cleanup — cancelling, disconnecting");
            cancelled = true;
            if (readyTimeoutId != null) {
                clearTimeout(readyTimeoutId);
                readyTimeoutId = null;
            }
            if (readyDebounceId != null) {
                clearTimeout(readyDebounceId);
                readyDebounceId = null;
            }
            connectingRef.current = false;
            disconnect();
        };
    }, [language, agentId, workspaceId, enabled, workspaceRoot, disconnect]);

    return {
        status,
        statusMessage,
        client,
    };
}
