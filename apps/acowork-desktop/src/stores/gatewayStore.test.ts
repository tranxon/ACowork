/**
 * ADR-05x frontend tests: single-topology Gateway ownership.
 *
 * `localOwnership` ("owned" | "foreign" | "none") is a NEW parameter that
 * sits BESIDE the existing `localState` state machine (which is never
 * modified). These tests pin the semantics agreed for the single-topology
 * design:
 *
 *   - "owned"   ⇔ this Desktop session spawned the Gateway child.
 *   - "foreign" ⇔ a Gateway answers at the configured URL, but Desktop
 *                 did NOT spawn it → Desktop must never show Stop /
 *                 force-kill it, and quitting never prompts for it.
 *   - "none"    ⇔ nothing reachable / ownership unknown yet.
 *
 * Covers: boot-result recording, status sync (recovery reload), the
 * probe-then-spawn outcome from `start_local_gateway`, and the reset on
 * stop.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

// ── Mock Tauri invoke ────────────────────────────────────────────────────
// The store actions use `await import("@tauri-apps/api/core")` — vi.mock
// intercepts dynamic imports too.

const mockInvoke = vi.fn<[string], Promise<unknown>>();
vi.mock("@tauri-apps/api/core", () => ({
    invoke: (cmd: string) => mockInvoke(cmd),
}));

// ── Mock the logger (keeps test output clean) ────────────────────────────

vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

// ── Mock global fetch (checkHealth posts to /health) ─────────────────────

vi.stubGlobal(
    "fetch",
    vi.fn(() =>
        Promise.resolve({
            ok: true,
            status: 200,
            json: () => Promise.resolve({ status: "healthy", version: "test" }),
        } as Response),
    ),
);

// ── SUT ───────────────────────────────────────────────────────────────��──

import { useGatewayStore } from "./gatewayStore";

function resetStore() {
    useGatewayStore.setState({
        status: "disconnected",
        health: null,
        localState: "idle",
        localOwnership: "none",
        migrationProgress: {},
    });
    mockInvoke.mockReset();
}

/** Simulate `get_local_gateway_status` returning `running`. */
function stubChildAlive(running: boolean) {
    mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === "get_local_gateway_status") return Promise.resolve(running);
        if (cmd === "stop_local_gateway") return Promise.resolve(undefined);
        return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
    });
}

beforeEach(() => {
    resetStore();
});

describe("gatewayStore.localOwnership (single-topology)", () => {
    it("starts at none / idle", () => {
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("none");
        expect(s.localState).toBe("idle");
    });

    it("recordBootResult owned → running + owned (Desktop spawned)", () => {
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "owned",
        });
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("owned");
        expect(s.localState).toBe("running");
    });

    it("recordBootResult foreign → stopped + foreign, NOT running (adopted)", () => {
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "foreign",
        });
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("foreign");
        // No in-process child ⇒ the localState machine must NOT report
        // running — stop/restart buttons stay hidden.
        expect(s.localState).toBe("stopped");
    });

    it("checkLocalStatus alive child ⇔ owned (recovery reload path)", async () => {
        stubChildAlive(true);
        await useGatewayStore.getState().checkLocalStatus();
        const s = useGatewayStore.getState();
        expect(s.localState).toBe("running");
        expect(s.localOwnership).toBe("owned");
    });

    it("checkLocalStatus with no child does NOT clobber foreign ownership", async () => {
        // Boot recorded a foreign adoption; the status probe later finds
        // no child (expected — Desktop never spawned it).
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "foreign",
        });
        stubChildAlive(false);
        await useGatewayStore.getState().checkLocalStatus();
        const s = useGatewayStore.getState();
        expect(s.localState).toBe("stopped");
        expect(s.localOwnership).toBe("foreign");
    });

    it("startLocalGateway spawns (owned) → running + owned", async () => {
        // First probe: no child alive.
        stubChildAlive(false);
        // start_local_gateway answers "owned" (Desktop spawned it).
        mockInvoke.mockImplementation((cmd: string) => {
            if (cmd === "start_local_gateway") {
                return Promise.resolve({
                    base_url: "http://127.0.0.1:19876",
                    ownership: "owned",
                });
            }
            return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
        });

        await useGatewayStore.getState().startLocalGateway();
        const s = useGatewayStore.getState();
        expect(s.localState).toBe("running");
        expect(s.localOwnership).toBe("owned");
        // Probe-then-spawn: health was re-checked after boot.
        expect(s.status).toBe("connected");
    });

    it("startLocalGateway adopts existing Gateway (foreign) → stopped + foreign", async () => {
        stubChildAlive(false);
        mockInvoke.mockImplementation((cmd: string) => {
            if (cmd === "start_local_gateway") {
                // A Gateway was already answering at the URL; nothing was
                // spawned. Desktop must record "foreign" and NOT show
                // managed controls.
                return Promise.resolve({
                    base_url: "http://127.0.0.1:19876",
                    ownership: "foreign",
                });
            }
            return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
        });

        await useGatewayStore.getState().startLocalGateway();
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("foreign");
        expect(s.localState).toBe("stopped");
        expect(s.status).toBe("connected");
    });

    it("stopLocalGateway resets ownership to none", async () => {
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "owned",
        });
        mockInvoke.mockImplementation((cmd: string) => {
            if (cmd === "stop_local_gateway") return Promise.resolve(undefined);
            return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
        });

        await useGatewayStore.getState().stopLocalGateway();
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("none");
        expect(s.localState).toBe("stopped");
        expect(s.status).toBe("disconnected");
    });
});
