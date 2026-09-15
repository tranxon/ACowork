/**
 * Regression coverage for distributed agent liveness.
 *
 * Background (2026-09-xx refactor):
 *   `running`/`connected` (process/PID-flavoured signals) are gone.
 *   The Gateway's `/api/agents` now exposes `alive` — the MQTT
 *   registry verdict (Runtime's broker-level session reachable:
 *   `online` / `sleeping` / `degraded`). It is topology independent:
 *   the same answer for local, remote and node-hosted Runtimes, and
 *   never consults a PID on the Gateway's machine.
 *
 *   The Desktop's contract:
 *   - `fetchAgents` adopts the Gateway's `meta.alive` verbatim
 *     (authoritative reconcile path).
 *   - `updateAgentLiveness` is the realtime MQTT path — it patches
 *     `meta.alive` / `meta.sleeping` immediately on `agent_status`.
 *
 * These tests pin that contract.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// ── Mock Tauri invoke: fetchAgents drives list_agents through this ──────

const mockListAgents = vi.fn<[], Promise<unknown[]>>();
const mockStartAgent = vi.fn<[], Promise<unknown>>();

vi.mock("@tauri-apps/api/core", () => ({
    invoke: (cmd: string) => {
        if (cmd === "list_agents") return mockListAgents();
        if (cmd === "start_agent") return mockStartAgent();
        return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
    },
}));

// ── Mock the logger (avoids touching console) ────────────────────────────

vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

// ── Mock profile persistence (keep tests hermetic) ───────────────────────

vi.mock("../lib/profileStore", () => ({
    loadAllProfiles: () => ({}),
    loadProfile: () => null,
    saveProfile: () => {},
    DEFAULT_PROFILE: {
        language: "zh-CN",
        timezone: "Asia/Shanghai",
        model_override: null,
        reasoning_effort: null,
        auto_approve_tools: [],
        yolo_mode: false,
    },
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { useAgentStore } from "./agentStore";
import type { AgentInfo } from "./agentStore";

const AGENT_ID = "com.acowork.architect";
// ADR-073: the store map is keyed by INSTANCE identity (UUID); the
// package `agent_id` is display/package identity only.
const INSTANCE_ID = "b7f0c6c2-4f10-4f5e-9a1e-3d8e9f2a1c33";
const REMOVED_INSTANCE_ID = "9f1e2d3c-8a7b-4c5d-9e0f-1a2b3c4d5e6f";

function makeMeta(overrides: Partial<AgentInfo>): AgentInfo {
    return {
        agent_id: AGENT_ID,
        instance_id: INSTANCE_ID,
        name: "Architect",
        version: "1.0.0",
        avatar: null,
        builtin_avatar: null,
        display_name: null,
        role: null,
        alive: false,
        sleeping: false,
        ready: false,
        debug_state: "disabled",
        debug_port: null,
        workspace: "",
        workspace_config_json: null,
        current_embed_dim: null,
        migration: null,
        started_at: "2026-01-01T00:00:00Z",
        last_interaction_at: "2026-01-01T00:00:00Z",
        ...overrides,
    };
}

function seedAgent(meta: Partial<AgentInfo>) {
    useAgentStore.setState({
        agents: {
            [INSTANCE_ID]: {
                meta: makeMeta(meta),
                profile: {} as never,
                sessions: [],
                sessionTitle: undefined,
                pagination: {
                    currentPage: 1,
                    totalPages: 1,
                    totalCount: 0,
                    pageSize: 20,
                },
                isLoading: false,
                agentTokenTotals: null,
            },
        },
        selectedAgentId: INSTANCE_ID,
    });
}

beforeEach(() => {
    // Reset the store to a clean state between tests.
    useAgentStore.setState({
        agents: {},
        selectedAgentId: null,
        loading: false,
        error: null,
    });
    mockListAgents.mockReset();
    mockStartAgent.mockReset();
});

afterEach(() => {
    vi.useRealTimers();
});

describe("fetchAgents — adopts the Gateway's `alive` verdict verbatim", () => {
    it("adopts alive=true when the Gateway reports the agent alive (local runtime)", async () => {
        seedAgent({ alive: false });
        mockListAgents.mockResolvedValue([makeMeta({ alive: true })]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(true);
        expect(storage.meta.sleeping).toBe(false);
    });

    it("adopts alive=true for a REMOTE runtime — no local PID probing involved", async () => {
        // A remote / node-hosted Runtime's PID is invisible from this
        // desktop and from a remote Gateway. The only trustworthy
        // signal is the MQTT registry verdict the Gateway already
        // computed — the desktop must adopt it without second-guessing.
        seedAgent({ alive: false });
        mockListAgents.mockResolvedValue([makeMeta({ alive: true })]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(true);
    });

    it("adopts alive=false + sleeping=false when the Gateway says the agent is gone", async () => {
        // Genuine shutdown (manual stop / crash / LWT offline): the
        // Gateway is authoritative, so the desktop converges to
        // alive=false even if a stale MQTT event left sleeping=true.
        seedAgent({ alive: true, sleeping: true });
        mockListAgents.mockResolvedValue([makeMeta({ alive: false, sleeping: false })]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(false);
        expect(storage.meta.sleeping).toBe(false);
    });

    it("adopts sleeping=true when the Gateway reports auto-sleep (retained `sleeping` status)", async () => {
        // Auto-sleep: alive=true (retained status still cached) +
        // sleeping=true → the UI renders the Start button + "auto-slept"
        // badge instead of a live session.
        seedAgent({ alive: true, sleeping: false });
        mockListAgents.mockResolvedValue([makeMeta({ alive: true, sleeping: true })]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(true);
        expect(storage.meta.sleeping).toBe(true);
    });

    it("creates a brand-new agent with the Gateway's verdict when not previously known", async () => {
        mockListAgents.mockResolvedValue([makeMeta({ alive: true })]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage).toBeDefined();
        expect(storage.meta.alive).toBe(true);
        expect(storage.meta.sleeping).toBe(false);
    });

    it("removes agents that are no longer in the Gateway's list", async () => {
        seedAgent({ alive: true });
        // Gateway only reports the architect agent now.
        mockListAgents.mockResolvedValue([makeMeta({ alive: true })]);

        await useAgentStore.getState().fetchAgents();

        const agents = useAgentStore.getState().agents;
        expect(agents[INSTANCE_ID]).toBeDefined();
        expect(agents[REMOVED_INSTANCE_ID]).toBeUndefined();
    });
});

describe("updateAgentLiveness — realtime MQTT path patches meta", () => {
    it("flips meta.alive=false on MQTT `offline`", () => {
        seedAgent({ alive: true, sleeping: false });
        useAgentStore.getState().updateAgentLiveness(INSTANCE_ID, false, false);

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(false);
        expect(storage.meta.sleeping).toBe(false);
    });

    it("carries the sleeping flag on MQTT `sleeping`", () => {
        seedAgent({ alive: false, sleeping: false });
        useAgentStore.getState().updateAgentLiveness(INSTANCE_ID, true, true);

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(true);
        expect(storage.meta.sleeping).toBe(true);
    });

    it("defaults sleeping=false for statuses that omit it (legacy Runtimes)", () => {
        seedAgent({ alive: true, sleeping: true });
        useAgentStore.getState().updateAgentLiveness(INSTANCE_ID, true);

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.meta.alive).toBe(true);
        expect(storage.meta.sleeping).toBe(false);
    });
});

describe("startAgent — waits for the MQTT online EVENT (no polling)", () => {
    it("resolves only when the online event arrives after start", async () => {
        seedAgent({ alive: false, ready: false });
        mockStartAgent.mockResolvedValue({}); // Gateway /start ack (async)

        const p = useAgentStore.getState().startAgent(INSTANCE_ID, false);
        // Give the invoke microtask time to reach the waiter registration.
        await new Promise((r) => setTimeout(r, 0));
        let settled = false;
        p.then(() => (settled = true)).catch(() => (settled = true));
        expect(settled).toBe(false); // still waiting — no polling, no state read

        // MQTT `agent_status online` event arrives.
        useAgentStore.getState().updateAgentLiveness(INSTANCE_ID, true, false);
        await expect(p).resolves.toBeUndefined();
    });

    it("rejects when the online event never arrives (15s timeout)", async () => {
        seedAgent({ alive: false, ready: false });
        mockStartAgent.mockResolvedValue({});
        vi.useFakeTimers();

        const p = useAgentStore.getState().startAgent(INSTANCE_ID, false);
        // Attach the assertion BEFORE advancing so the rejection is handled
        // as it fires (no unhandled-rejection noise).
        const expectation = expect(p).rejects.toThrow(/did not come online within 15s/);
        // Async advance drains the invoke microtask (waiter registration)
        // and then the 15s timer.
        await vi.advanceTimersByTimeAsync(15_000);
        await expectation;
    });

    it("resolves immediately when the agent is already online", async () => {
        seedAgent({ alive: true, ready: false });
        mockStartAgent.mockResolvedValue({});
        await expect(
            useAgentStore.getState().startAgent(INSTANCE_ID, false),
        ).resolves.toBeUndefined();
    });
});
