import { useState } from "react";
import { ChevronRight, ChevronDown, Clock } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

export interface ThinkBlockProps {
  content: string;
  isStreaming?: boolean;
  hasReplyStarted?: boolean;
  startTime?: number;
  /** Fixed end time (set by done event); if absent, duration keeps ticking in streaming mode */
  endTime?: number;
  /** Whether to default to expanded state (e.g. when this is the last message) */
  defaultExpanded?: boolean;
}

/**
 * Simple collapsible think block with timer.
 * Shows "Thinking (Xs)" header, click to expand/collapse content.
 * When endTime is provided (from done event), duration is frozen;
 * otherwise it keeps counting during streaming.
 */
export function ThinkBlock({ content, isStreaming: _isStreaming, startTime, endTime, defaultExpanded }: ThinkBlockProps) {
  const [expanded, setExpanded] = useState(defaultExpanded ?? false);

  // Calculate duration: use fixed endTime if available, otherwise live timer
  const duration = startTime
    ? Math.round(((endTime ?? Date.now()) - startTime) / 1000)
    : null;

  return (
    <div className="my-1">
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex items-center gap-2 text-xs text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300 transition-colors"
      >
        {expanded ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
        <Clock className="h-3 w-3" />
        <span>Thinking</span>
        {duration !== null && <span className="text-[10px]">({duration}s)</span>}
      </button>

      {expanded && (
        <div className="ml-5 mt-1 rounded bg-zinc-50 dark:bg-zinc-800/50 p-3 text-sm text-zinc-600 dark:text-zinc-400 border border-zinc-200 dark:border-zinc-700">
          <div className="prose prose-sm prose-zinc max-w-none dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{content.trim() || "..."}</ReactMarkdown>
          </div>
        </div>
      )}
    </div>
  );
}
