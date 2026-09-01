/**
 * ADR-058: Workspace FS change event listener (Desktop frontend).
 *
 * Bridges the Tauri `acowork:workspace-fs-changed` event (emitted by
 * commands/chat_mqtt.rs from MQTT
 * `acowork/agents/{id}/workspaces/{wid}/fs-changed`) into:
 *
 * 1. fileTreeStore — per-parent-path incremental tree fetch
 *    (only directories already present in the cache are re-fetched, so
 *    expanded state is preserved and no unused dirs are pulled; bursts
 *    of events for the same parent are coalesced into one refresh per
 *    `FS_REFRESH_DEBOUNCE_MS` window — see ADR-058 follow-up).
 * 2. fileEditorStore — external-modification conflict UX:
 *    - clean file modified on disk  → silent `refreshFile` (VSCode-style)
 *    - dirty file modified on disk  → re-check disk `modified`+`size`
 *      (touch/chmod produces Modified events too — pure metadata changes
 *      are silently skipped) → toast with a Reload action
 *    - clean file deleted           → close tab + toast
 *    - dirty file deleted           → conflict marker + toast
 *    Own saves are suppressed via `lastSavedAtMs` (same-clock-domain
 *    comparison — see ADR-058 §3.3 for why the event's Runtime-side
 *    timestamp is deliberately NOT used).
 * 3. Reconnect / Runtime-wake fallback — events are non-retained and the
 *    Desktop uses clean_session=true, so anything that happened while
 *    disconnected is lost. Two triggers force a full tree re-sync:
 *    - `mqtt-status` connected:true (Desktop reconnect / Gateway restart)
 *    - agent `status` offline|sleeping → online transition (Runtime wake
 *      from idle sleep, which is a process exit by design)
 */

import { listen } from "@tauri-apps/api/event";
import { useFileTreeStore, treeKey, isReadyNode } from "../stores/fileTree";
import { useFileEditorStore } from "../stores/fileEditorStore";
import { useSettingsStore } from "../stores/settingsStore";
import { DEFAULT_GATEWAY_URL } from "./config";
import { showToast } from "../components/common/ToastProvider";
import { log } from "./logger";
import { initWorkspaceWatchReporter, disposeWorkspaceWatchReporter } from "./workspaceFsWatch";

/** One aggregated change within a 500ms window (Runtime → Desktop). */
export interface FsChange {
    kind: "created" | "modified" | "deleted" | "unspecified";
    /** Forward-slash workspace-relative path. */
    path: string;
    timestamp_ms: number;
}

/** Payload of the `acowork:workspace-fs-changed` Tauri event. */
export interface WorkspaceFsChangeEvent {
    agent_id: string;
    workspace_id: string;
    changes: FsChange[];
    window_end_ms: number;
}

/**
 * Echo suppression window (ms). Must cover save → PollWatcher capture
 * (≤500ms) → aggregation flush (≤500ms) → MQTT → Tauri emit with margin.
 */
const ECHO_SUPPRESS_MS = 1500;

/** Minimum spacing between full tree re-syncs (dedupe reconnect storms). */
const FULL_SYNC_DEDUPE_MS = 2000;

/**
 * Per-parent debounce window for incremental tree refreshes (ms).
 *
 * The Runtime aggregates fs changes into 500ms windows, but a single
 * user operation that spans multiple windows (bulk paste, `git
 * checkout`, a build writing N files) still produces several fs-changed
 * events in quick succession. Refreshing the same parent for each event
 * = N aborts + N refetches where only the last one matters. We coalesce
 * per-parent refreshes into one authoritative pull per window.
 *
 * Must be >= the Runtime aggregation window so an operation spanning
 * multiple windows still coalesces; small enough that external changes
 * still appear promptly (the tree is not the user's active surface for
 * external edits, so a half-second delay is invisible in practice).
 */
export const FS_REFRESH_DEBOUNCE_MS = 600;

