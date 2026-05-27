import { useState, useRef, useEffect } from "react";
import { ChevronRight, ChevronDown, Search, Wrench, Terminal, Check, X } from "lucide-react";
import type { ChatMessage, ToolApprovalNeededEvent } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";

interface ExploreBlockProps {
  items: ChatMessage[];
  isStreaming: boolean;
  /** Map of tool_call_id → approval event for precise matching with tool call items. */
  pendingApproval?: Record<string, ToolApprovalNeededEvent> | null;
  currentSessionId?: string | null;
  onApprove?: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void;
  /** True when an assistant reply message follows this explore block in display order.
   *  This is the ONLY condition that triggers auto-collapse. */
  hasFollowUpReply?: boolean;
}

const SHELL_TOOLS = ["bash", "powershell", "shell"];

/** Font size for ExploreBlock content: 90% of app font size */
const EXPLORE_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
/** Font size for detail panels (params/result): 80% of app font size */
const EXPLORE_DETAIL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

function isShellTool(name: string): boolean {
  return SHELL_TOOLS.includes(name);
}

/** Check if a specific approval event belongs to the current session.
 *  If session_id is absent (old Runtime), assume it matches (backward compat). */
function approvalMatchesSession(
  approval: ToolApprovalNeededEvent,
  currentSessionId?: string | null,
): boolean {
  if (approval.session_id === undefined || approval.session_id === null) return true;
  return approval.session_id === currentSessionId;
}

/**
 * ExploreBlock: aggregates consecutive think + tool_call + tool_result
 * messages into a single collapsible block with full rendering inside.
 *
 * - Default: expanded (for new active blocks).
 * - Collapsed: "Exploring... (N steps)" + chevron.
 * - Expanded: max-height 240px container with ThinkBlock and ToolCallItem.
 * - Streaming: auto-scrolls to bottom.
 * - Collapse (auto): ONLY when hasFollowUpReply=true — an assistant reply
 *   message appears after this explore block in display order.
 * - Collapse (manual): user can collapse at any time.
 */
