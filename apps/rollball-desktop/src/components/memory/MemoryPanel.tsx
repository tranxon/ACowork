import { useEffect } from "react";
import { useMemoryStore } from "../../stores/memoryStore";
import { useAgentStore } from "../../stores/agentStore";
import { MemoryNodeList } from "./MemoryNodeList";
import { MemoryNodeDetail } from "./MemoryNodeDetail";
import { Brain, RefreshCw, Zap, AlertTriangle } from "lucide-react";
import { cn } from "../../lib/utils";

export function MemoryPanel() {
  const { selectedAgentId } = useAgentStore();
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
    fetchNodes,
    fetchStats,
    consolidate,
    setFilters,
    setPage,
    setSelectedNodeId,
    clearMemory,
  } = useMemoryStore();

  const selectedNode = nodes.find((n) => n.node_id === selectedNodeId) ?? null;

  // Load data when agent changes
  useEffect(() => {
    if (!selectedAgentId) return;
    clearMemory();
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
  }, [selectedAgentId, clearMemory, fetchNodes, fetchStats]);

  // Re-fetch when filters or pagination change
  useEffect(() => {
    if (!selectedAgentId) return;
    void fetchNodes(selectedAgentId);
  }, [filters, page, pageSize, selectedAgentId, fetchNodes]);

  const handleConsolidate = () => {
    if (!selectedAgentId) return;
    void consolidate(selectedAgentId);
  };

  const handleRefresh = () => {
    if (!selectedAgentId) return;
    void fetchNodes(selectedAgentId);
    void fetchStats(selectedAgentId);
  };

  const totalPages = Math.max(1, Math.ceil(total / pageSize));

  // ── Empty state: no agent selected ──
  if (!selectedAgentId) {
    return (
      <div className="flex flex-1 items-center justify-center bg-white dark:bg-zinc-900">
        <div className="text-center">
          <Brain className="mx-auto h-12 w-12 text-zinc-300 dark:text-zinc-600" />
          <p className="mt-3 text-sm text-zinc-400 dark:text-zinc-500">Select an agent to view memory</p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col bg-white dark:bg-zinc-900">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-6 py-4 dark:border-zinc-800">
        <h1 className="text-xl font-semibold">Memory Management</h1>
        <div className="flex gap-2">
          <button
            onClick={handleConsolidate}
            disabled={loading}
            className="inline-flex items-center gap-1.5 rounded-md btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            <Zap className="h-3.5 w-3.5" />
            Consolidate
          </button>
          <button
            onClick={handleRefresh}
            disabled={loading}
            className="inline-flex items-center gap-1.5 rounded-md border border-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
          >
            <RefreshCw className={cn("h-3.5 w-3.5", loading && "animate-spin")} />
            Refresh
          </button>
        </div>
      </div>

      {/* Filters */}
      <div className="flex flex-wrap items-center gap-3 border-b border-zinc-200 px-6 py-3 dark:border-zinc-800">
        <input
          type="text"
          value={filters.keyword}
          onChange={(e) => setFilters({ keyword: e.target.value })}
          placeholder="Search nodes..."
          className="min-w-[200px] flex-1 rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-sm outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:focus:border-zinc-500"
        />
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
          className="rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
        >
          <option value="All">All Types</option>
          <option value="Knowledge">Knowledge</option>
          <option value="Episodic">Episodic</option>
          <option value="Procedural">Procedural</option>
          <option value="Autobiographical">Autobiographical</option>
        </select>
        <select
          value={filters.timeRange}
          onChange={(e) =>
            setFilters({
              timeRange: e.target.value as "1h" | "1d" | "7d" | "30d" | "all",
            })
          }
          className="rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
        >
          <option value="all">All Time</option>
          <option value="1h">Last Hour</option>
          <option value="1d">Last Day</option>
          <option value="7d">Last 7 Days</option>
          <option value="30d">Last 30 Days</option>
        </select>
      </div>

      {/* Stats cards */}
      {stats && (
        <div className="grid grid-cols-2 gap-3 border-b border-zinc-200 px-6 py-3 sm:grid-cols-4 dark:border-zinc-800">
          <StatCard label="Total Nodes" value={stats.total_nodes} />
          <StatCard label="Active" value={stats.by_status["Active"] ?? 0} color="green" />
          <StatCard label="Dormant" value={stats.by_status["Dormant"] ?? 0} color="amber" />
          <StatCard
            label="Health"
            value={stats.index_health}
            color={stats.index_health === "healthy" ? "green" : stats.index_health === "degraded" ? "amber" : "red"}
          />
        </div>
      )}

      {/* Error banner */}
      {error && (
        <div className="flex items-center gap-2 border-b border-red-200 bg-red-50 px-6 py-2 dark:border-red-900 dark:bg-red-950">
          <AlertTriangle className="h-4 w-4 text-red-600 dark:text-red-400" />
          <span className="text-xs text-red-700 dark:text-red-300">{error}</span>
        </div>
      )}

      {/* Main content: drawer-style master-detail toggle */}
      <div className="flex flex-1 overflow-hidden">
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
    </div>
  );
}

function StatCard({
  label,
  value,
  color,
}: {
  label: string;
  value: string | number;
  color?: "green" | "amber" | "red";
}) {
  const valueColor =
    color === "green"
      ? "text-green-700 dark:text-green-400"
      : color === "amber"
        ? "text-amber-700 dark:text-amber-400"
        : color === "red"
          ? "text-red-700 dark:text-red-400"
          : "text-zinc-900 dark:text-zinc-100";

  return (
    <div className="rounded-lg border border-zinc-200 p-3 dark:border-zinc-700">
      <p className="text-xs text-zinc-500 dark:text-zinc-400">{label}</p>
      <p className={cn("mt-1 text-lg font-semibold", valueColor)}>{value}</p>
    </div>
  );
}
