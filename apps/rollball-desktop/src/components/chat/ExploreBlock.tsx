import { useState, useRef, useEffect } from "react";
import { ChevronRight, ChevronDown, Atom, Wrench, Terminal, Check, X } from "lucide-react";
import type { ChatMessage } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";

interface ExploreBlockProps {
  items: ChatMessage[];
  isStreaming: boolean;
}

const SHELL_TOOLS = ["bash", "powershell", "shell"];

/** Font size for ExploreBlock content: 90% of app font size */
const EXPLORE_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
/** Font size for detail panels (params/result): 80% of app font size */
const EXPLORE_DETAIL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

function isShellTool(name: string): boolean {
  return SHELL_TOOLS.includes(name);
}

/**
 * ExploreBlock: aggregates consecutive think + tool_call + tool_result
 * messages into a single collapsible block with full rendering inside.
 *
 * - Default: expanded when streaming, collapsed for history.
 * - Collapsed: "Exploring... (N steps)" + chevron.
 * - Expanded: max-height 240px container with ThinkBlock and ToolCallItem.
 * - Streaming: auto-scrolls to bottom.
 */
export function ExploreBlock({ items, isStreaming }: ExploreBlockProps) {
  const [expanded, setExpanded] = useState(isStreaming);
  const contentRef = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom when streaming and expanded
  useEffect(() => {
    if (expanded && isStreaming && contentRef.current) {
      contentRef.current.scrollTop = contentRef.current.scrollHeight;
    }
  }, [expanded, isStreaming, items]);

  // Keep expanded state synced with streaming
  useEffect(() => {
    if (isStreaming) setExpanded(true);
  }, [isStreaming]);

  const stepCount = buildPairedItems(items).length;

  return (
    <div className="my-1 max-w-[var(--content-max-width)]">
      {/* Header: clickable toggle */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex w-fit items-center gap-2 rounded-lg bg-zinc-50 px-3 py-2 text-zinc-500 transition-colors hover:bg-zinc-100 dark:bg-zinc-800/30 dark:text-zinc-400 dark:hover:bg-zinc-800/50"
        style={{ fontSize: EXPLORE_FONT_SIZE }}
      >
        <Atom className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
        <span className="font-medium text-zinc-400 dark:text-zinc-500">
          {isStreaming ? "Exploring..." : "Explored"}
        </span>
        <span className="text-zinc-400 dark:text-zinc-500">
          ({stepCount} {stepCount === 1 ? "step" : "steps"})
        </span>
        {expanded ? (
          <ChevronDown className="ml-auto h-3.5 w-3.5 shrink-0 text-zinc-400" />
        ) : (
          <ChevronRight className="ml-auto h-3.5 w-3.5 shrink-0 text-zinc-400" />
        )}
      </button>

      {/* Expanded content: full ThinkBlock + paired ToolCall rendering */}
      {expanded && (
        <div
          ref={contentRef}
          className="ml-2 mt-1 overflow-y-auto rounded-lg border-l-2 border-zinc-300 bg-zinc-50 pl-3 pr-2 py-2 dark:border-zinc-600 dark:bg-zinc-800/30"
          style={{ maxHeight: "240px" }}
        >
          <div className="flex flex-col gap-2">
            {buildPairedItems(items).map((paired, idx) => (
              <PairedExploreItem key={idx} item={paired} isStreaming={isStreaming} />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/** Pair tool_call with its corresponding tool_result by toolName */
type PairedItem =
  | { kind: "thought"; msg: ChatMessage }
  | { kind: "tool"; call: ChatMessage; result?: ChatMessage }
  | { kind: "other"; msg: ChatMessage };

function buildPairedItems(items: ChatMessage[]): PairedItem[] {
  const paired: PairedItem[] = [];
  // Collect all tool_results indexed by toolName for matching
  const resultsByName = new Map<string, ChatMessage[]>();
  for (const msg of items) {
    if (msg.type === "tool_result" && msg.toolName) {
      const list = resultsByName.get(msg.toolName) || [];
      list.push(msg);
      resultsByName.set(msg.toolName, list);
    }
  }

  // Track which results have been consumed
  const consumedResults = new Set<string>();

  for (const msg of items) {
    if (msg.type === "thought") {
      paired.push({ kind: "thought", msg });
    } else if (msg.type === "tool_call") {
      // Find matching result by toolName (consume in order)
      const candidates = resultsByName.get(msg.toolName ?? "") || [];
      const result = candidates.find((r) => !consumedResults.has(r.id));
      if (result) {
        consumedResults.add(result.id);
      }
      paired.push({ kind: "tool", call: msg, result });
    } else if (msg.type === "tool_result") {
      // Skip if already consumed by a tool_call pairing
      if (consumedResults.has(msg.id)) continue;
      // Orphan result — show standalone
      paired.push({ kind: "tool", call: msg });
    } else {
      paired.push({ kind: "other", msg });
    }
  }
  return paired;
}

/** Render a paired item */
function PairedExploreItem({ item, isStreaming }: { item: PairedItem; isStreaming: boolean }) {
  if (item.kind === "thought") {
    return (
      <ThinkBlock
        content={item.msg.content}
        isStreaming={isStreaming && !item.msg.endTime}
        hasReplyStarted={false}
        startTime={item.msg.startTime}
        endTime={item.msg.endTime}
        defaultExpanded={false}
      />
    );
  }

  if (item.kind === "tool") {
    return <ToolCallItem call={item.call} result={item.result} />;
  }

  // Fallback
  return (
    <div className="text-zinc-500 dark:text-zinc-400" style={{ fontSize: EXPLORE_FONT_SIZE }}>
      {item.msg.content.slice(0, 120)}
    </div>
  );
}

/** Tool call + result paired display: icon + tool name + status indicator + expandable details */
function ToolCallItem({ call, result }: { call: ChatMessage; result?: ChatMessage }) {
  const [showDetails, setShowDetails] = useState(false);
  const toolName = call.toolName ?? "tool";
  const isShell = isShellTool(toolName);
  const Icon = isShell ? Terminal : Wrench;

  // Determine status from result
  const isSuccess = result?.toolStatus === "success";
  const isError = result?.toolStatus === "error";
  const isPending = !result;

  let summary = "";
  try {
    const params = JSON.parse(call.content || "{}");
    if (isShell) {
      summary = (params.command as string) || "";
    } else if (params.path) {
      summary = params.path as string;
    } else {
      const entries = Object.entries(params);
      if (entries.length > 0) {
        summary = `${entries[0][0]}: ${String(entries[0][1]).slice(0, 60)}`;
      }
    }
  } catch {
    summary = call.content.slice(0, 60);
  }

  return (
    <div className="min-w-0">
      <button
        onClick={() => setShowDetails(!showDetails)}
        className="flex min-w-0 w-full items-center gap-2 rounded-md bg-zinc-100 px-2.5 py-1.5 text-left transition-colors hover:bg-zinc-200 dark:bg-zinc-700/50 dark:hover:bg-zinc-700"
        style={{ fontSize: EXPLORE_FONT_SIZE }}
      >
        <Icon className="h-3.5 w-3.5 shrink-0 text-zinc-500" />
        <span className="shrink-0 font-medium text-zinc-700 dark:text-zinc-300">{toolName}</span>
        {summary && (
          <span className="min-w-0 flex-1 truncate text-zinc-500 dark:text-zinc-400">
            {summary}
          </span>
        )}
        {/* Status indicator */}
        {isSuccess ? (
          <Check className="h-3 w-3 shrink-0 text-emerald-500" />
        ) : isError ? (
          <X className="h-3 w-3 shrink-0 text-red-500" />
        ) : isPending ? (
          <span className="h-3 w-3 shrink-0 animate-pulse rounded-full bg-zinc-300 dark:bg-zinc-500" />
        ) : null}
        {showDetails ? (
          <ChevronDown className="h-3 w-3 shrink-0 text-zinc-400" />
        ) : (
          <ChevronRight className="h-3 w-3 shrink-0 text-zinc-400" />
        )}
      </button>
      {showDetails && (
        <div className="mt-1 ml-5 space-y-1">
          {/* Call params */}
          <pre className="overflow-x-auto rounded bg-zinc-100 p-2 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400" style={{ fontSize: EXPLORE_DETAIL_FONT_SIZE }}>
            {call.content}
          </pre>
          {/* Result */}
          {result && (
            <pre className={`overflow-x-auto rounded p-2 ${isError ? "bg-red-50 text-red-600 dark:bg-red-900/20 dark:text-red-400" : "bg-emerald-50 text-zinc-600 dark:bg-emerald-900/10 dark:text-zinc-400"}`} style={{ fontSize: EXPLORE_DETAIL_FONT_SIZE }}>
              {result.content.length > 500 ? result.content.slice(0, 500) + "\n..." : result.content}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}