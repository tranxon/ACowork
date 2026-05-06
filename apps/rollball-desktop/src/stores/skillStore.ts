import { create } from "zustand";
import type {
  SkillListEntry,
  SkillDetailResponse,
  SkillExecutionHistoryResponse,
} from "../lib/types";
import { getGatewayUrl } from "../lib/config";

interface SkillStore {
  skills: SkillListEntry[];
  total: number;
  selectedSkillName: string | null;
  selectedSkillDetail: SkillDetailResponse | null;
  executionHistory: SkillExecutionHistoryResponse | null;
  /** Currently active skill for chat command injection */
  activeSkill: SkillListEntry | null;

  loading: boolean;
  error: string | null;

  // Actions
  fetchSkills: (agentId: string) => Promise<void>;
  selectSkill: (agentId: string, skillName: string) => Promise<void>;
  fetchExecutionHistory: (agentId: string, skillName: string, page?: number) => Promise<void>;
  importSkill: (agentId: string, file: File) => Promise<{ success: boolean; skillName?: string; message?: string }>;
  clearSkills: () => void;
  deselectSkill: () => void;
  setActiveSkill: (skill: SkillListEntry | null) => void;
  clearActiveSkill: () => void;
}

export const useSkillStore = create<SkillStore>((set, get) => ({
  skills: [],
  total: 0,
  selectedSkillName: null,
  selectedSkillDetail: null,
  executionHistory: null,
  activeSkill: null,
  loading: false,
  error: null,

  fetchSkills: async (agentId) => {
    set({ loading: true, error: null });
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/skills?page=1&size=100`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      set({ skills: data.skills, total: data.total, loading: false });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Unknown error" });
    }
  },

  selectSkill: async (agentId, skillName) => {
    set({ selectedSkillName: skillName, selectedSkillDetail: null, executionHistory: null });
    try {
      const res = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/skills/${encodeURIComponent(skillName)}`,
      );
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const detail: SkillDetailResponse = await res.json();
      set({ selectedSkillDetail: detail });
    } catch (e) {
      set({ error: e instanceof Error ? e.message : "Failed to load skill detail" });
    }
  },

  fetchExecutionHistory: async (agentId, skillName, page = 1) => {
    try {
      const res = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/skills/${encodeURIComponent(skillName)}/history?page=${page}&size=50`,
      );
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: SkillExecutionHistoryResponse = await res.json();
      set({ executionHistory: data });
    } catch (e) {
      console.error("Failed to fetch skill history:", e);
    }
  },

  importSkill: async (agentId, file) => {
    try {
      const formData = new FormData();
      formData.append("package", file);

      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/skills/import`, {
        method: "POST",
        body: formData,
      });
      const data = await res.json();
      if (!res.ok) {
        return { success: false, message: data.message || `HTTP ${res.status}` };
      }
      // Refresh skill list after successful import
      await get().fetchSkills(agentId);
      return { success: true, skillName: data.skill_name, message: data.message };
    } catch (e) {
      return { success: false, message: e instanceof Error ? e.message : "Unknown error" };
    }
  },

  clearSkills: () =>
    set({
      skills: [],
      total: 0,
      selectedSkillName: null,
      selectedSkillDetail: null,
      executionHistory: null,
      activeSkill: null,
      error: null,
    }),

  deselectSkill: () =>
    set({
      selectedSkillName: null,
      selectedSkillDetail: null,
      executionHistory: null,
    }),

  setActiveSkill: (skill) => set({ activeSkill: skill }),
  clearActiveSkill: () => set({ activeSkill: null }),
}));
