/**
 * Multi-language LSP Client Pool hook.
 *
 * Thin React wrapper around `LspPoolManager`, which orchestrates a pool of
 * `LspConnection` instances — one per language.  The heavy lifting (WebSocket,
 * MonacoLanguageClient, progress tracking, concurrency) lives in the
 * framework-agnostic `LspConnection` class.
 *
 * Lifecycle rules:
 *   - When a new language appears in `openLanguages`, a connection is created.
 *   - When a language disappears, a 30-second grace timer starts.  If the
 *     language reappears within the timer, the existing connection is reused.
 *     Otherwise it is evicted.
 *   - On unmount, every connection is torn down.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { MonacoLanguageClient } from "monaco-languageclient";
import { type LspStatus } from "../lib/lspUtils";
import { log } from "../lib/logger";
import {
    LspConnection,
    type LspConnectParams,
} from "../lib/lspConnection";

// ── Types ──────────────────────────────────────────────────────────────

export type { LspStatus };

interface PublicEntry {
    status: LspStatus;
    statusMessage: string;
    client: MonacoLanguageClient | null;
}

export interface LspClientPoolResult {
    getClient: (language: string) => MonacoLanguageClient | null;
    getStatus: (language: string) => { status: LspStatus; statusMessage: string };
    activeStatus: LspStatus;
    activeStatusMessage: string;
    activeClient: MonacoLanguageClient | null;
    allStatuses: Map<string, { status: LspStatus; statusMessage: string }>;
}

// ── Constants ──────────────────────────────────────────────────────────

const DISCONNECT_GRACE_MS = 30_000;
const EMPTY_ENTRY: PublicEntry = {
    status: "disconnected",
    statusMessage: "",
    client: null,
};

// ── LspPoolManager ─────────────────────────────────────────────────────

type SnapshotCallback = (snapshot: Map<string, PublicEntry>) => void;

class LspPoolManager {
    private _connections = new Map<string, LspConnection>();
    private _evictionTimers = new Map<string, ReturnType<typeof setTimeout>>();
    private _onSnapshotChange?: SnapshotCallback;

    constructor(onChange?: SnapshotCallback) {
        this._onSnapshotChange = onChange;
    }

    // ── public ───────────────────────────────────────────────────────

    /**
     * Ensure a language has a live connection.  Idempotent — if already
     * connected (or connecting) this returns the existing promise.
     */
    async ensureConnected(params: LspConnectParams): Promise<void> {
        const { language } = params;

        let conn = this._connections.get(language);
        if (!conn) {
            conn = new LspConnection(() => this._notify());
            this._connections.set(language, conn);
        }

        this._cancelEviction(language);
        await conn.connect(params);
        this._notify();
    }

    /** Schedule eviction after `graceMs`. */
    scheduleEviction(language: string, graceMs: number): void {
        if (this._evictionTimers.has(language)) return;

        log.debug(
            "[LSP] pool schedule disconnect —",
            language,
            `${graceMs / 1000}s`,
        );

        this._evictionTimers.set(
            language,
            setTimeout(() => {
                this._evictionTimers.delete(language);
                this.evict(language);
            }, graceMs),
        );
    }

    /** Immediately evict a language's connection. */
    evict(language: string): void {
        this._cancelEviction(language);

        const conn = this._connections.get(language);
        if (conn) {
            log.debug("[LSP] pool evict —", language);
            conn.disconnect();
            this._connections.delete(language);
        }

        this._notify();
    }

    /** Evict all connections. */
    evictAll(): void {
        for (const lang of this._connections.keys()) {
            this._cancelEviction(lang);
            this._connections.get(lang)?.disconnect();
        }
        this._connections.clear();
        this._notify();
    }

    /** Get a snapshot of every tracked language (for React state). */
    getSnapshot(): Map<string, PublicEntry> {
        const out = new Map<string, PublicEntry>();
        for (const [lang, conn] of this._connections) {
            out.set(lang, {
                status: conn.status,
                statusMessage: conn.statusMessage,
                client: conn.client,
            });
        }
        return out;
    }

    getClient(language: string): MonacoLanguageClient | null {
        return this._connections.get(language)?.client ?? null;
    }

    getLanguages(): string[] {
        return Array.from(this._connections.keys());
    }

    hasEvictionTimer(language: string): boolean {
        return this._evictionTimers.has(language);
    }

    // ── private ──────────────────────────────────────────────────────

    private _cancelEviction(language: string): void {
        const timer = this._evictionTimers.get(language);
        if (timer != null) {
            clearTimeout(timer);
            this._evictionTimers.delete(language);
        }
    }

    private _notify(): void {
        this._onSnapshotChange?.(this.getSnapshot());
    }
}

// ── Hook ───────────────────────────────────────────────────────────────

export function useLspClientPool(
    activeLanguage: string | null,
    openLanguages: Set<string>,
    agentId: string | undefined,
    workspaceId: string | undefined,
    enabled: boolean,
    workspaceRoot?: string,
): LspClientPoolResult {
    const [snapshot, setSnapshot] = useState<Map<string, PublicEntry>>(
        () => new Map(),
    );

    const poolRef = useRef<LspPoolManager | null>(null);
    if (!poolRef.current) {
        poolRef.current = new LspPoolManager(setSnapshot);
    }
    const pool = poolRef.current;

    // Stable serialised key to avoid re-running on identical Set instances.
    const openLanguagesKey = useMemo(
        () => Array.from(openLanguages).sort().join(","),
        [openLanguages],
    );

    // ── reconcile openLanguages with the pool ──────────────────────────
    useEffect(() => {
        if (!enabled || !workspaceRoot) {
            pool.evictAll();
            return;
        }

        const wanted = new Set(openLanguages);

        // New / existing wanted languages
        for (const lang of wanted) {
            void pool.ensureConnected({
                language: lang,
                workspaceRoot,
                agentId,
                workspaceId,
            });
        }

        // Languages no longer wanted — schedule eviction
        for (const lang of pool.getLanguages()) {
            if (!wanted.has(lang) && !pool.hasEvictionTimer(lang)) {
                pool.scheduleEviction(lang, DISCONNECT_GRACE_MS);
            }
        }
    }, [
        openLanguagesKey,
        agentId,
        workspaceId,
        enabled,
        workspaceRoot,
        pool,
    ]);

    // ── teardown on unmount ────────────────────────────────────────────
    useEffect(() => {
        return () => {
            log.debug("[LSP] pool unmount — tearing down all clients");
            pool.evictAll();
        };
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    // ── Public API ─────────────────────────────────────────────────────

    const getClient = useCallback(
        (language: string): MonacoLanguageClient | null => {
            return snapshot.get(language)?.client ?? null;
        },
        [snapshot],
    );

    const getStatus = useCallback(
        (language: string): { status: LspStatus; statusMessage: string } => {
            const entry = snapshot.get(language) ?? EMPTY_ENTRY;
            return { status: entry.status, statusMessage: entry.statusMessage };
        },
        [snapshot],
    );

    const activeEntry = activeLanguage
        ? (snapshot.get(activeLanguage) ?? EMPTY_ENTRY)
        : EMPTY_ENTRY;

    const allStatuses = useMemo(() => {
        const out = new Map<string, { status: LspStatus; statusMessage: string }>();
        for (const [lang, entry] of snapshot) {
            out.set(lang, {
                status: entry.status,
                statusMessage: entry.statusMessage,
            });
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
