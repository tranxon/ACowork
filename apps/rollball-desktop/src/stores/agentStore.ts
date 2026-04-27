import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { AgentInfo, AgentDetail } from "../lib/types";

/** System Agent ID — always auto-started by Gateway */
const SYSTEM_AGENT_ID = "com.rollball.system";

interface AgentStore {
  agents: AgentInfo[];
  selectedAgentId: string | null;
  loading: boolean;
  error: string | null;

  fetchAgents: () => Promise<void>;
  selectAgent: (id: string | null) => void;
  installAgent: (packagePath: string) => Promise<void>;
  uninstallAgent: (agentId: string) => Promise<void>;
  startAgent: (agentId: string) => Promise<void>;
  stopAgent: (agentId: string) => Promise<void>;
  getAgentDetail: (agentId: string) => Promise<AgentDetail>;
}

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: [],
  selectedAgentId: null,
  loading: false,
  error: null,

  fetchAgents: async () => {
    set({ loading: true, error: null });
    try {
      const agents = await invoke<AgentInfo[]>("list_agents");
      set({ agents, loading: false });
      
      // Auto-select System Agent if available and nothing is selected
      const current = get();
      if (!current.selectedAgentId && agents.length > 0) {
        const systemAgent = agents.find((a) => a.agent_id === SYSTEM_AGENT_ID);
        if (systemAgent) {
          set({ selectedAgentId: SYSTEM_AGENT_ID });
        } else {
          // Fallback: select first available agent
          set({ selectedAgentId: agents[0].agent_id });
        }
      }
    } catch (e) {
      set({ error: String(e), loading: false });
    }
  },

  selectAgent: (id) => {
    set({ selectedAgentId: id });
  },

  installAgent: async (packagePath) => {
    try {
      // dev_mode=true for local development (skip signature verification)
      await invoke("install_agent", { packagePath, devMode: true });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  uninstallAgent: async (agentId) => {
    // Prevent uninstalling System Agent
    if (agentId === SYSTEM_AGENT_ID) {
      throw new Error("System Agent cannot be uninstalled");
    }
    try {
      await invoke("uninstall_agent", { agentId });
      // If the uninstalled agent was selected, fallback to System Agent
      if (get().selectedAgentId === agentId) {
        const agents = get().agents.filter((a) => a.agent_id !== agentId);
        const systemAgent = agents.find((a) => a.agent_id === SYSTEM_AGENT_ID);
        set({ selectedAgentId: systemAgent?.agent_id ?? (agents[0]?.agent_id ?? null) });
      }
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  startAgent: async (agentId) => {
    try {
      await invoke("start_agent", { agentId });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  stopAgent: async (agentId) => {
    try {
      await invoke("stop_agent", { agentId });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  getAgentDetail: async (agentId) => {
    return await invoke<AgentDetail>("get_agent_detail", { agentId });
  },
}));
