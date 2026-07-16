import React, { useState, useRef, useEffect, useMemo, useDeferredValue, Children, isValidElement } from "react";
import { ChevronRight, ChevronDown, Atom } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "./CodeBlock";
import { useAgentStore } from "../../stores/agentStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useTranslation } from "../../i18n/useTranslation";

/** ReactMarkdown component overrides — code blocks with title bar */
const thinkMarkdownComponents = {
  pre: ({ children }: { children?: React.ReactNode }) => {
    const childArray = Children.toArray(children);
    const codeEl = childArray.find(
      (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
        isValidElement(child) && child.type === "code"
    );
    if (codeEl) {
      const { className, children: codeContent } = codeEl.props;
      const language = className?.replace(/^language-/, "") || "";
      const code = Children.toArray(codeContent).join("");
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
  /** Intercept link clicks: open URLs in a file block tab instead of navigating the webview. */
  a: ({ href, children, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement>) => {
    const handleClick = (e: React.MouseEvent) => {
      if (!href) return;
      if (!/^https?:\/\//i.test(href)) return;
      e.preventDefault();
      const agentId = useAgentStore.getState().selectedAgentId;
      if (agentId) {
        useFileEditorStore.getState().openUrl(agentId, href);
      }
    };
    return (
      <a href={href} onClick={handleClick} {...rest}>
        {children}
      </a>
    );
  },
};

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
/** Font size: 90% of app font size, matches ExploreBlock items */
const THINK_HEADER_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
const THINK_DURATION_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

/**
 * ADR-035 / D4: during thinking the active buffer already holds only the
 * last 5 lines (see chatStore `stream_delta` → `activeStream` truncation,
 * D9.1). So `content` passed here is ALWAYS at most 5 lines. We render it
 * directly inside a fixed-height, non-scrolling container — React reuses
 * the same DOM nodes and only overwrites their text on each 500ms tick
 * (no append, no scroll). This keeps markdown re-parse cheap and avoids
 * the memory churn of an ever-growing auto-scrolling log.
 */

/**
 * Collapsible think block with timer and auto-expand/collapse.
 *
 * - "Thinking" phase (streaming, no endTime): auto-expanded so the user
 *   can watch the thinking process in real time.
 * - "Thought" phase (completed): auto-collapsed to reduce visual clutter.
 * - User can manually toggle at any time; manual collapse during thinking
 *   is respected until the next thinking phase.
 * - Content is capped at 5 visible lines with auto-scroll to bottom,
 *   so only the latest output is visible during long thinking phases.
 */
export const ThinkBlock = React.memo(function ThinkBlock({ content, isStreaming, startTime, endTime, defaultExpanded }: ThinkBlockProps) {
  const { t } = useTranslation();
  const isThinking = !!(isStreaming && endTime == null);
  const [expanded, setExpanded] = useState(defaultExpanded ?? isThinking);
  const contentRef = useRef<HTMLDivElement>(null);
  const manuallyCollapsed = useRef(false);

  // Auto-expand when thinking starts (respect user manual collapse)
  useEffect(() => {
    if (isThinking && !manuallyCollapsed.current) {
      setExpanded(true);
    }
  }, [isThinking]);

  // Auto-collapse when thinking completes (transition from Thinking → Thought)
  useEffect(() => {
    if (!isThinking) {
      setExpanded(false);
      manuallyCollapsed.current = false;
    }
  }, [isThinking]);

  // Calculate duration: use fixed endTime if available, otherwise live timer
  const duration = startTime
    ? Math.round(((endTime ?? Date.now()) - startTime) / 1000)
    : null;

  // ADR-035 D4: content already holds at most the last 5 lines (D9.1), so we
  // always render it directly with a "latest 5 lines" notice. No tail/scroll.
  const { display: renderContent, truncated } = useMemo(
    () => ({ display: content, truncated: true }),
    [content],
  );

  // Defer the ReactMarkdown input so rapid poll-cycle updates (every ~500ms)
  // don't pile up parse tasks.  React 18's useDeferredValue lets the browser
  // skip intermediate values when under pressure, keeping the UI responsive.
  const deferredContent = useDeferredValue(renderContent);

  return (
    <div className="my-1">
      <button
        onClick={() => {
          const next = !expanded;
          setExpanded(next);
          // Track manual collapse during thinking; reset on manual expand
          if (!next && isThinking) {
            manuallyCollapsed.current = true;
          } else if (next) {
            manuallyCollapsed.current = false;
          }
        }}
        className="flex items-center gap-2 text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300 transition-colors"
        style={{ fontSize: THINK_HEADER_FONT_SIZE }}
      >
        <Atom className="h-3 w-3 shrink-0" />
        <span>{(!isStreaming || endTime != null) ? t("thinkBlock.thought") : t("thinkBlock.thinking")}</span>
        {duration !== null && <span style={{ fontSize: THINK_DURATION_FONT_SIZE }}>({duration}s)</span>}
        {expanded ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
      </button>

      {expanded && (
        <div
          ref={contentRef}
          className="w-full ml-5 mt-1 pl-3 py-2 bg-zinc-50 dark:bg-zinc-800/50 text-zinc-500 dark:text-zinc-400 border-l-2 border-zinc-300 dark:border-zinc-600 overflow-hidden"
          style={{ maxHeight: `${MAX_VISIBLE_LINES * LINE_HEIGHT_REM}rem` }}
        >
          <div className="prose prose-sm prose-zinc max-w-none [&_*]:!text-zinc-500 dark:[&_*]:!text-zinc-400 [&_table]:bg-zinc-200/20 [&_tbody_tr]:!bg-transparent dark:[&_table]:bg-zinc-900/30" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
            {truncated && (
              <div className="text-xs text-zinc-400 dark:text-zinc-500 italic mb-1 select-none">
                … ({t("thinkBlock.showingLatest")})
              </div>
            )}
            <ReactMarkdown remarkPlugins={[remarkGfm]} components={thinkMarkdownComponents}>{deferredContent.trim() || "..."}</ReactMarkdown>
          </div>
        </div>
      )}
    </div>
  );
}, (prev, next) => {
  // Only re-render when observable output changes, not on every poll tick.
  // startTime is excluded — it's set once at creation and never changes.
  return prev.content === next.content
    && prev.isStreaming === next.isStreaming
    && prev.endTime === next.endTime
    && prev.defaultExpanded === next.defaultExpanded;
});
