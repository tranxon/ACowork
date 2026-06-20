import { useState } from "react";
import { ChevronRight, ChevronDown, FileText } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { CompactionEventMeta } from "../../lib/types";

interface CompactionCardProps {
  /** Summary text (already stripped of `<summary>` tags by the store). */
  summary: string;
  /** Structured metadata from the compaction event. */
  meta?: CompactionEventMeta;
  /** ISO timestamp of the compaction event (already converted to ms upstream). */
  timestampMs: number;
}

/** Font size matches ExploreBlock so the visual "weight" of folded blocks is consistent. */
const CARD_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
const DETAIL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

/**
 * Format a Date into a short locale time string (HH:MM).
 * Used for the optional secondary timestamp display.
 */
function formatTime(ms: number): string {
  try {
    return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  } catch {
    return "";
  }
}

/**
 * CompactionCard: renders an LLM-driven context compaction event as a
 * folded summary card. Visually mirrors {@link ExploreBlock} so both
 * "folded metadata" blocks share the same language:
 *
 * - Outer wrapper is fully transparent — no border, no background.
 * - Header is a compact pill (`bg-zinc-50` / `hover:bg-zinc-100`,
 *   `bg-zinc-800/30` / `hover:bg-zinc-800/50` dark), `w-fit` so it hugs
 *   its text — no full-width row, no chevron pushed to the far right.
 * - Expanded content uses the same `border-l-2 + bg-zinc-50` recipe as
 *   ExploreBlock's expanded tool trace, so the visual rhythm of the two
 *   cards is identical.
 * - Collapsed header (left → right):
 *     [icon]  title  · token stat(s)  [chevron]
 *
 * The component is intentionally read-only — there is no "undo compaction"
 * affordance. The summary represents the LLM's actual memory, not a
 * draft.
 */
export function CompactionCard({ summary, meta, timestampMs }: CompactionCardProps) {
  const [expanded, setExpanded] = useState(false);

  const before = meta?.before_tokens ?? 0;
  const after = meta?.after_tokens ?? 0;
  const hasTokenStats = before > 0 || after > 0;
  const beforeStr = before >= 1000 ? `${(before / 1000).toFixed(1)}k` : `${before}`;
  const afterStr = after >= 1000 ? `${(after / 1000).toFixed(1)}k` : `${after}`;

  return (
    <div className="my-1 max-w-[var(--content-max-width)]">
      {/* Header pill — same recipe as ExploreBlock header. */}
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="flex w-fit items-center gap-2 rounded-md bg-zinc-50 px-2.5 py-1.5 text-zinc-500 transition-colors hover:bg-zinc-100 dark:bg-zinc-800/30 dark:text-zinc-400 dark:hover:bg-zinc-800/50"
        style={{ fontSize: CARD_FONT_SIZE }}
      >
        <FileText className="h-3.5 w-3.5 shrink-0 text-zinc-500" />
        <span className="shrink-0 font-medium">上下文摘要</span>
        {hasTokenStats && (
          <span className="shrink-0 text-zinc-500 dark:text-zinc-400">
            · {beforeStr} → {afterStr} tokens
          </span>
        )}
        {expanded ? (
          <ChevronDown className="h-3.5 w-3.5 shrink-0 text-zinc-500" />
        ) : (
          <ChevronRight className="h-3.5 w-3.5 shrink-0 text-zinc-500" />
        )}
      </button>

      {/* Expanded content — same recipe as ExploreBlock expanded tool trace:
          left accent border + soft zinc background, capped height + scroll
          so a long summary cannot blow up the chat layout. */}
      {expanded && (
        <div
          className="ml-2 mt-1 overflow-y-auto rounded-md border-l-2 border-zinc-300 bg-zinc-50 pl-3 pr-2 py-2 dark:border-zinc-600 dark:bg-zinc-800/30"
          style={{ maxHeight: "240px" }}
        >
          <div
            className="prose prose-sm max-w-none text-zinc-700 dark:prose-invert dark:text-zinc-300 select-text"
            style={{ fontSize: CARD_FONT_SIZE }}
          >
            {summary ? (
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{summary}</ReactMarkdown>
            ) : (
              <span className="italic text-zinc-400">（空摘要）</span>
            )}
          </div>
          <div
            className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-zinc-500 dark:text-zinc-400 select-text"
            style={{ fontSize: DETAIL_FONT_SIZE }}
          >
            {meta?.model && <span>model: {meta.model}</span>}
            {meta?.keep_last_rounds != null && meta.keep_last_rounds > 0 && (
              <span>keep last rounds: {meta.keep_last_rounds}</span>
            )}
            <span>{formatTime(timestampMs)}</span>
            {hasTokenStats && (
              <span>
                tokens: {before} → {after}
              </span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}