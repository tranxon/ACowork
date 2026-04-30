import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { cn } from "../../lib/utils";
import { Bot, Play, Send, ChevronDown, ChevronRight, Wrench, AlertTriangle, Check, Brain, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage, VaultKeyEntry } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";
import { MemoryPanel } from "../memory/MemoryPanel";
import { SkillBrowser } from "../skills/SkillBrowser";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";

export function ChatPanel() {
  const { agents, selectedAgentId, startAgent } = useAgentStore();
  const { messages, sending, ws, connectStream, sendMessage, streamingMessageId, currentModel, currentProvider, availableModels, setCurrentModel, setAvailableModels, loadAgentModel, loadConversationHistory } = useChatStore();
  const gatewayStatus = useGatewayStore((s) => s.status);
  const [inputValue, setInputValue] = useState("");
  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking
  const [activeDrawer, setActiveDrawer] = useState<"memory" | "skills" | null>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Load available models from Vault keys
  useEffect(() => {
    const loadModels = async () => {
      try {
        const keys = await invoke<VaultKeyEntry[]>("list_keys");
        const allModels: string[] = [];
        for (const key of keys) {
          if (key.models?.length) {
            allModels.push(...key.models);
          } else if (key.default_model) {
            allModels.push(key.default_model);
          }
        }
        setAvailableModels([...new Set(allModels)]);
        setHasLlmConfig(keys.length > 0);
      } catch {
        // Gateway may not be running
      }
    };
    loadModels();
  }, [gatewayStatus, setAvailableModels]);

  // Connect WebSocket when agent changes + restore per-agent model
  useEffect(() => {
    // Clear stale messages from previous agent
    useChatStore.getState().clearMessages();

    if (selectedAgentId && selectedAgent?.running) {
      connectStream(selectedAgentId, "http://127.0.0.1:19876");
      // Always load model from Gateway API (reads per-agent .agent_model.json)
      loadAgentModel(selectedAgentId);
      // Load conversation history for the new agent
      loadConversationHistory(selectedAgentId);
    }
    return () => {
      useChatStore.getState().disconnectStream();
    };
  }, [selectedAgentId, selectedAgent?.running, connectStream, loadAgentModel, loadConversationHistory]);

  // Auto-scroll to bottom
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const handleSend = () => {
    const content = inputValue.trim();
    if (!content || sending || !selectedAgentId) return;
    // sendMessage is async but we fire-and-forget here —
    // the store handles all state updates internally
    void sendMessage(content, selectedAgentId);
    setInputValue("");
  };

  // ── Empty state: no agents at all ──
  if (agents.length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
        <div className="text-center">
          <Bot className="mx-auto h-12 w-12 text-zinc-300 dark:text-zinc-600" />
          <p className="mt-3 text-sm text-zinc-400 dark:text-zinc-500">No agents available</p>
          <p className="mt-1 text-xs text-zinc-400 dark:text-zinc-600">Connect to Gateway and install the System Agent</p>
        </div>
      </div>
    );
  }

  // ── No agent selected ──
  if (!selectedAgent) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
        <div className="text-center">
          <Bot className="mx-auto h-12 w-12 text-zinc-300 dark:text-zinc-600" />
          <p className="mt-3 text-sm text-zinc-400 dark:text-zinc-500">Select an agent to start chatting</p>
          <p className="mt-1 text-xs text-zinc-400 dark:text-zinc-600">or install a new agent from the sidebar</p>
        </div>
      </div>
    );
  }

  // ── Agent not running ──
  if (!selectedAgent.running) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
        <div className="text-center">
          <div className="mx-auto text-3xl text-zinc-300 dark:text-zinc-600">⏸</div>
          <p className="mt-3 text-sm text-zinc-600 dark:text-zinc-400">{selectedAgent.name} is stopped</p>
          <button
            onClick={() => startAgent(selectedAgent.agent_id)}
            className="mt-3 inline-flex items-center gap-1.5 rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
          >
            <Play className="h-3.5 w-3.5" /> Start Agent
          </button>
        </div>
      </div>
    );
  }

  // ── Chat view ──
  const inputDisabled = sending || gatewayStatus !== "connected";

  return (
    <div className="flex flex-1 flex-col bg-white dark:bg-zinc-900">
      {/* LLM config warning */}
      {hasLlmConfig === false && (
        <div className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-4 py-2 dark:border-amber-900 dark:bg-amber-950">
          <AlertTriangle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
          <span className="text-xs text-amber-700 dark:text-amber-300">
            No LLM provider configured. Please add an API key in Settings → Providers.
          </span>
        </div>
      )}
      {/* Messages area with drawer overlay */}
      <div className="relative flex-1 overflow-hidden">
        <div className="h-full overflow-y-auto px-4 py-3" role="log" aria-label="Chat messages">
          {messages.length === 0 && (
            <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
              Start a conversation with {selectedAgent.name}
            </div>
          )}
          <div className="space-y-2">
            {(() => {
              // Reorder messages: ensure assistant messages come after tool calls/results
              // in the same conversation turn
              const reordered = [...messages];
              
              // Group messages by conversation turn (between user messages)
              const turns: ChatMessage[][] = [];
              let currentTurn: ChatMessage[] = [];
              
              for (const msg of reordered) {
                if (msg.type === "user") {
                  if (currentTurn.length > 0) {
                    turns.push(currentTurn);
                  }
                  currentTurn = [msg];
                } else {
                  currentTurn.push(msg);
                }
              }
              if (currentTurn.length > 0) {
                turns.push(currentTurn);
              }
              
              // Within each turn, move assistant messages to the end
              const finalMessages: ChatMessage[] = [];
              for (const turn of turns) {
                const userMsg = turn.find(m => m.type === "user");
                const assistantMsgs = turn.filter(m => m.type === "assistant");
                const toolMsgs = turn.filter(m => m.type === "tool_call" || m.type === "tool_result");
                const otherMsgs = turn.filter(m => m.type !== "user" && m.type !== "assistant" && m.type !== "tool_call" && m.type !== "tool_result");
                
                if (userMsg) finalMessages.push(userMsg);
                finalMessages.push(...toolMsgs);
                finalMessages.push(...assistantMsgs);
                finalMessages.push(...otherMsgs);
              }
              
              return finalMessages.map((msg) => (
                <MessageBubble key={msg.id} message={msg} isStreaming={msg.id === streamingMessageId} />
              ));
            })()}
          </div>
          <div ref={messagesEndRef} />
        </div>

        {/* Drawer panel — slides in from the right */}
        {activeDrawer && (
          <div
            className="absolute inset-0 flex justify-end bg-black/20 z-20"
            onClick={() => setActiveDrawer(null)}
          >
            <div
              className="w-[480px] max-w-full h-full bg-white dark:bg-zinc-900 shadow-xl overflow-y-auto"
              onClick={(e) => e.stopPropagation()}
            >
              <div className="sticky top-0 flex items-center justify-between p-3 border-b border-zinc-200 dark:border-zinc-800 bg-white dark:bg-zinc-900 z-10">
                <span className="font-medium text-sm text-zinc-900 dark:text-zinc-100">
                  {activeDrawer === "memory" ? "Memory" : "Skills"}
                </span>
                <button
                  className="rounded-md p-1 text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                  onClick={() => setActiveDrawer(null)}
                >
                  <X size={16} />
                </button>
              </div>
              {activeDrawer === "memory" && <MemoryPanel />}
              {activeDrawer === "skills" && <SkillBrowser />}
            </div>
          </div>
        )}
      </div>

      {/* Unified input container with toolbar */}
      <div className="mx-3 mb-3 rounded-xl border border-zinc-200 dark:border-zinc-700 bg-zinc-50 dark:bg-zinc-800/50">
        {/* Textarea area — borderless, transparent background */}
        <textarea
          value={inputValue}
          onChange={(e) => setInputValue(e.target.value)}
          placeholder={
            gatewayStatus !== "connected"
              ? "Gateway not connected"
              : !ws || ws.readyState !== WebSocket.OPEN
                ? "Type a message... (HTTP mode — streaming unavailable)"
                : "Type a message... (Enter to send, Shift+Enter for new line)"
          }
          disabled={inputDisabled}
          rows={3}
          className="w-full resize-none border-0 bg-transparent p-3 pb-2 text-sm outline-none placeholder:text-zinc-500 dark:placeholder:text-zinc-500 dark:text-zinc-100 disabled:cursor-not-allowed disabled:opacity-50"
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              handleSend();
            }
          }}
        />

        {/* Bottom toolbar */}
        <div className="flex items-center justify-between px-3 pb-2">
          {/* Left: feature buttons */}
          <div className="flex items-center gap-1">
            {/* Model switcher — only enabled when agent is running */}
            {availableModels.length > 1 && selectedAgent?.running && (
              <ModelMenu
                models={availableModels}
                currentModel={currentModel}
                currentProvider={currentProvider}
                onSelect={(m) => selectedAgentId && setCurrentModel(m, selectedAgentId)}
              />
            )}
            {/* Workspace button */}
            <WorkspaceSelector />
            {/* Memory button */}
            <button
              className={`inline-flex items-center gap-1 rounded-lg px-2 py-1 text-xs transition-colors ${
                activeDrawer === "memory"
                  ? "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100"
                  : "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200"
              }`}
              onClick={() => setActiveDrawer(activeDrawer === "memory" ? null : "memory")}
            >
              <Brain size={14} /> Memory
            </button>
            {/* Skills button */}
            <button
              className={`inline-flex items-center gap-1 rounded-lg px-2 py-1 text-xs transition-colors ${
                activeDrawer === "skills"
                  ? "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100"
                  : "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200"
              }`}
              onClick={() => setActiveDrawer(activeDrawer === "skills" ? null : "skills")}
            >
              <Wrench size={14} /> Skills
            </button>
          </div>

          {/* Right: send button */}
          <button
            className="rounded-lg p-1.5 text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200 disabled:opacity-50"
            onClick={handleSend}
            disabled={inputDisabled || !inputValue.trim()}
            aria-label="Send message"
          >
            <Send size={16} />
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Parse <think>...</think> tags from assistant content.
 *
 * Returns the think content, reply content, and whether the think tag is closed.
 * If the content does not start with <think>, all content is treated as reply.
 * The <think> and </think> tags are stripped from the output.
 */
