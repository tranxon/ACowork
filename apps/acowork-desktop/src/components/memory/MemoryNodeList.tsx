//! Memory node master list — level-1 collapsible card in the same
//! grammar as the Debug-panel "Context Snapshots" list: a box card whose
//! header (chevron + title + count Badge + page controls on the empty
//! right side) toggles the whole list, which sits on the level-1 inset
//! surface (`bg-panel-inset`). Clicking a row drills into the
//! MemoryNodeDetail view (master-detail; intentionally NOT an inline
//! expansion — the detail pane carries edit/delete controls).
import { useState } from "react";
import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Loader2, ChevronLeft, ChevronRight } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useNodeTypeLabel, useSubTypeLabel } from "./nodeTypeI18n";
import { ListBox, ListRow, ExpandableRow, Badge, EmptyState } from "../common/list";

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

const accentText = "text-[var(--color-accent)]";

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
  totalPages,
  loading,
  selectedNodeId,
  onSelectNode,
  onPageChange,
}: MemoryNodeListProps) {
  // Level-1 collapse of the whole list card — same interaction as the
  // Context Snapshots card. Default open.
  const [listOpen, setListOpen] = useState(true);
  const labelOf = useNodeTypeLabel();
  const subLabelOf = useSubTypeLabel();
  const { t } = useTranslation();

  // Page controls live in the empty right side of the card header.
  // Clicking them must not toggle the collapse, and flips the card open
  // when it was collapsed (mirrors the snapshot pager behaviour).
  const pager =
    totalPages > 1 ? (
      <span
        className="flex items-center gap-1"
        onClick={(e) => e.stopPropagation()}
      >
        <button
          type="button"
          aria-label="Previous memory page"
          disabled={page <= 1}
          onClick={() => {
            setListOpen(true);
            onPageChange(page - 1);
          }}
          className="rounded p-1 text-zinc-400 transition-colors hover:bg-zinc-100 hover:text-zinc-600 disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-zinc-400 dark:text-zinc-500 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 dark:disabled:hover:bg-transparent dark:disabled:hover:text-zinc-500"
        >
          <ChevronLeft className="h-3.5 w-3.5" />
        </button>
        <span className="min-w-[3ch] text-center font-mono text-[10px] tabular-nums text-zinc-400 dark:text-zinc-500">
          {page}/{totalPages}
        </span>
        <button
          type="button"
          aria-label="Next memory page"
          disabled={page >= totalPages}
          onClick={() => {
            setListOpen(true);
            onPageChange(page + 1);
          }}
          className="rounded p-1 text-zinc-400 transition-colors hover:bg-zinc-100 hover:text-zinc-600 disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-zinc-400 dark:text-zinc-500 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 dark:disabled:hover:bg-transparent dark:disabled:hover:text-zinc-500"
        >
          <ChevronRight className="h-3.5 w-3.5" />
        </button>
      </span>
    ) : undefined;

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <ListBox
        dividers={false}
        className={cn(
          "overflow-hidden",
          listOpen && "flex min-h-0 flex-1 flex-col",
        )}
      >
        <ExpandableRow
          open={listOpen}
          onToggle={() => setListOpen((v) => !v)}
          title={t("memoryPanel.memoryList", { count: total })}
          ariaLabel={t("memoryPanel.memoryList", { count: total })}
          trailing={pager}
          bodyClassName="flex min-h-0 flex-1 flex-col overflow-hidden rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
        >
          {loading && nodes.length === 0 ? (
            <div className="flex flex-1 items-center justify-center py-10">
              <Loader2 className="h-5 w-5 animate-spin text-zinc-400 dark:text-zinc-500" />
            </div>
          ) : !loading && nodes.length === 0 ? (
            <EmptyState
              message={t("memoryPanel.emptyNodes")}
              className="flex-1 items-center justify-center"
            />
          ) : (
            <div className="min-h-0 flex-1 overflow-y-auto">
              <ListBox variant="plain">
                {nodes.map((node) => {
                  const isSelected = node.node_id === selectedNodeId;

                  return (
                    <ListRow
                      key={node.node_id}
                      selected={isSelected}
                      surface="inset"
                      onClick={() => onSelectNode(node.node_id)}
                    >
                      <div className="flex flex-col gap-1">
                        {/* Top row: type + status */}
                        <div className="flex items-center gap-2">
                          <Badge tone="accent" uppercase data-node-type={node.node_type}>
                            {labelOf(node.node_type)}
                          </Badge>
                          {node.sub_type && (
                            <Badge
                              tone="neutral"
                              uppercase
                              data-sub-type={node.sub_type}
                              title={node.sub_type}
                            >
                              {subLabelOf(node.node_type, node.sub_type)}
                            </Badge>
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
                      </div>
                    </ListRow>
                  );
                })}
              </ListBox>
            </div>
          )}
        </ExpandableRow>
      </ListBox>
    </div>
  );
}