/** Debounced per-parent refresh bookkeeping (key = agent\0ws\0parent). */
interface PendingRefresh {
    timer: ReturnType<typeof setTimeout>;
    agentId: string;
    workspaceId: string;
    parent: string;
}
const _pendingRefreshes = new Map<string, PendingRefresh>();

/** Fire one pending refresh (parent was already checked at schedule time). */
function fireRefresh(key: string): void {
    const pending = _pendingRefreshes.get(key);
    if (!pending) return;
    _pendingRefreshes.delete(key);
    const treeStore = useFileTreeStore.getState();
    // Re-check cache presence at fire time — the parent may have been
    // evicted / invalidated while we were debouncing.
    const cacheKey = treeKey(pending.agentId, pending.workspaceId, pending.parent);
    if (!isReadyNode(treeStore.getNode(cacheKey))) return;
    treeStore
        .refresh(pending.agentId, pending.workspaceId, pending.parent)
        .catch((e: unknown) => log.error("[WorkspaceFsEvents] incremental fetchTree failed:", e));
}

/** Coalesce burst fs-changed events into one refresh per parent. */
function refreshParentDebounced(agentId: string, workspaceId: string, parent: string): void {
    const key = `${agentId}\u0000${workspaceId}\u0000${parent}`;
    const existing = _pendingRefreshes.get(key);
    if (existing) {
        // More changes for the same parent are coming — slide the window.
        clearTimeout(existing.timer);
        existing.timer = setTimeout(() => fireRefresh(key), FS_REFRESH_DEBOUNCE_MS);
        return;
    }
    const entry: PendingRefresh = {
        timer: setTimeout(() => fireRefresh(key), FS_REFRESH_DEBOUNCE_MS),
        agentId,
        workspaceId,
        parent,
    };
    _pendingRefreshes.set(key, entry);
}

/** Cancel pending debounced refreshes (listener re-init / dispose). */
function clearPendingRefreshes(): void {
    for (const { timer } of _pendingRefreshes.values()) clearTimeout(timer);
    _pendingRefreshes.clear();
}

let _fsUnlisten: (() => void) | null = null;
let _statusUnlisten: (() => void) | null = null;
let _agentEventUnlisten: (() => void) | null = null;
let _initPromise: Promise<void> | null = null;

/** Last full-sync moment (epoch ms) — dedupes reconnect-triggered storms. */
let _lastFullSyncAt = 0;

/** Previous agent online/sleeping status, keyed by agent id. */
const _prevAgentStatus = new Map<string, AgentStatusSnapshot>();

/** One agent status snapshot as delivered by the `agent-event` channel. */
export interface AgentStatusSnapshot {
    online: boolean;
    sleeping: boolean;
}

/**
 * ADR-058 §3.4 wake detection: should a status transition force a full
 * tree re-sync?
 *
 * A wake is any "down-ish" previous state followed by a genuinely
 * online next state. "Down-ish" covers BOTH:
 * - `online=false` (the Will-message "offline", e.g. crash disconnect)
 * - `sleeping=true` — the idle-sleep path publishes "sleeping" and then
 *   performs a CLEAN disconnect + process::exit(0). Per MQTT spec (and
 *   rumqttd's `Packet::Disconnect` handler, which deletes the last
 *   will) the LWT "offline" is NEVER published on a clean disconnect —
 *   so "sleeping → online" is the ONLY transition sequence a connected
 *   Desktop ever sees for the normal idle-sleep wake path. Testing only
 *   `!prev.online` would silently miss it.
 *
 * `prev === undefined` (cold start: retained "online" re-delivered on
 * subscribe) is NOT a wake — the initial tree fetch happens on mount.
 */
export function isWakeTransition(
    prev: AgentStatusSnapshot | undefined,
    next: AgentStatusSnapshot,
): boolean {
    if (!prev) return false;
    const wasDown = !prev.online || prev.sleeping;
    return wasDown && next.online && !next.sleeping;
}

export async function initWorkspaceFsListener(): Promise<void> {
    if (_initPromise) {
        await _initPromise;
        return;
    }
    _initPromise = doInit();
    try {
        await _initPromise;
    } finally {
        _initPromise = null;
    }
}

