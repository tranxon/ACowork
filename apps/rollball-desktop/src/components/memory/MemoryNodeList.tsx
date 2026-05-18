import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Loader2, ChevronLeft, ChevronRight } from "lucide-react";

interface MemoryNodeListProps {
  nodes: MemoryNodeResponse[];
  total: number;
  page: number;
  pageSize: number;
  totalPages: number;
  loading: boolean;
  selectedNodeId: number | null;
  onSelectNode: (id: number | null) => void;
  onPageChange: (page: number) => void;
}

const typeColors: Record<string, { bg: string; text: string; darkBg: string; darkText: string }> = {
  Knowledge: {
    bg: "bg-blue-100",
    text: "text-blue-800",
    darkBg: "dark:bg-blue-900",
    darkText: "dark:text-blue-200",
  },
  Episodic: {
    bg: "bg-green-100",
    text: "text-green-800",
    darkBg: "dark:bg-green-900",
    darkText: "dark:text-green-200",
  },
  Procedural: {
    bg: "bg-purple-100",
    text: "text-purple-800",
    darkBg: "dark:bg-purple-900",
    darkText: "dark:text-purple-200",
  },
  Autobiographical: {
    bg: "bg-orange-100",
    text: "text-orange-800",
    darkBg: "dark:bg-orange-900",
    darkText: "dark:text-orange-200",
  },
};

function getTypeColor(nodeType: string) {
  return (
    typeColors[nodeType] ?? {
      bg: "bg-zinc-100",
      text: "text-zinc-800",
      darkBg: "dark:bg-zinc-800",
      darkText: "dark:text-zinc-200",
    }
  );
}

function getDecayColor(score: number): string {
  if (score <= 0.3) return "bg-green-500";
  if (score <= 0.7) return "bg-amber-500";
  return "bg-red-500";
}

function formatDate(ts: number): string {
  if (ts === 0) return "—";
  const d = new Date(ts * 1000);
  return d.toLocaleString();
}

function truncateContent(content: string, maxLen = 80): string {
  if (content.length <= maxLen) return content;
  return content.slice(0, maxLen) + "…";
}

export function MemoryNodeList({
  nodes,
  total,
  page,
  pageSize,
  totalPages,
  loading,
  selectedNodeId,
  onSelectNode,
  onPageChange,
}: MemoryNodeListProps) {
  const start = (page - 1) * pageSize + 1;
  const end = Math.min(page * pageSize, total);

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      {/* List header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-1.5 text-[11px] text-zinc-500 dark:border-zinc-800 dark:text-zinc-400">
        <span>
          {total > 0 ? (
            <>Showing {start}–{end} of {total}</>
          ) : (
            <>No nodes</>
          )}
        </span>
      </div>

      {/* Node list */}
      <div className="flex-1 overflow-y-auto">
        {loading && nodes.length === 0 && (
          <div className="flex h-full items-center justify-center">
            <Loader2 className="h-6 w-6 animate-spin text-zinc-400 dark:text-zinc-500" />
          </div>
        )}

        {!loading && nodes.length === 0 && (
          <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
            No memory data available
          </div>
        )}

        <div className="divide-y divide-zinc-100 dark:divide-zinc-800">
          {nodes.map((node) => {
            const colors = getTypeColor(node.node_type);
            const isSelected = node.node_id === selectedNodeId;

            return (
              <button
                key={node.node_id}
                onClick={() => onSelectNode(node.node_id)}
                className={cn(
                  "flex w-full flex-col gap-1 px-3 py-2 text-left transition-colors",
                  isSelected
                    ? "bg-zinc-100 dark:bg-zinc-800"
                    : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50",
                )}
              >
                {/* Top row: type + status */}
                <div className="flex items-center gap-2">
                  <span
                    className={cn(
                      "rounded px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wider",
                      colors.bg,
                      colors.text,
                      colors.darkBg,
                      colors.darkText,
                    )}
                  >
                    {node.node_type}
                  </span>
                  <span
                    className={cn(
                      "text-[10px] font-medium",
                      node.status === "active"
                        ? "text-green-600 dark:text-green-400"
                        : "text-zinc-400 dark:text-zinc-500",
                    )}
                  >
                    {node.status}
                  </span>
                </div>

                {/* Content summary */}
                <p className="text-xs text-zinc-700 dark:text-zinc-300">
                  {truncateContent(node.content)}
                </p>

                {/* Bottom row: confidence + decay + date */}
                <div className="flex items-center gap-2 text-[11px] text-zinc-400 dark:text-zinc-500">
                  <span>Confidence: {(node.confidence * 100).toFixed(0)}%</span>

                  {/* Decay score bar */}
                  <div className="flex items-center gap-1">
                    <span>Decay:</span>
                    <div className="h-1 w-12 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                      <div
                        className={cn("h-full rounded-full", getDecayColor(node.decay_score))}
                        style={{ width: `${node.decay_score * 100}%` }}
                      />
                    </div>
                    <span>{node.decay_score.toFixed(2)}</span>
                  </div>

                  <span className="ml-auto">{formatDate(node.created_at)}</span>
                </div>
              </button>
            );
          })}
        </div>
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between border-t border-zinc-200 px-3 py-1.5 dark:border-zinc-800">
          <button
            onClick={() => onPageChange(page - 1)}
            disabled={page <= 1}
            className="inline-flex items-center rounded p-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
          </button>
          <span className="text-[11px] text-zinc-500 dark:text-zinc-400">
            Page {page} of {totalPages}
          </span>
          <button
            onClick={() => onPageChange(page + 1)}
            disabled={page >= totalPages}
            className="inline-flex items-center rounded p-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronRight className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  );
}