function parseThinkContent(content: string): {
  thinkContent: string | null;
  replyContent: string;
  thinkClosed: boolean;
} {
  if (!content.startsWith("<think>")) {
    return { thinkContent: null, replyContent: content, thinkClosed: false };
  }

  const closeIndex = content.indexOf("</think>");

  if (closeIndex === -1) {
    // Think tag is still open — everything after <think> is think content
    const thinkContent = content.slice(7); // length of "<think>"
    return { thinkContent, replyContent: "", thinkClosed: false };
  }

  // Think tag is closed
  const thinkContent = content.slice(7, closeIndex);
  let replyContent = content.slice(closeIndex + 8); // length of "</think>"
  
  // Trim leading whitespace/newlines from reply content
  replyContent = replyContent.trimStart();

  return { thinkContent, replyContent, thinkClosed: true };
}

/** Single message bubble */
function MessageBubble({ message, isStreaming }: { message: ChatMessage; isStreaming: boolean }) {
  const [expanded, setExpanded] = useState(false);

  if (message.type === "user") {
    return (
      <div className="flex justify-end">
        <div className="max-w-[70%] rounded-lg rounded-br-sm bg-zinc-800 px-3 py-2 text-sm text-white dark:bg-zinc-700">
          {message.content}
        </div>
      </div>
    );
  }

  if (message.type === "assistant") {
    const { thinkContent, replyContent, thinkClosed } = parseThinkContent(message.content);
    const hasReplyStarted = thinkClosed && replyContent.length > 0;
    const showPlaceholder = !message.content;

    return (
      <div className="flex justify-start">
        <div className="max-w-[85%] rounded-lg rounded-bl-sm bg-zinc-100 px-3 py-2 text-sm dark:bg-zinc-800 dark:text-zinc-200">
          {thinkContent !== null && (
            <ThinkBlock
              content={thinkContent}
              isStreaming={isStreaming}
              hasReplyStarted={hasReplyStarted}
            />
          )}
          {replyContent && (
            <div className={`prose prose-sm prose-zinc max-w-none dark:prose-invert ${thinkContent !== null ? "mt-2" : ""}`}>
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{replyContent}</ReactMarkdown>
            </div>
          )}
          {showPlaceholder && (
            <span className="text-zinc-400">Thinking...</span>
          )}
          {isStreaming && <span className="ml-0.5 inline-block animate-pulse">▌</span>}
        </div>
      </div>
    );
  }

  if (message.type === "system") {
    return (
      <div className="flex justify-center">
        <div className="rounded bg-zinc-100 px-3 py-1 text-xs text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
          {message.content}
        </div>
      </div>
    );
  }

  if (message.type === "tool_call") {
    return (
      <div className="flex justify-start">
        <button
          className="flex w-full max-w-[85%] items-center gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
          onClick={() => setExpanded(!expanded)}
        >
          <Wrench className="h-3 w-3 shrink-0" />
          <span className="font-medium">{message.toolName}</span>
          <span className="truncate text-zinc-400 dark:text-zinc-500">{message.content.substring(0, 50)}{message.content.length > 50 ? "..." : ""}</span>
          {expanded ? <ChevronDown className="ml-auto h-3 w-3 shrink-0" /> : <ChevronRight className="ml-auto h-3 w-3 shrink-0" />}
        </button>
      </div>
    );
  }

  if (message.type === "tool_result") {
    return (
      <div className="flex justify-start">
        <button
          className="flex w-full max-w-[85%] items-center gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
          onClick={() => setExpanded(!expanded)}
        >
          <Wrench className="h-3 w-3 shrink-0" />
          <span className="font-medium">{message.toolName}</span>
          <span className="text-zinc-400 dark:text-zinc-500">→ Result</span>
          <span className="ml-auto text-[10px] text-zinc-400 dark:text-zinc-500">Click to view</span>
          {expanded ? <ChevronDown className="ml-2 h-3 w-3 shrink-0" /> : <ChevronRight className="ml-2 h-3 w-3 shrink-0" />}
        </button>
        {expanded && (
          <pre className="mt-1 max-w-[85%] overflow-x-auto rounded-lg bg-zinc-50 p-3 text-xs text-zinc-600 dark:bg-zinc-800/50 dark:text-zinc-400">
            {message.content}
          </pre>
        )}
      </div>
    );
  }

  return null;
}

