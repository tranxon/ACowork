import type { SkillDetailResponse } from "../../lib/types";
import { Loader2 } from "lucide-react";


interface SkillDetailProps {
  detail: SkillDetailResponse | null;
  loading: boolean;
}

export function SkillDetail({ detail, loading }: SkillDetailProps) {
  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="h-6 w-6 animate-spin text-zinc-400 dark:text-zinc-500" />
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="text-center">
          <p className="text-sm text-zinc-400 dark:text-zinc-500">
            Select a skill to view details
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-y-auto p-6">
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
                className="rounded-full bg-blue-50 px-2.5 py-0.5 text-xs font-medium text-blue-700 dark:bg-blue-900/30 dark:text-blue-300"
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
  );
}
