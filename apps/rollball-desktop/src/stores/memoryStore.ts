import { create } from "zustand";
import type {
  MemoryNodeResponse,
  MemoryNodesListResponse,
  MemoryStatsResponse,
  DeleteNodeResponse,
} from "../lib/types";
import { getGatewayUrl } from "../lib/config";

interface MemoryFilters {
  type: "All" | "Knowledge" | "Episodic" | "Procedural" | "Autobiographical";
  keyword: string;
  timeRange: "1h" | "1d" | "7d" | "30d" | "all";
}

interface MemoryStore {
  nodes: MemoryNodeResponse[];
  total: number;
  stats: MemoryStatsResponse | null;
  selectedNodeId: number | null;

  filters: MemoryFilters;
  page: number;
  pageSize: number;

  loading: boolean;
  error: string | null;

  // Actions
  fetchNodes: (agentId: string) => Promise<void>;
  fetchStats: (agentId: string) => Promise<void>;
  deleteNode: (agentId: string, nodeId: number) => Promise<void>;
  consolidate: (agentId: string, force?: boolean) => Promise<void>;
  setFilters: (partial: Partial<MemoryFilters>) => void;
  setPage: (page: number) => void;
  setSelectedNodeId: (id: number | null) => void;
  clearMemory: () => void;
}

export const useMemoryStore = create<MemoryStore>((set, get) => ({
  nodes: [],
  total: 0,
  stats: null,
  selectedNodeId: null,
  filters: { type: "All", keyword: "", timeRange: "all" },
  page: 1,
  pageSize: 20,
  loading: false,
  error: null,

  fetchNodes: async (agentId) => {
    const { page, pageSize, filters } = get();
    set({ loading: true, error: null });
    try {
      const params = new URLSearchParams({
        page: String(page),
        size: String(pageSize),
      });
      if (filters.type !== "All") params.set("type", filters.type);
      if (filters.keyword) params.set("keyword", filters.keyword);
      if (filters.timeRange !== "all") params.set("time_range", filters.timeRange);

      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes?${params}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: MemoryNodesListResponse = await res.json();
      set({ nodes: data.nodes, total: data.total, loading: false });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Unknown error" });
    }
  },

  fetchStats: async (agentId) => {
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/stats`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: MemoryStatsResponse = await res.json();
      set({ stats: data });
    } catch (e) {
      console.error("Failed to fetch memory stats:", e);
    }
  },

  deleteNode: async (agentId, nodeId) => {
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes/${nodeId}`, {
        method: "DELETE",
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: DeleteNodeResponse = await res.json();
      if (data.deleted) {
        set((s) => ({
          nodes: s.nodes.filter((n) => n.node_id !== nodeId),
          total: s.total - 1,
          selectedNodeId: s.selectedNodeId === nodeId ? null : s.selectedNodeId,
        }));
        // Refresh stats
        get().fetchStats(agentId);
      }
    } catch (e) {
      set({ error: e instanceof Error ? e.message : "Delete failed" });
    }
  },

  consolidate: async (agentId, force = false) => {
    set({ loading: true, error: null });
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/consolidate`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ force }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      // Refresh after consolidation
      await get().fetchNodes(agentId);
      await get().fetchStats(agentId);
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Consolidation failed" });
    }
  },

  setFilters: (partial) => {
    set((s) => ({ filters: { ...s.filters, ...partial }, page: 1 }));
  },

  setPage: (page) => set({ page }),

  setSelectedNodeId: (id) => set({ selectedNodeId: id }),

  clearMemory: () =>
    set({
      nodes: [],
      total: 0,
      stats: null,
      selectedNodeId: null,
      page: 1,
      error: null,
    }),
}));
