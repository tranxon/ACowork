/**
 * Regression coverage for the MQTT `agent_status` HTTP double-check.
 *
 * 2026-09-02 09:12 incident: during system sleep the MQTT connection
 * dropped (KeepAlive timeout) → Gateway marked the agent offline → the
 * desktop rendered it as sleeping (zzz) even though the Runtime process
 * itself stayed alive. The fix lives in `handleMessageEvent`'s
 * `agent_status` branch (chatStore.ts) which, when `online=false` arrives,
 * fires an HTTP probe of the Runtime's `/health` endpoint via the Gateway
 * reverse-proxy (`/api/agents/{id}/health`). A 2xx answer overrides the
 * MQTT signal back to online so the desktop does NOT mis-render sleeping.
 *
 * These tests pin:
 *   - online=true → no HTTP probe (avoid wasted work on every status tick)
 *   - online=false + health=alive → override back to online=true, sleeping=false
 *   - online=false + health=dead  → stays offline (genuine shutdown)
 *   - online=false + health throws → stays offline (defensive)
 *   - sleeping flag is preserved when overridden (always reset to false
 *     because if the Runtime is alive over HTTP it isn't sleeping)
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// ── Hoisted mocks: vi.mock factories are hoisted to the top of the file,
//    so any shared state they reference must also be hoisted via
//    `vi.hoisted`. This is the standard vitest pattern for sharing spies
//    between the mock factory and the test body.

const { updateCalls, mockVerifyAgentHealth, mockUpdateAgentOnlineStatus } =
    vi.hoisted(() => {
        const updateCalls: Array<{
            agentId: string;
            online: boolean;
            sleeping: boolean;
        }> = [];
        const mockVerifyAgentHealth = vi.fn<
            [agentId: string, timeoutMs?: number, gatewayUrl?: string],
            Promise<boolean>
        >();
        const mockUpdateAgentOnlineStatus = vi.fn(
            (agentId: string, online: boolean, sleeping = false) => {
                updateCalls.push({ agentId, online, sleeping });
            },
        );
        return {
            updateCalls,
            mockVerifyAgentHealth,
            mockUpdateAgentOnlineStatus,
        };
    });

// ── Mock the HTTP health check before chatStore imports it ───────────────

vi.mock("../lib/gateway-api", async () => {
    const actual =
        await vi.importActual<typeof import("../lib/gateway-api")>(
            "../lib/gateway-api",
        );
    return {
        ...actual,
        verifyAgentHealth: mockVerifyAgentHealth,
    };
});

// ── Mock the agent store so we can spy on updateAgentOnlineStatus ───────

vi.mock("./agentStore", () => ({
    useAgentStore: {
        getState: () => ({
            updateAgentOnlineStatus: mockUpdateAgentOnlineStatus,
        }),
    },
}));

// ── Now import the SUT ──────────────────────────────────────────────────

import { handleMessageEvent, useChatStore } from "./chatStore";

const AGENT = "com.acowork.architect";

beforeEach(() => {
    updateCalls.length = 0;
    // `mockReset` clears implementations; re-establish a safe default so
    // tests that don't care about the probe's answer still get a resolved
    // promise (instead of `undefined`, which would crash `.then()`).
    mockVerifyAgentHealth.mockReset();
    mockVerifyAgentHealth.mockResolvedValue(false);
    mockUpdateAgentOnlineStatus.mockClear();
});

afterEach(() => {
    vi.useRealTimers();
});

// ── Tests ────────────────────────────────────────────────────────────────

describe("agent_status handler: HTTP double-check on offline events", () => {
    it("does NOT probe /health when online=true arrives (avoid wasted work)", async () => {
        // Drive the handler with online=true — the most common case. The
        // MQTT signal is authoritative when it says alive; we don't burn
        // an HTTP round-trip per status tick.
        handleMessageEvent(
            { type: "agent_status", agent_id: AGENT, online: true },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        // Let any microtasks settle.
        await Promise.resolve();
        await Promise.resolve();

        expect(mockVerifyAgentHealth).not.toHaveBeenCalled();
        // updateAgentOnlineStatus must still be called once (with online=true).
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledTimes(1);
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledWith(AGENT, true, false);
    });

    it("probes /health on online=false AND overrides back to online when the Runtime is alive", async () => {
        // Simulates the 09:12 incident: MQTT drops (system sleep) →
        // Gateway republishes offline → WITHOUT the fix the desktop
        // would render sleeping. WITH the fix, the probe finds the
        // Runtime alive and the state is corrected back to online.
        mockVerifyAgentHealth.mockResolvedValue(true);

        handleMessageEvent(
            { type: "agent_status", agent_id: AGENT, online: false },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        // First call: the agent_status event itself.
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledTimes(1);
        expect(mockUpdateAgentOnlineStatus).toHaveBeenLastCalledWith(
            AGENT,
            false,
            false,
        );

        // Let the probe's promise resolve.
        await vi.waitFor(() => {
            expect(mockVerifyAgentHealth).toHaveBeenCalledWith(AGENT);
        });
        await Promise.resolve();

        // Second call: the override after the probe resolves.
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledTimes(2);
        expect(mockUpdateAgentOnlineStatus).toHaveBeenLastCalledWith(
            AGENT,
            true,
            false,
        );
    });

    it("stays offline when the probe finds the Runtime dead (genuine shutdown)", async () => {
        // Even with MQTT saying offline, we confirm with HTTP. If HTTP
        // also says dead, we leave the agent offline — no second
        // updateAgentOnlineStatus call.
        mockVerifyAgentHealth.mockResolvedValue(false);

        handleMessageEvent(
            { type: "agent_status", agent_id: AGENT, online: false },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        await vi.waitFor(() => {
            expect(mockVerifyAgentHealth).toHaveBeenCalledWith(AGENT);
        });
        await Promise.resolve();

        // Only the initial offline update — no override back to online.
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledTimes(1);
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledWith(AGENT, false, false);
    });

    it("does not crash if the probe throws (network error, DNS, etc.)", async () => {
        // Defensive: verifyAgentHealth's own try/catch should swallow
        // exceptions and return false. We verify that even if a
        // throw leaks, the desktop doesn't blow up — the .then handler
        // must not reject uncaught.
        mockVerifyAgentHealth.mockRejectedValue(new Error("ECONNREFUSED"));

        expect(() => {
            handleMessageEvent(
                { type: "agent_status", agent_id: AGENT, online: false },
                useChatStore.setState,
                useChatStore.getState,
                AGENT,
            );
        }).not.toThrow();

        // Drain the microtask queue so the .then handler runs.
        await new Promise((r) => setTimeout(r, 10));

        // No override — only the initial offline update.
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledTimes(1);
        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledWith(AGENT, false, false);
    });

    it("preserves the sleeping flag from the MQTT event when overriding online", async () => {
        // Older Runtime revisions publish `sleeping=true` together with
        // `online=false`. The handler must keep that flag on the initial
        // update, but reset it to false on the override (an alive HTTP
        // probe means the agent is NOT sleeping — it was a transient
        // MQTT drop).
        mockVerifyAgentHealth.mockResolvedValue(true);

        handleMessageEvent(
            {
                type: "agent_status",
                agent_id: AGENT,
                online: false,
                sleeping: true,
            },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        // Initial: pass through sleeping=true from the event.
        expect(mockUpdateAgentOnlineStatus).toHaveBeenNthCalledWith(
            1,
            AGENT,
            false,
            true,
        );

        await vi.waitFor(() => {
            expect(mockVerifyAgentHealth).toHaveBeenCalled();
        });
        await Promise.resolve();

        // Override: sleeping=false regardless of what MQTT said.
        expect(mockUpdateAgentOnlineStatus).toHaveBeenNthCalledWith(
            2,
            AGENT,
            true,
            false,
        );
    });

    it("defaults sleeping to false when the event omits it (older protobuf payload)", () => {
        // Older protobuf branches may not include `sleeping` at all.
        // The handler must not crash on `undefined`.
        handleMessageEvent(
            { type: "agent_status", agent_id: AGENT, online: false },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        expect(mockUpdateAgentOnlineStatus).toHaveBeenCalledWith(AGENT, false, false);
    });

    it("ignores malformed events without an agent_id", () => {
        handleMessageEvent(
            { type: "agent_status", online: false },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        // No agent_id → no update, no probe.
        expect(mockUpdateAgentOnlineStatus).not.toHaveBeenCalled();
        expect(mockVerifyAgentHealth).not.toHaveBeenCalled();
    });

    it("ignores events without a defined online flag", () => {
        handleMessageEvent(
            { type: "agent_status", agent_id: AGENT },
            useChatStore.setState,
            useChatStore.getState,
            AGENT,
        );

        // No `online` → no update, no probe.
        expect(mockUpdateAgentOnlineStatus).not.toHaveBeenCalled();
        expect(mockVerifyAgentHealth).not.toHaveBeenCalled();
    });
});