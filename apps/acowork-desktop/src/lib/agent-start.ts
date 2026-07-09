//! Agent start orchestration utility.
//!
//! Provides `startAgentAndSyncUI` — an atomic function that:
//! 1. Starts the agent process
//! 2. Waits for the Runtime to become ready
//! 3. Initializes the session (fetch list, determine active, pull state)
//! 4. Synchronizes UI (connect WebSocket, fetch workspaces, refresh config)
//!
//! All callers (AgentList right-click, ChatPanel "Start Agent" button) use
//! this single entry point so session data is always ready before rendering.

import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { getGatewayUrl } from "./config";
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

    // Prefer the remembered session if it matches the latest;
    // otherwise just use the latest session from the backend.
    const rememberedSessionId =
        useAgentStore.getState().agents[agentId]?.rememberedSessionId;
    const targetSessionId =
        rememberedSessionId === latestSession.session_id
            ? rememberedSessionId
            : latestSession.session_id;

    // Load the full session list before switching.  ChatPanel's
    // message-loading useEffect checks that the session exists in
    // agentStore.sessions (agent-start.ts → agentStore.switchSession →
    // fire-and-forget fetchSessions → ChatPanel.tsx useEffect guard).
    // Without this await the effect fires while the list is still
    // empty and permanently skips loadSessionMessages.
    await useAgentStore.getState().fetchSessions(agentId);

    await useAgentStore
        .getState()
        .switchSession(targetSessionId, agentId);
    await useChatStore
        .getState()
        .fetchSessionState(agentId, targetSessionId);
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

    // 4. Sync UI — connect stream, workspaces, config refresh (pure render)
    useChatStore.getState().connectStream(agentId, getGatewayUrl());
    useWorkspaceStore.getState().fetchWorkspaces(agentId);
    emitAgentConfigRefresh(agentId);
}
