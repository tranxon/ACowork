/**
 * Self-check for `agentStore.stopAgent` cleanup parity.
 *
 * Regression: stopAgent used to `invoke("stop_agent")` + `fetchAgents()`
 * only — the cached chat-store sessions (messages with attachment blobs,
 * pending approvals, tool progress, abort controllers) stayed alive in
 * memory even though the Runtime was gone. The Gateway-disconnect path
 * (`clearAgentSessions`) must be the shared cleanup, so a manual stop
 * leaves no stale footprint either.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const mockInvoke = vi.fn<[string, unknown?], Promise<unknown>>();
vi.mock("@tauri-apps/api/core", () => ({
    invoke: (cmd: string, args?: unknown) => mockInvoke(cmd, args),
}));

vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

import { useAgentStore } from "./agentStore";
import { useChatStore } from "./chatStore";

const AGENT_ID = "com.acowork.architect";
const INSTANCE_ID = "b7f0c6c2-4f10-4f5e-9a1e-3d8e9f2a1c33";

function seedAgent() {
    useAgentStore.setState({
        agents: {
            [INSTANCE_ID]: {
                meta: {
                    agent_id: AGENT_ID,
                    instance_id: INSTANCE_ID,
                    name: "Architect",
                    version: "1.0.0",
                    avatar: null,
                    builtin_avatar: null,
                    display_name: null,
                    role: null,
                    alive: true,
                    sleeping: false,
                    ready: true,
                    debug_state: "disabled",
                    debug_port: null,
                    workspace: "",
                    workspace_config_json: null,
                    current_embed_dim: null,
                    migration: null,
                    started_at: "2026-01-01T00:00:00Z",
                    last_interaction_at: "2026-01-01T00:00:00Z",
                },
                profile: {} as never,
                sessions: [],
                sessionTitle: undefined,
                pagination: { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 },
                isLoading: false,
                agentTokenTotals: null,
            },
        },
        selectedAgentId: INSTANCE_ID,
    });
}

describe("agentStore.stopAgent", () => {
    beforeEach(() => {
        mockInvoke.mockReset();
        useAgentStore.setState({ agents: {}, selectedAgentId: null, loading: false, error: null });
        useChatStore.setState({ agentStates: {} } as never);
        seedAgent();
        // stopAgent must ask the backend to stop...
        mockInvoke.mockImplementation(async (cmd: string) => {
            if (cmd === "stop_agent") return undefined;
            if (cmd === "list_agents") return [];
            throw new Error(`Unexpected invoke: ${cmd}`);
        });
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    it("clears the agent's cached session state after stop (parity with Gateway-disconnect cleanup)", async () => {
        const clearSpy = vi.spyOn(useChatStore.getState(), "clearAgentSessions");
        useChatStore.setState({
            agentStates: {
                [INSTANCE_ID]: {
                    activeSessionId: "s1",
                    openSessionIds: ["s1"],
                    sessionStates: { s1: { messages: [{ id: "m1" }] } as never },
                },
            },
        } as never);

        await useAgentStore.getState().stopAgent(INSTANCE_ID);

        expect(mockInvoke).toHaveBeenCalledWith("stop_agent", { agentId: INSTANCE_ID });
        expect(clearSpy).toHaveBeenCalledWith(INSTANCE_ID);
        // fetchAgents is still the reconcile tail of stop.
        expect(mockInvoke).toHaveBeenCalledWith("list_agents", undefined);
        clearSpy.mockRestore();
    });

    it("does NOT clear sessions when stop_agent rejects (user can retry with data intact)", async () => {
        mockInvoke.mockImplementation(async (cmd: string) => {
            if (cmd === "stop_agent") throw new Error("boom");
            throw new Error(`Unexpected invoke: ${cmd}`);
        });
        const clearSpy = vi.spyOn(useChatStore.getState(), "clearAgentSessions");
        useChatStore.setState({
            agentStates: {
                [INSTANCE_ID]: {
                    activeSessionId: "s1",
                    openSessionIds: ["s1"],
                    sessionStates: { s1: { messages: [{ id: "m1" }] } as never },
                },
            },
        } as never);

        await expect(useAgentStore.getState().stopAgent(INSTANCE_ID)).rejects.toThrow("boom");
        expect(clearSpy).not.toHaveBeenCalled();
        expect(
            useChatStore.getState().getSessionState(INSTANCE_ID, "s1").messages.length,
        ).toBe(1);
        clearSpy.mockRestore();
    });
});