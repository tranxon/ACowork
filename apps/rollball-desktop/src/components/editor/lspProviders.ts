/**
 * Monaco LSP Language Feature Providers
 *
 * Registers Completion, Definition, Reference, Hover, and Document Symbol
 * providers that forward requests to an LSP client (via Gateway relay).
 *
 * Each provider converts between Monaco's API and LSP protocol types.
 *
 * LSP uses 0-based line/column, Monaco uses 1-based.
 */

import type { editor, languages, IRange, IPosition, IDisposable } from "monaco-editor";
import type { MonacoLanguageClient } from "monaco-languageclient";
import { getGatewayUrl } from "../../lib/config";
import { buildAbsoluteUri } from "../../hooks/useLspClient";

// ── Preview Model Pool (LRU) ───────────────────────────────────────────

/**
 * LRU tracker for preview models created by ensureModelsForUris.
 * These models are needed for LSP peek widgets but are not explicitly
 * opened in a tab. We cap them at MAX_PREVIEW_MODELS to avoid memory leaks.
 */
const MAX_PREVIEW_MODELS = 20;

// Ordered list: most recently accessed at the end
const previewModelUris: string[] = [];

/**
 * Register a model URI as a preview model (created for LSP, not opened in tab).
 * If the pool exceeds MAX_PREVIEW_MODELS, dispose the least recently used.
 */
function trackPreviewModel(
    monacoInst: typeof import("monaco-editor"),
    uri: import("monaco-editor").Uri
): void {
    const key = uri.toString();

    // Move to end if already tracked (mark as recently used)
    const idx = previewModelUris.indexOf(key);
    if (idx >= 0) {
        previewModelUris.splice(idx, 1);
    }
    previewModelUris.push(key);

    // Evict oldest if over limit
    while (previewModelUris.length > MAX_PREVIEW_MODELS) {
        const oldestKey = previewModelUris.shift()!;
        const oldUri = monacoInst.Uri.parse(oldestKey);
        const oldModel = monacoInst.editor.getModel(oldUri);
        if (oldModel) {
            console.log("[LSP] ModelPool: evicting preview model:", oldestKey);
            oldModel.dispose();
        }
    }
}

/**
 * Remove a URI from the preview model tracker (e.g. when user opens it in a tab).
 * This prevents the model from being evicted by LRU since it's now "pinned" by the tab.
 */
export function unpinPreviewModel(uri: string): void {
    const idx = previewModelUris.indexOf(uri);
    if (idx >= 0) {
        previewModelUris.splice(idx, 1);
    }
}

/**
 * Check if a model URI is tracked as a preview model.
 */
export function isPreviewModel(uri: string): boolean {
    return previewModelUris.includes(uri);
}

/**
 * Dispose a Monaco model when its tab is closed.
 * Only the caller should ensure no other tab references the same file.
 * Also removes from preview model tracker if present.
 */
export function disposeModelForFile(
    monacoInst: typeof import("monaco-editor"),
    relPath: string
): void {
    const monacoUri = monacoInst.Uri.parse(relPath);
    const model = monacoInst.editor.getModel(monacoUri);
    if (model) {
        // Remove from preview tracker (covers both pinned-tab and stray entries)
        unpinPreviewModel(monacoUri.toString());

        model.dispose();
        console.log("[LSP] ModelPool: disposed model for closed tab:", relPath);
    }
}

// ── Coordinate helpers ─────────────────────────────────────────────────

/** Convert Monaco 1-based position to LSP 0-based position */
function toLspPosition(pos: IPosition): { line: number; character: number } {
    return { line: pos.lineNumber - 1, character: pos.column - 1 };
}

/** Convert LSP 0-based range to Monaco 1-based range */
function toMonacoRange(range: {
    start: { line: number; character: number };
    end: { line: number; character: number };
}): IRange {
    return {
        startLineNumber: range.start.line + 1,
        startColumn: range.start.character + 1,
        endLineNumber: range.end.line + 1,
        endColumn: range.end.character + 1,
    };
}

