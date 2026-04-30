import { useState, useEffect } from "react";
import { ChevronRight } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

export interface ThinkBlockProps {
  /** Raw text content inside the <think> tags */
  content: string;
  /** Whether the parent message is currently being streamed */
  isStreaming: boolean;
  /** Whether the reply after </think> has already started */
  hasReplyStarted: boolean;
}

/**
 * Collapsible think-block UI for assistant reasoning output.
 *
 * - During streaming and before the actual reply starts: auto-expanded
 * - After reply starts or streaming ends: auto-collapsed
 * - User can click the header to toggle expansion at any time
 */
export function ThinkBlock({ content, isStreaming, hasReplyStarted }: ThinkBlockProps) {
  const [expanded, setExpanded] = useState(true);

  // Auto-collapse when the reply starts or streaming finishes
  useEffect(() => {
    if (hasReplyStarted || !isStreaming) {
      setExpanded(false);
    }
  }, [hasReplyStarted, isStreaming]);

  // While streaming and before reply starts, force expanded
  const showExpanded = isStreaming && !hasReplyStarted ? true : expanded;

  return (
    <div className="mb-2">
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex items-center gap-1 text-xs text-gray-500 dark:text-gray-400 hover:text-gray-700 dark:hover:text-gray-300 transition-colors"
      >
        <ChevronRight
          className={`h-3 w-3 transition-transform duration-200 ${showExpanded ? "rotate-90" : ""}`}
        />
        <span>Thinking{isStreaming && !hasReplyStarted ? "..." : ""}</span>
      </button>
      {showExpanded && (
        <div className="mt-1 rounded bg-zinc-50 dark:bg-zinc-800/50 p-3 text-sm text-zinc-600 dark:text-zinc-400 border border-zinc-200 dark:border-zinc-700">
          <div className="prose prose-sm prose-zinc max-w-none dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{content.trim() || "…"}</ReactMarkdown>
          </div>
        </div>
      )}
    </div>
  );
}