async function doInit(): Promise<void> {
    // Unregister previous listeners (recovery reload re-inits).
    disposeWorkspaceFsListener();
    // Start the demand-driven watch-set reporter (frontend state → Runtime
    // fs-watch). Must come before the listeners below: a wake/reconnect
    // full-sync and the watch reporter are independent, but both are torn
    // down together in disposeWorkspaceFsListener.
    initWorkspaceWatchReporter();

    _fsUnlisten = await listen<WorkspaceFsChangeEvent>(
        "acowork:workspace-fs-changed",
        (event) => {
            handleFsChanged(event.payload).catch((e) =>
                log.error("[WorkspaceFsEvents] handleFsChanged error:", e),
            );
        },
    );

    // Fallback trigger 1: Desktop MQTT reconnect (also covers Gateway
    // restarts — broker comes back, client re-CONNACKs).
    _statusUnlisten = await listen<{ connected: boolean }>("mqtt-status", (event) => {
        if (event.payload.connected) {
            scheduleFullTreeSync("mqtt-reconnect");
        }
    });

    // Fallback trigger 2: Runtime wake from idle sleep (= process exit +
    // respawn). The retained `agents/{id}/status` topic re-delivers the
    // current status on (re)subscribe; we only act on a genuine
    // down-ish → online TRANSITION (see isWakeTransition for why
    // "sleeping" counts as down) so cold start does not trigger a
    // pointless full sync (the initial tree fetch happens on mount).
    _agentEventUnlisten = await listen<Record<string, unknown>>("agent-event", (event) => {
        const data = event.payload;
        if (data.type !== "agent_status" || typeof data.agent_id !== "string") return;
        const agentId = data.agent_id as string;
        const next: AgentStatusSnapshot = {
            online: data.online === true,
            sleeping: data.sleeping === true,
        };
        const prev = _prevAgentStatus.get(agentId);
        _prevAgentStatus.set(agentId, next);
        if (isWakeTransition(prev, next)) {
            scheduleFullTreeSync(`agent-wake:${agentId}`);
        }
    });
}

export function disposeWorkspaceFsListener(): void {
    // Tear down the watch-set reporter first — its subscriptions reference
    // the stores, independent of the Tauri listeners below.
    disposeWorkspaceWatchReporter();
    if (_fsUnlisten) {
        _fsUnlisten();
        _fsUnlisten = null;
    }
    if (_statusUnlisten) {
        _statusUnlisten();
        _statusUnlisten = null;
    }
    if (_agentEventUnlisten) {
        _agentEventUnlisten();
        _agentEventUnlisten = null;
    }
    // Stale per-agent status snapshots must not leak into a fresh
    // listener lifecycle (a retained "online" after re-init would
    // otherwise read as "was online" and mask a later wake).
    _prevAgentStatus.clear();
    // Drop any debounced refreshes scheduled by the old listener —
    // they were captured with the old stores' closures and would fire
    // pointlessly after re-init.
    clearPendingRefreshes();
}

// ── fs-changed event handling ────────────────────────────────────────────

/**
 * @internal Exported for tests — production entry is the Tauri listener
 * registered by [`initWorkspaceFsListener`].
 */
export async function handleFsChanged(ev: WorkspaceFsChangeEvent): Promise<void> {
    if (!ev.changes?.length) return;
    refreshTreesForChanges(ev);
    await handleEditorConflicts(ev);
}

/**
 * Per-parent-path incremental refresh (ADR-058 §3.3).
 *
 * Only the parent directory of each changed path is re-fetched, and
 * only when that parent already exists in `treeCache` — a directory
 * the user never expanded is invisible, so fetching it would waste a
 * round-trip; the (cheap) cache lookup acts as the "is this visible?"
 * filter. Top-level changes resolve to parent "" which refreshes the
 * root. Expanded state is preserved because no other nodes are touched.
 */