export function ExploreBlock({ items, isStreaming, pendingApproval, currentSessionId, onApprove, hasFollowUpReply }: ExploreBlockProps) {
  // Start collapsed only if this block already has a follow-up reply (historical/loaded).
  // For new active blocks, always start expanded — collapses ONLY when
  // an assistant reply appears after it.
  const [expanded, setExpanded] = useState(!hasFollowUpReply);
  const contentRef = useRef<HTMLDivElement>(null);
  const manuallyCollapsed = useRef(false);

  // Auto-scroll to bottom when expanded and new items arrive
  useEffect(() => {
    if (expanded && contentRef.current) {
      contentRef.current.scrollTop = contentRef.current.scrollHeight;
    }
  }, [expanded, items]);

  const pairedItems = buildPairedItems(items);
  const stepCount = pairedItems.length;

  // Still have tool_calls without results
  const hasPendingTools = pairedItems.some(
    (item) => item.kind === "tool" && !item.result
  );

  const isExploring = isStreaming || hasPendingTools;

  // Auto-expand when exploring starts (respect user manual collapse),
  // but only if no follow-up reply has appeared (once collapsed by reply, stay collapsed)
  useEffect(() => {
    if (isExploring && !hasFollowUpReply && !manuallyCollapsed.current) {
      setExpanded(true);
    }
  }, [isExploring, hasFollowUpReply]);

  // Auto-collapse when agent response appears after this explore block.
  // This is the ONLY auto-collapse condition — tools finishing alone does NOT collapse.
  useEffect(() => {
    if (hasFollowUpReply) {
      setExpanded(false);
      manuallyCollapsed.current = false;
    }
  }, [hasFollowUpReply]);

  // Check if this block has any pending shell approval for current session
  const pendingKeys = pendingApproval ? Object.keys(pendingApproval) : [];
  const itemToolCallIds = items.filter(m => m.type === "tool_call").map(m => m.toolCallId);
  const hasPendingApproval = pendingApproval && Object.values(pendingApproval).some(
    (ev) => {
      const sessionMatch = approvalMatchesSession(ev, currentSessionId);
      const toolMatch = items.some(
        (m) => m.type === "tool_call" && m.toolCallId === ev.tool_call_id && !items.some(
          (r) => r.type === "tool_result" && r.toolName === m.toolName
        )
      );
      return sessionMatch && toolMatch;
    }
  );

  // Auto-expand when pending approval — always, even if user collapsed
  useEffect(() => {
    if (hasPendingApproval) {
      setExpanded(true);
      manuallyCollapsed.current = false;
      // Auto-scroll to bottom so the approval button is visible
      setTimeout(() => {
        if (contentRef.current) {
          contentRef.current.scrollTop = contentRef.current.scrollHeight;
        }
      }, 0);
    }
  }, [hasPendingApproval]);

  return (
    <div className="my-1 max-w-[var(--content-max-width)]">
      {/* Header: clickable toggle */}
      <button
        onClick={() => {
          const next = !expanded;
          setExpanded(next);
          // Track manual collapse during exploring; reset on manual expand
          if (!next && isExploring) {
            manuallyCollapsed.current = true;
          } else if (next) {
            manuallyCollapsed.current = false;
          }
        }}
        className="flex w-fit items-center gap-2 rounded-lg bg-zinc-50 px-2.5 py-1.5 text-zinc-500 transition-colors hover:bg-zinc-100 dark:bg-zinc-800/30 dark:text-zinc-400 dark:hover:bg-zinc-800/50"
        style={{ fontSize: EXPLORE_FONT_SIZE }}
      >
        <Search className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
        <span className="font-medium text-zinc-400 dark:text-zinc-500">
          {hasFollowUpReply ? "Explored" : "Exploring..."}
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
          <div className="flex flex-col gap-0.5">
            {pairedItems.map((paired, idx) => (
              <PairedExploreItem key={idx} item={paired} isStreaming={isStreaming} pendingApproval={pendingApproval} currentSessionId={currentSessionId} onApprove={onApprove} />
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
function PairedExploreItem({ item, isStreaming, pendingApproval, currentSessionId, onApprove }: { item: PairedItem; isStreaming: boolean; pendingApproval?: Record<string, ToolApprovalNeededEvent> | null; currentSessionId?: string | null; onApprove?: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void }) {
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
    return <ToolCallItem call={item.call} result={item.result} pendingApproval={pendingApproval} currentSessionId={currentSessionId} onApprove={onApprove} />;
  }

  // Fallback
  return (
    <div className="text-zinc-500 dark:text-zinc-400" style={{ fontSize: EXPLORE_FONT_SIZE }}>
      {item.msg.content.slice(0, 120)}
    </div>
  );
}

/** Tool call + result paired display: icon + tool name + status indicator + expandable details */
function ToolCallItem({ call, result, pendingApproval, currentSessionId, onApprove }: { call: ChatMessage; result?: ChatMessage; pendingApproval?: Record<string, ToolApprovalNeededEvent> | null; currentSessionId?: string | null; onApprove?: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void }) {
  const [showDetails, setShowDetails] = useState(false);
  const toolName = call.toolName ?? "tool";
  const isShell = isShellTool(toolName);
  const Icon = isShell ? Terminal : Wrench;

  // Determine status from result
  const isSuccess = result?.toolStatus === "success";
  const isError = result?.toolStatus === "error";
  const isPendingResult = !result;

  // Check if this specific tool_call has a pending approval for the current session
  const specificApproval = pendingApproval && call.toolCallId ? pendingApproval[call.toolCallId] : undefined;
  const needsApproval = specificApproval
    ? approvalMatchesSession(specificApproval, currentSessionId) && isPendingResult
    : false;

  // Countdown timer for approval timeout
  const [remainingSecs, setRemainingSecs] = useState<number | null>(null);
  useEffect(() => {
    if (!needsApproval || !specificApproval?.approval_timeout_secs) {
      setRemainingSecs(null);
      return;
    }
    const total = specificApproval.approval_timeout_secs;
    setRemainingSecs(total);
    const interval = setInterval(() => {
      setRemainingSecs((prev) => {
        if (prev === null || prev <= 1) {
          clearInterval(interval);
          return 0;
        }
        return prev - 1;
      });
    }, 1000);
    return () => clearInterval(interval);
  }, [needsApproval, specificApproval?.approval_timeout_secs]);

  // Hide approval when countdown reaches 0 (Runtime auto-rejects)
  const showApproval = needsApproval && remainingSecs !== 0;
  const countdownLabel = remainingSecs !== null && remainingSecs > 0
    ? `${Math.floor(remainingSecs / 60)}:${String(remainingSecs % 60).padStart(2, "0")}`
    : remainingSecs === 0 ? "expired" : null;

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
      <div
        className="flex min-w-0 w-full items-center gap-2 rounded-md bg-zinc-100 px-2.5 py-1.5 text-left transition-colors hover:bg-zinc-200 dark:bg-zinc-700/50 dark:hover:bg-zinc-700"
        style={{ fontSize: EXPLORE_FONT_SIZE }}
      >
        <button className="flex min-w-0 flex-1 items-center gap-2" onClick={() => setShowDetails(!showDetails)}>
          <Icon className="h-3.5 w-3.5 shrink-0 text-zinc-500" />
          <span className="shrink-0 font-medium text-zinc-700 dark:text-zinc-300">{toolName}</span>
          {summary && (
            <span className="min-w-0 flex-1 truncate ml-1 text-left text-zinc-500 dark:text-zinc-400">
              {summary}
            </span>
          )}
        </button>
        {/* Approval buttons — shown when this tool needs user approval */}
        {showApproval && onApprove && specificApproval && (
          <div className="flex items-center gap-1 shrink-0" onClick={(e) => e.stopPropagation()}>
            {countdownLabel && countdownLabel !== "expired" && (
              <span className="text-[10px] font-mono text-amber-600 dark:text-amber-400 shrink-0 min-w-[2.5rem] text-right">
                {countdownLabel}
              </span>
            )}
            <button
              onClick={() => onApprove("deny", specificApproval)}
              className="rounded-md border border-zinc-300 px-2 py-0.5 text-[11px] font-medium text-zinc-600 transition-colors hover:bg-zinc-200 dark:border-zinc-500 dark:text-zinc-400 dark:hover:bg-zinc-600"
            >
              Deny
            </button>
            <button
              onClick={() => onApprove("allow", specificApproval)}
              className="rounded-md px-2 py-0.5 text-[11px] font-medium text-white transition-opacity hover:opacity-90"
              style={{ backgroundColor: "var(--color-accent)" }}
            >
              Allow
            </button>
          </div>
        )}
        {/* Expired indicator */}
        {needsApproval && remainingSecs === 0 && (
          <span className="text-[10px] text-red-500 dark:text-red-400 shrink-0">
            Timed out
          </span>
        )}
        {/* Status indicator */}
        {isSuccess ? (
          <Check className="h-3 w-3 shrink-0" style={{ color: "var(--color-accent)" }} />
        ) : isError ? (
          <X className="h-3 w-3 shrink-0 text-red-500" />
        ) : isPendingResult ? (
          <span className="h-3 w-3 shrink-0 animate-pulse rounded-full bg-zinc-300 dark:bg-zinc-500" />
        ) : null}
        <button onClick={() => setShowDetails(!showDetails)}>
          {showDetails ? (
            <ChevronDown className="h-3 w-3 shrink-0 text-zinc-400" />
          ) : (
            <ChevronRight className="h-3 w-3 shrink-0 text-zinc-400" />
          )}
        </button>
      </div>
      {showDetails && (
        <div className="mt-0.5 ml-5 space-y-0.5">
          {/* Call params */}
          <pre className="rounded bg-zinc-100 p-2 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400 whitespace-pre-wrap break-all" style={{ fontSize: EXPLORE_DETAIL_FONT_SIZE }}>
            {call.content}
          </pre>
          {/* Result */}
          {result && (
            <pre className={`rounded p-2 whitespace-pre-wrap break-all ${isError ? "bg-red-50 text-red-600 dark:bg-red-900/20 dark:text-red-400" : "bg-[var(--color-accent)]/10 text-zinc-600 dark:bg-[var(--color-accent)]/10 dark:text-zinc-400"}`} style={{ fontSize: EXPLORE_DETAIL_FONT_SIZE }}>
              {result.content.length > 500 ? result.content.slice(0, 500) + "\n..." : result.content}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}