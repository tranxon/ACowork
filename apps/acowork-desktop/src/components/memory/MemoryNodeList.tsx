import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Loader2, ChevronLeft, ChevronRight } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useNodeTypeLabel, useSubTypeLabel } from "./nodeTypeI18n";

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

const accentBg = "bg-[var(--color-accent)]/10 dark:bg-[var(--color-accent)]/20";
const accentText = "text-[var(--color-accent)]";

function getTypeColor(_nodeType: string) {
  return { bg: accentBg, text: accentText, darkBg: "", darkText: "" };
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
  const labelOf = useNodeTypeLabel();
  const subLabelOf = useSubTypeLabel();
  const { t } = useTranslation();

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
                    )}
                    data-node-type={node.node_type}
                  >
                    {labelOf(node.node_type)}
                  </span>
                  {node.sub_type && (
                    <span
                      className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wider text-zinc-600 dark:bg-zinc-800 dark:text-zinc-300"
                      data-sub-type={node.sub_type}
                      title={node.sub_type}
                    >
                      {subLabelOf(node.node_type, node.sub_type)}
                    </span>
                  )}
                  <span
                    className={cn(
                      "text-[10px] font-medium",
                      node.status === "active"
                        ? accentText
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

                {/* Bottom row: score + decay + date.
                    Episodic nodes carry `importance` (重要程度) but no
                    `confidence`; the other three types carry `confidence`
                    (置信度) but no `importance`. Each is shown verbatim —
                    the backend never derives one from the other. */}
                <div className="flex items-center gap-2 text-[11px] text-zinc-400 dark:text-zinc-500">
                  {node.node_type === "Episodic" ? (
                    <span>
                      {t("memoryNodeDetail.labelImportance")}:{" "}
                      {(node.importance * 100).toFixed(0)}%
                    </span>
                  ) : (
                    <span>
                      {t("memoryNodeDetail.labelConfidence")}:{" "}
                      {(node.confidence * 100).toFixed(0)}%
                    </span>
                  )}

                  <span>Decay: {node.decay_score.toFixed(2)}</span>

                  <span className="ml-auto">{formatDate(node.created_at)}</span>
                </div>
              </button>
            );
          })}
        </div>
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between border-t border-zinc-200 px-1 py-1.5 dark:border-zinc-800">
          <button
            onClick={() => onPageChange(page - 1)}
            disabled={page <= 1}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
          </button>
          <span className="text-[11px] text-zinc-500 dark:text-zinc-400">
            Page {page} of {totalPages}
          </span>
          <button
            onClick={() => onPageChange(page + 1)}
            disabled={page >= totalPages}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronRight className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  );
}