/** Convert an LSP absolute file URI back to a Monaco model URI.
 *
 * LSP servers return absolute URIs like "file:///F:/work/project/core/foo.rs".
 * Monaco models use relative URIs like "file:///core/foo.rs" (because
 * Monaco's Uri.parse() cannot handle Windows file URIs). This function
 * strips the workspace root from the absolute path so the resulting URI
 * matches an existing Monaco model, enabling navigation (F12, Shift+F12).
 *
 * If the URI doesn't match the workspace root (e.g. external file),
 * falls back to Uri.parse() which may work on non-Windows paths.
 */
function lspUriToMonacoUri(
    lspUri: string,
    workspaceRoot: string,
    monacoInst: typeof import("monaco-editor")
): import("monaco-editor").Uri {
    // Only handle file:// URIs
    if (!lspUri.startsWith("file://")) {
        return monacoInst.Uri.parse(lspUri);
    }

    // For file:// URIs, always extract the path manually using regex.
    // Never use Uri.parse() for Windows file URIs because:
    // - Uppercase drive (file:///F:/...) → UriError crash
    // - Lowercase drive (file:///f:/...) → path gets percent-encoded (f%3A)
    // Both cases break the workspace root comparison.
    const winMatch = lspUri.match(/^file:\/\/+([A-Za-z]:\/.*)$/);
    let absPath: string;
    if (winMatch) {
        absPath = "/" + winMatch[1]; // /F:/work/project/core/foo.rs
    } else {
        // Non-Windows file URI — extract path after file://
        const pathPart = lspUri.replace(/^file:\/\//, "");
        absPath = pathPart.startsWith("/") ? pathPart : "/" + pathPart;
    }

    // Strip workspace root from absPath to get relative path
    const root = workspaceRoot.replace(/\\/g, "/").replace(/^\/\/\?\//, "").replace(/^\/\?\//, "");
    // absPath: "/f:/work/project/core/foo.rs" (rust-analyzer uses lowercase drive)
    // root in path form: "F:/work/project" (workspaceRoot uses uppercase)
    const rootInPath = root.startsWith("/") ? root : "/" + root;
    // Windows drive letters are case-insensitive — compare lowercase
    const relPath = absPath.toLowerCase().startsWith(rootInPath.toLowerCase() + "/")
        ? absPath.slice(rootInPath.length + 1)  // "core/foo.rs"
        : null;

    if (relPath) {
        // Create Monaco URI from relative path (same as @monaco-editor/react does)
        // Uri.parse("core/foo.rs") produces file:///core/foo.rs
        return monacoInst.Uri.parse(relPath);
    }

    // Fallback: try to find an existing model whose path is a suffix of the LSP path
    return findModelByUriSuffix(lspUri, monacoInst);
}

/** Find a Monaco model whose URI path is a suffix of the given LSP URI path. */
function findModelByUriSuffix(
    lspUri: string,
    monacoInst: typeof import("monaco-editor")
): import("monaco-editor").Uri {
    // Extract file name segments from the LSP URI
    const segments = lspUri.replace(/^file:\/\/+/, "").split("/");
    const models = monacoInst.editor.getModels();

    for (const model of models) {
        const modelPath = model.uri.path;
        // Try to match from the end: e.g. "core/runtime/src/foo.rs"
        for (let i = 0; i < segments.length; i++) {
            const suffix = segments.slice(i).join("/");
            if (modelPath.endsWith(suffix) || modelPath.endsWith("/" + suffix)) {
                return model.uri;
            }
        }
    }

    // Last resort: try Uri.parse (works on non-Windows)
    return monacoInst.Uri.parse(lspUri);
}

/** Language detection from file extension (subset of Monaco's built-in mappings) */
function detectLanguageFromPath(path: string): string {
    const ext = path.split(".").pop()?.toLowerCase() ?? "";
    const map: Record<string, string> = {
        rs: "rust", py: "python", ts: "typescript", tsx: "typescript",
        js: "javascript", jsx: "javascript", go: "go", json: "json",
        yaml: "yaml", yml: "yaml", md: "markdown", html: "html",
        css: "css", scss: "scss", less: "less", c: "c", cpp: "cpp",
        h: "c", hpp: "cpp", toml: "ini", xml: "xml", sh: "shell",
        bash: "shell", sql: "sql", java: "java", kt: "kotlin",
        swift: "swift", rb: "ruby", lua: "lua", dockerfile: "dockerfile",
    };
    return map[ext] ?? "plaintext";
}

/**
 * Send `textDocument/didOpen` notification to the LSP server for a model.
 *
 * This is necessary because we use `documentSelector: []` to prevent
 * MonacoLanguageClient from auto-tracking documents (which would send
 * didOpen with the wrong URI). So we must manually notify the server
 * whenever a new model is created.
 */
export function sendDidOpenForModel(
    client: MonacoLanguageClient,
    model: editor.ITextModel,
    workspaceRoot: string,
): void {
    const relPath = model.uri.path.replace(/^\/+/, "");
    const absUri = buildAbsoluteUri(workspaceRoot, relPath);
    console.log("[LSP] sendDidOpenForModel — relPath:", relPath, "absUri:", absUri);
    try {
        client.sendNotification("textDocument/didOpen", {
            textDocument: {
                uri: absUri,
                languageId: model.getLanguageId(),
                version: 0,
                text: model.getValue(),
            },
        });
    } catch (err) {
        console.warn("[LSP] sendDidOpenForModel failed:", err);
    }
}

/**
 * Ensure Monaco models exist for all URIs referenced in LSP results.
 *
 * When rust-analyzer returns references/definitions pointing to other files,
 * Monaco needs a model for each file to display the peek widget and enable
 * navigation. This function:
 * 1. Extracts all URIs from the LSP result
 * 2. Maps each absolute LSP URI to a relative Monaco URI
 * 3. For URIs without an existing model, fetches the file content from
 *    Gateway API and creates a model on the fly
 */
async function ensureModelsForUris(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    result: any,
    monacoInst: typeof import("monaco-editor"),
    workspaceRoot: string,
    agentId: string,
    workspaceId: string,
    client?: MonacoLanguageClient
): Promise<void> {
    // Extract all URIs from the result (handles both Location[] and LocationLink[])
    const rawUris: string[] = [];
    if (Array.isArray(result)) {
        for (const item of result) {
            if (item.uri) rawUris.push(item.uri);
            if (item.targetUri) rawUris.push(item.targetUri);
        }
    }
    // Deduplicate URIs — multiple references may point to the same file
    const uris = [...new Set(rawUris)];
    console.log("[LSP] ensureModelsForUris — extracted URIs:", uris.length, "(deduplicated from", rawUris.length, ") workspaceRoot:", workspaceRoot);
    if (uris.length === 0) return;

    // For each URI, check if a model exists; if not, create one
    const fetchPromises: Promise<void>[] = [];
    for (const lspUri of uris) {
        // Compute the relative path from the absolute LSP URI
        const relPath = extractRelPath(lspUri, workspaceRoot);
        console.log("[LSP] ensureModelsForUris — lspUri:", lspUri, "→ relPath:", relPath);
        if (!relPath) continue;

        // Check if a model already exists for this relative path
        const monacoUri = monacoInst.Uri.parse(relPath);
        const existingModel = monacoInst.editor.getModel(monacoUri);
        if (existingModel) {
            console.log("[LSP] ensureModelsForUris — model already exists for:", relPath);
            continue;
        }

        console.log("[LSP] ensureModelsForUris — no model for:", relPath, "fetching...");
        // No model — fetch content and create one
        fetchPromises.push(
            (async () => {
                try {
                    const baseUrl = getGatewayUrl();
                    const params = new URLSearchParams();
                    if (workspaceId && workspaceId !== "__agent_home__") {
                        params.set("workspace_id", workspaceId);
                    }
                    params.set("path", relPath);
                    const url = `${baseUrl}/api/agents/${agentId}/workspaces/file?${params.toString()}`;
                    const resp = await fetch(url);
                    if (!resp.ok) {
                        console.warn("[LSP] Failed to fetch file for model creation:", relPath, resp.status);
                        return;
                    }
                    const data = (await resp.json()) as { content: string };
                    const lang = detectLanguageFromPath(relPath);
                    // Re-check before creating — another parallel fetch may have
                    // already created the model for this URI
                    if (monacoInst.editor.getModel(monacoUri)) {
                        console.log("[LSP] Model created by parallel fetch, skipping:", relPath);
                        return;
                    }
                    monacoInst.editor.createModel(data.content, lang, monacoUri);
                    console.log("[LSP] Created model for:", relPath, "lang:", lang, "size:", data.content.length);
                    // Register as preview model — capped by LRU eviction
                    trackPreviewModel(monacoInst, monacoUri);
                    // Notify the LSP server about the new model so it can
                    // provide language features (hover, definition, etc.)
                    const newModel = monacoInst.editor.getModel(monacoUri);
                    if (newModel && client) {
                        sendDidOpenForModel(client, newModel, workspaceRoot);
                    }
                } catch (err) {
                    console.warn("[LSP] Error creating model for:", relPath, err);
                }
            })()
        );
    }

    if (fetchPromises.length > 0) {
        console.log("[LSP] ensureModelsForUris — awaiting", fetchPromises.length, "fetch(es)");
        await Promise.all(fetchPromises);
        console.log("[LSP] ensureModelsForUris — all fetches done");
    } else {
        console.log("[LSP] ensureModelsForUris — no fetches needed");
    }
}

/** Extract relative path from an absolute LSP file URI, given workspace root. */
function extractRelPath(lspUri: string, workspaceRoot: string): string | null {
    if (!lspUri.startsWith("file://")) return null;

    // Extract path from URI — handle Windows file URIs that Uri.parse can't handle
    let absPath: string;
    const match = lspUri.match(/^file:\/\/+([A-Za-z]:\/.*)$/);
    if (match) {
        absPath = "/" + match[1]; // /F:/work/project/core/foo.rs
    } else {
        // Non-Windows or already-parsed path
        const pathPart = lspUri.replace(/^file:\/\//, "");
        absPath = pathPart.startsWith("/") ? pathPart : "/" + pathPart;
    }

    const root = workspaceRoot.replace(/\\/g, "/").replace(/^\/\/\?\//, "").replace(/^\/\?\//, "");
    const rootInPath = root.startsWith("/") ? root : "/" + root;
    // Windows drive letters are case-insensitive (rust-analyzer uses lowercase,
    // workspaceRoot uses uppercase). Normalize both to lowercase for comparison.
    if (absPath.toLowerCase().startsWith(rootInPath.toLowerCase() + "/")) {
        // Use the length of rootInPath (not lowercase) to slice from absPath
        return absPath.slice(rootInPath.length + 1);
    }
    return null;
}

/** Convert Monaco model URI to an absolute LSP file URI.
 *
 * Monaco models use relative paths as URIs (e.g. file:///core/rollball-runtime/src/main.rs)
 * because Monaco's Uri.parse() cannot handle Windows absolute file URIs.
 * This function maps the relative model URI to an absolute file URI using
 * the workspace root, matching the URI used in textDocument/didOpen.
 */
function toLspUri(model: editor.ITextModel, workspaceRoot: string): string {
    const uri = model.uri;
    // Extract the relative path from the model URI
    // file:///core/rollball-runtime/src/main.rs → core/rollball-runtime/src/main.rs
    const relPath = uri.path.replace(/^\/+/, "");
    let root = workspaceRoot.replace(/\\/g, "/");
    // Strip Windows extended-length path prefix: //?/ or /?/
    root = root.replace(/^\/\/\?\//, "").replace(/^\/\?\//, "");
    const absPath = `${root}/${relPath}`;
    // file:///F:/work/... on Windows, file:///home/... on Unix
    if (/^[A-Za-z]:/.test(absPath)) {
        return `file:///${absPath}`;
    }
    return `file://${absPath}`;
}

// ── Provider factory ───────────────────────────────────────────────────

export interface LspProvidersConfig {
    /** The LSP client to forward requests to */
    client: MonacoLanguageClient;
    /** Language ID (e.g. "rust", "python") */
    language: string;
    /** Absolute workspace root path for URI mapping */
    workspaceRoot: string;
    /** Agent ID for fetching file content from Gateway API */
    agentId: string;
    /** Workspace ID for fetching file content from Gateway API */
    workspaceId: string;
}

/**
 * Register all LSP-backed language feature providers for the given language.
 * Call this after Monaco editor is mounted and the LSP client is connected.
 *
 * Returns a disposable that unregisters all providers.
 */
export function registerLspProviders(
    monaco: typeof import("monaco-editor"),
    config: LspProvidersConfig
): IDisposable {
    const disposables: IDisposable[] = [];

    const { client, language, workspaceRoot, agentId, workspaceId } = config;

    // Debug: log what models exist for this language
    const models = monaco.editor.getModels();
    console.log("[LSP] registerLspProviders — language:", language, "models:", models.map(m => ({ uri: m.uri.toString(), lang: m.getLanguageId() })));

    // ── Completion Provider (Ctrl+Space) ───────────────────────────────
    disposables.push(
        monaco.languages.registerCompletionItemProvider(
            language,
            {
                triggerCharacters: [".", ":", "'", '"', "/", "@", ">", "-"],
                async provideCompletionItems(model, position) {
                    const params = {
                        textDocument: { uri: toLspUri(model, workspaceRoot) },
                        position: toLspPosition(position),
                    };
                    try {
                        const result = await client.sendRequest(
                            "textDocument/completion",
                            params
                        );
                        return asCompletionList(result);
                    } catch {
                        return { suggestions: [] };
                    }
                },
                async resolveCompletionItem(item) {
                    // LSP completion items already contain all info;
                    // resolve is only needed for additionalTextEdits etc.
                    return item;
                },
            }
        )
    );

    // ── Definition Provider (F12 / Ctrl+Click) ─────────────────────────
    disposables.push(
        monaco.languages.registerDefinitionProvider(language, {
            async provideDefinition(model, position) {
                const lspUri = toLspUri(model, workspaceRoot);
                const params = {
                    textDocument: { uri: lspUri },
                    position: toLspPosition(position),
                };
                console.log("[LSP] definition request — modelUri:", model.uri.toString(),
                    "lspUri:", lspUri, "pos:", JSON.stringify(params.position));
                try {
                    const t0 = performance.now();
                    const result = await client.sendRequest(
                        "textDocument/definition",
                        params
                    );
                    const ms = (performance.now() - t0).toFixed(0);
                    console.log("[LSP] definition result:", result, `(${ms}ms)`);
                    // Ensure models exist for all referenced files so Monaco
                    // can display the peek widget and navigate to them
                    await ensureModelsForUris(result, monaco, workspaceRoot, agentId, workspaceId, client);
                    const links = asLocationLinks(result, monaco, workspaceRoot);
                    console.log("[LSP] definition asLocationLinks:", links.length, "items", links.map(l => l.uri.toString()));
                    return links;
                } catch (err) {
                    console.warn("[LSP] definition error:", err);
                    return [];
                }
            },
        })
    );

    // ── Reference Provider (Shift+F12) ─────────────────────────────────
    disposables.push(
        monaco.languages.registerReferenceProvider(language, {
            async provideReferences(model, position, context) {
                const lspUri = toLspUri(model, workspaceRoot);
                const params = {
                    textDocument: { uri: lspUri },
                    position: toLspPosition(position),
                    context: { includeDeclaration: context.includeDeclaration },
                };
                console.log("[LSP] references request — modelUri:", model.uri.toString(), "lspUri:", lspUri);
                try {
                    const t0 = performance.now();
                    const result = await client.sendRequest(
                        "textDocument/references",
                        params
                    );
                    const ms = (performance.now() - t0).toFixed(0);
                    console.log("[LSP] references result:", result, `(${ms}ms)`);
                    // Ensure models exist for all referenced files so Monaco
                    // can display the reference peek widget and navigate
                    await ensureModelsForUris(result, monaco, workspaceRoot, agentId, workspaceId, client);
                    const locs = asLocations(result, monaco, workspaceRoot);
                    console.log("[LSP] references asLocations:", locs.length, "items", locs.map(l => l.uri.toString()));
                    return locs;
                } catch (err) {
                    console.warn("[LSP] references error:", err);
                    return [];
                }
            },
        })
    );

    // ── Hover Provider (mouse hover) ───────────────────────────────────
    disposables.push(
        monaco.languages.registerHoverProvider(language, {
            async provideHover(model, position) {
                const lspUri = toLspUri(model, workspaceRoot);
                const params = {
                    textDocument: { uri: lspUri },
                    position: toLspPosition(position),
                };
                console.log("[LSP] hover request — modelUri:", model.uri.toString(), "lspUri:", lspUri);
                try {
                    const t0 = performance.now();
                    const result = await client.sendRequest("textDocument/hover", params);
                    const ms = (performance.now() - t0).toFixed(0);
                    console.log("[LSP] hover result:", result, `(${ms}ms)`);
                    return asHover(result);
                } catch (err) {
                    console.warn("[LSP] hover error:", err);
                    return null;
                }
            },
        })
    );

    // ── Document Symbol Provider (Ctrl+Shift+O) ────────────────────────
    disposables.push(
        monaco.languages.registerDocumentSymbolProvider(language, {
            async provideDocumentSymbols(model) {
                const params = {
                    textDocument: { uri: toLspUri(model, workspaceRoot) },
                };
                try {
                    const result = await client.sendRequest(
                        "textDocument/documentSymbol",
                        params
                    );
                    return asDocumentSymbols(result);
                } catch {
                    return [];
                }
            },
        })
    );

    return {
        dispose() {
            for (const d of disposables) d.dispose();
        },
    };
}

// ── LSP → Monaco type converters ──────────────────────────────────────

interface LspPosition {
    line: number;
    character: number;
}

interface LspRange {
    start: LspPosition;
    end: LspPosition;
}

interface LspLocation {
    uri: string;
    range: LspRange;
}

interface LspLocationLink {
    targetUri: string;
    targetRange: LspRange;
    targetSelectionRange: LspRange;
    originSelectionRange?: LspRange;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    [key: string]: any;
}

interface LspCompletionItem {
    label: string;
    kind?: number;
    detail?: string;
    documentation?: string | { kind: string; value: string };
    insertText?: string;
    insertTextFormat?: number;
    sortText?: string;
    filterText?: string;
    textEdit?:
    | LspRange
    | { range: LspRange; newText: string };
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    [key: string]: any;
}

interface LspHover {
    contents:
    | string
    | { language: string; value: string }
    | Array<string | { language: string; value: string }>;
    range?: LspRange;
}

interface LspSymbolInformation {
    name: string;
    kind: number;
    location: LspLocation;
    containerName?: string;
}

interface LspDocumentSymbol {
    name: string;
    kind: number;
    range: LspRange;
    selectionRange: LspRange;
    detail?: string;
    children?: LspDocumentSymbol[];
}

function asCompletionList(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    result: any
): languages.CompletionList {
    const items = Array.isArray(result) ? result : result?.items ?? [];
    return {
        suggestions: items.map((item: LspCompletionItem) => ({
            label: item.label,
            kind: (item.kind ?? 0) as languages.CompletionItemKind,
            detail: item.detail,
            documentation: item.documentation,
            insertText: item.insertText ?? item.label,
            sortText: item.sortText,
            filterText: item.filterText,
            range: item.textEdit
                ? "range" in item.textEdit
                    ? toMonacoRange(item.textEdit.range)
                    : undefined
                : undefined,
        })),
    };
}

function asLocationLinks(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    result: any,
    monacoInst: typeof import("monaco-editor"),
    workspaceRoot: string
): languages.LocationLink[] {
    const items: LspLocationLink[] | LspLocation[] = result ?? [];
    if (!Array.isArray(items)) return [];

    const mapped = items
        .map((item) => {
            if ("targetUri" in item) {
                // LocationLink
                return {
                    uri: lspUriToMonacoUri(item.targetUri, workspaceRoot, monacoInst),
                    range: toMonacoRange(item.targetRange),
                    originSelectionRange: item.originSelectionRange
                        ? toMonacoRange(item.originSelectionRange)
                        : undefined,
                };
            }
            // Location
            return {
                uri: lspUriToMonacoUri(item.uri, workspaceRoot, monacoInst),
                range: toMonacoRange(item.range),
            };
        });
    console.log("[LSP] asLocationLinks — mapped:", mapped.map(l => ({
        uri: l.uri.toString(),
        hasModel: !!monacoInst.editor.getModel(l.uri),
    })));

    return mapped
        .filter((loc) => {
            // Safety filter: only include locations that have a Monaco model.
            // Without this, Monaco's reference widget crashes with "Model not found"
            // when trying to create a preview for a file without a model.
            const model = monacoInst.editor.getModel(loc.uri);
            if (!model) {
                console.warn("[LSP] asLocationLinks — skipping location, no model:", loc.uri.toString());
                return false;
            }
            return true;
        });
}

function asLocations(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    result: any,
    monacoInst: typeof import("monaco-editor"),
    workspaceRoot: string
): languages.Location[] {
    const items: LspLocation[] = result ?? [];
    if (!Array.isArray(items)) return [];
    const mapped = items
        .map((item) => ({
            uri: lspUriToMonacoUri(item.uri, workspaceRoot, monacoInst),
            range: toMonacoRange(item.range),
        }));
    console.log("[LSP] asLocations — mapped:", mapped.map(l => ({
        uri: l.uri.toString(),
        hasModel: !!monacoInst.editor.getModel(l.uri),
    })));
    return mapped
        .filter((loc) => {
            // Safety filter: only include locations that have a Monaco model.
            const model = monacoInst.editor.getModel(loc.uri);
            if (!model) {
                console.warn("[LSP] asLocations — skipping location, no model:", loc.uri.toString());
                return false;
            }
            return true;
        });
}

function asHover(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    result: any
): languages.Hover | null {
    if (!result) return null;
    const hover = result as LspHover;
    const contents: Array<{ value: string }> = [];
    const items: Array<unknown> = Array.isArray(hover.contents) ? hover.contents : [hover.contents];
    for (const raw of items) {
        if (typeof raw === "string") {
            contents.push({ value: raw });
        } else if (typeof raw === "object" && raw !== null && "value" in raw) {
            const item = raw as { value: string; kind?: string; language?: string };
            if (item.kind) {
                // MarkupContent (LSP 3.0+) — { kind: "markdown"|"plaintext", value: string }
                contents.push({
                    value: item.kind === "markdown" ? item.value : `\`\`\`plaintext\n${item.value}\n\`\`\``
                });
            } else if (item.language) {
                // MarkedString with language — { language: string, value: string }
                contents.push({ value: `\`\`\`${item.language}\n${item.value}\n\`\`\`` });
            } else {
                contents.push({ value: String(item.value) });
            }
        }
    }
    return {
        contents,
        range: hover.range ? toMonacoRange(hover.range) : undefined,
    };
}

function asDocumentSymbols(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    result: any
): languages.DocumentSymbol[] {
    const items: Array<LspSymbolInformation | LspDocumentSymbol> = result ?? [];
    if (!Array.isArray(items)) return [];

    // If result contains SymbolInformation (flat), skip for now.
    // DocumentSymbol is hierarchical.
    return items
        .filter((s): s is LspDocumentSymbol => "range" in s && "selectionRange" in s)
        .map(convertDocumentSymbol);
}

function convertDocumentSymbol(sym: LspDocumentSymbol): languages.DocumentSymbol {
    return {
        name: sym.name,
        detail: sym.detail ?? "",
        kind: (sym.kind ?? 0) as languages.SymbolKind,
        tags: [],
        range: toMonacoRange(sym.range),
        selectionRange: toMonacoRange(sym.selectionRange),
        children: (sym.children ?? []).map(convertDocumentSymbol),
    };
}
