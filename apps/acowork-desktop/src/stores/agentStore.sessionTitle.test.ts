/**
 * Regression coverage for `updateSessionTitle`.
 *
 * Background:
 *   When the Runtime auto-generates a session title (e.g. from the first
 *   user message), it pushes a `session_config` MQTT event with the new
 *   title. `chatStore` forwards that to `agentStore.updateSessionTitle`.
 *   Two surfaces consume the title:
 *
 *   1. The AgentList sidebar row → reads `agents[id].sessionTitle`.
 *   2. The SessionTabBar / SessionListDropdown → read `sessions[i].title`.
 *
 *   The function used to only patch (2), leaving (1) stale. The sidebar
 *   then showed "未命名" until the user clicked the session-list dropdown,
 *   which triggers `fetchSessions` and re-derives `sessionTitle` from the
 *   server's response.
 *
 *   `renameSession` (manual rename from the tab UI) already patches both
 *   fields in one go. `updateSessionTitle` should mirror that so the two
 *   code paths stay symmetric.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

// Keep the Tauri invoke stub — agentStore imports it at module load.
vi.mock("@tauri-apps/api/core", () => ({
    invoke: () => Promise.reject(new Error("not used in this test")),
}));

// Silence the logger so the test output is clean.
vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

// Profile persistence is loaded at module init.
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
import type { SessionInfo } from "../lib/types";

const AGENT_ID = "com.acowork.architect";
const INSTANCE_ID = "b7f0c6c2-4f10-4f5e-9a1e-3d8e9f2a1c33";

function makeMeta(overrides: Partial<AgentInfo> = {}): AgentInfo {
    return {
        agent_id: AGENT_ID,
        instance_id: INSTANCE_ID,
        name: "Architect",
        version: "1.0.0",
        avatar: null,
        builtin_avatar: null,
        display_name: null,
        role: null,
        running: true,
        ready: true,
        connected: true,
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

function makeSession(overrides: Partial<SessionInfo> = {}): SessionInfo {
    return {
        session_id: "sess-1",
        created_at: "2026-01-01T00:00:00Z",
        last_active_at: "2026-01-01T00:00:00Z",
        message_count: 0,
        title: null,
        ...overrides,
    };
}

function seedAgent(sessions: SessionInfo[], sessionTitle: string | undefined) {
    useAgentStore.setState({
        agents: {
            [INSTANCE_ID]: {
                meta: makeMeta(),
                profile: {} as never,
                sessions,
                sessionTitle,
                pagination: {
                    currentPage: 1,
                    totalPages: 1,
                    totalCount: sessions.length,
                    pageSize: 20,
                },
                isLoading: false,
                agentTokenTotals: null,
                online: true,
                sleeping: false,
            },
        },
        selectedAgentId: INSTANCE_ID,
    });
}

beforeEach(() => {
    useAgentStore.setState({
        agents: {},
        selectedAgentId: null,
        loading: false,
        error: null,
    });
});

describe("updateSessionTitle — sidebar/tab sync", () => {
    it("updates BOTH sessions[i].title and sessionTitle when the latest session gets a title", () => {
        // Pre-condition: a brand-new (untitled) session is the latest one.
        // The sidebar shows the placeholder "Untitled" because sessionTitle
        // was fetched as "" by an earlier fetchLatestSession round-trip.
        seedAgent([makeSession({ session_id: "sess-1", title: null })], "");

        // Backend generated a title from the first user message and
        // pushed it via the session_config MQTT event.
        useAgentStore.getState().updateSessionTitle("sess-1", "Hello world");

        const storage = useAgentStore.getState().agents[INSTANCE_ID];

        // (1) Tab bar / session list read this field — must reflect the new
        // title so the tab stops showing "未命名".
        expect(storage.sessions[0]?.title).toBe("Hello world");

        // (2) Sidebar reads THIS field — must reflect the new title so the
        // sidebar row stops showing "未命名" without a manual refresh.
        // Before the fix this stayed "" until the next fetchSessions.
        expect(storage.sessionTitle).toBe("Hello world");
    });

    it("does NOT touch sessionTitle when the updated session is not the latest one", () => {
        // Two sessions, the older one is at idx=1.
        seedAgent(
            [
                makeSession({ session_id: "sess-new", created_at: "2026-02-01T00:00:00Z", title: null }),
                makeSession({ session_id: "sess-old", created_at: "2026-01-01T00:00:00Z", title: null }),
            ],
            "",
        );

        // The latest one gets titled first.
        useAgentStore.getState().updateSessionTitle("sess-new", "Brand new");
        expect(useAgentStore.getState().agents[INSTANCE_ID].sessionTitle).toBe("Brand new");

        // Then the older one gets a title — sidebar should NOT change,
        // matching the `renameSession` heuristic (idx === 0 only).
        useAgentStore.getState().updateSessionTitle("sess-old", "Old chat");
        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.sessions[1]?.title).toBe("Old chat");
        expect(storage.sessionTitle).toBe("Brand new");
    });

    it("still skips when the session already has a non-empty title (race protection)", () => {
        // Pretend fetchSessions already loaded this session with a title
        // set by some other code path. The "skip if already has title"
        // guard in updateSessionTitle is intentional — we keep it so a
        // stale MQTT replay can't clobber a manual rename. This test
        // pins that behavior.
        seedAgent(
            [makeSession({ session_id: "sess-1", title: "Manual rename" })],
            "Manual rename",
        );

        useAgentStore.getState().updateSessionTitle("sess-1", "Stale replay");

        const storage = useAgentStore.getState().agents[INSTANCE_ID];
        expect(storage.sessions[0]?.title).toBe("Manual rename");
        expect(storage.sessionTitle).toBe("Manual rename");
    });
});
