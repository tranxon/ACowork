//! Global resource refresh notification utility.
//!
//! Used by stores and components that mutate global resources (provider, MCP
//! catalog, etc.) to notify the AgentSetupTab to re-fetch agent config.
//! This is the Desktop-side companion to Gateway's GlobalResourcePusher.

import { useAgentStore } from "../stores/agentStore";

/** Dispatch a custom event to trigger AgentSetupTab config re-fetch.
 * @param agentId Optional explicit agent ID. Defaults to currently selected agent. */
export function emitAgentConfigRefresh(agentId?: string) {
    const id = agentId ?? useAgentStore.getState().selectedAgentId;
    if (!id) return;
    window.dispatchEvent(
        new CustomEvent("rollball:refresh-agent-config", {
            detail: { agentId: id },
        }),
    );
}
