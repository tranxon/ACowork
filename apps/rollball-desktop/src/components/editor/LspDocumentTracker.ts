import type { editor } from "monaco-editor";
import type { MonacoLanguageClient } from "monaco-languageclient";
import { buildAbsoluteUri } from "../../hooks/useLspClient";

/**
 * Manages LSP textDocument lifecycle notifications (didOpen/didChange/didClose).
 *
 * Ensures each document is opened exactly once per LSP client,
 * tracks content versions, and sends proper close notifications.
 */
export class LspDocumentTracker {
    // absUri → { version, language, disposable (onDidChangeContent listener) }
    private openDocuments = new Map<string, {
        version: number;
        language: string;
        changeDisposable: import("monaco-editor").IDisposable | null;
    }>();

    private workspaceRoot: string;

    constructor(workspaceRoot: string) {
        this.workspaceRoot = workspaceRoot;
    }

    /**
     * Track a model as open — send didOpen if not already tracked.
     * Also registers onDidChangeContent listener for didChange notifications.
     */
    trackOpen(client: MonacoLanguageClient, model: editor.ITextModel): void {
        const relPath = model.uri.path.replace(/^\/+/, "");
        const absUri = buildAbsoluteUri(this.workspaceRoot, relPath);

        if (this.openDocuments.has(absUri)) {
            // Already tracked — skip duplicate didOpen
            return;
        }

        // Send didOpen
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
            console.warn("[LSP] DocumentTracker: didOpen failed:", absUri, err);
            return;
        }

        // Register change listener
        const changeDisposable = model.onDidChangeContent((e) => {
            this.sendDidChange(client, model, absUri, e);
        });

        this.openDocuments.set(absUri, {
            version: 0,
            language: model.getLanguageId(),
            changeDisposable,
        });

        console.log("[LSP] DocumentTracker: opened", absUri);
    }

    /**
     * Send textDocument/didChange with incremental content changes.
     */
    private sendDidChange(
        client: MonacoLanguageClient,
        _model: editor.ITextModel,
        absUri: string,
        event: editor.IModelContentChangedEvent
    ): void {
        const entry = this.openDocuments.get(absUri);
        if (!entry) return;

        entry.version++;

        // Convert Monaco content changes to LSP format
        const contentChanges = event.changes.map((change) => ({
            range: {
                start: { line: change.range.startLineNumber - 1, character: change.range.startColumn - 1 },
                end: { line: change.range.endLineNumber - 1, character: change.range.endColumn - 1 },
            },
            rangeLength: change.rangeLength,
            text: change.text,
        }));

        try {
            client.sendNotification("textDocument/didChange", {
                textDocument: { uri: absUri, version: entry.version },
                contentChanges,
            });
        } catch (err) {
            console.warn("[LSP] DocumentTracker: didChange failed:", absUri, err);
        }
    }

    /**
     * Send textDocument/didClose and stop tracking.
     */
    trackClose(client: MonacoLanguageClient, model: editor.ITextModel): void {
        const relPath = model.uri.path.replace(/^\/+/, "");
        const absUri = buildAbsoluteUri(this.workspaceRoot, relPath);

        const entry = this.openDocuments.get(absUri);
        if (!entry) return;

        // Dispose change listener
        entry.changeDisposable?.dispose();

        // Send didClose
        try {
            client.sendNotification("textDocument/didClose", {
                textDocument: { uri: absUri },
            });
        } catch (err) {
            console.warn("[LSP] DocumentTracker: didClose failed:", absUri, err);
        }

        this.openDocuments.delete(absUri);
        console.log("[LSP] DocumentTracker: closed", absUri);
    }

    /**
     * Close a document by absUri (for when model reference is not available).
     */
    trackCloseByUri(client: MonacoLanguageClient, absUri: string): void {
        const entry = this.openDocuments.get(absUri);
        if (!entry) return;

        entry.changeDisposable?.dispose();

        try {
            client.sendNotification("textDocument/didClose", {
                textDocument: { uri: absUri },
            });
        } catch (err) {
            console.warn("[LSP] DocumentTracker: didClose failed:", absUri, err);
        }

        this.openDocuments.delete(absUri);
    }

    /** Check if a document is currently tracked as open */
    isOpen(model: editor.ITextModel): boolean {
        const relPath = model.uri.path.replace(/^\/+/, "");
        const absUri = buildAbsoluteUri(this.workspaceRoot, relPath);
        return this.openDocuments.has(absUri);
    }

    /** Get all currently tracked URIs */
    getOpenUris(): string[] {
        return [...this.openDocuments.keys()];
    }

    /**
     * Re-open all previously tracked documents with a new client.
     * Used after LSP client reconnection to re-synchronize state.
     */
    reopenAll(client: MonacoLanguageClient, monacoInst: typeof import("monaco-editor")): void {
        const entries = [...this.openDocuments.entries()];
        // Clear existing tracking (listeners are stale after reconnect)
        for (const [, entry] of entries) {
            entry.changeDisposable?.dispose();
        }
        this.openDocuments.clear();

        // Re-track each document with the new client
        for (const [absUri] of entries) {
            // Find the Monaco model matching this absUri
            const allModels = monacoInst.editor.getModels();
            for (const model of allModels) {
                const relPath = model.uri.path.replace(/^\/+/, "");
                const modelAbsUri = buildAbsoluteUri(this.workspaceRoot, relPath);
                if (modelAbsUri === absUri) {
                    this.trackOpen(client, model);
                    break;
                }
            }
        }
    }

    /** Dispose all tracking — send didClose for all open documents, remove listeners */
    dispose(client: MonacoLanguageClient | null): void {
        for (const [absUri, entry] of this.openDocuments) {
            entry.changeDisposable?.dispose();
            if (client) {
                try {
                    client.sendNotification("textDocument/didClose", {
                        textDocument: { uri: absUri },
                    });
                } catch { /* ignore during cleanup */ }
            }
        }
        this.openDocuments.clear();
        console.log("[LSP] DocumentTracker: disposed all");
    }
}
