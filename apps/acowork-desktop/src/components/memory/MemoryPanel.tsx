import { useEffect, useRef } from "react";
import { useMemoryStore } from "../../stores/memoryStore";
import { useAgentStore } from "../../stores/agentStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { MemoryNodeList } from "./MemoryNodeList";
import { MemoryNodeDetail } from "./MemoryNodeDetail";
import { AlertTriangle, Info } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { ErrorBox } from "../common/ErrorBox";

export function MemoryPanel() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  // Gate data fetching on agent readiness — memory endpoints proxy through
  // the Runtime and 503 against an unregistered one.  Stopped agents are a
  // legitimate UI state; the user picks the Start button to bring them up,
  // and once `running && ready` flips to true this selector re-runs the
  // load effects below.
  const isAgentReady = useAgentStore((s) =>
    selectedAgentId ? !!(s.agents[selectedAgentId]?.meta.running && s.agents[selectedAgentId]?.meta.ready) : false
  );
  const {
    nodes,
    total,
    stats,
    selectedNodeId,
    filters,
    page,
    pageSize,
    loading,
    error,
    consolidateMessage,
    migrationInProgress,
    fetchNodes,
    fetchStats,
    consolidate,
    rebuildIndex,
    setFilters,
    setPage,
    setSelectedNodeId,
    clearMemory,
  } = useMemoryStore();

  // Live migration progress for the currently selected agent, used to drive
  // the "重建中…" button label and a tiny progress fraction in the banner.
  const agentMigration = useGatewayStore((s) =>
    selectedAgentId ? s.migrationProgress[selectedAgentId] : undefined,
  );

  const selectedNode = nodes.find((n) => n.node_id === selectedNodeId) ?? null;

  // Load data when agent changes (or transitions from stopped → running).
  useEffect(() => {
    if (!selectedAgentId || !isAgentReady) return;
    clearMemory();
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
  }, [selectedAgentId, isAgentReady, clearMemory, fetchNodes, fetchStats]);

  // Re-fetch when filters or pagination change
  useEffect(() => {
    if (!selectedAgentId || !isAgentReady) return;
    void fetchNodes(selectedAgentId);
  }, [filters, page, pageSize, selectedAgentId, isAgentReady, fetchNodes]);

  // Re-fetch when the memory tab becomes visible (e.g. agent was started
  // while another tab was active, so data was never loaded for the running agent)
  const activePanelTab = useLayoutStore((s) => s.activePanelTab);
  useEffect(() => {
    if (!selectedAgentId || !isAgentReady) return;
    if (activePanelTab !== "memory") return;
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
  }, [activePanelTab, selectedAgentId, isAgentReady, fetchNodes, fetchStats]);

  // Auto-dismiss consolidate message after 6 seconds
  const dismissTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!consolidateMessage) return;
    if (dismissTimer.current) clearTimeout(dismissTimer.current);
    dismissTimer.current = setTimeout(() => {
      useMemoryStore.setState({ consolidateMessage: null });
    }, 6000);
    return () => {
      if (dismissTimer.current) clearTimeout(dismissTimer.current);
    };
  }, [consolidateMessage]);

  const handleConsolidate = () => {
    if (!selectedAgentId) return;
    void consolidate(selectedAgentId);
  };

  const handleRefresh = () => {
    if (!selectedAgentId) return;
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
  };

  const handleRebuildIndex = () => {
    if (!selectedAgentId) return;
    void rebuildIndex(selectedAgentId);
  };

  // True when the persisted HNSW index dimension disagrees with the active
  // embedding model's output dimension. `stored_dim == 0` means the index
  // hasn't been built yet (fresh store) — not a mismatch, just an empty state.
  // `model_dim == 0` means no provider is configured — also not actionable.
  const dimMismatch =
    !!stats &&
    stats.stored_dim > 0 &&
    stats.model_dim > 0 &&
    stats.stored_dim !== stats.model_dim;

  // True when some memory nodes are missing vector embeddings (NULL or failed
  // write). This is common after an embedding model change where existing nodes
  // were stored with a different dimension and their embeddings were rejected
  // by the HNSW index — they exist as metadata-only nodes.
  const missingEmbeddings =
    !!stats &&
    stats.model_dim > 0 &&
    stats.total_nodes > 0 &&
    stats.nodes_with_embedding < stats.total_nodes;

  // While migration is in flight, show the rebuilt/total fraction so the
  // user can see progress. Falls back to plain "重建中…" if the Gateway has
  // not yet produced a progress payload.
  const migrationProgressLabel = (() => {
    if (!migrationInProgress) return t("memoryPanel.rebuildIndex");
    const p = agentMigration?.progress;
    if (p && p.total_scanned > 0) {
      return `${t("memoryPanel.rebuildIndexInProgress")} ${p.rebuilt}/${p.total_scanned}`;
    }
    return t("memoryPanel.rebuildIndexInProgress");
  })();

  const totalPages = Math.max(1, Math.ceil(total / pageSize));

  // ── Empty state: no agent selected ──
  if (!selectedAgentId) {
    return (
      <div className="flex flex-1 items-center justify-center p-6 text-xs text-zinc-400 dark:text-zinc-500">
        {t("memoryPanel.selectAgent")}
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      {/* Filters */}
      <div className="flex flex-col gap-2 border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <StyledInput
          type="text"
          value={filters.keyword}
          onChange={(e) => setFilters({ keyword: e.target.value })}
          placeholder={t("memoryPanel.searchNodes")}
          className="rounded-md bg-modal-surface px-2.5 py-1.5"
        />
        <div className="flex gap-2">
          <select
            value={filters.type}
            onChange={(e) =>
              setFilters({
                type: e.target.value as
                  | "All"
                  | "Knowledge"
                  | "Episodic"
                  | "Procedural"
                  | "Autobiographical",
              })
            }
            className="min-w-0 flex-1 appearance-none rounded-md border border-zinc-200 bg-modal-surface py-1.5 pl-2.5 pr-7 text-xs outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-700 dark:text-zinc-200"
            style={{
              backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
              backgroundPosition: 'right 0.5rem center',
              backgroundRepeat: 'no-repeat',
              backgroundSize: '1.5em 1.5em',
            }}
          >
            <option value="All">{t("memoryPanel.allTypes")}</option>
            <option value="Knowledge">{t("memoryPanel.typeKnowledge")}</option>
            <option value="Episodic">{t("memoryPanel.typeEpisodic")}</option>
            <option value="Procedural">{t("memoryPanel.typeProcedural")}</option>
            <option value="Autobiographical">{t("memoryPanel.typeAutobiographical")}</option>
          </select>
          <select
            value={filters.timeRange}
            onChange={(e) =>
              setFilters({
                timeRange: e.target.value as "1h" | "1d" | "7d" | "30d" | "all",
              })
            }
            className="min-w-0 flex-1 appearance-none rounded-md border border-zinc-200 bg-modal-surface py-1.5 pl-2.5 pr-7 text-xs outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-700 dark:text-zinc-200"
            style={{
              backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
              backgroundPosition: 'right 0.5rem center',
              backgroundRepeat: 'no-repeat',
              backgroundSize: '1.5em 1.5em',
            }}
          >
            <option value="all">{t("memoryPanel.allTime")}</option>
            <option value="1h">{t("memoryPanel.lastHour")}</option>
            <option value="1d">{t("memoryPanel.lastDay")}</option>
            <option value="7d">{t("memoryPanel.last7Days")}</option>
            <option value="30d">{t("memoryPanel.last30Days")}</option>
          </select>
        </div>
      </div>

      {/* Stats cards */}
      {stats && (
        <div className="grid grid-cols-2 gap-2 border-b border-zinc-200 px-3 py-2 sm:grid-cols-4 dark:border-zinc-800">
          <StatCard label={t("memoryPanel.totalNodes")} value={stats.total_nodes} />
          {/* Optional chain on by_status defends against any future wire-format
              drift on the stats endpoint — the panel must render zeros rather
              than crash the entire panel tree if a contract field is missing
              (see MemoryStatsResponse in acowork-gateway). */}
          <StatCard label={t("memoryPanel.active")} value={stats.by_status?.["Active"] ?? 0} />
          <StatCard label={t("memoryPanel.dormant")} value={stats.by_status?.["Dormant"] ?? 0} />
          <StatCard
            label={t("memoryPanel.health")}
            value={stats.index_health}
          />
        </div>
      )}

      {/* Index-health banner — shown when:
          1. Dim-mismatch: persisted HNSW index dim differs from active model dim.
          2. Missing embeddings: some nodes lack vector embeddings (nodes_with_embedding < total_nodes).
          Clicking the button triggers the same /api/embedding-models/{id}/start-migration
          flow that the Harness tab already uses. */}
      {(dimMismatch || missingEmbeddings) && stats && (
        <div
          className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-3 py-2 text-amber-900 dark:border-amber-900/60 dark:bg-amber-950/40 dark:text-amber-100"
          role="alert"
          data-testid="index-health-banner"
        >
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <div className="min-w-0 flex-1">
            <p className="truncate text-[11px] font-semibold">
              {dimMismatch
                ? t("memoryPanel.dimMismatchTitle")
                : t("memoryPanel.missingEmbeddingsTitle")}
            </p>
            <p className="truncate text-[10px] opacity-80">
              {dimMismatch
                ? t("memoryPanel.dimMismatchDetail", {
                    stored: stats.stored_dim,
                    model: stats.model_dim,
                  })
                : t("memoryPanel.missingEmbeddingsDetail", {
                    indexed: stats.nodes_with_embedding,
                    total: stats.total_nodes,
                  })}
              {dimMismatch && stats.total_nodes > 0 && (
                <>
                  {" · "}
                  {t("memoryPanel.dimMismatchIndexedDetail", {
                    indexed: stats.nodes_with_embedding,
                    total: stats.total_nodes,
                  })}
                </>
              )}
            </p>
          </div>
          <button
            onClick={handleRebuildIndex}
            disabled={migrationInProgress}
            data-testid="rebuild-index-button"
            className="shrink-0 rounded btn-solid px-2.5 py-1 text-[11px] font-medium disabled:opacity-50"
          >
            {migrationProgressLabel}
          </button>
        </div>
      )}

      {/* Error banner */}
      {error && (
        <div className="border-b border-red-200 dark:border-red-900">
          <ErrorBox message={error} className="!rounded-none !border-0" />
        </div>
      )}

      {/* Consolidate feedback banner */}
      {consolidateMessage && (
        <div className="flex items-center gap-1.5 border-b border-[var(--color-accent)]/30 bg-[var(--color-accent)]/10 px-3 py-1.5">
          <Info className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
          <span className="text-[11px] text-[var(--color-accent)]">{consolidateMessage}</span>
        </div>
      )}

      {/* Main content: master-detail toggle */}
      <div className="flex min-h-0 flex-1 overflow-hidden">
        {!selectedNode ? (
          <MemoryNodeList
            nodes={nodes}
            total={total}
            page={page}
            pageSize={pageSize}
            totalPages={totalPages}
            loading={loading}
            selectedNodeId={selectedNodeId}
            onSelectNode={setSelectedNodeId}
            onPageChange={setPage}
          />
        ) : (
          <MemoryNodeDetail
            node={selectedNode}
            onClose={() => setSelectedNodeId(null)}
            onDelete={(nodeId) => {
              if (!selectedAgentId) return;
              void useMemoryStore.getState().deleteNode(selectedAgentId, nodeId);
            }}
          />
        )}
      </div>

      {/* Bottom actions */}
      <div className="flex gap-3 border-t border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <button
          onClick={handleConsolidate}
          disabled={loading}
          className="flex-1 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {t("memoryPanel.consolidate")}
        </button>
        <button
          onClick={handleRefresh}
          disabled={loading}
          className="flex-1 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {t("memoryPanel.refresh")}
        </button>
      </div>
    </div>
  );
}

function StatCard({
  label,
  value,
}: {
  label: string;
  value: string | number;
}) {
  return (
    <div className="min-w-0 overflow-hidden rounded border border-zinc-200 p-2 dark:border-zinc-700">
      <p className="truncate text-[10px] text-zinc-500 dark:text-zinc-400" title={label}>{label}</p>
      <p
        className="mt-0.5 truncate text-xs font-semibold text-zinc-700 dark:text-zinc-200"
        title={String(value)}
      >
        {value}
      </p>
    </div>
  );
}
