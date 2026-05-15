import type { SkillDetailResponse } from "../../lib/types";
import { Loader2, ArrowLeft } from "lucide-react";

interface SkillDetailProps {
  detail: SkillDetailResponse | null;
  loading: boolean;
  onBack?: () => void;
}

export function SkillDetail({ detail, loading, onBack }: SkillDetailProps) {
  if (loading) {
    return (
      <div className="flex h-full flex-col overflow-hidden">
        {onBack && (
          <div className="flex items-center gap-3 border-b border-zinc-200 px-4 py-3 dark:border-zinc-800">
            <button
              onClick={onBack}
              className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
              aria-label="Back to list"
            >
              <ArrowLeft className="h-4 w-4" />
              Back to List
            </button>
          </div>
        )}
        <div className="flex flex-1 items-center justify-center">
          <Loader2 className="h-6 w-6 animate-spin text-zinc-400 dark:text-zinc-500" />
        </div>
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="flex h-full flex-col overflow-hidden">
        {onBack && (
          <div className="flex items-center gap-3 border-b border-zinc-200 px-4 py-3 dark:border-zinc-800">
            <button
              onClick={onBack}
              className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
              aria-label="Back to list"
            >
              <ArrowLeft className="h-4 w-4" />
              Back to List
            </button>
          </div>
        )}
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <p className="text-sm text-zinc-400 dark:text-zinc-500">
              Select a skill to view details
            </p>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Back header */}
      {onBack && (
        <div className="flex items-center gap-3 border-b border-zinc-200 px-4 py-3 dark:border-zinc-800">
          <button
            onClick={onBack}
            className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
            aria-label="Back to list"
          >
            <ArrowLeft className="h-4 w-4" />
            Back to List
          </button>
        </div>
      )}
      <div className="flex-1 overflow-y-auto p-6">
      {/* Basic info card */}
      <div className="rounded-lg border border-zinc-200 p-4 dark:border-zinc-700">
        <h2 className="text-lg font-semibold text-zinc-900 dark:text-zinc-100">
          {detail.name}
        </h2>
        <p className="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
          {detail.description}
        </p>
        <div className="mt-3 flex flex-wrap gap-2 text-xs text-zinc-500 dark:text-zinc-400">
          {detail.version && (
            <span className="rounded bg-zinc-100 px-2 py-0.5 dark:bg-zinc-800">
              Version: {detail.version}
            </span>
          )}
          {detail.author && (
            <span className="rounded bg-zinc-100 px-2 py-0.5 dark:bg-zinc-800">
              Author: {detail.author}
            </span>
          )}
        </div>
      </div>

      {/* Triggers */}
      {detail.triggers.length > 0 && (
        <div className="mt-5">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
            Triggers
          </h3>
          <div className="mt-2 flex flex-wrap gap-2">
            {detail.triggers.map((t) => (
              <span
                key={t}
                className="rounded-full bg-accent-green/10 px-2.5 py-0.5 text-xs font-medium text-accent-green"
              >
                {t}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* Tool dependencies */}
      {detail.tool_deps.length > 0 && (
        <div className="mt-5">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
            Tool Dependencies
          </h3>
          <div className="mt-2 flex flex-wrap gap-2">
            {detail.tool_deps.map((tool) => (
              <span
                key={tool}
                className="rounded-full bg-zinc-100 px-2.5 py-0.5 text-xs font-medium text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300"
              >
                {tool}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* Instructions */}
      <div className="mt-5">
        <h3 className="text-xs font-semibold uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
          Instructions
        </h3>
        <div className="mt-2 rounded-lg border border-zinc-200 bg-zinc-50 p-4 dark:border-zinc-700 dark:bg-zinc-800/50">
          <pre className="whitespace-pre-wrap text-sm text-zinc-700 dark:text-zinc-300">
            {detail.instructions}
          </pre>
        </div>
      </div>

      {/* Execution stats */}
      <div className="mt-5">
        <h3 className="text-xs font-semibold uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
          Execution Stats
        </h3>
        <p className="mt-2 text-sm text-zinc-400 dark:text-zinc-500">
          No execution history available
        </p>
      </div>
    </div>
    </div>
  );
}
