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
    //   4. activateSession       — atomically write activeSessionId (LAST).
    //                              We MUST call this AFTER messages are in
    //                              the cache because writing activeSessionId
    //                              causes ChatPanel to re-render with
    //                              `key={currentScrollKey}` →
    //                              VirtualMessageList mounts → the mount
    //                              effect runs `virtualizer.scrollToIndex(end)`.
    //                              If messages haven't loaded yet (virtualCount
    //                              == 0) the effect sets didInitialScrollRef
    //                              and returns early, permanently missing the
    //                              scroll-to-bottom.  Calling activateSession
    //                              last guarantees virtualCount > 0 on mount.
    //
    // We deliberately do NOT call switchSession here: that path is for the
    // user manually clicking a different session tab, and it aborts in-flight
    // loads, persists the user's "preferred session" choice, and re-runs
    // fetchSessions — all of which are either no-ops or double-work on first
    // launch.
    await useAgentStore.getState().fetchSessions(agentId);
    await useChatStore
        .getState()
        .fetchSessionState(agentId, targetSessionId);
    await useChatStore
        .getState()
        .ensureLatestInCache(agentId, targetSessionId);
    useChatStore.getState().activateSession(agentId, targetSessionId);
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
