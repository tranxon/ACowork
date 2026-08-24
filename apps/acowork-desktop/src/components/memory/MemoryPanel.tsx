import { useEffect, useMemo, useRef } from "react";
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
import { Dropdown } from "../common/Dropdown";
import { subTypeOptions } from "./nodeTypeI18n";

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

  // Sub-filter dropdown is only meaningful for Knowledge and
  // Autobiographical nodes — those are the labels that carry a sub_type.
  // Computing the option list here (rather than in `subTypeOptions`) keeps
  // the i18n t() binding reactive when the user switches locales.
  const subTypeChoices = useMemo(
    () => subTypeOptions(t, filters.type),
    [t, filters.type],
  );
  const subFilterVisible = filters.type === "Knowledge" || filters.type === "Autobiographical";

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
          <Dropdown
            className="min-w-0 flex-1"
            value={filters.type}
            onChange={(v) => {
              const nextType = v as
                | "All"
                | "Knowledge"
                | "Episodic"
                | "Procedural"
                | "Autobiographical";
              // When the user moves off a label that supports sub_type, the
              // previous sub-filter becomes meaningless. Clearing it here
              // keeps the URL state honest and avoids sending a stale
              // `sub_type=` param on subsequent fetches.
              setFilters({
                type: nextType,
                subType:
                  nextType === "Knowledge" || nextType === "Autobiographical"
                    ? filters.subType
                    : "",
              });
            }}
            options={[
              { value: "All", label: t("memoryPanel.allTypes") },
              { value: "Knowledge", label: t("memoryPanel.typeKnowledge") },
              { value: "Episodic", label: t("memoryPanel.typeEpisodic") },
              { value: "Procedural", label: t("memoryPanel.typeProcedural") },
              { value: "Autobiographical", label: t("memoryPanel.typeAutobiographical") },
            ]}
          />
          <Dropdown
            className="min-w-0 flex-1"
            value={filters.timeRange}
            onChange={(v) =>
              setFilters({
                timeRange: v as "1h" | "1d" | "7d" | "30d" | "all",
              })
            }
            options={[
              { value: "all", label: t("memoryPanel.allTime") },
              { value: "1h", label: t("memoryPanel.lastHour") },
              { value: "1d", label: t("memoryPanel.lastDay") },
              { value: "7d", label: t("memoryPanel.last7Days") },
              { value: "30d", label: t("memoryPanel.last30Days") },
            ]}
          />
        </div>
        {subFilterVisible && subTypeChoices.length > 0 && (
          <Dropdown
            className="w-full"
            value={filters.subType}
            onChange={(v) => setFilters({ subType: v })}
            aria-label={t("memoryPanel.subTypeAriaLabel")}
            data-testid="memory-sub-type-filter"
            options={[
              { value: "", label: t("memoryPanel.allSubTypes") },
              ...subTypeChoices.map((opt) => ({ value: opt.value, label: opt.label })),
            ]}
          />
        )}
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
