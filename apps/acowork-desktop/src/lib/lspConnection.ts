/**
 * LspConnection — single-language LSP connection state machine.
 *
 * Framework-agnostic domain class.  Owns the WebSocket, MonacoLanguageClient,
 * progress tracking, and connection lifecycle.  Reports status changes via a
 * callback so that React hooks can bridge them into component state.
 *
 * Concurrency model:
 *   - connect() is idempotent: concurrent callers await the same promise.
 *   - disconnect() aborts any in-flight connect and tears down resources.
 */

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
    formatLspError,
    toLspLanguageId,
} from "./lspUtils";
import { discoverProjectRoot } from "./lspProjectRoot";

// ── Types ──────────────────────────────────────────────────────────────

export interface LspConnectParams {
    language: string;
    workspaceRoot: string;
    agentId?: string;
    workspaceId?: string;
}

export type StatusCallback = (status: LspStatus, message: string) => void;

// ── Constants ──────────────────────────────────────────────────────────

const START_TIMEOUT_MS = 30_000;
const READY_FALLBACK_MS = 5_000;
const READY_DEBOUNCE_MS = 1_500;

// ── LspConnection ──────────────────────────────────────────────────────

export class LspConnection {
    private _ws: WebSocket | null = null;
    private _client: MonacoLanguageClient | null = null;
    private _status: LspStatus = "disconnected";
    private _statusMessage = "";
    private _handshakeDone = false;
    private _paramsKey = "";
    private _connectPromise: Promise<void> | null = null;
    private _abortController: AbortController | null = null;

    // ── progress tracking ────────────────────────────────────────────
    private _activeProgressTokens = new Set<string | number>();
    private _readyTimeoutId: ReturnType<typeof setTimeout> | null = null;
    private _readyDebounceId: ReturnType<typeof setTimeout> | null = null;

    constructor(
        private _onStatusChange?: StatusCallback,
    ) {}

    // ── public accessors ─────────────────────────────────────────────

    get client(): MonacoLanguageClient | null {
        return this._client;
    }
    get status(): LspStatus {
        return this._status;
    }
    get statusMessage(): string {
        return this._statusMessage;
    }
    get handshakeDone(): boolean {
        return this._handshakeDone;
    }
    get paramsKey(): string {
        return this._paramsKey;
    }

    // ── connect (idempotent entry point) ─────────────────────────────

    /**
     * Connect (or re-connect) to the LSP server for this language.
     *
     * Idempotent — if a connection is already in flight the caller awaits
     * the same promise.  If already connected with the same params the
     * call returns immediately.
     */
    async connect(params: LspConnectParams): Promise<void> {
        // ── concurrency guard ──
        if (this._connectPromise) {
            try {
                await this._connectPromise;
            } catch {
                // connection failed — caller may retry
            }
            return;
        }

        // ── already connected with same params ──
        const key = this._buildParamsKey(params);
        if (this._handshakeDone && key === this._paramsKey) {
            return;
        }

        // ── allocate before any await ──
        this._abortController = new AbortController();
        let resolveConnect: () => void;
        this._connectPromise = new Promise<void>((r) => {
            resolveConnect = r;
        });

        try {
            await this._doConnect(params, key);
        } finally {
            this._connectPromise = null;
            this._abortController = null;
            resolveConnect!();
        }
    }

    // ── disconnect ───────────────────────────────────────────────────

    /**
     * Tear down the connection.  Safe to call multiple times.
     *
     * Fire-and-forget calls {@link MonacoLanguageClient.stop} to unregister
     * Monaco commands and dispose feature handlers.  shutdown/exit messages
     * may fail (WebSocket already closed), but the feature cleanup phase
     * still executes — preventing "command already exists" errors when a
     * new client connects later.
     */
    disconnect(): void {
        this._abortController?.abort();
        this._clearTimers();
        this._handshakeDone = false;
        this._paramsKey = "";

        // Save refs before nulling
        const client = this._client;
        this._client = null;

        const ws = this._ws;
        this._ws = null;

        if (ws) {
            try {
                ws.close();
            } catch {
                // ignore
            }
        }

        // Cleanly dispose the previous client to unregister commands and
        // feature handlers.  shutdown/exit will fail gracefully (WS closed),
        // but the dispose phase still clears Monaco's command registry.
        if (client) {
            client.stop().catch(() => {
                // shutdown failed — features already cleaned up
            });
        }

        this._setStatus("disconnected", "");
    }

    // ── internal helpers ─────────────────────────────────────────────

    private _setStatus(status: LspStatus, message: string): void {
        this._status = status;
        this._statusMessage = message;
        this._onStatusChange?.(status, message);
    }

    private _buildParamsKey(p: LspConnectParams): string {
        return `${p.language}|${p.agentId ?? ""}|${p.workspaceId ?? ""}|${p.workspaceRoot}`;
    }

