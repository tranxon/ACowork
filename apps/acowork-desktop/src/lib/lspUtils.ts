/**
 * Shared LSP utilities and types.
 *
 * Extracted from useLspClient.ts and useLspClientPool.ts so that the
 * LspConnection domain class can use them without importing from hooks.
 */

import type { IWebSocket } from "vscode-ws-jsonrpc";
import { getCachedLspRelayEndpoint, invalidateLspRelayEndpointCache } from "./gateway-api";
import { MonacoVscodeApiWrapper } from "monaco-languageclient/vscodeApiWrapper";
import type { MonacoVscodeApiConfig } from "monaco-languageclient/vscodeApiWrapper";

// ── Types ──────────────────────────────────────────────────────────────

export type LspStatus =
    | "disconnected"
    | "connecting"
    | "connected"
    | "indexing"
    | "ready"
    | "error";

// ── VS Code API initialization ────────────────────────────────────────

let vscodeApiInitPromise: Promise<void> | null = null;
let vscodeApiInitDone = false;

/** Initialize VS Code API services (required by MonacoLanguageClient v10+). */
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
                await wrapper.start({ caller: "LspConnection" });

                // ── Silence Monaco's built-in TypeScript worker diagnostics ──
                // Monaco spawns its own TS worker that uses default compiler
                // options (no tsconfig.json).  With `moduleResolution: "bundler"`
                // in the project, the worker's default options cause false
                // "Cannot find module" errors for packages without `exports`.
                // We disable the worker's diagnostics entirely — the external
                // tsserver (via LSP relay) is the source of truth and reports
                // diagnostics via textDocument/publishDiagnostics.
                const monaco = await import("monaco-editor");
                // monaco.languages.typescript is marked deprecated in type defs
                // but exists at runtime.  Use `any` to bypass TS errors.
                // eslint-disable-next-line @typescript-eslint/no-explicit-any
                const ts = (monaco.languages as any).typescript;
                ts.typescriptDefaults.setDiagnosticsOptions({
                    noSemanticValidation: true,
                    noSyntaxValidation: true,
                    noSuggestionDiagnostics: true,
                });
                ts.javascriptDefaults.setDiagnosticsOptions({
                    noSemanticValidation: true,
                    noSyntaxValidation: true,
                    noSuggestionDiagnostics: true,
                });
                console.log("[LSP] Monaco built-in TS diagnostics disabled — using tsserver");

                vscodeApiInitDone = true;
                console.log(
                    "[LSP] VS Code API services initialized successfully",
                    `elapsed: ${Math.round(performance.now() - t0)}ms`,
                );
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

/** Adapt a browser WebSocket to vscode-ws-jsonrpc's IWebSocket interface. */
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

/**
 * Build the LSP Relay WebSocket URL for a given language.
 *
 * Queries the Gateway for the LSP Relay endpoint (cached), then constructs
 * a direct WebSocket URL to the relay process. The relay's WebSocket
 * handler expects `workspace_root` as a query parameter.
 *
 * @throws if the LSP Relay is not available (not running or not ready).
 */
export async function buildLspWsUrl(
    language: string,
    workspaceRoot?: string,
): Promise<string> {
    const ep = await getCachedLspRelayEndpoint();
    if (!ep || ep.port == null) {
        throw new Error("LSP Relay not available");
    }
    const wsUrl = `ws://${ep.host}:${ep.port}`;
    let url = `${wsUrl}/lsp/${encodeURIComponent(language)}`;
    const params = new URLSearchParams();
    if (workspaceRoot) params.set("workspace_root", workspaceRoot);
    const qs = params.toString();
    const result = qs ? `${url}?${qs}` : url;
    console.log(
        "[LSP] buildLspWsUrl — relay endpoint:",
        `${ep.host}:${ep.port}`,
        "→ result:",
        result,
    );
    return result;
}

// ── Absolute URI builder ───────────────────────────────────────────────

/**
 * Build an absolute LSP file URI from a workspace root and relative path.
 *
 * Handles the Windows extended-length path prefix (\\?\) that
 * std::fs::canonicalize() produces on Windows.
 */
export function buildAbsoluteUri(workspaceRoot: string, relPath: string): string {
    let root = workspaceRoot.replace(/\\/g, "/");
    root = root.replace(/^\/\/\?\//, "").replace(/^\/\?\//, "");
    const rel = relPath.replace(/\\/g, "/");
    const absPath = `${root}/${rel}`;
    if (/^[A-Za-z]:/.test(absPath)) {
        return `file:///${absPath}`;
    }
    return `file://${absPath}`;
}

// ── Language ID mapping ────────────────────────────────────────────────

/**
 * Map a Monaco model's language ID to the correct LSP language ID.
 *
 * Monaco uses "typescript" for both .ts and .tsx, and "javascript" for
 * both .js and .jsx.  tsserver (typescript-language-server) needs explicit
 * "typescriptreact" / "javascriptreact" to enable JSX parsing.  Without
 * this mapping, .tsx files are parsed as plain TypeScript and JSX syntax
 * produces spurious "'>' expected" errors.
 */
export function toLspLanguageId(modelUri: string, monacoLanguageId: string): string {
    const ext = modelUri.split("?")[0].split(".").pop()?.toLowerCase();
    if (monacoLanguageId === "typescript" && ext === "tsx") {
        return "typescriptreact";
    }
    if (monacoLanguageId === "javascript" && ext === "jsx") {
        return "javascriptreact";
    }
    return monacoLanguageId;
}

// ── Error formatting ───────────────────────────────────────────────────

/** Language-specific LSP install hints shown in error messages. */
export const LSP_INSTALL_HINTS: Record<string, string> = {
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

export function formatLspError(language: string, reason: string): string {
    const hint = LSP_INSTALL_HINTS[language.toLowerCase()];
    if (hint) {
        return `${reason}. Install: ${hint}`;
    }
    return reason;
}

/** Re-export for convenience. */
export { invalidateLspRelayEndpointCache };
