import { useEffect, useRef, useState, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { Bot, Play, Send, ChevronDown, ChevronRight, Wrench, AlertTriangle, Check, Brain, X, Square, Copy, FileText, Terminal } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage, VaultKeyEntry } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";
import { MemoryPanel } from "../memory/MemoryPanel";
import { SkillBrowser } from "../skills/SkillBrowser";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";

export function ChatPanel() {
  const { agents, selectedAgentId, startAgent } = useAgentStore();
  const { messages, sending, ws, connectStream, sendMessage, stopCurrentMessage, streamingMessageId, currentModel, availableModels, setCurrentModel, setAvailableModels, loadAgentModel, loadConversationHistory, iterationLimitPaused, continueExecution } = useChatStore();
  const gatewayStatus = useGatewayStore((s) => s.status);
  const [inputValue, setInputValue] = useState("");
  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking
  const [activeDrawer, setActiveDrawer] = useState<"memory" | "skills" | null>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Memoize sorted messages to avoid unnecessary re-renders
  const sortedMessages = useMemo(() => {
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

    return finalMessages;
  }, [messages]);

  // Group consecutive tool calls for compact display
  const displayMessages = useMemo(() => {
    const grouped: Array<ChatMessage | { type: 'tool_group'; id: string; items: ChatMessage[] }> = [];
    let currentGroup: ChatMessage[] = [];

    for (const msg of sortedMessages) {
      if (msg.type === "tool_call" || msg.type === "tool_result") {
        currentGroup.push(msg);
      } else {
        if (currentGroup.length > 0) {
          grouped.push({
            type: 'tool_group',
            id: `group-${currentGroup[0].id}`,
            items: currentGroup,
          });
          currentGroup = [];
        }
        grouped.push(msg);
      }
    }
    if (currentGroup.length > 0) {
      grouped.push({
        type: 'tool_group',
        id: `group-${currentGroup[0].id}`,
        items: currentGroup,
      });
    }

    return grouped;
  }, [sortedMessages]);

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
      connectStream(selectedAgentId, getGatewayUrl());
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
        <div className="h-full overflow-y-auto px-4 py-3 select-text cursor-text" role="log" aria-label="Chat messages">
          {messages.length === 0 && (
            <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
              Start a conversation with {selectedAgent.name}
            </div>
          )}
          <div className="space-y-2">
            {displayMessages.map((item) => {
              if ('type' in item && item.type === 'tool_group') {
                return <ToolCallGroup key={item.id} items={item.items} />;
              }
              const msg = item as ChatMessage;
              return (
                <MessageBubble key={msg.id} message={msg} isStreaming={msg.id === streamingMessageId} />
              );
            })}
          </div>
          {/* Iteration limit pause — Continue button */}
          {iterationLimitPaused && (
            <div className="flex items-center justify-center gap-3 rounded-lg border border-amber-200 bg-amber-50 px-4 py-3 dark:border-amber-800 dark:bg-amber-900/20">
              <span className="text-sm text-amber-700 dark:text-amber-300">
                Iteration limit reached ({iterationLimitPaused.iteration}/{iterationLimitPaused.maxIterations})
              </span>
              <button
                onClick={() => {
                  if (selectedAgentId) {
                    continueExecution(selectedAgentId);
                  }
                }}
                className="flex items-center gap-1.5 rounded-md bg-amber-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-amber-700 transition-colors"
              >
                <Play className="h-3.5 w-3.5" />
                Continue
              </button>
            </div>
          )}
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

          {/* Right: send/stop button */}
          <button
            className={`rounded-lg p-1.5 transition-colors ${
              sending
                ? "text-blue-500 hover:bg-blue-100 dark:hover:bg-blue-900/30 hover:text-blue-600 dark:hover:text-blue-400"
                : "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200 disabled:opacity-50"
            }`}
            onClick={sending ? () => stopCurrentMessage() : handleSend}
            disabled={!sending && (inputDisabled || !inputValue.trim())}
            aria-label={sending ? "Stop generation" : "Send message"}
          >
            {sending ? <Square size={16} fill="currentColor" /> : <Send size={16} />}
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
 * Handles multiple <think> blocks by extracting the first one and stripping all others.
 */
function parseThinkContent(content: string): {
  thinkContent: string | null;
  replyContent: string;
  thinkClosed: boolean;
} {
  // Find the first <think> block
  const firstThinkStart = content.indexOf("<think>");

  if (firstThinkStart === -1) {
    // No <think> tag found — treat entire content as reply
    return { thinkContent: null, replyContent: content, thinkClosed: false };
  }

  // Find the closing </think> for the first <think>
  const firstThinkEnd = content.indexOf("</think>", firstThinkStart);

  if (firstThinkEnd === -1) {
    // <think> tag is still open — everything after <think> is think content
    const thinkContent = content.slice(firstThinkStart + 7); // length of "<think>"
    return { thinkContent, replyContent: "", thinkClosed: false };
  }

  // Extract think content (between first <think> and its closing </think>)
  const thinkContent = content.slice(firstThinkStart + 7, firstThinkEnd);

  // Extract reply content (after the first </think>)
  // Also strip any remaining <think>...</think> tags from the reply
  let replyContent = content.slice(firstThinkEnd + 8); // length of "</think>"
  
  // Remove any remaining <think>...</think> blocks from reply content
  const thinkRegex = new RegExp('<think>[\\s\\S]*?</think>', 'g');
  replyContent = replyContent.replace(thinkRegex, "");
  // Remove any unclosed <think> at the end
  const lastUnclosedThink = replyContent.lastIndexOf("<think>");
  if (lastUnclosedThink !== -1 && replyContent.indexOf("</think>", lastUnclosedThink + 7) === -1) {
    replyContent = replyContent.slice(0, lastUnclosedThink);
  }
  
  // Trim leading whitespace/newlines from reply content
  replyContent = replyContent.trimStart();

  return { thinkContent, replyContent, thinkClosed: true };
}

/** Aggregated tool call group with smart summary */
function ToolCallGroup({ items }: { items: ChatMessage[] }) {
  const [expanded, setExpanded] = useState(false);
  const [expandedItem, setExpandedItem] = useState<number | null>(null);

  // Group by tool name and count
  const toolCounts = items.reduce((acc, msg) => {
    if (msg.type === "tool_call" && msg.toolName) {
      acc[msg.toolName] = (acc[msg.toolName] || 0) + 1;
    }
    return acc;
  }, {} as Record<string, number>);

  // Determine primary tool and count
  const primaryTool = Object.entries(toolCounts)[0];
  if (!primaryTool) return null;
  const [toolName, count] = primaryTool;

  // Generate human-readable action name
  const actionMap: Record<string, string> = {
    "file_read": "Read",
    "file_write": "Write",
    "file_edit": "Edit",
    "shell": "Run",
    "web_search": "Search",
    "web_fetch": "Fetch",
  };
  const actionName = actionMap[toolName] || toolName;

  // Generate summary
  const callItems = items.filter(m => m.type === "tool_call");
  const summaryItems = callItems.slice(0, 3).map(item => {
    const params = JSON.parse(item.content || "{}");
    if (toolName === "file_read" || toolName === "file_write" || toolName === "file_edit") {
      return params.path?.split(/[\\/]/).pop() || "file";
    } else if (toolName === "shell") {
      const cmd = params.command || "";
      return cmd.split(' ')[0] || "cmd";
    }
    return item.toolName || "tool";
  });

  const hasMore = callItems.length > 3;
  const summary = summaryItems.join(", ") + (hasMore ? ` + ${callItems.length - 3} more` : "");

  // Icon based on tool type
  const Icon = toolName === "shell" ? Terminal : FileText;

  // Pair tool_call with its corresponding tool_result
  const pairedItems: Array<{ call: ChatMessage; result?: ChatMessage }> = [];
  let currentCall: ChatMessage | null = null;
  
  for (const item of items) {
    if (item.type === "tool_call") {
      if (currentCall) {
        pairedItems.push({ call: currentCall });
      }
      currentCall = item;
    } else if (item.type === "tool_result" && currentCall) {
      pairedItems.push({ call: currentCall, result: item });
      currentCall = null;
    }
  }
  if (currentCall) {
    pairedItems.push({ call: currentCall });
  }

  return (
    <div className="space-y-1">
      {/* Collapsed/Summary card */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs text-zinc-600 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
      >
        <Icon className="h-4 w-4 shrink-0 text-zinc-400" />
        <span className="font-medium">
          {actionName} {count} {count === 1 ? "call" : "calls"}
        </span>
        <span className="truncate text-zinc-500 dark:text-zinc-500">
          {summary}
        </span>
        {expanded ? (
          <ChevronDown className="ml-auto h-4 w-4 shrink-0" />
        ) : (
          <ChevronRight className="ml-auto h-4 w-4 shrink-0" />
        )}
      </button>

      {/* Expanded details - paired call + result */}
      {expanded && (
        <div className="ml-4 space-y-1 border-l-2 border-zinc-200 pl-4 dark:border-zinc-700">
          {pairedItems.map((pair, idx) => {
            const isExpanded = expandedItem === idx;
            const { call, result } = pair;
            
            return (
              <div key={call.id} className="space-y-1">
                {/* Tool call row */}
                <button
                  onClick={() => setExpandedItem(isExpanded ? null : idx)}
                  className="flex w-full items-center gap-2 rounded-md bg-zinc-50 px-2 py-1.5 text-xs text-zinc-600 hover:bg-zinc-100 dark:bg-zinc-800/30 dark:text-zinc-400 dark:hover:bg-zinc-800"
                >
                  <Wrench className="h-3 w-3 shrink-0" />
                  <span className="font-medium">{call.toolName}</span>
                  <span className="min-w-0 flex-1 break-all text-zinc-500 dark:text-zinc-500">
                    {(() => {
                      try {
                        const params = JSON.parse(call.content || "{}");
                        if (toolName === "file_read" || toolName === "file_write" || toolName === "file_edit") {
                          return params.path || call.content.substring(0, 60);
                        } else if (toolName === "shell") {
                          return params.command || call.content.substring(0, 60);
                        }
                        return call.content.substring(0, 60);
                      } catch {
                        return call.content.substring(0, 60);
                      }
                    })()}
                    {call.content.length > 60 ? "..." : ""}
                  </span>
                  {isExpanded ? (
                    <ChevronDown className="h-3 w-3 shrink-0" />
                  ) : (
                    <ChevronRight className="h-3 w-3 shrink-0" />
                  )}
                </button>

                {/* Expanded details */}
                {isExpanded && (
                  <div className="ml-5 space-y-2">
                    {/* Call details */}
                    <div>
                      <div className="mb-1 text-[10px] font-medium text-zinc-500">Arguments:</div>
                      <pre className="overflow-x-auto rounded bg-zinc-50 p-2 text-[10px] text-zinc-600 dark:bg-zinc-800/50 dark:text-zinc-400">
                        {call.content}
                      </pre>
                    </div>
                    
                    {/* Result if exists */}
                    {result && (
                      <div>
                        <div className="mb-1 flex items-center gap-1 text-[10px] font-medium text-emerald-600 dark:text-emerald-400">
                          <Check className="h-3 w-3" />
                          Result ({result.content.length} chars)
                        </div>
                        <pre className="overflow-x-auto rounded bg-emerald-50/30 p-2 text-[10px] text-zinc-600 dark:bg-emerald-900/10 dark:text-zinc-400">
                          {result.content.length > 500 
                            ? result.content.substring(0, 500) + "\n\n... (truncated)"
                            : result.content
                          }
                        </pre>
                      </div>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

/** Wrapper that provides right-click context menu for copying text */
function MessageContentWrapper({ children }: { children: React.ReactNode }) {
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const wrapperRef = useRef<HTMLDivElement>(null);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const selection = window.getSelection();
    const selectedText = selection?.toString().trim();
    
    // Only show context menu if there's selected text
    if (selectedText) {
      setContextMenu({ x: e.clientX, y: e.clientY });
    }
  }, []);

  const handleCopy = useCallback(async () => {
    const selection = window.getSelection();
    const selectedText = selection?.toString();
    if (selectedText) {
      try {
        await navigator.clipboard.writeText(selectedText);
      } catch (err) {
        // Fallback for older browsers
        const textArea = document.createElement("textarea");
        textArea.value = selectedText;
        textArea.style.position = "fixed";
        textArea.style.left = "-9999px";
        document.body.appendChild(textArea);
        textArea.select();
        document.execCommand("copy");
        document.body.removeChild(textArea);
      }
    }
    setContextMenu(null);
  }, []);

  // Close context menu on outside click (but not on right-click)
  useEffect(() => {
    if (!contextMenu) return;
    
    const handleClick = (e: MouseEvent) => {
      // Check if click is outside the context menu
      const target = e.target as Node;
      if (wrapperRef.current && !wrapperRef.current.contains(target)) {
        setContextMenu(null);
      }
    };
    
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setContextMenu(null);
      }
    };
    
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [contextMenu]);

  return (
    <>
      <div ref={wrapperRef} onContextMenu={handleContextMenu}>{children}</div>
      {contextMenu && (
        <div
          ref={wrapperRef}
          className="fixed z-[100] min-w-[120px] rounded-lg border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onContextMenu={(e) => e.stopPropagation()}
        >
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            onClick={handleCopy}
          >
            <Copy size={14} />
            <span>复制</span>
          </button>
        </div>
      )}
    </>
  );
}

/** Single message bubble */
function MessageBubble({ message, isStreaming }: { message: ChatMessage; isStreaming: boolean }) {
  const [expanded, setExpanded] = useState(false);

  if (message.type === "user") {
    return (
      <MessageContentWrapper>
        <div className="flex justify-end">
          <div className="max-w-[70%] rounded-lg rounded-br-sm bg-zinc-800 px-3 py-2 text-sm text-white dark:bg-zinc-700 select-text">
            {message.content}
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "assistant") {
    const { thinkContent, replyContent, thinkClosed } = parseThinkContent(message.content);
    const hasReplyStarted = thinkClosed && replyContent.length > 0;
    const showPlaceholder = !message.content;

    return (
      <MessageContentWrapper>
        <div className="flex justify-start">
          <div className="max-w-[85%] rounded-lg rounded-bl-sm bg-zinc-100 px-3 py-2 text-sm dark:bg-zinc-800 dark:text-zinc-200 select-text">
            {thinkContent !== null && (
              <ThinkBlock
                content={thinkContent}
                isStreaming={isStreaming}
                hasReplyStarted={hasReplyStarted}
              />
            )}
            {replyContent && (
              <div className={`prose prose-sm prose-zinc max-w-none dark:prose-invert ${thinkContent !== null ? "mt-2" : ""} select-text`}>
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{replyContent}</ReactMarkdown>
              </div>
            )}
            {showPlaceholder && (
              <span className="text-zinc-400">Thinking...</span>
            )}
            {isStreaming && <span className="ml-0.5 inline-block animate-pulse">▌</span>}
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "system") {
    return (
      <MessageContentWrapper>
        <div className="flex justify-center">
          <div className="rounded bg-zinc-100 px-3 py-1 text-xs text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400 select-text">
            {message.content}
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "tool_call") {
    return (
      <div className="flex justify-start">
        <button
          className="flex w-full max-w-[85%] items-start gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
          onClick={() => setExpanded(!expanded)}
        >
          <Wrench className="mt-0.5 h-3 w-3 shrink-0" />
          <span className="font-medium">{message.toolName}</span>
          <span className="min-w-0 break-all text-zinc-400 dark:text-zinc-500">{message.content}</span>
          {expanded ? <ChevronDown className="ml-auto h-3 w-3 shrink-0" /> : <ChevronRight className="ml-auto h-3 w-3 shrink-0" />}
        </button>
      </div>
    );
  }

  if (message.type === "tool_result") {
    return (
      <MessageContentWrapper>
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
            <pre className="mt-1 max-w-[85%] overflow-x-auto rounded-lg bg-zinc-50 p-3 text-xs text-zinc-600 dark:bg-zinc-800/50 dark:text-zinc-400 select-text">
              {message.content}
            </pre>
          )}
        </div>
      </MessageContentWrapper>
    );
  }

  return null;
}

/** Popup-style model selector with provider shown in gray */
function ModelMenu({
  models,
  currentModel,
  onSelect,
}: {
  models: string[];
  currentModel: string | null;
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

              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
