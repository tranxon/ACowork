import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ArrowLeft, Trash2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useNodeTypeLabel } from "./nodeTypeI18n";

interface MemoryNodeDetailProps {
  node: MemoryNodeResponse;
  onClose: () => void;
  onDelete: (nodeId: number) => void;
}

const accentBg = "bg-[var(--color-accent)]/10 dark:bg-[var(--color-accent)]/20";
const accentText = "text-[var(--color-accent)]";

function getTypeColor(_nodeType: string) {
  return { bg: accentBg, text: accentText, darkBg: "", darkText: "" };
}

function getDecayColor(_score: number): string {
  return "bg-[var(--color-accent)]";
}

function getDecayTier(score: number): "Stable" | "Decaying" | "Critical" {
  if (score <= 0.3) return "Stable";
  if (score <= 0.7) return "Decaying";
  return "Critical";
}

function formatDate(ts: number): string {
  if (ts === 0) return "—";
  const d = new Date(ts * 1000);
  return d.toLocaleString();
}

export function MemoryNodeDetail({ node, onClose, onDelete }: MemoryNodeDetailProps) {
  const { t } = useTranslation();
  const labelOf = useNodeTypeLabel();
  const colors = getTypeColor(node.node_type);
  const decayLabel = (() => {
    const tier = getDecayTier(node.decay_score);
    if (tier === "Stable") return t("memoryNodeDetail.statusStable");
    if (tier === "Decaying") return t("memoryNodeDetail.statusDecaying");
    return t("memoryNodeDetail.statusCritical");
  })();

  const handleDelete = () => {
    if (confirm(`Delete node #${node.node_id}? This action cannot be undone.`)) {
      onDelete(node.node_id);
      onClose();
    }
  };

  return (
    <div className="flex flex-1 flex-col overflow-hidden bg-chat-area">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <button
          onClick={onClose}
          className="inline-flex items-center gap-1 rounded p-0.5 text-[11px] text-zinc-500 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
          aria-label={t("memoryNodeDetail.ariaLabelBackToList")}
        >
          <ArrowLeft className="h-3.5 w-3.5" />
          Back to List
        </button>
        <div className="flex items-center gap-1.5">
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
          <span className="text-[11px] text-zinc-400 dark:text-zinc-500">#{node.node_id}</span>
        </div>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-3">
        {/* Full content */}
        <div className="mb-3">
          <h3 className="mb-1 text-[11px] font-medium text-zinc-500 dark:text-zinc-400">{t("memoryNodeDetail.content")}</h3>
          <p className="whitespace-pre-wrap text-xs text-zinc-800 dark:text-zinc-200">{node.content}</p>
        </div>

        {/* Metadata grid */}
        <div className="mb-3 grid grid-cols-2 gap-2">
          <MetaItem label={t("memoryNodeDetail.labelStatus")} value={node.status} />
          <MetaItem label={t("memoryNodeDetail.labelConfidence")} value={`${(node.confidence * 100).toFixed(1)}%`} />
          <MetaItem label={t("memoryNodeDetail.labelAccessCount")} value={String(node.access_count)} />
          <MetaItem label={t("memoryNodeDetail.labelCreated")} value={formatDate(node.created_at)} />
          <MetaItem label={t("memoryNodeDetail.labelLastAccessed")} value={formatDate(node.last_accessed_at)} />
        </div>

        {/* Decay score visualization */}
        <div className="mb-3">
          <div className="mb-1 flex items-center justify-between">
            <h3 className="text-[11px] font-medium text-zinc-500 dark:text-zinc-400">Decay Score</h3>
            <span
              className={cn(
                "text-[11px] font-medium",
                accentText,
              )}
            >
              {decayLabel}
            </span>
          </div>
          <div className="h-2 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
            <div
              className={cn("h-full rounded-full transition-all", getDecayColor(node.decay_score))}
              style={{ width: `${node.decay_score * 100}%` }}
            />
          </div>
          <p className="mt-1 text-right text-[11px] text-zinc-500 dark:text-zinc-400">
            {node.decay_score.toFixed(3)}
          </p>
        </div>
      </div>

      {/* Actions footer */}
      <div className="border-t border-zinc-200 p-3 dark:border-zinc-800">
        <button
          onClick={handleDelete}
          className="inline-flex w-full items-center justify-center gap-1 rounded border border-red-200 bg-red-50 px-2 py-1.5 text-[11px] font-medium text-red-700 hover:bg-red-100 dark:border-red-900 dark:bg-red-950 dark:text-red-400 dark:hover:bg-red-900"
        >
          <Trash2 className="h-3 w-3" />
          Delete Node
        </button>
      </div>
    </div>
  );
}

function MetaItem({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <p className="text-[10px] uppercase tracking-wider text-zinc-400 dark:text-zinc-500">{label}</p>
      <p className="mt-0.5 text-[11px] text-zinc-700 dark:text-zinc-300">{value}</p>
    </div>
  );
}
