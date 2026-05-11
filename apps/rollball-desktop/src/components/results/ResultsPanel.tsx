import { useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import type { ChatMessage } from "../../lib/types";
import { cn } from "../../lib/utils";
import { PanelRight } from "lucide-react";
import { AgentSetupTab } from "./AgentSetupTab";

interface ResultsPanelProps {
  onCollapse: () => void;
}

type PanelTab = "results" | "setup";

// Stable empty array reference to avoid Zustand selector infinite loop
const EMPTY_MESSAGES: ChatMessage[] = [];

export function ResultsPanel({ onCollapse }: ResultsPanelProps) {
  const { agents, selectedAgentId } = useAgentStore();
  const tokenUsage = useChatStore((s) => selectedAgentId ? (s.agentStates[selectedAgentId]?.tokenUsage ?? null) : null);
  const messages = useChatStore((s) => selectedAgentId ? (s.agentStates[selectedAgentId]?.messages ?? EMPTY_MESSAGES) : EMPTY_MESSAGES);
  const [activeTab, setActiveTab] = useState<PanelTab>("results");

  // Selected agent info
  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Count iterations (number of assistant messages)
  const iterations = messages.filter((m) => m.type === "assistant").length;

  return (
    <div className="flex w-[320px] flex-col border-l border-zinc-200 bg-zinc-50 transition-[width] duration-250 ease-in-out dark:border-zinc-800 dark:bg-zinc-900">
      {/* Header with tabs */}
      <div className="border-b border-zinc-200 dark:border-zinc-800">
        <div className="flex items-center justify-between px-3 pt-2">
          <div className="flex gap-0">
            <TabButton
              active={activeTab === "results"}
              onClick={() => setActiveTab("results")}
            >
              Results
            </TabButton>
            <TabButton
              active={activeTab === "setup"}
              onClick={() => setActiveTab("setup")}
            >
              Setup
            </TabButton>
          </div>
          <button
            onClick={onCollapse}
            className="text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
            aria-label="Collapse results panel"
          >
            <PanelRight className="h-4 w-4" />
          </button>
        </div>
      </div>

      {/* Tab content */}
      {activeTab === "results" && (
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
      )}

      {activeTab === "setup" && <AgentSetupTab />}
    </div>
  );
}

function StatRow({ label, value }: { label: string; value?: string }) {
  return (
    <div className="flex justify-between py-1">
      <span className="text-zinc-500">{label}</span>
      <span className="font-mono text-zinc-700 dark:text-zinc-300">{value ?? "\u2014"}</span>
    </div>
  );
}

function TabButton({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "px-3 py-2 text-xs font-medium transition-colors border-b-2 -mb-px",
        active
          ? "border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
          : "border-transparent text-zinc-400 hover:text-zinc-600 dark:text-zinc-500 dark:hover:text-zinc-300",
      )}
    >
      {children}
    </button>
  );
}
