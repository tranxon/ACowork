//! Agent start UI orchestration utility.
//!
//! Encapsulates the common sequence of post-start UI state sync:
//! waitForAgentReady → connectStream → fetchWorkspaces → emitAgentConfigRefresh.
//!
//! Both AgentList (right-click Start) and ChatPanel ("Agent is stopped" button)
//! invoke this after calling startAgent(), avoiding duplicate code.

import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { getGatewayUrl } from "./config";
import { emitAgentConfigRefresh } from "./refresh";

/**
 * Wait for the agent Runtime to become ready, then synchronize all UI state.
 *
 * - Connects the chat WebSocket stream
 * - Refreshes the workspace directory list
 * - Refreshes the agent config (model, provider, tools)
 *
 * Should be called *after* `startAgent()` has been invoked.
 * The caller is responsible for toast messages and any local loading state.
 */
export async function syncAgentUI(agentId: string) {
    await useAgentStore.getState().waitForAgentReady(agentId);
    useChatStore.getState().connectStream(agentId, getGatewayUrl());
    useWorkspaceStore.getState().fetchWorkspaces(agentId);
    emitAgentConfigRefresh(agentId);
}
