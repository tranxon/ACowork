import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { AgentInfo, AgentDetail } from "../lib/types";
import { useAgentProfileStore } from "./agentProfileStore";

/** System Agent ID — always auto-started by Gateway */
const SYSTEM_AGENT_ID = "com.acowork.system";

interface AgentStore {
  agents: AgentInfo[];
  selectedAgentId: string | null;
  _userHasSelected: boolean;
  loading: boolean;
  error: string | null;

  fetchAgents: () => Promise<void>;
  selectAgent: (id: string | null) => void;
  installAgent: (packagePath: string) => Promise<void>;
  uninstallAgent: (agentId: string) => Promise<void>;
  startAgent: (agentId: string, devMode?: boolean) => Promise<void>;
  stopAgent: (agentId: string) => Promise<void>;
  restartAgentInDebug: (agentId: string) => Promise<void>;
  getAgentDetail: (agentId: string) => Promise<AgentDetail>;
  /** Poll fetchAgents until agent.ready === true (max 30×500ms = 15s). */
  waitForAgentReady: (agentId: string) => Promise<void>;
}

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: [],
  selectedAgentId: null,
  _userHasSelected: false,
  loading: false,
  error: null,

  fetchAgents: async () => {
    const t0 = performance.now();
    set({ loading: true, error: null });
    try {
      const agents = await invoke<AgentInfo[]>("list_agents");
      const t1 = performance.now();
      // Diagnostic: log timing and ready flags to trace poll delay
      const sr = agents.find((a: AgentInfo) => a.agent_id === "com.acowork.senior-engineer");
      if (sr) {
        console.log(`[AgentStore] fetchAgents took ${(t1 - t0).toFixed(0)}ms | senior-engineer: running=${sr.running} ready=${sr.ready} connected=${sr.connected}`);
      }
      set({ agents, loading: false });

      // Backfill builtin avatars for any agent that has neither a packaged
      // avatar (manifest.avatar) nor a profile icon set. Idempotent and safe
      // to call on every fetch — the helper skips already-assigned agents.
      // Each manifest's `builtin_avatar` hint is preferred over a random pick.
      useAgentProfileStore.getState().ensureBuiltinAvatars(
        agents.map((a) => ({
          agent_id: a.agent_id,
          avatar: a.avatar ?? null,
          builtin_avatar: a.builtin_avatar ?? null,
        })),
      );

      // Auto-select System Agent on initial load (user has not explicitly picked yet)
      const current = get();
      if (!current.selectedAgentId && !current._userHasSelected && agents.length > 0) {
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
    set({ selectedAgentId: id, _userHasSelected: true });
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

  startAgent: async (agentId, devMode) => {
    try {
      await invoke("start_agent", { agentId, devMode: devMode ?? false });
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

  restartAgentInDebug: async (agentId) => {
    try {
      await invoke("restart_agent_in_debug", { agentId });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  getAgentDetail: async (agentId) => {
    return await invoke<AgentDetail>("get_agent_detail", { agentId });
  },

  waitForAgentReady: async (agentId) => {
    for (let attempt = 0; attempt < 30; attempt++) {
      await get().fetchAgents();
      const agent = get().agents.find((a) => a.agent_id === agentId);
      if (agent?.ready) {
        return;
      }
      if (!agent?.running) {
        throw new Error("Agent process exited before becoming ready");
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    throw new Error("Agent did not become ready within 15 seconds");
  },
}));
