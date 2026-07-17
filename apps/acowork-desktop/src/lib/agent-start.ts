//! Agent start orchestration utility.
//!
//! Provides `startAgentAndSyncUI` — an atomic function that:
//! 1. Starts the agent process
//! 2. Waits for the Runtime to become ready
//! 3. Initializes the session (fetch list, determine active, pull state)
//! 4. Synchronizes UI (fetch workspaces, refresh config)
//!
//! All callers (AgentList right-click, ChatPanel "Start Agent" button) use
//! this single entry point so session data is always ready before rendering.

import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { emitAgentConfigRefresh } from "./refresh";

/**
 * Initialize the session for an agent using the lightweight
 * `fetchLatestSession` endpoint (no full disk scan).
 *
 * The Runtime caches the latest session (by last_active_at desc) during
 * startup, so this is a cheap in-memory read. Falls back to the
 * remembered session if it matches the latest; otherwise uses the latest.
 *
 * Extracted from ChatPanel's useEffect so it can run atomically
 * inside `startAgentAndSyncUI` before any UI rendering.
 */
async function initSessionForAgent(agentId: string): Promise<void> {
    // Retry until the startup scan completes (max 10 attempts, 1s interval).
    // The scan runs in a background task and may not have finished yet.
    const maxRetries = 10;
    let latestSession: { session_id: string; title: string | null } | null = null;

    for (let i = 0; i < maxRetries; i++) {
        latestSession = await useAgentStore.getState().fetchLatestSession(agentId);
        if (latestSession) break;
        if (i < maxRetries - 1) {
            await new Promise((resolve) => setTimeout(resolve, 1000));
        }
    }

    if (!latestSession) return;

    // Backend /latest-session is the source of truth — no client-side
    // rememberedSessionId needed.
    const targetSessionId = latestSession.session_id;

    // Atomically bootstrap the session BEFORE returning.  This must run as a
    // single serial chain so the ChatPanel sees a fully-rendered chat on its
    // very first mount — no "blank chat then messages pop in" flicker, no
    // mount effect that has to re-fire when activeSessionId finally lands:
    //
    //   1. fetchSessions         — populate agentStore.sessions (sidebar list).
    //   2. fetchSessionState     — pulls model/provider/workspace_id and
    //                              context usage via applySessionMeta so the
    //                              header bar and metadata don't pop in
    //                              piecewise.
    //   3. ensureLatestInCache   — loads the latest message window into the
    //                              cache so messages are available when
    //                              ChatPanel first renders.
    // ADR-038: opening a freshly-resolved session on first agent start is
    // a "first-open" scenario, so we use `openSession` (UI + MQTT
    // open_session + HTTP messages reload) instead of the strict
    // `setActiveTab`.  `openSession` sets both `openSessionIds` (so the
    // tab renders without a follow-up remount) and `activeSessionId`
    // (so ChatPanel mounts with the right key from the very first render
    // → no flicker).
    //
    // We deliberately do NOT use the legacy `switchSession` helper here
    // (already removed in ADR-038): it aborted in-flight loads and
    // re-ran fetchSessions — both are either no-ops or double-work on
    // first launch.
    await useAgentStore.getState().fetchSessions(agentId);
    await useChatStore
        .getState()
        .fetchSessionState(agentId, targetSessionId);
    await useChatStore
        .getState()
        .ensureLatestInCache(agentId, targetSessionId);
    await useChatStore.getState().openSession(agentId, targetSessionId);
}

/**
 * Atomic agent start + session init + UI sync.
 *
 * Replaces the previous two-step pattern:
 *   `await startAgentAndSyncUI(id);`
 *
 * @param agentId  The agent package ID to start.
 * @param devMode  If true, start in debug mode (DevTools WebSocket enabled).
 */
export async function startAgentAndSyncUI(
    agentId: string,
    devMode = false,
): Promise<void> {
    // 1. Start the agent process
    await useAgentStore.getState().startAgent(agentId, devMode);

    // 2. Wait for the Runtime to become ready
    await useAgentStore.getState().waitForAgentReady(agentId);

    // 3. Initialize session — fetch list, determine active, pull state
    await initSessionForAgent(agentId);

    // 4. Sync UI — workspaces, config refresh (pure render)
    useWorkspaceStore.getState().fetchWorkspaces(agentId);
    emitAgentConfigRefresh(agentId);
}