    private _clearTimers(): void {
        if (this._readyTimeoutId != null) {
            clearTimeout(this._readyTimeoutId);
            this._readyTimeoutId = null;
        }
        if (this._readyDebounceId != null) {
            clearTimeout(this._readyDebounceId);
            this._readyDebounceId = null;
        }
    }

    private _cancelled(): boolean {
        return this._abortController?.signal.aborted ?? false;
    }

    // ── connection flow ──────────────────────────────────────────────

    private async _doConnect(
        params: LspConnectParams,
        paramsKey: string,
    ): Promise<void> {
        const { language, workspaceRoot } = params;
        const signal = this._abortController!.signal;

        // 1. Drop any previous resources (inline — do NOT call disconnect(),
        //    which would abort the AbortController we just created in connect()).
        this._clearTimers();
        this._handshakeDone = false;
        this._paramsKey = "";

        // Close previous WebSocket if any
        const prevWs = this._ws;
        this._ws = null;
        if (prevWs) {
            try {
                prevWs.close();
            } catch {
                // ignore
            }
        }

        // Dispose previous client to unregister commands and feature handlers.
        // Fire-and-forget — the new client's start() will register fresh commands.
        const prevClient = this._client;
        this._client = null;
        if (prevClient) {
            prevClient.stop().catch(() => {
                // shutdown failed — features already cleaned up
            });
        }

        if (signal.aborted) return;

        this._paramsKey = paramsKey;
        this._setStatus("connecting", "");

        // 2. Discover language-specific project root
        let projectRoot = workspaceRoot;
        try {
            const monaco = await import("monaco-editor");
            const models = monaco.editor.getModels();
            const firstModel = models.find(
                (m) => m.getLanguageId() === language,
            );
            if (firstModel) {
                const relPath = firstModel.uri.path.replace(/^\/+/, "");
                const absPath = `${workspaceRoot.replace(/\\/g, "/")}/${relPath}`;
                projectRoot = await discoverProjectRoot(
                    absPath,
                    language,
                    workspaceRoot,
                );
                console.log(
                    "[LSP] LspConnection project root —",
                    language,
                    "workspace:",
                    workspaceRoot,
                    "→ project:",
                    projectRoot,
                );
            }
        } catch (err) {
            console.warn(
                "[LSP] LspConnection project root discovery failed —",
                language,
                err,
            );
        }
        if (signal.aborted) return;

        // 3. Build WebSocket URL and connect
        const t0 = performance.now();
        let wsUrl: string;
        try {
            wsUrl = await buildLspWsUrl(language, projectRoot);
        } catch (err) {
            if (signal.aborted) return;
            console.error("[LSP] LspConnection buildLspWsUrl failed —", language, err);
            this._setStatus(
                "error",
                formatLspError(language, `LSP relay unavailable: ${err}`),
            );
            return;
        }
        console.log("[LSP] LspConnection connecting —", language, "url:", wsUrl);

        let ws: WebSocket;
        try {
            ws = new WebSocket(wsUrl);
        } catch (err) {
            if (signal.aborted) return;
            console.error("[LSP] LspConnection ws ctor failed —", language, err);
            this._setStatus(
                "error",
                formatLspError(language, `Failed to connect: ${err}`),
            );
            return;
        }
        this._ws = ws;

        // 4. Wait for socket open
        try {
            await this._waitForOpen(ws, signal);
        } catch (err) {
            if (signal.aborted) return;
            console.error("[LSP] LspConnection ws open failed —", language, err);
            this._setStatus(
                "error",
                formatLspError(language, String(err)),
            );
            return;
        }
        if (signal.aborted) return;

        const t1 = performance.now();
        console.log(
            "[LSP] LspConnection ws opened —",
            language,
            `elapsed: ${Math.round(t1 - t0)}ms`,
        );

        // 5. Init VS Code API
        try {
            await ensureVscodeApiInitialized();
        } catch (err) {
            if (signal.aborted) return;
            console.error("[LSP] LspConnection VS Code API init failed —", language, err);
            this._setStatus(
                "error",
                formatLspError(language, `VS Code API init failed: ${err}`),
            );
            return;
        }
        if (signal.aborted) return;

        const t2 = performance.now();
        console.log(
            "[LSP] LspConnection vscode api ready —",
            language,
            `elapsed: ${Math.round(t2 - t1)}ms`,
        );

        // 6. Create transports
        const socket = adaptWebSocket(ws);
        const reader = new WebSocketMessageReader(socket);
        const writer = new WebSocketMessageWriter(socket);
        const messageTransports: MessageTransports = { reader, writer };

        // 7. Build client options
        const monaco = await import("monaco-editor");
        const rootFolderUri = monaco.Uri.file(projectRoot);
        const clientOptions: LanguageClientOptions = {
            documentSelector: [],
            workspaceFolder: {
                uri: rootFolderUri,
                name: "workspace",
                index: 0,
            },
            initializationOptions: {
                workspaceFolders: [projectRoot],
                settings: {
                    java: {
                        import: {
                            gradle: {
                                enabled: true,
                                wrapper: { enabled: true },
                            },
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
        console.log(
            "[LSP] LspConnection transports ready —",
            language,
            `elapsed: ${Math.round(t3 - t2)}ms`,
        );

        // 8. Create and start MonacoLanguageClient
        const lspClient = new MonacoLanguageClient({
            name: `${language} LSP`,
            clientOptions,
            messageTransports,
        });

        const t4 = performance.now();
        console.log(
            "[LSP] LspConnection MonacoLanguageClient created —",
            language,
            `elapsed: ${Math.round(t4 - t3)}ms`,
        );

        // Wire state-change listener
        lspClient.onDidChangeState((e) => {
            console.log(
                "[LSP] LspConnection client state —",
                language,
                State[e.oldState],
                "→",
                State[e.newState],
            );
            if (e.newState === State.Stopped && !this._cancelled()) {
                this._setStatus("disconnected", "");
            }
        });

        // Post-open ws handlers
        ws.onclose = (e) => {
            if (this._cancelled()) return;
            console.warn(
                "[LSP] LspConnection ws closed after open —",
                language,
                "code:",
                e.code,
                "reason:",
                e.reason,
            );
            this._handshakeDone = false;
            this._paramsKey = "";

            // Dispose the dead client to unregister commands before nulling.
            // The client is already Stopped (onDidChangeState fired first),
            // but stop() still executes feature cleanup.
            const deadClient = this._client;
            this._client = null;
            this._ws = null;
            if (deadClient) {
                deadClient.stop().catch(() => {});
            }

            this._setStatus(
                "error",
                formatLspError(language, `Connection lost (${e.code})`),
            );
        };
        ws.onerror = (ev) => {
            if (!this._cancelled()) {
                console.error(
                    "[LSP] LspConnection ws error after open —",
                    language,
                    ev,
                );
            }
        };

        // 9. Start handshake (with timeout + retry for shutdown races)
        console.log("[LSP] LspConnection calling lspClient.start() —", language);

        let attempt = 0;
        let startResult: "ok" | "timeout";
        // eslint-disable-next-line no-constant-condition
        while (true) {
            let timeoutId: undefined | ReturnType<typeof setTimeout> = undefined;
            attempt++;
            try {
                startResult = await Promise.race([
                    lspClient.start().then(() => {
                        clearTimeout(timeoutId);
                        return "ok" as const;
                    }),
                    new Promise<"timeout">((resolve) => {
                        timeoutId = setTimeout(
                            () => resolve("timeout"),
                            START_TIMEOUT_MS,
                        );
                    }),
                ]);
                break;
            } catch (err: any) {
                if (timeoutId !== undefined) clearTimeout(timeoutId);
                const msg = String(err?.message ?? err ?? "");
                if (
                    msg.includes("Shutdown already requested") &&
                    attempt < 5
                ) {
                    console.warn(
                        "[LSP] LspConnection start retry —",
                        language,
                        "attempt",
                        attempt,
                        "reason:",
                        msg,
                    );
                    await new Promise((r) => setTimeout(r, 600 * attempt));
                    continue;
                }
                throw err;
            }
        }

        if (signal.aborted) return;

        if (startResult === "timeout") {
            console.error(
                "[LSP] LspConnection start timeout —",
                language,
                `total elapsed: ${Math.round(performance.now() - t0)}ms`,
            );
            this._client = null;
            try {
                lspClient.stop();
            } catch {
                // ignore
            }
            this._setStatus(
                "error",
                formatLspError(
                    language,
                    `Initialize timed out (${START_TIMEOUT_MS / 1000}s). Check Gateway logs for LSP errors.`,
                ),
            );
            return;
        }

        const t5 = performance.now();
        console.log(
            "[LSP] LspConnection client started —",
            language,
            `start() elapsed: ${Math.round(t5 - t4)}ms`,
            `total elapsed: ${Math.round(t5 - t0)}ms`,
        );

        // 10. Send didOpen for all currently-open models of this language
        try {
            const models = monaco.editor.getModels();
            for (const model of models) {
                if (model.getLanguageId() !== language) continue;
                const absUri = buildAbsoluteUri(
                    workspaceRoot,
                    model.uri.path.replace(/^\/+/, ""),
                );
                try {
                    lspClient.sendNotification("textDocument/didOpen", {
                        textDocument: {
                            uri: absUri,
                            languageId: toLspLanguageId(absUri, model.getLanguageId()),
                            version: 0,
                            text: model.getValue(),
                        },
                    });
                } catch (err) {
                    console.warn(
                        "[LSP] LspConnection manual didOpen failed —",
                        language,
                        err,
                    );
                }
            }
        } catch (err) {
            console.warn(
                "[LSP] LspConnection monaco import failed —",
                language,
                err,
            );
        }

        // 11. Send workspace/didChangeConfiguration (jdtls needs this)
        try {
            lspClient.sendNotification(
                "workspace/didChangeConfiguration",
                {
                    settings: {
                        java: {
                            import: {
                                gradle: {
                                    enabled: true,
                                    wrapper: { enabled: true },
                                },
                                maven: { enabled: true },
                            },
                        },
                    },
                },
            );
            console.log(
                "[LSP] LspConnection sent workspace/didChangeConfiguration —",
                language,
            );
        } catch (err) {
            console.warn(
                "[LSP] LspConnection didChangeConfiguration failed —",
                language,
                err,
            );
        }

        // 12. Mark handshake complete
        this._handshakeDone = true;
        this._client = lspClient;
        this._setStatus("connected", language);

        const t6 = performance.now();
        console.log(
            "[LSP] LspConnection handshake complete —",
            language,
            `didOpen+publish elapsed: ${Math.round(t6 - t5)}ms`,
            `total elapsed: ${Math.round(t6 - t0)}ms`,
        );

        // 13. Ready fallback timer (servers that never emit workDoneProgress)
        this._readyTimeoutId = setTimeout(() => {
            this._readyTimeoutId = null;
            if (this._cancelled()) return;
            if (this._status === "connected") {
                this._setStatus("ready", language);
            }
        }, READY_FALLBACK_MS);

        // 14. Progress tracking
        lspClient.onNotification(
            "window/workDoneProgress/create",
            (p: any) => {
                const token = p?.token;
                if (token != null) this._activeProgressTokens.add(token);
            },
        );

        lspClient.onNotification("$/progress" as any, (p: any) => {
            console.log(
                "[LSP] $/progress received —",
                p?.value?.kind,
                p?.value?.title || "",
                "token:",
                p?.token,
            );
            if (this._cancelled()) return;

            const token = p?.token;
            const kind = p?.value?.kind;
            const title = p?.value?.title || "";

            if (kind === "begin") {
                if (token != null) this._activeProgressTokens.add(token);
                // Cancel debounce — a new phase started
                if (this._readyDebounceId != null) {
                    clearTimeout(this._readyDebounceId);
                    this._readyDebounceId = null;
                }
                // Cancel fallback timer
                if (this._readyTimeoutId != null) {
                    clearTimeout(this._readyTimeoutId);
                    this._readyTimeoutId = null;
                }
                this._setStatus(
                    "indexing",
                    title || `${language} analyzing`,
                );
            } else if (kind === "report") {
                const percentage = p?.value?.percentage;
                if (percentage != null) {
                    this._setStatus(
                        "indexing",
                        `${title || "analyzing"} ${Math.round(percentage)}%`,
                    );
                }
            } else if (kind === "end") {
                if (token != null) this._activeProgressTokens.delete(token);
                if (this._activeProgressTokens.size === 0) {
                    if (this._readyDebounceId != null) {
                        clearTimeout(this._readyDebounceId);
                    }
                    this._readyDebounceId = setTimeout(() => {
                        this._readyDebounceId = null;
                        if (this._cancelled()) return;
                        this._setStatus("ready", language);
                    }, READY_DEBOUNCE_MS);
                }
            }
        });
    }

    // ── WebSocket open helper ────────────────────────────────────────

    private _waitForOpen(
        ws: WebSocket,
        signal: AbortSignal,
    ): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            const onAbort = () => {
                cleanup();
                ws.close();
                resolve(); // don't reject — caller checks signal.aborted
            };

            const onOpen = () => {
                cleanup();
                if (signal.aborted) {
                    ws.close();
                    resolve();
                    return;
                }
                resolve();
            };

            const onError = () => {
                cleanup();
                reject(new Error("WebSocket connection failed"));
            };

            const onClose = (e: CloseEvent) => {
                cleanup();
                reject(
                    new Error(
                        `Connection closed (${e.code})${e.reason ? ": " + e.reason : ""}`,
                    ),
                );
            };

            const cleanup = () => {
                ws.removeEventListener("open", onOpen);
                ws.removeEventListener("error", onError);
                ws.removeEventListener("close", onClose);
                signal.removeEventListener("abort", onAbort);
            };

            ws.addEventListener("open", onOpen);
            ws.addEventListener("error", onError);
            ws.addEventListener("close", onClose);
            signal.addEventListener("abort", onAbort);

            // Belt-and-suspenders: if already open, resolve immediately
            if (ws.readyState === WebSocket.OPEN) {
                cleanup();
                resolve();
            }
        });
    }
}
