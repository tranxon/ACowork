/**
 * Doc library tree-change listener.
 *
 * Bridges the Tauri `acowork:doc-tree-changed` event (emitted by
 * commands/chat_mqtt.rs from MQTT `acowork/doc/tree/changed`, published
 * by the standalone acowork-doc process after every structural mutation)
 * into `useDocTreeStore.refreshVisible()` — force-refresh every expanded
 * tree layer.
 *
 * Why event-driven instead of polling: doc writes all go through the doc
 * process (REST + MCP share the same service layer), so it can publish
 * the change itself — no filesystem watcher needed (unlike the Runtime's
 * fs-changed, which must catch edits the Runtime never sees).
 *
 * Fallbacks (lost QoS-1 events are possible):
 *   1. `mqtt-status` connected:true → refreshVisible() — anything
 *      published while the Desktop was disconnected is non-retained and
 *      gone; a reconnect forces a full visible refresh.
 *   2. The existing 30s health poll in DocsView still calls
 *      refreshVisible() as the last-resort sweep.
 */

import { listen } from "@tauri-apps/api/event";
import { useDocTreeStore } from "../stores/doc/treeStore";
import { log } from "./logger";

/** Payload of the `acowork:doc-tree-changed` Tauri event. */
export interface DocTreeChangedEvent {
    /** dir_ids whose direct children may have changed (informational —
     *  the store refreshes all expanded layers regardless). */
    changed_dirs: string[];
}

let _fsUnlisten: (() => void) | null = null;
let _statusUnlisten: (() => void) | null = null;
let _initPromise: Promise<void> | null = null;

/** Trigger a visible-layer refresh, swallowing errors (listener context). */
function refreshVisibleSafe(reason: string): void {
    useDocTreeStore
        .getState()
        .refreshVisible()
        .catch((e: unknown) => log.warn(`[DocFsEvents] ${reason}:`, e));
}

export async function initDocTreeChangeListener(): Promise<void> {
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
    // Recovery-reload re-inits — drop stale listeners first.
    disposeDocTreeChangeListener();

    _fsUnlisten = await listen<DocTreeChangedEvent>("acowork:doc-tree-changed", () => {
        refreshVisibleSafe("tree-changed refresh");
    });

    // Fallback: Desktop MQTT (re)connect. The doc service's events are
    // non-retained, so everything published while we were disconnected
    // is lost — a ConnAck (initial connect, Gateway restart, broker
    // bounce) forces a full visible refresh. Cheap: only expanded dirs
    // are re-fetched, and refreshVisible is re-entrancy-guarded.
    _statusUnlisten = await listen<{ connected: boolean }>("mqtt-status", (event) => {
        if (event.payload.connected) {
            refreshVisibleSafe("mqtt reconnect refresh");
        }
    });
}

/** Unregister both listeners (recovery reload re-inits on a fresh page). */
export function disposeDocTreeChangeListener(): void {
    _fsUnlisten?.();
    _fsUnlisten = null;
    _statusUnlisten?.();
    _statusUnlisten = null;
}
