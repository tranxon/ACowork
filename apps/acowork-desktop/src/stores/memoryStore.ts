import { create } from "zustand";
import type {
  MemoryNodeResponse,
  MemoryNodesListResponse,
  MemoryStatsResponse,
  DeleteNodeResponse,
  ConsolidationStatusResponse,
  DistillResponse,
  DistillerStatus,
  ForgettingStatus,
} from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import {
  fetchEmbeddingModels,
  startMigration,
} from "../lib/gateway-api";
import { useGatewayStore } from "./gatewayStore";
import { log } from "../lib/logger";
import { with503Retry } from "../lib/httpRetry";

interface MemoryFilters {
  type: "All" | "Knowledge" | "Episodic" | "Procedural" | "Autobiographical";
  /**
   * Sub-classification filter (only meaningful when `type` is `Knowledge`,
   * `Autobiographical`, or `Episodic`).
   *
   * Knowledge:    `Fact` | `Preference` | `Relation` | `Procedure`
   * Autobiographical: `Identity` | `Capability` | `Limitation`
   *                | `Preference` | `History` | `Relationship`
   * Episodic:    `Fact` | `Preference` | `Relation` | `Procedure`
   *              — read from the `knowledge_subtype` distillation routing
   *              tag (ADR-068 §3.4.2). Same enum as Knowledge because an
   *              episode tagged with `knowledge_subtype=X` is the
   *              distiller's input for promoting a `X` node into the
   *              semantic layer.
   *
   * `""` = no filter. Ignored by the backend when `type` is `Procedural`
   * (that label has no sub-classification).
   */
  subType: string;
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
  consolidateMessage: string | null;

  /**
   * Runtime distiller trigger state (ADR-071) — `GET /memory/consolidation/
   * status` `distiller` payload. `null` until the first successful fetch.
   * Drives the "记忆蒸馏" card (enabled switch reflects the runtime state;
   * the card also shows backlog / last-run summary).
   */
  distillerStatus: DistillerStatus | null;

  /**
   * Runtime episodic forgetting state (ADR-057 §5.3 redesign) —
   * `GET /memory/consolidation/status` `forgetting` payload. `null` until
   * the first successful fetch. Drives the "记忆遗忘" card (enabled switch
   * reflects the runtime state).
   */
  forgettingStatus: ForgettingStatus | null;

  /**
   * Set while a "Rebuild Index" migration is in flight for the currently
   * selected agent. Driven by the same harness /api/embedding-models/{id}/
   * start-migration endpoint the Harness tab already uses — we just call it
   * from the memory panel when stored_dim ≠ model_dim.
   *
   * `rebuildPollingRef` (closure-scoped, not in state) holds the interval
   * handle so we can clear it on completion or on agent switch.
   */
  migrationInProgress: boolean;

  // Actions
  fetchNodes: (agentId: string) => Promise<void>;
  fetchStats: (agentId: string) => Promise<void>;
  deleteNode: (agentId: string, nodeId: number) => Promise<void>;
  /**
   * Trigger one manual EpisodicDistiller pass — `POST /memory/distill`
   * (ADR-071 D2). Replaces the retired "合并节点" (legacy `consolidate`)
   * button in the memory panel.
   *
   * Returns the `DistillResponse` on a completed run, `null` when the
   * run could not start (distiller disabled / providers missing / HTTP
   * error). On success the node list, stats and distiller status are
   * refreshed so the panel reflects promoted nodes immediately.
   */
  distill: (agentId: string) => Promise<DistillResponse | null>;
  /**
   * Fetch the runtime distiller trigger state — `GET /memory/
   * consolidation/status`. Best-effort: failures leave the previous
   * status in place (the card falls back to agent_config-driven state).
   */
  fetchDistillerStatus: (agentId: string) => Promise<void>;
  /**
   * Fetch the runtime episodic forgetting state — `GET /memory/
   * consolidation/status` `forgetting` payload (ADR-057 §5.3 redesign).
   * Best-effort: failures leave the previous status in place.
   */
  fetchForgettingStatus: (agentId: string) => Promise<void>;
  /**
   * Rebuild the Grafeo HNSW vector index for `agentId` using the currently
   * active embedding model. Re-embeds every node so that mismatched-dim stores
   * (or stores that pre-date the embedding provider) become searchable again.
   *
   * Internally calls `startMigration(activeModelId, [agentId])` on the same
   * endpoint the Harness tab uses, then polls `pollMigrationProgress()` every
   * 2s until the agent reports `done` or an error.
   */
  rebuildIndex: (agentId: string) => Promise<void>;
  setFilters: (partial: Partial<MemoryFilters>) => void;
  setPage: (page: number) => void;
  setSelectedNodeId: (id: number | null) => void;
  clearMemory: () => void;
}

