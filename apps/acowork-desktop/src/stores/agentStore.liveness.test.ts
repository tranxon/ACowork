/**
 * Regression coverage for the distributed-liveness fix in `fetchAgents`.
 *
 * Background (2026-09-02 09:12 incident):
 *   The Runtime may run on a remote Node Agent (ADR-055) — its process
 *   is not on the same machine as the desktop. The Gateway's
 *   `/api/agents` endpoint exposes a process-based `running` flag
 *   (Gateway checks the PID it spawned). For REMOTE runtimes that
 *   flag is always false because the Gateway can't see the remote PID.
 *
 *   The fix: `running` OR `connected` is the authoritative liveness
 *   signal. `connected` already merges the MQTT online view (Gateway
 *   `list_agents` reflects MQTT status), so a remote Runtime that is
 *   reachable over MQTT is correctly reported as alive even though
 *   its `running` (PID) flag is false.
 *
 *   Only when BOTH `running=false` AND `connected=false` do we force
 *   `online=false, sleeping=false` on the agent's storage. The
 *   `agent_status` MQTT handler further double-checks offline
 *   transitions against `/health`.
 *
 *   These tests pin that `running || connected` semantics.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// ── Mock Tauri invoke: fetchAgents drives list_agents through this ──────

const mockListAgents = vi.fn<[], Promise<unknown[]>>();

vi.mock("@tauri-apps/api/core", () => ({
    invoke: (cmd: string) => {
        if (cmd === "list_agents") return mockListAgents();
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

function makeMeta(overrides: Partial<AgentInfo>): AgentInfo {
    return {
        agent_id: AGENT_ID,
        name: "Architect",
        version: "1.0.0",
        avatar: null,
        builtin_avatar: null,
        display_name: null,
        role: null,
        running: false,
        ready: false,
        connected: false,
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

beforeEach(() => {
    // Reset the store to a clean state between tests.
    useAgentStore.setState({
        agents: {},
        selectedAgentId: null,
        loading: false,
        error: null,
    });
    mockListAgents.mockReset();
});

afterEach(() => {
    vi.useRealTimers();
});

describe("fetchAgents — distributed liveness (`running || connected`)", () => {
    it("keeps an existing agent online when Gateway says `running=true` (local process alive)", async () => {
        // Pre-condition: agent exists, online=true, sleeping=false
        // (some previous MQTT/agent_status event).
        useAgentStore.setState({
            agents: {
                [AGENT_ID]: {
                    meta: makeMeta({ running: true }),
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
                    online: true,
                    sleeping: false,
                },
            },
            selectedAgentId: AGENT_ID,
        });
        mockListAgents.mockResolvedValue([
            makeMeta({ running: true, connected: false }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[AGENT_ID];
        expect(storage.online).toBe(true);
        expect(storage.sleeping).toBe(false);
    });

    it("keeps an existing agent online when Gateway says `connected=true` even with `running=false` (REMOTE runtime)", async () => {
        // The whole point of the fix: a remote Runtime's PID is not
        // visible to the Gateway, so `running=false`. But MQTT shows
        // it connected, so the desktop must NOT render it as offline.
        useAgentStore.setState({
            agents: {
                [AGENT_ID]: {
                    meta: makeMeta({ running: false }),
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
                    online: true,
                    sleeping: false,
                },
            },
            selectedAgentId: AGENT_ID,
        });
        mockListAgents.mockResolvedValue([
            makeMeta({ running: false, connected: true }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[AGENT_ID];
        expect(storage.online).toBe(true);
        expect(storage.sleeping).toBe(false);
        expect(storage.meta.running).toBe(false);
        expect(storage.meta.connected).toBe(true);
    });

    it("forces offline when BOTH `running=false` AND `connected=false` (genuine shutdown)", async () => {
        // Even if the in-memory MQTT view still says online=true
        // (stale), the Gateway's authoritative answer is "gone". This
        // prevents the stale-online window where the ChatPanel keeps
        // showing the input after the Runtime actually died.
        useAgentStore.setState({
            agents: {
                [AGENT_ID]: {
                    meta: makeMeta({ running: true }),
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
                    online: true,
                    sleeping: false,
                },
            },
            selectedAgentId: AGENT_ID,
        });
        mockListAgents.mockResolvedValue([
            makeMeta({ running: false, connected: false }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[AGENT_ID];
        expect(storage.online).toBe(false);
        expect(storage.sleeping).toBe(false);
    });

    it("keeps an existing agent offline when Gateway still says alive (preserves offline state across polls)", async () => {
        // Once the desktop has decided the agent is offline, a subsequent
        // fetchAgents that returns `running=true` must NOT flip it back
        // to online — the `agent_status` handler (with HTTP /health
        // double-check) is the only path that can revive an offline
        // agent. This keeps the offline state stable until an explicit
        // MQTT-online or HTTP-alive signal arrives.
        useAgentStore.setState({
            agents: {
                [AGENT_ID]: {
                    meta: makeMeta({ running: false }),
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
                    online: false,
                    sleeping: false,
                },
            },
            selectedAgentId: AGENT_ID,
        });
        mockListAgents.mockResolvedValue([
            makeMeta({ running: true, connected: true }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[AGENT_ID];
        // fetchAgents keeps `existing.online` when alive — so the offline
        // state stays preserved (not flipped back to online without an
        // explicit MQTT/HTTP revival).
        expect(storage.online).toBe(false);
        expect(storage.sleeping).toBe(false);
    });

    it("forces `sleeping=false` along with `online=false` on shutdown (no zombie sleeping animation)", async () => {
        // Pre-condition: agent was in some odd state with sleeping=true
        // (e.g. a stale MQTT message). When the Gateway definitively
        // reports both signals gone, BOTH must reset.
        useAgentStore.setState({
            agents: {
                [AGENT_ID]: {
                    meta: makeMeta({ running: false }),
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
                    online: true,
                    sleeping: true,
                },
            },
            selectedAgentId: AGENT_ID,
        });
        mockListAgents.mockResolvedValue([
            makeMeta({ running: false, connected: false }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[AGENT_ID];
        expect(storage.online).toBe(false);
        expect(storage.sleeping).toBe(false);
    });

    it("creates a brand-new agent with default `online=true` when not previously known", async () => {
        // No existing entry → createStorage defaults to online=true,
        // sleeping=false. This is the first-paint optimisation so the
        // user doesn't see an empty/disabled UI for a moment.
        mockListAgents.mockResolvedValue([
            makeMeta({ running: true, connected: true }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const storage = useAgentStore.getState().agents[AGENT_ID];
        expect(storage).toBeDefined();
        expect(storage.online).toBe(true);
        expect(storage.sleeping).toBe(false);
        expect(storage.meta.running).toBe(true);
    });

    it("removes agents that are no longer in the Gateway's list", async () => {
        useAgentStore.setState({
            agents: {
                [AGENT_ID]: {
                    meta: makeMeta({ running: true }),
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
                    online: true,
                    sleeping: false,
                },
                "com.acowork.removed": {
                    meta: makeMeta({
                        agent_id: "com.acowork.removed",
                        running: true,
                    }),
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
                    online: true,
                    sleeping: false,
                },
            },
            selectedAgentId: AGENT_ID,
        });
        // Gateway only reports the architect agent now.
        mockListAgents.mockResolvedValue([
            makeMeta({ running: true, connected: true }),
        ]);

        await useAgentStore.getState().fetchAgents();

        const agents = useAgentStore.getState().agents;
        expect(agents[AGENT_ID]).toBeDefined();
        expect(agents["com.acowork.removed"]).toBeUndefined();
    });
});