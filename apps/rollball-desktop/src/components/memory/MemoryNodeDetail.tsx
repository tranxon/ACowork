import type { MemoryNodeResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ArrowLeft, Trash2 } from "lucide-react";

interface MemoryNodeDetailProps {
  node: MemoryNodeResponse;
  onClose: () => void;
  onDelete: (nodeId: number) => void;
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

function getDecayLabel(score: number): string {
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
  const colors = getTypeColor(node.node_type);

  const handleDelete = () => {
    if (confirm(`Delete node #${node.node_id}? This action cannot be undone.`)) {
      onDelete(node.node_id);
      onClose();
    }
  };

  return (
    <div className="flex flex-1 flex-col overflow-hidden bg-white dark:bg-zinc-900">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <button
          onClick={onClose}
          className="inline-flex items-center gap-1 rounded p-0.5 text-[11px] text-zinc-500 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
          aria-label="Back to list"
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
              colors.darkBg,
              colors.darkText,
            )}
          >
            {node.node_type}
          </span>
          <span className="text-[11px] text-zinc-400 dark:text-zinc-500">#{node.node_id}</span>
        </div>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-3">
        {/* Full content */}
        <div className="mb-3">
          <h3 className="mb-1 text-[11px] font-medium text-zinc-500 dark:text-zinc-400">Content</h3>
          <p className="whitespace-pre-wrap text-xs text-zinc-800 dark:text-zinc-200">{node.content}</p>
        </div>

        {/* Metadata grid */}
        <div className="mb-3 grid grid-cols-2 gap-2">
          <MetaItem label="Status" value={node.status} />
          <MetaItem label="Confidence" value={`${(node.confidence * 100).toFixed(1)}%`} />
          <MetaItem label="Access Count" value={String(node.access_count)} />
          <MetaItem label="Created" value={formatDate(node.created_at)} />
          <MetaItem label="Last Accessed" value={formatDate(node.last_accessed_at)} />
        </div>

        {/* Decay score visualization */}
        <div className="mb-3">
          <div className="mb-1 flex items-center justify-between">
            <h3 className="text-[11px] font-medium text-zinc-500 dark:text-zinc-400">Decay Score</h3>
            <span
              className={cn(
                "text-[11px] font-medium",
                node.decay_score <= 0.3
                  ? "text-green-600 dark:text-green-400"
                  : node.decay_score <= 0.7
                    ? "text-amber-600 dark:text-amber-400"
                    : "text-red-600 dark:text-red-400",
              )}
            >
              {getDecayLabel(node.decay_score)}
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
