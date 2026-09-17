/**
 * DocFsEvents — tree-change listener regression.
 *
 * The doc sidebar used to rely on a 30s poll whose `loadDir(root)` call
 * was a no-op once the root was cached (second poll onward) — mcp-added
 * files never appeared. The fix: the doc service publishes a
 * `acowork/doc/tree/changed` MQTT event after every structural write;
 * the Desktop re-emits it as a Tauri event and this listener must
 * forward it to `refreshVisible()` (force-refetch all expanded dirs).
 *
 * Also covers the reconnect fallback: QoS-1 non-retained events are lost
 * across a disconnect, so a `mqtt-status` connected:true must also
 * trigger `refreshVisible()`.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import {
    initDocTreeChangeListener,
    disposeDocTreeChangeListener,
} from "./docFsEvents";

// ── Mocks ────────────────────────────────────────────────────────────────

/** listen(name, cb) → captured handlers keyed by event name. */
const handlers = new Map<string, (payload: unknown) => void>();

const mocks = vi.hoisted(() => ({
    refreshVisible: vi.fn().mockResolvedValue(true),
}));

vi.mock("@tauri-apps/api/event", () => ({
    listen: vi.fn(
        (name: string, cb: (payload: unknown) => void) =>
            new Promise<() => void>((resolve) => {
                handlers.set(name, cb);
                resolve(() => {
                    handlers.delete(name);
                });
            }),
    ),
}));

vi.mock("../stores/doc/treeStore", () => ({
    useDocTreeStore: {
        getState: () => ({ refreshVisible: mocks.refreshVisible }),
    },
}));

vi.mock("./logger", () => ({ log: { warn: () => {}, error: () => {} } }));

function fire(name: string, payload: unknown): void {
    const cb = handlers.get(name);
    if (!cb) throw new Error(`no listener registered for "${name}"`);
    // Tauri `listen` handlers receive an event object `{ payload }`.
    cb({ payload });
}

describe("initDocTreeChangeListener", () => {
    beforeEach(() => {
        handlers.clear();
        mocks.refreshVisible.mockClear();
        disposeDocTreeChangeListener();
    });

    it("forwards acowork:doc-tree-changed to refreshVisible()", async () => {
        await initDocTreeChangeListener();
        expect(handlers.has("acowork:doc-tree-changed")).toBe(true);

        fire("acowork:doc-tree-changed", { changed_dirs: ["root"] });
        // flush microtasks (listener handler is async)
        await Promise.resolve();
        expect(mocks.refreshVisible).toHaveBeenCalledTimes(1);
    });

    it("reconnect (mqtt-status connected) triggers refreshVisible()", async () => {
        await initDocTreeChangeListener();
        expect(handlers.has("mqtt-status")).toBe(true);

        fire("mqtt-status", { connected: false });
        await Promise.resolve();
        expect(mocks.refreshVisible).not.toHaveBeenCalled();

        fire("mqtt-status", { connected: true });
        await Promise.resolve();
        expect(mocks.refreshVisible).toHaveBeenCalledTimes(1);
    });

    it("is idempotent and re-registers on re-init (recovery reload)", async () => {
        await initDocTreeChangeListener();
        await initDocTreeChangeListener(); // second call awaits the same promise
        // After the first init settles, listeners remain registered once.
        expect(handlers.get("acowork:doc-tree-changed")).toBeDefined();

        // Re-init after dispose re-registers fresh handlers.
        disposeDocTreeChangeListener();
        await initDocTreeChangeListener();
        expect(handlers.get("acowork:doc-tree-changed")).toBeDefined();
        fire("acowork:doc-tree-changed", { changed_dirs: [] });
        await Promise.resolve();
        expect(mocks.refreshVisible).toHaveBeenCalled();
    });
});
