import { useEffect, useRef } from "react";
import { useMemoryStore } from "../../stores/memoryStore";
import { useAgentStore } from "../../stores/agentStore";
import { MemoryNodeList } from "./MemoryNodeList";
import { MemoryNodeDetail } from "./MemoryNodeDetail";
import { AlertTriangle, Info } from "lucide-react";

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
    consolidateMessage,
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

  const totalPages = Math.max(1, Math.ceil(total / pageSize));

  // ── Empty state: no agent selected ──
  if (!selectedAgentId) {
    return (
      <div className="flex flex-1 items-center justify-center p-6 text-xs text-zinc-400 dark:text-zinc-500">
        Select an agent to view memory
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      {/* Toolbar */}
      <div className="flex gap-3 border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <button
          onClick={handleConsolidate}
          disabled={loading}
          className="flex-1 rounded-lg btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          Consolidate
        </button>
        <button
          onClick={handleRefresh}
          disabled={loading}
          className="flex-1 rounded-lg btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          Refresh
        </button>
      </div>

      {/* Filters */}
      <div className="flex flex-wrap items-center gap-2 border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <input
          type="text"
          value={filters.keyword}
          onChange={(e) => setFilters({ keyword: e.target.value })}
          placeholder="Search nodes..."
          className="min-w-[160px] flex-1 rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:focus:border-zinc-500"
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
          className="appearance-none rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          style={{
            backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
            backgroundPosition: 'right 0.5rem center',
            backgroundRepeat: 'no-repeat',
            backgroundSize: '1.5em 1.5em',
          }}
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
          className="appearance-none rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          style={{
            backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
            backgroundPosition: 'right 0.5rem center',
            backgroundRepeat: 'no-repeat',
            backgroundSize: '1.5em 1.5em',
          }}
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
        <div className="grid grid-cols-2 gap-2 border-b border-zinc-200 px-3 py-2 sm:grid-cols-4 dark:border-zinc-800">
          <StatCard label="Total Nodes" value={stats.total_nodes} />
          <StatCard label="Active" value={stats.by_status["Active"] ?? 0} />
          <StatCard label="Dormant" value={stats.by_status["Dormant"] ?? 0} />
          <StatCard
            label="Health"
            value={stats.index_health}
          />
        </div>
      )}

      {/* Error banner */}
      {error && (
        <div className="flex items-center gap-1.5 border-b border-red-200 bg-red-50 px-3 py-1.5 dark:border-red-900 dark:bg-red-950">
          <AlertTriangle className="h-3 w-3 text-red-600 dark:text-red-400" />
          <span className="text-[11px] text-red-700 dark:text-red-300">{error}</span>
        </div>
      )}

      {/* Consolidate feedback banner */}
      {consolidateMessage && (
        <div className="flex items-center gap-1.5 border-b border-blue-200 bg-blue-50 px-3 py-1.5 dark:border-blue-900 dark:bg-blue-950">
          <Info className="h-3 w-3 shrink-0 text-blue-600 dark:text-blue-400" />
          <span className="text-[11px] text-blue-700 dark:text-blue-300">{consolidateMessage}</span>
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
    <div className="rounded border border-zinc-200 p-2 dark:border-zinc-700">
      <p className="text-[10px] text-zinc-500 dark:text-zinc-400">{label}</p>
      <p className="mt-0.5 text-xs font-semibold text-zinc-700 dark:text-zinc-200">{value}</p>
    </div>
  );
}