// Polling-interval handle for the in-flight Rebuild Index action. Kept outside
// the store so clearing memory / switching agents cancels any active poll
// without needing to thread the handle through state.
let rebuildPollingTimer: ReturnType<typeof setInterval> | null = null;

export const useMemoryStore = create<MemoryStore>((set, get) => ({
  nodes: [],
  total: 0,
  stats: null,
  selectedNodeId: null,
  filters: { type: "All", subType: "", keyword: "", timeRange: "all" },
  page: 1,
  pageSize: 20,
  loading: false,
  error: null,
  consolidateMessage: null,
  distillerStatus: null,
  forgettingStatus: null,
  migrationInProgress: false,

  fetchNodes: async (agentId) => {
    const { page, pageSize, filters } = get();
    set({ loading: true, error: null });
    try {
      const params = new URLSearchParams({
        page: String(page),
        size: String(pageSize),
      });
      if (filters.type !== "All") params.set("type", filters.type);
      if (filters.subType) params.set("sub_type", filters.subType);
      if (filters.keyword) params.set("keyword", filters.keyword);
      if (filters.timeRange !== "all") params.set("time_range", filters.timeRange);

      // Bug B v3 fix: memory endpoints proxy through the Runtime and
      // 503 during the boot window between Gateway discovery and
      // Runtime HTTP port registration. `with503Retry` rides out
      // transient 503s transparently so the MemoryPanel does not
      // have to gate on a UI-side `isAgentReady` flag (see
      // MemoryPanel.tsx — `isAgentReady` was removed in v3).
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes?${params}`),
        { tag: `MemoryStore.fetchNodes(${agentId})`, logger: log },
      );
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: MemoryNodesListResponse = await res.json();
      set({ nodes: data.nodes, total: data.total, loading: false });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Unknown error" });
    }
  },

  fetchStats: async (agentId) => {
    try {
      // Same 503 retry rationale as fetchNodes above.
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/stats`),
        { tag: `MemoryStore.fetchStats(${agentId})`, logger: log },
      );
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: MemoryStatsResponse = await res.json();
      set({ stats: data });
    } catch (e) {
      log.error("Failed to fetch memory stats:", e);
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

  distill: async (agentId) => {
    set({ loading: true, error: null, consolidateMessage: null });
    try {
      const res = await with503Retry(
        () =>
          fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/distill`, {
            method: "POST",
          }),
        { tag: `MemoryStore.distill(${agentId})`, logger: log },
      );
      if (!res.ok) {
        // 409 "distiller is disabled" / 503 "not ready" — surface the
        // runtime error string so the user knows why the run didn't go.
        const body = (await res.json().catch(() => null)) as {
          error?: string;
        } | null;
        set({
          loading: false,
          consolidateMessage: body?.error ?? `Distill failed (HTTP ${res.status})`,
        });
        return null;
      }
      const data: DistillResponse = await res.json();
      // Refresh everything the run may have changed: node list (promoted
      // nodes), stats (episode → node counts) and the distiller status
      // (last_run / secs_since_distill reset).
      await get().fetchNodes(agentId);
      await get().fetchStats(agentId);
      await get().fetchDistillerStatus(agentId);
      set({ loading: false });
      return data;
    } catch (e) {
      set({
        loading: false,
        error: e instanceof Error ? e.message : "Distill failed",
      });
      return null;
    }
  },

  fetchDistillerStatus: async (agentId) => {
    try {
      const res = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
          ),
        { tag: `MemoryStore.fetchDistillerStatus(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data: ConsolidationStatusResponse = await res.json();
      set({ distillerStatus: data.distiller });
    } catch {
      // Best-effort — the card still works from agent_config alone.
    }
  },

  fetchForgettingStatus: async (agentId) => {
    try {
      const res = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
          ),
        { tag: `MemoryStore.fetchForgettingStatus(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data: ConsolidationStatusResponse = await res.json();
      set({ forgettingStatus: data.forgetting });
    } catch {
      // Best-effort — the card still works from agent_config alone.
    }
  },

  rebuildIndex: async (agentId: string) => {
    // Guard: cancel any in-flight poll before starting a new rebuild.
    if (rebuildPollingTimer) {
      clearInterval(rebuildPollingTimer);
      rebuildPollingTimer = null;
    }
    set({ migrationInProgress: true, error: null });
    try {
      // Resolve the currently active embedding model — start-migration ignores
      // this id for vector dim/endpoint (it uses gw.embed_process directly) but
      // still requires a non-empty model_id to satisfy the route.
      const models = await fetchEmbeddingModels();
      const activeModelId = models.active_model_id;
      if (!activeModelId) {
        throw new Error(
          "No active embedding model is configured. Configure one in the Harness tab first.",
        );
      }
      const resp = await startMigration(activeModelId, [agentId]);
      // Backend `start_migration` (gateway/src/http/embedding_api.rs) returns
      // `{"status":"ok", ...}` on success and `{"status":"error", ...}` on
      // failure. The previous strict whitelist ("migration_started" / "loaded")
      // did not match the live contract, so the catch branch always fired and
      // the progress setInterval never started, leaving the panel stuck.
      if (resp.status === "error") {
        throw new Error(resp.message || `Migration start failed: ${resp.status}`);
      }

      // Poll progress every 2s. We piggy-back on gatewayStore.pollMigrationProgress
      // (the same helper the Harness tab uses) so the progress map stays in sync
      // with the rest of the app.
      const gateway = useGatewayStore.getState();
      const finish = async () => {
        if (rebuildPollingTimer) {
          clearInterval(rebuildPollingTimer);
          rebuildPollingTimer = null;
        }
        set({ migrationInProgress: false });
        try {
          await get().fetchStats(agentId);
        } catch {
          // Best-effort refresh — the MigrationProgress array still tells the
          // user whether it succeeded; a fetch failure here shouldn't blank
          // the "in progress" flag back on.
        }
        const final = useGatewayStore.getState().migrationProgress[agentId];
        if (final?.error) {
          set({ error: `索引重建失败: ${final.error}` });
        }
      };
      rebuildPollingTimer = setInterval(async () => {
        const stillInProgress = await gateway.pollMigrationProgress();
        if (!stillInProgress) {
          await finish();
        }
      }, 2000);
    } catch (e) {
      if (rebuildPollingTimer) {
        clearInterval(rebuildPollingTimer);
        rebuildPollingTimer = null;
      }
      set({
        migrationInProgress: false,
        error: e instanceof Error ? e.message : "Rebuild index failed",
      });
    }
  },

  setFilters: (partial) => {
    set((s) => ({ filters: { ...s.filters, ...partial }, page: 1 }));
  },

  setPage: (page) => set({ page }),

  setSelectedNodeId: (id) => set({ selectedNodeId: id }),

  clearMemory: () => {
    if (rebuildPollingTimer) {
      clearInterval(rebuildPollingTimer);
      rebuildPollingTimer = null;
    }
    return set({
      nodes: [],
      total: 0,
      stats: null,
      selectedNodeId: null,
      page: 1,
      error: null,
      consolidateMessage: null,
      distillerStatus: null,
      forgettingStatus: null,
      migrationInProgress: false,
    });
  },
}));