function refreshTreesForChanges(ev: WorkspaceFsChangeEvent): void {
    const treeStore = useFileTreeStore.getState();
    const parents = new Set<string>();
    for (const change of ev.changes) {
        const idx = change.path.lastIndexOf("/");
        parents.add(idx >= 0 ? change.path.substring(0, idx) : "");
    }

    let scheduled = 0;
    for (const parent of parents) {
        const key = treeKey(ev.agent_id, ev.workspace_id, parent);
        // Skip parents we have never loaded — nothing visible to update.
        if (!isReadyNode(treeStore.getNode(key))) continue;
        // Force a fresh fetch — the fs-watcher just told us the
        // directory's contents changed (create / rename / delete on
        // disk), so the cached entries are stale by definition. Plain
        // `fetch()` would short-circuit on a hit inside the SWR fresh
        // window and the new entry would be invisible until the next
        // background revalidation. `refresh()` drops the entry first,
        // guaranteeing an authoritative pull. The same per-key dedup
        // (via inflight promise sharing) still applies.
        //
        // Debounced per parent: a burst of fs-changed events (bulk
        // paste / git operation spanning multiple Runtime aggregation
        // windows) would otherwise abort+refetch the same parent N
        // times; coalescing keeps it to one authoritative pull per
        // window.
        refreshParentDebounced(ev.agent_id, ev.workspace_id, parent);
        scheduled++;
    }
    if (scheduled > 0) {
        log.debug(
            "[WorkspaceFsEvents] scheduled incremental tree refresh",
            { agent: ev.agent_id, workspace: ev.workspace_id, dirs: scheduled },
        );
    }
}

/**
 * Editor conflict UX (ADR-058 §3.3). See module docs for the four cases.
 */
async function handleEditorConflicts(ev: WorkspaceFsChangeEvent): Promise<void> {
    const editor = useFileEditorStore.getState();
    for (const change of ev.changes) {
        const file = editor.openFiles.find(
            (f) =>
                f.kind === "file" &&
                f.agentId === ev.agent_id &&
                f.workspaceId === ev.workspace_id &&
                f.relPath === change.path,
        );
        if (!file) continue;

        // ── Deleted on disk ──
        if (change.kind === "deleted") {
            if (!file.dirty) {
                // Clean: close the tab, notify.
                useFileEditorStore.getState().closeFile(file.id, true);
                showToast({ type: "warning", message: `File deleted: ${file.relPath}` });
            } else {
                useFileEditorStore.setState((state) => ({
                    openFiles: state.openFiles.map((f) =>
                        f.id === file.id ? { ...f, diskDeleted: true, diskConflict: "deleted" } : f,
                    ),
                }));
                showToast({
                    type: "warning",
                    message: `File deleted on disk (you have unsaved changes): ${file.relPath}`,
                });
            }
            continue;
        }

        if (change.kind !== "modified") continue;

        // ── Echo suppression: skip the bounce of our own save. ──
        // Same-clock-domain comparison (Date.now vs lastSavedAtMs, both
        // Desktop-local). The event's Runtime-side timestamp_ms is
        // deliberately unused — cross-machine clock skew in Remote mode
        // would break the window.
        if (file.lastSavedAtMs !== undefined && Date.now() - file.lastSavedAtMs < ECHO_SUPPRESS_MS) {
            continue;
        }

        if (!file.dirty) {
            // Clean file: silent reload (VSCode-style). skipIfDirty guards
            // the race where the user starts typing while the fetch is in
            // flight — their edits win over the reload (review M-3).
            await useFileEditorStore.getState().refreshFile(file.id, { skipIfDirty: true });
            continue;
        }

        // ── Dirty file: re-check before prompting. PollWatcher maps
        // Modify(Metadata) (chmod / touch) to "modified" the same as a
        // content write — pure metadata changes must NOT prompt.
        const meta = await statDiskFile(ev.agent_id, ev.workspace_id, change.path);
        if (!meta) continue; // stat failed (file gone?) — next event decides
        if (meta.modified === file.diskModified && meta.size === file.diskSize) {
            // Pure metadata change — silently adopt the new baseline.
            useFileEditorStore.setState((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === file.id ? { ...f, diskModified: meta.modified, diskSize: meta.size } : f,
                ),
            }));
            continue;
        }

        // Disk content really changed: conflict marker + toast.
        // The toast supports a single action — Reload is the primary
        // answer; dismissing the toast means "keep mine".
        useFileEditorStore.setState((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === file.id ? { ...f, diskConflict: "modified" } : f,
            ),
        }));
        showToast({
            type: "warning",
            message: `File changed on disk: ${file.relPath}`,
            action: {
                label: "Reload",
                onClick: () => {
                    useFileEditorStore.getState().refreshFile(file.id);
                },
            },
        });
    }
}

