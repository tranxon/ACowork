import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { cn } from "../../lib/utils";
import { Bot, Play, Send, ChevronDown, ChevronRight, Wrench, AlertTriangle, Check } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage, VaultKeyEntry } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";

export function ChatPanel() {
  const { agents, selectedAgentId, startAgent } = useAgentStore();
  const { messages, sending, ws, connectStream, sendMessage, streamingMessageId, currentModel, currentProvider, availableModels, setCurrentModel, setAvailableModels, loadAgentModel } = useChatStore();
  const gatewayStatus = useGatewayStore((s) => s.status);
  const [inputValue, setInputValue] = useState("");
  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking
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
    }
    return () => {
      useChatStore.getState().disconnectStream();
    };
  }, [selectedAgentId, selectedAgent?.running, connectStream]);

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
      {/* Messages area */}
      <div className="flex-1 overflow-y-auto px-4 py-3" role="log" aria-label="Chat messages">
        {messages.length === 0 && (
          <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
            Start a conversation with {selectedAgent.name}
          </div>
        )}
        <div className="space-y-2">
          {messages.map((msg) => (
            <MessageBubble key={msg.id} message={msg} isStreaming={msg.id === streamingMessageId} />
          ))}
        </div>
        <div ref={messagesEndRef} />
      </div>

      {/* Input area */}
      <div className="border-t border-zinc-200 p-3 dark:border-zinc-800">
        {/* Model switcher — only enabled when agent is running */}
        {availableModels.length > 1 && selectedAgent?.running && (
          <ModelMenu
            models={availableModels}
            currentModel={currentModel}
            currentProvider={currentProvider}
            onSelect={(m) => selectedAgentId && setCurrentModel(m, selectedAgentId)}
          />
        )}
        <div className="flex gap-2">
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
            rows={1}
            className="max-h-32 min-h-[36px] flex-1 resize-none rounded-md border border-zinc-200 bg-white px-3 py-2 text-sm outline-none focus:border-zinc-400 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:focus:border-zinc-500"
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                handleSend();
              }
            }}
          />
          <button
            onClick={handleSend}
            disabled={inputDisabled || !inputValue.trim()}
            className="flex h-9 w-9 items-center justify-center rounded-md bg-zinc-800 text-white transition-colors hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
            aria-label="Send message"
          >
            <Send className="h-4 w-4" />
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
  const replyContent = content.slice(closeIndex + 8); // length of "</think>"

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
        <div className="max-w-[85%] rounded-lg rounded-bl-sm border border-blue-200 bg-blue-50 px-3 py-2 dark:border-blue-900 dark:bg-blue-950">
          <button
            className="flex w-full items-center gap-1.5 text-xs font-medium text-blue-700 dark:text-blue-300"
            onClick={() => setExpanded(!expanded)}
          >
            <Wrench className="h-3 w-3" />
            {message.toolName}
            {expanded ? <ChevronDown className="ml-auto h-3 w-3" /> : <ChevronRight className="ml-auto h-3 w-3" />}
          </button>
          {expanded && (
            <pre className="mt-2 overflow-x-auto rounded bg-white/50 p-2 text-xs dark:bg-black/20">
              {message.content}
            </pre>
          )}
        </div>
      </div>
    );
  }

  if (message.type === "tool_result") {
    return (
      <div className="flex justify-start">
        <div className="max-w-[85%] rounded-lg rounded-bl-sm border border-green-200 bg-green-50 px-3 py-2 dark:border-green-900 dark:bg-green-950">
          <button
            className="flex w-full items-center gap-1.5 text-xs font-medium text-green-700 dark:text-green-300"
            onClick={() => setExpanded(!expanded)}
          >
            <Wrench className="h-3 w-3" />
            {message.toolName} → Result
            {expanded ? <ChevronDown className="ml-auto h-3 w-3" /> : <ChevronRight className="ml-auto h-3 w-3" />}
          </button>
          {expanded && (
            <pre className="mt-2 overflow-x-auto rounded bg-white/50 p-2 text-xs dark:bg-black/20">
              {message.content}
            </pre>
          )}
        </div>
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
    <div ref={ref} className="relative mb-2 inline-block">
      {/* Trigger button */}
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className={cn(
          "flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs transition-colors",
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
