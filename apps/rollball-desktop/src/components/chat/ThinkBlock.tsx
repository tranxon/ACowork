import { useState, useRef, useEffect } from "react";
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

/** Max visible lines in the think content area (overflow scrolls to bottom) */
const MAX_VISIBLE_LINES = 5;
const LINE_HEIGHT_REM = 1.5; // text-sm line-height

/**
 * Simple collapsible think block with timer.
 * Shows "Thinking (Xs)" header, click to expand/collapse content.
 * Content is capped at 5 visible lines with auto-scroll to bottom,
 * so only the latest output is visible during long thinking phases.
 */
export function ThinkBlock({ content, isStreaming: _isStreaming, startTime, endTime, defaultExpanded }: ThinkBlockProps) {
  const [expanded, setExpanded] = useState(defaultExpanded ?? false);
  const contentRef = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom when content updates
  useEffect(() => {
    if (expanded && contentRef.current) {
      contentRef.current.scrollTop = contentRef.current.scrollHeight;
    }
  }, [content, expanded]);

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
        <div
          ref={contentRef}
          className="ml-5 mt-1 pl-3 py-2 bg-zinc-50 dark:bg-zinc-800/50 text-sm text-zinc-600 dark:text-zinc-400 border-l-2 border-zinc-300 dark:border-zinc-600 overflow-y-auto"
          style={{ maxHeight: `${MAX_VISIBLE_LINES * LINE_HEIGHT_REM}rem` }}
        >
          <div className="prose prose-sm prose-zinc max-w-none dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{content.trim() || "..."}</ReactMarkdown>
          </div>
        </div>
      )}
    </div>
  );
}