/** Popup-style model selector with provider shown in gray */
function ModelMenu({
  models,
  currentModel,
  currentProvider,
  onSelect,
}: {
  models: string[];
  currentModel: string | null;
  currentProvider: string | null;
  onSelect: (model: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  return (
    <div ref={ref} className="relative inline-block">
      {/* Trigger button */}
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className={cn(
          "inline-flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs transition-colors",
          "border-zinc-200 bg-white text-zinc-700 hover:bg-zinc-50",
          "dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700",
          open && "ring-1 ring-zinc-300 dark:ring-zinc-600",
        )}
      >
        <span className="font-medium">{currentModel ?? "Model"}</span>
        {currentProvider && (
          <span className="text-[10px] text-zinc-400 dark:text-zinc-500">{currentProvider}</span>
        )}
        <ChevronDown className="h-3 w-3 text-zinc-400" />
      </button>

      {/* Popup menu */}
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 min-w-[180px] overflow-hidden rounded-lg border shadow-lg",
            "border-zinc-200 bg-white dark:border-zinc-700 dark:bg-zinc-800",
          )}
        >
          <div className="px-2.5 py-1.5 text-[10px] font-medium uppercase tracking-wider text-zinc-400 dark:text-zinc-500">
            Switch Model
          </div>
          {models.map((m) => {
            const isActive = m === currentModel;
            return (
              <button
                key={m}
                type="button"
                onClick={() => {
                  onSelect(m);
                  setOpen(false);
                }}
                className={cn(
                  "flex w-full items-center gap-2 px-2.5 py-1.5 text-xs transition-colors",
                  isActive
                    ? "bg-zinc-100 text-zinc-900 dark:bg-zinc-700 dark:text-white"
                    : "text-zinc-600 hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50",
                )}
              >
                <span className="w-3.5 shrink-0">
                  {isActive && <Check className="h-3 w-3 text-blue-500" />}
                </span>
                <span className={cn("font-medium", isActive && "text-blue-600 dark:text-blue-400")}>
                  {m}
                </span>
                {currentProvider && (
                  <span className="ml-auto text-[10px] text-zinc-400 dark:text-zinc-500">
                    {currentProvider}
                  </span>
                )}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
