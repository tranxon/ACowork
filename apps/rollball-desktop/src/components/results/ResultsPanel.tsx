import { useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { cn } from "../../lib/utils";
import type { ChatMessage } from "../../lib/types";

interface ResultsPanelProps {
  onCollapse: () => void;
}

export function ResultsPanel({ onCollapse }: ResultsPanelProps) {
  const { tokenUsage, messages } = useChatStore();
  const { agents, selectedAgentId } = useAgentStore();

  // Collect tool call records from messages
  const toolCalls = messages.filter(
    (m) => m.type === "tool_call" || m.type === "tool_result",
  );

  // Selected agent info
  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Count iterations (number of assistant messages)
  const iterations = messages.filter((m) => m.type === "assistant").length;

  return (
    <div className="flex w-[320px] flex-col border-l border-zinc-200 bg-zinc-50 transition-[width] duration-250 ease-in-out dark:border-zinc-800 dark:bg-zinc-900">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
        <span className="text-xs font-medium uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
          Execution Results
        </span>
        <button
          onClick={onCollapse}
          className="text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
          aria-label="Collapse results panel"
        >
          ◀
        </button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-3">
        {/* Token statistics */}
        <div className="mb-4">
          <h3 className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
            Session Stats
          </h3>
          <div className="rounded-md bg-white p-3 text-xs dark:bg-zinc-800">
            <StatRow label="Prompt tokens" value={tokenUsage?.prompt_tokens?.toLocaleString()} />
            <StatRow label="Completion tokens" value={tokenUsage?.completion_tokens?.toLocaleString()} />
            <StatRow label="Total tokens" value={tokenUsage?.total_tokens?.toLocaleString()} />
            <StatRow label="Iterations" value={iterations ? String(iterations) : undefined} />
          </div>
        </div>

        {/* Tool call records */}
        <div className="mb-4">
          <h3 className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
            Tool Calls
          </h3>
          {toolCalls.length === 0 ? (
            <div className="rounded-md bg-white p-3 text-xs text-zinc-400 dark:bg-zinc-800 dark:text-zinc-500">
              No tool calls yet
            </div>
          ) : (
            <div className="space-y-1">
              {toolCalls.map((msg) => (
                <ToolCallItem key={msg.id} message={msg} />
              ))}
            </div>
          )}
        </div>

        {/* Agent running status */}
        <div>
          <h3 className="mb-2 text-xs font-medium text-zinc-500 dark:text-zinc-400">
            Agent Status
          </h3>
          <div className="rounded-md bg-white p-3 text-xs dark:bg-zinc-800">
            {selectedAgent ? (
              <>
                <div className="flex justify-between py-1">
                  <span className="text-zinc-500">Status</span>
                  <span className="flex items-center gap-1.5">
                    <span
                      className={cn(
                        "inline-block h-2 w-2 rounded-full",
                        selectedAgent.running ? "bg-green-500" : "bg-zinc-300 dark:bg-zinc-600",
                      )}
                    />
                    <span className="text-zinc-700 dark:text-zinc-300">
                      {selectedAgent.running ? "Running" : "Stopped"}
                    </span>
                  </span>
                </div>
                <div className="flex justify-between py-1">
                  <span className="text-zinc-500">Agent</span>
                  <span className="text-zinc-700 dark:text-zinc-300">{selectedAgent.name}</span>
                </div>
                <div className="flex justify-between py-1">
                  <span className="text-zinc-500">Version</span>
                  <span className="text-zinc-700 dark:text-zinc-300">{selectedAgent.version}</span>
                </div>
              </>
            ) : (
              <div className="py-1 text-zinc-400 dark:text-zinc-500">No agent selected</div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function StatRow({ label, value }: { label: string; value?: string }) {
  return (
    <div className="flex justify-between py-1">
      <span className="text-zinc-500">{label}</span>
      <span className="font-mono text-zinc-700 dark:text-zinc-300">{value ?? "—"}</span>
    </div>
  );
}

function ToolCallItem({ message }: { message: ChatMessage }) {
  const [expanded, setExpanded] = useState(false);
  const isCall = message.type === "tool_call";
  const statusIcon =
    message.toolStatus === "success" ? "✅" : message.toolStatus === "error" ? "" : "⏳";

  const summary = isCall
    ? `${message.toolName ?? "tool"}`
    : `${message.toolName ?? "tool"} result`;

  return (
    <div className="rounded-md bg-white p-2 text-xs dark:bg-zinc-800">
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center justify-between text-left"
      >
        <span className="flex items-center gap-1">
          <span className="font-medium text-zinc-700 dark:text-zinc-300">{summary}</span>
        </span>
        <span className="flex items-center gap-1.5 text-zinc-400">
          {message.duration && <span>{(message.duration / 1000).toFixed(1)}s</span>}
          <span>{statusIcon}</span>
          <span className="text-[10px]">{expanded ? "▼" : "▶"}</span>
        </span>
      </button>
      {expanded && message.content && (
        <pre className="mt-1.5 max-h-40 overflow-auto rounded bg-zinc-100 p-2 font-mono text-[11px] text-zinc-600 dark:bg-zinc-900 dark:text-zinc-400">
          {message.content}
        </pre>
      )}
    </div>
  );
}