/** Fetch disk `modified` + `size` for a workspace file (conflict re-check). */
async function statDiskFile(
    agentId: string,
    workspaceId: string,
    relPath: string,
): Promise<{ modified?: string; size?: number } | null> {
    try {
        const baseUrl = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
        const params = new URLSearchParams();
        if (workspaceId && workspaceId !== "__agent_home__") {
            params.set("workspace_id", workspaceId);
        }
        params.set("path", relPath);
        const resp = await fetch(
            `${baseUrl}/api/agents/${agentId}/workspaces/file?${params.toString()}`,
        );
        if (!resp.ok) return null;
        const data = (await resp.json()) as { modified?: string; size?: number };
        return { modified: data.modified, size: data.size };
    } catch (e) {
        log.error("[WorkspaceFsEvents] statDiskFile error:", e);
        return null;
    }
}

// ── Reconnect / wake fallback (ADR-058 §3.4, W5 acceptance item) ────────

/**
 * Full tree re-sync: invalidate every cached tree, then re-fetch the
 * root of every (agent, workspace) pair that previously had a cached
 * tree. Children re-fetch lazily as the user re-expands them.
 *
 * Deduped: reconnect sequences can fire several connected:true events
 * within a second (poll fallback + event listener); only the first
 * within FULL_SYNC_DEDUPE_MS runs.
 */
/**
 * @internal Exported for tests — production entry is the mqtt-status /
 * agent-event listeners registered by [`initWorkspaceFsListener`].
 */
export function scheduleFullTreeSync(reason: string): void {
    const now = Date.now();
    if (now - _lastFullSyncAt < FULL_SYNC_DEDUPE_MS) return;
    _lastFullSyncAt = now;

    const treeStore = useFileTreeStore.getState();
    // Collect every (agent, workspace) pair that currently has at
    // least one cached node. The keys are NUL-delimited (see
    // fileTree/types.ts), so a safe split is split("\u0000").
    const pairs = new Set<string>();
    for (const key of Object.keys(treeStore.nodes)) {
        const idx = key.indexOf("\u0000");
        if (idx < 0) continue;
        const agentId = key.substring(0, idx);
        const rest = key.substring(idx + 1);
        const wsIdx = rest.indexOf("\u0000");
        if (wsIdx < 0) continue;
        const workspaceId = rest.substring(0, wsIdx);
        pairs.add(`${agentId}\u0000${workspaceId}`);
    }
    if (pairs.size === 0) return;

    log.debug("[WorkspaceFsEvents] full tree re-sync", { reason, pairs: pairs.size });
    for (const pair of pairs) {
        const [agentId, workspaceId] = pair.split("\u0000");
        // `invalidate(agentId)` already drops every cached entry for the
        // agent, so a plain `fetch()` would not hit the SWR fast path.
        // Use `refresh()` for consistency with the authoritative-refresh
        // contract (it's effectively the same call here, but keeps the
        // intent obvious at every fs-event call site).
        treeStore.invalidate(agentId);
        treeStore.refresh(agentId, workspaceId, "").catch((e: unknown) =>
            log.error("[WorkspaceFsEvents] full sync fetchTree failed:", e),
        );
    }
}
