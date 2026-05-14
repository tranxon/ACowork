import { useEffect, useLayoutEffect, useRef, useState, useCallback, useMemo } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useSessionStore } from "../../stores/sessionStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentProfileStore } from "../../stores/agentProfileStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { needsApiKey, keyPlaceholder } from "../../lib/providers";
import { fetchProviderModels, fetchProviders } from "../../lib/gateway-api";
import { toolbarButton, toolbarButtonActive } from "../../lib/ui-styles";
import { Bot, Play, Send, ChevronDown, ChevronRight, Wrench, AlertTriangle, X, Square, Copy, Plus, RefreshCw, Layers, Cpu, Loader } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage, ContextUsageInfo, VaultKeyEntry, ModelInfo } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";
import { ExploreBlock } from "./ExploreBlock";
import { MemoryPanel } from "../memory/MemoryPanel";
import { SessionPanel } from "./SessionPanel";
import { SkillsPanel } from "../skills/SkillsPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";

// Module-level: persists across ChatPanel mount/unmount cycles
// so nav-back (Settings→Chat) doesn't trigger full reinit
let lastInitAgentId: string | null = null;
/** Tracks the last session ID for which messages were loaded.
 *  Prevents redundant reload when remounting after navigation. */
let lastLoadedSessionId: string | null = null;

export function ChatPanel() {
  const { agents, selectedAgentId, startAgent } = useAgentStore();

  // Per-agent state selectors — read from agentStates[selectedAgentId]
  const agentState = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId] : null);
  const messages = agentState?.messages ?? [];
  const streamingMessageId = agentState?.streamingMessageId ?? null;
  const thinkingMessageId = agentState?.thinkingMessageId ?? null;
  const contextUsage = agentState?.contextUsage ?? null;
  const iterationLimitPaused = agentState?.iterationLimitPaused ?? null;
  const isReasoning = agentState?.isReasoning ?? false;

  // Global state and actions
  const { sending, wsMap, connectStream, sendMessage, sendInterrupt, currentModel, currentProvider, availableModels, setCurrentModel, setAvailableModels, loadAgentModel, continueExecution } = useChatStore();
  const isLoadingSession = agentState?.isLoadingSession ?? false;
  const loadError = agentState?.loadError ?? null;
  const currentSessionId = useSessionStore((s) => s.currentSessionId);
  const gatewayStatus = useGatewayStore((s) => s.status);
  const { activeSkill, clearActiveSkill } = useSkillStore();
  const [inputValue, setInputValue] = useState("");
  const [queuedMessage, setQueuedMessage] = useState<string | null>(null);
  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking
  const [activeDrawer, setActiveDrawer] = useState<"memory" | null>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const messagesContainerRef = useRef<HTMLDivElement>(null);
  const prevScrollHeightRef = useRef<number>(0);
  const isLoadingMoreRef = useRef<boolean>(false);
  const isInitialLoadRef = useRef<boolean>(false);
  const initAbortedRef = useRef(false);
  /** Tracks previous running state to detect genuine agent stop vs transient remount. */
  const wasRunningRef = useRef(false);
  /** True only during the first useEffect run of this mount. */
  const justMountedRef = useRef(true);

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);

  // Group consecutive messages for display
  // - Consecutive think + tool_call + tool_result → explore_group (aggregated)
  // - assistant reply (non-think) → display as-is
  // - Everything else → display as-is
  const displayMessages = useMemo(() => {
    const grouped: Array<
      | ChatMessage
      | { type: 'explore_group'; items: ChatMessage[] }
    > = [];
    
    let exploreBuffer: ChatMessage[] = [];

    const flushExplore = () => {
      if (exploreBuffer.length > 0) {
        grouped.push({ type: 'explore_group', items: [...exploreBuffer] });
        exploreBuffer = [];
      }
    };

    for (const msg of messages) {
      if (msg.type === 'tool_call' || msg.type === 'tool_result') {
        exploreBuffer.push(msg);
      } else if (msg.type === 'thought') {
        exploreBuffer.push(msg);
      } else if (msg.type === 'assistant') {
        // Streaming: if content starts with a think tag but no closing tag yet,
        // treat entire content as thinking
        if (msg.id === streamingMessageId) {
          const trimmed = msg.content.trimStart();
          if (trimmed.startsWith('<think>') && !trimmed.includes('</think>')) {
            const thinkContent = trimmed.slice(7);
            if (thinkContent) {
              exploreBuffer.push({ ...msg, type: 'thought' as any, content: thinkContent });
            }
            continue;
          }
        }

        const { thinkContent, replyContent } = parseThinkContent(msg.content);
        if (thinkContent) {
          exploreBuffer.push({ ...msg, type: 'thought' as any, content: thinkContent });
        }
        if (replyContent.trim()) {
          flushExplore();
          grouped.push({ ...msg, content: replyContent });
        } else if (!thinkContent) {
          // Empty message (streaming)
          flushExplore();
          grouped.push(msg);
        }
      } else {
        // user message or other
        flushExplore();
        grouped.push(msg);
      }
    }

    flushExplore();
    return grouped;
  }, [messages, streamingMessageId]);
  
  // Virtual scrolling: only render visible messages
  const virtualizer = useVirtualizer({
    count: displayMessages.length,
    getScrollElement: () => messagesContainerRef.current,
    estimateSize: () => 80,
    overscan: 5,
    gap: 4,
  });

  // Load available models from Vault keys
  const loadModels = useCallback(async () => {
    try {
      const keys = await invoke<VaultKeyEntry[]>("list_keys");
      const allModels: { name: string; provider: string }[] = [];
      for (const key of keys) {
        const provider = key.provider;
        if (key.models?.length) {
          for (const model of key.models) {
            allModels.push({ name: model, provider });
          }
        } else if (key.default_model) {
          allModels.push({ name: key.default_model, provider });
        }
      }
      // Deduplicate by model name + provider
      const uniqueModels = allModels.filter(
        (m, i, arr) => arr.findIndex(x => x.name === m.name && x.provider === m.provider) === i
      );
      setAvailableModels(uniqueModels);
      setHasLlmConfig(keys.length > 0);
    } catch {
      // Gateway may not be running
    }
  }, [setAvailableModels]);

  useEffect(() => {
    loadModels();
  }, [gatewayStatus, loadModels]);

  // Listen for models-added event from AddModelDialog
  useEffect(() => {
    const handler = () => loadModels();
    window.addEventListener('models-added', handler);
    return () => window.removeEventListener('models-added', handler);
  }, [loadModels]);

  // Connect WebSocket when agent changes + restore per-agent model + init session
  useEffect(() => {
    // Skip re-init if this agent was already initialized and is still running.
    // This prevents redundant clearMessages + reload when selectedAgent.running
    // is re-evaluated without actually changing (e.g. agent list refresh).
    if (selectedAgentId && selectedAgentId === lastInitAgentId && selectedAgent?.running) {
      return;
    }

    // DEFENSIVE: If we reached here for the SAME agent (e.g. remount after
    // Settings→Chat navigation where selectedAgent?.running was temporarily
    // falsy), skip all destructive init to preserve WebSocket-streamed messages.
    const isSameAgentRemount = !!(selectedAgentId && selectedAgentId === lastInitAgentId);

    // Remember the current session for the agent we're leaving (saved in store for remount survival)
    const leavingAgentId = lastInitAgentId;
    const leavingSessionId = useSessionStore.getState().currentSessionId;
    if (leavingAgentId && leavingSessionId) {
      useSessionStore.getState().saveSessionForAgent(leavingAgentId, leavingSessionId);
    }

    if (!isSameAgentRemount) {
      // Allow reload for new agent/session
      lastLoadedSessionId = null;
      // Reset session state (sessions, currentSessionId, etc.)
      useSessionStore.getState().reset();
    }

    if (selectedAgentId && selectedAgent?.running) {
      if (!isSameAgentRemount) {
        lastInitAgentId = selectedAgentId;
        connectStream(selectedAgentId, getGatewayUrl());
      }
      // Load model from Gateway API FIRST (reads per-agent .agent_model.json),
      // THEN reload the model list. This ensures currentModel is set before
      // setAvailableModels runs, preventing the fallback-to-first-model bug.
      const initModel = async () => {
        await loadAgentModel(selectedAgentId);
        loadModels();
      };
      void initModel();

      if (!isSameAgentRemount) {
        // Only load session messages on first switch to this agent if no messages yet.
        // Per-agent state preserves messages across remounts, so skip reload if already loaded.
        const agentMessages = useChatStore.getState().agentStates[selectedAgentId]?.messages;
        if (!agentMessages || agentMessages.length === 0) {
          // 3. Fetch sessions and 4. restore previously selected session (or latest)
          const initSession = async () => {
            isInitialLoadRef.current = true;
            initAbortedRef.current = false;

            // Retry fetching sessions until Agent is ready (max 10 attempts, 1s interval)
            const maxRetries = 10;
            let sessions = useSessionStore.getState().sessions;

            for (let i = 0; i < maxRetries; i++) {
              if (initAbortedRef.current) return;
              await useSessionStore.getState().fetchSessions(selectedAgentId);
              sessions = useSessionStore.getState().sessions;
              if (sessions.length > 0) break;
              if (i < maxRetries - 1) {
                await new Promise(resolve => setTimeout(resolve, 1000));
              }
            }

            if (initAbortedRef.current) return;

            if (sessions.length === 0) {
              isInitialLoadRef.current = false;
              return;
            }

            // Restore previously selected session for this agent, fallback to latest
            const rememberedSessionId = useSessionStore.getState().agentSessionMap[selectedAgentId];
            const targetSession = rememberedSessionId
              ? sessions.find((s) => s.session_id === rememberedSessionId) ?? sessions[0]
              : sessions[0];
            if (targetSession) {
              await useChatStore
                .getState()
                .loadSessionMessages(selectedAgentId, targetSession.session_id);
              await useSessionStore.getState().switchSession(targetSession.session_id, selectedAgentId);
            }
            isInitialLoadRef.current = false;
          };
          void initSession();
        }
      }
    } else if (!isSameAgentRemount) {
      lastInitAgentId = null;
    }
    // Only reset lastInitAgentId on genuine agent stop (running true→false
    // within the same component lifecycle), not during a remount transient.
    if (!selectedAgent?.running && wasRunningRef.current && !justMountedRef.current) {
      lastInitAgentId = null;
    }
    wasRunningRef.current = selectedAgent?.running ?? false;
    justMountedRef.current = false;
    return () => {
      initAbortedRef.current = true;
      // Do NOT disconnect the old agent's ws — keep it alive for reuse.
      // Only clear reconnect timers for the old agent to avoid stale reconnects.
      // The ws connections are per-agent and managed in wsMap.
    };
  }, [selectedAgentId, selectedAgent?.running, connectStream, loadAgentModel, loadModels]);

  // Load messages when active session changes (from SessionPanel or createSession)
  useEffect(() => {
    if (!currentSessionId || !selectedAgentId) return;

    // Skip if agent initialization is in progress — initSession already calls loadSessionMessages
    if (isInitialLoadRef.current) return;

    // Guard: only proceed if this session belongs to the current agent's session list.
    const session = useSessionStore
      .getState()
      .sessions.find((s) => s.session_id === currentSessionId);
    if (!session) return;

    // If this session was already loaded (e.g. returning from Settings navigation
    // while WebSocket was still streaming), skip reload to avoid overwriting
    // in-flight messages.
    if (currentSessionId === lastLoadedSessionId) return;
    lastLoadedSessionId = currentSessionId;

    // Mark as initial load to trigger scroll-to-bottom after messages are loaded
    isInitialLoadRef.current = true;
    void useChatStore
      .getState()
      .loadSessionMessages(selectedAgentId, currentSessionId)
      .finally(() => {
        isInitialLoadRef.current = false;
      });
  }, [currentSessionId, selectedAgentId]);

  // Initial load / agent switch / session switch: scroll to bottom synchronously before paint
  const prevDisplayCountRef = useRef(0);
  const prevScrollAgentRef = useRef<string | null>(null);
  const prevScrollSessionRef = useRef<string | null>(null);
  useLayoutEffect(() => {
    // Reset count tracking when agent OR session changes, so we jump to bottom
    // (instead of smooth-scrolling, or failing to scroll when new count <= old count)
    if (
      prevScrollAgentRef.current !== selectedAgentId ||
      prevScrollSessionRef.current !== currentSessionId
    ) {
      prevDisplayCountRef.current = 0;
      prevScrollAgentRef.current = selectedAgentId ?? null;
      prevScrollSessionRef.current = currentSessionId ?? null;
    }
    const prevCount = prevDisplayCountRef.current;

    if (isLoadingMoreRef.current) {
      // Loading more: restore scroll position to keep view stable
      if (prevScrollHeightRef.current > 0 && displayMessages.length > 0) {
        const prevOffset = prevScrollHeightRef.current;
        prevScrollHeightRef.current = 0;
        isLoadingMoreRef.current = false;
        virtualizer.scrollToOffset(prevOffset, { align: "start" });
      }
      prevDisplayCountRef.current = displayMessages.length;
      return;
    }

    if (displayMessages.length > 0) {
      if (prevCount === 0) {
        // Agent switch or initial load: jump to bottom instantly (before paint)
        virtualizer.scrollToIndex(displayMessages.length - 1, { align: "end" });
      } else if (displayMessages.length > prevCount) {
        // New message arrived: smooth scroll to bottom
        virtualizer.scrollToIndex(displayMessages.length - 1, { align: "end", behavior: "smooth" });
      }
    }

    prevDisplayCountRef.current = displayMessages.length;
  }, [messages, displayMessages.length, virtualizer, selectedAgentId, currentSessionId]);

  // Scroll handler: load more messages when scrolled to top
  const handleScroll = useCallback(() => {
    const container = messagesContainerRef.current;
    if (!container || !selectedAgentId) return;

    const { isLoadingMore } = useChatStore.getState();
    const agentState = useChatStore.getState().agentStates[selectedAgentId];
    const hasMoreMessages = agentState?.hasMoreMessages ?? false;
    const currentSessionId = useSessionStore.getState().currentSessionId;
    if (isLoadingMore || !hasMoreMessages || !currentSessionId) return;

    // Trigger when within 50px of the top
    if (container.scrollTop < 50) {
      // Store current scroll offset for position restoration after prepending messages
      prevScrollHeightRef.current = virtualizer.scrollOffset ?? 0;
      isLoadingMoreRef.current = true;
      void useChatStore
        .getState()
        .loadMoreMessages(selectedAgentId, currentSessionId);
    }
  }, [selectedAgentId, virtualizer]);

  const handleSend = () => {
    const content = inputValue.trim();
    if (!content || sending || !selectedAgentId) return;
    // sendMessage is async but we fire-and-forget here —
    // the store handles all state updates internally
    void sendMessage(content, selectedAgentId, activeSkill?.name).then(() => {
      clearActiveSkill();
    });
    setInputValue("");
  };

  // Stop button dual-action:
  //   input has content → send to queue (no interrupt, message waits for next loop)
  //   input empty       → interrupt current loop
  const handleStop = () => {
    const content = inputValue.trim();
    if (content && selectedAgentId) {
      // First click: queue the message — do NOT send yet.
      // The message waits in a banner above the input area.
      setQueuedMessage(content);
      setInputValue("");
    } else if (queuedMessage && selectedAgentId) {
      // Second click: commit queued message + interrupt current loop.
      // The message enters the backend inbound channel, then interrupt
      // stops the current loop so the queued message is picked up next.
      void sendMessage(queuedMessage, selectedAgentId, activeSkill?.name).then(() => {
        clearActiveSkill();
      });
      setQueuedMessage(null);
      sendInterrupt();
    } else {
      // No queued message: just interrupt
      sendInterrupt();
    }
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
  const inputDisabled = gatewayStatus !== "connected";

  return (
    <div
      className="flex flex-1 flex-col bg-[#FAFAFA] dark:bg-zinc-900"
    >
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
        <div
          ref={messagesContainerRef}
          onScroll={handleScroll}
          className="h-full overflow-y-auto px-4 py-3 select-text cursor-text"
          role="log"
          aria-label="Chat messages"
        >
          {/* Loading more indicator at top */}
          {useChatStore.getState().isLoadingMore && (
            <div className="flex items-center justify-center py-2">
              <Loader className="h-4 w-4 animate-spin text-zinc-400 dark:text-zinc-500" />
              <span className="ml-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">Loading more...</span>
            </div>
          )}
          
          {/* Loading session indicator */}
          {isLoadingSession && messages.length === 0 && (
            <div className="flex h-full items-center justify-center">
              <div className="text-center">
                <Loader className="mx-auto h-8 w-8 animate-spin text-zinc-400 dark:text-zinc-500" />
                <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">Loading conversation...</p>
              </div>
            </div>
          )}
          
          {loadError && !isLoadingSession && (
            <div className="flex h-full flex-col items-center justify-center gap-3 px-4">
              <div className="text-sm text-red-500 dark:text-red-400">Session 加载失败</div>
              <div className="max-w-xs text-center text-xs text-zinc-500 dark:text-zinc-400">
                {loadError}
              </div>
              <button
                onClick={() => {
                  const sessionId = useSessionStore.getState().currentSessionId;
                  const agentId = useAgentStore.getState().selectedAgentId;
                  if (sessionId && agentId) {
                    useChatStore.getState().loadSessionMessages(agentId, sessionId);
                  }
                }}
                className="rounded-md bg-zinc-100 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
              >
                重试
              </button>
            </div>
          )}
          {!loadError && !isLoadingSession && messages.length === 0 && (
            <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
              Start a conversation with {selectedAgent.name}
            </div>
          )}
          {/* Virtualized message list — only renders visible items */}
          {displayMessages.length > 0 && (
            <div
              style={{
                height: virtualizer.getTotalSize(),
                width: '100%',
                position: 'relative',
              }}
            >
              {virtualizer.getVirtualItems().map((virtualRow) => {
                const item = displayMessages[virtualRow.index];
                const displayItem = item as any;

                return (
                  <div
                    key={virtualRow.key}
                    ref={virtualizer.measureElement}
                    data-index={virtualRow.index}
                    style={{
                      position: 'absolute',
                      top: 0,
                      left: 0,
                      width: '100%',
                      transform: `translateY(${virtualRow.start}px)`,
                    }}
                    className=""
                  >
                    {/* Explore group - aggregated think + tool calls/results */}
                    {displayItem.type === 'explore_group' && (
                      <ExploreBlock
                        items={displayItem.items}
                        isStreaming={displayItem.items.some(
                          (m: ChatMessage) => m.id === streamingMessageId || m.id === thinkingMessageId
                        )}
                      />
                    )}

                    {/* Regular message */}
                    {displayItem.type !== 'explore_group' && (
                      <MessageBubble message={item as ChatMessage} isStreaming={(item as ChatMessage).id === streamingMessageId} agentId={selectedAgentId ?? ""} />
                    )}
                  </div>
                );
              })}
            </div>
          )}
          {/* LLM reasoning indicator — pulsing "..." shown while waiting for first token */}
          {isReasoning && !streamingMessageId && !thinkingMessageId && (
            <div className="flex items-center gap-2.5 px-4 py-1.5">
              <span className="animate-pulse text-base font-bold tracking-widest text-zinc-400 dark:text-zinc-500">. . .</span>
            </div>
          )}
          {/* Iteration limit pause — hint + Continue button */}
          {iterationLimitPaused && (
            <div className="flex flex-col items-start gap-1.5">
              <span
                className="text-amber-700 dark:text-amber-300/80"
                style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}
              >
                {iterationLimitPaused.message}
              </span>
              <button
                onClick={() => {
                  if (selectedAgentId) {
                    continueExecution(selectedAgentId);
                  }
                }}
                className="flex w-fit max-w-full items-center gap-2 rounded-md bg-amber-100 px-2.5 py-1.5 text-amber-800 transition-colors hover:bg-amber-200 dark:bg-amber-800/40 dark:text-amber-200 dark:hover:bg-amber-800/60"
                style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
              >
                <Play className="h-3.5 w-3.5" />
                <span>
                  Continue ({iterationLimitPaused.iteration}/{iterationLimitPaused.maxIterations})
                </span>
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
                <span className="font-medium text-sm text-zinc-900 dark:text-zinc-100">Memory</span>
                <button
                  className="rounded-md p-1 text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                  onClick={() => setActiveDrawer(null)}
                >
                  <X size={16} />
                </button>
              </div>
              {activeDrawer === "memory" && <MemoryPanel />}
            </div>
          </div>
        )}
      </div>

      {/* Unified input container with toolbar */}
      <div className="mx-3 mb-3 rounded-xl border border-zinc-200 dark:border-zinc-700 bg-zinc-50 dark:bg-zinc-800/50">
        {/* Active skill badge */}
        {activeSkill && (
          <div className="flex items-center gap-1 px-3 pt-2">
            <span className="inline-flex items-center gap-1 rounded bg-blue-50 px-1.5 py-0.5 text-xs font-medium text-blue-700 border border-blue-200 dark:bg-blue-900/30 dark:text-blue-300 dark:border-blue-800">
              /{activeSkill.name}
              <button
                type="button"
                onClick={clearActiveSkill}
                className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-blue-100 dark:hover:bg-blue-800"
                aria-label="Clear active skill"
              >
                <X size={12} />
              </button>
            </span>
          </div>
        )}
        {/* Queued message banner — shown when user clicked Stop with input content */}
        {queuedMessage && sending && (
          <div className="flex items-center justify-between gap-2 px-3 pt-2 pb-0">
            <div className="flex items-center gap-1.5 min-w-0">
              <span className="shrink-0 text-[10px] text-blue-500 dark:text-blue-400">⏳</span>
              <span className="truncate text-xs text-blue-600 dark:text-blue-400">
                Queued: {queuedMessage}
              </span>
            </div>
            <button
              type="button"
              onClick={() => setQueuedMessage(null)}
              className="shrink-0 rounded-sm p-0.5 text-zinc-400 hover:text-zinc-600 hover:bg-zinc-200 dark:hover:text-zinc-300 dark:hover:bg-zinc-700"
              aria-label="Dismiss queued message"
            >
              <X size={12} />
            </button>
          </div>
        )}
        {/* Textarea area — borderless, transparent background */}
        <textarea
          value={inputValue}
          onChange={(e) => setInputValue(e.target.value)}
          placeholder={
            gatewayStatus !== "connected"
              ? "Gateway not connected"
              : !wsMap[selectedAgentId!] || wsMap[selectedAgentId!].readyState !== WebSocket.OPEN
                ? activeSkill
                  ? "输入参数... (HTTP mode — streaming unavailable)"
                  : "Type a message... (HTTP mode — streaming unavailable)"
                : activeSkill
                  ? "输入参数... (Enter to send, Shift+Enter for new line)"
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
                onSelect={(m, p) => selectedAgentId && setCurrentModel(m, p, selectedAgentId)}
              />
            )}
            {/* Workspace button */}
            <WorkspaceSelector />
            {/* Session button */}
            {selectedAgentId && <SessionPanel agentId={selectedAgentId} onOpenMemory={() => setActiveDrawer("memory")} />}
            {/* Skills dropdown */}
            <SkillsPanel />
          </div>

          {/* Right: context usage + send/stop button */}
          <div className="flex items-center gap-1.5">
            {/* Context usage tooltip */}
            {contextUsage && <ContextUsageTooltip usage={contextUsage} />}

            {/* Send/Stop button with tooltip above */}
            <div className="group relative">
              <button
                className={`rounded-lg p-1.5 transition-colors ${
                  sending
                    ? "text-blue-500 hover:bg-blue-100 dark:hover:bg-blue-900/30 hover:text-blue-600 dark:hover:text-blue-400"
                    : "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200 disabled:opacity-50"
                }`}
                onClick={sending ? handleStop : handleSend}
                disabled={!sending && (inputDisabled || !inputValue.trim())}
                aria-label={sending ? (inputValue.trim() || queuedMessage ? "Send to queue" : "Stop generation") : "Send message"}
              >
                {sending ? <Square size={16} fill="currentColor" /> : <Send size={16} />}
              </button>
              {/* Tooltip — shown above the button on hover */}
              <div className="pointer-events-none absolute bottom-full left-1/2 -translate-x-1/2 mb-1.5 hidden group-hover:block z-50">
                <div className="whitespace-nowrap rounded-md bg-zinc-800 dark:bg-zinc-200 px-2.5 py-1.5 text-[11px] leading-tight text-white dark:text-zinc-800 shadow-lg">
                  {sending
                    ? (inputValue.trim()
                        ? "Send to queue"
                        : queuedMessage
                          ? "Stop & send queued"
                          : "Stop")
                    : "Send message"}
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

/** Format token count for display (e.g. 1.2K, 128K, 200K) */
function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

/** Context usage tooltip — icon button in input toolbar, shows usage on hover */
function ContextUsageTooltip({ usage }: { usage: ContextUsageInfo }) {
  const percent = usage.usage_percent;
  const total = formatTokenCount(usage.total_tokens);
  const usable = formatTokenCount(usage.usable_context);

  // Color coding based on usage percentage
  const iconColor =
    percent >= 90
      ? "text-red-400 hover:text-red-300"
      : percent >= 70
        ? "text-amber-400 hover:text-amber-300"
        : "text-emerald-400 hover:text-emerald-300";

  const barColor =
    percent >= 90
      ? "bg-red-500"
      : percent >= 70
        ? "bg-amber-500"
        : "bg-emerald-500";

  return (
    <div className="group relative">
      {/* Icon button */}
      <button
        className={`inline-flex items-center gap-1 rounded-lg px-2 py-1 text-xs transition-colors ${iconColor} hover:bg-zinc-200 dark:hover:bg-zinc-700`}
        aria-label="Context usage"
      >
        <Layers size={14} />
        <span className="font-medium">{percent}%</span>
      </button>

      {/* Tooltip popup — appears on hover */}
      <div className="absolute bottom-full right-0 mb-2 w-56 rounded-xl border border-zinc-200/80 bg-white px-4 py-3 shadow-xl opacity-0 invisible group-hover:opacity-100 group-hover:visible transition-all duration-200 z-50 dark:border-zinc-700/50 dark:bg-zinc-800">
        {/* Header */}
        <div className="flex items-center justify-between mb-3">
          <span className="text-sm font-semibold text-zinc-800 dark:text-zinc-100">Context Usage</span>
          <span className={`text-xs font-medium ${
            percent >= 90 ? "text-red-500 dark:text-red-400" : percent >= 70 ? "text-amber-500 dark:text-amber-400" : "text-emerald-500 dark:text-emerald-400"
          }`}>
            {percent}%
          </span>
        </div>

        {/* Stats */}
        <div className="space-y-2 mb-3">
          <div className="flex justify-between text-xs">
            <span className="text-zinc-500 dark:text-zinc-400">Used</span>
            <span className="text-zinc-700 font-mono dark:text-zinc-200">{total} tokens</span>
          </div>
          <div className="flex justify-between text-xs">
            <span className="text-zinc-500 dark:text-zinc-400">Available</span>
            <span className="text-zinc-700 font-mono dark:text-zinc-200">{usable} tokens</span>
          </div>
          <div className="flex justify-between text-xs">
            <span className="text-zinc-500 dark:text-zinc-400">Context Window</span>
            <span className="text-zinc-700 font-mono dark:text-zinc-200">{formatTokenCount(usage.context_window)}</span>
          </div>
        </div>

        {/* Progress bar */}
        <div className="h-1.5 rounded-full bg-zinc-200 overflow-hidden dark:bg-zinc-700">
          <div
            className={`h-full rounded-full transition-all duration-300 ${barColor}`}
            style={{ width: `${percent}%` }}
          />
        </div>

        {/* Arrow pointer */}
        <div className="absolute -bottom-1 right-6 w-2 h-2 rotate-45 bg-white border-r border-b border-zinc-200/80 dark:bg-zinc-800 dark:border-zinc-700/50" />
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

/** Shell tools (bash, powershell, shell) need Terminal icon and command preview. */

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
function MessageBubble({ message, isStreaming, agentId }: { message: ChatMessage; isStreaming: boolean; agentId: string }) {
  const [expanded, setExpanded] = useState(false);
  // Use CSS custom property for font size — set once in store, global effect
  const fontSizeStyle = { fontSize: "var(--ui-font-size, 0.875rem)" };
  // Agent icon from profile settings
  const agentIconId = useAgentProfileStore((s) => s.profiles[agentId]?.avatarIconId);
  // Live names — subscribe to profile stores so name edits update all messages instantly
  // (instead of relying on the senderDisplayName snapshot captured at message creation time)
  const userDisplayName = useUserProfileStore((s) => s.profile.displayName);
  const agentProfileName = useAgentProfileStore((s) => s.profiles[agentId]?.displayName);
  const agentInfo = useAgentStore((s) => s.agents.find((a) => a.agent_id === agentId));
  const liveAgentName = agentProfileName ?? agentInfo?.display_name ?? agentInfo?.name ?? message.senderDisplayName;
  const liveUserName = userDisplayName ?? message.senderDisplayName;

  if (message.type === "user") {
    return (
      <MessageContentWrapper>
        <div className="flex items-start justify-end gap-2">
          <div className="min-w-0 flex-1 flex flex-col items-end">
            {liveUserName && (
              <span className="mb-1 text-xs text-zinc-400 dark:text-zinc-500">{liveUserName}</span>
            )}
            <div className="max-w-[85%] rounded-lg rounded-br-sm bg-[#9DF29F] px-4 py-2.5 text-zinc-900 select-text" style={fontSizeStyle}>
              {message.content}
            </div>
          </div>
          <UserAvatar
            displayName={liveUserName}
            size={40}
            className="shrink-0 mt-1"
          />
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "assistant") {
    const showPlaceholder = !message.content;

    return (
      <MessageContentWrapper>
        <div className="flex items-start gap-2">
          <AgentAvatar
            agentId={agentId}
            displayName={liveAgentName}
            iconId={agentIconId}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5 mb-1">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-zinc-200 px-1 py-0 text-[10px] font-medium text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="w-full max-w-[var(--content-max-width)] rounded-lg rounded-bl-sm bg-zinc-100 px-4 py-2.5 dark:bg-zinc-800 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              {message.content && (
                <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text" style={fontSizeStyle}>
                  <ReactMarkdown remarkPlugins={[remarkGfm]}>{message.content}</ReactMarkdown>
                </div>
              )}
              {showPlaceholder && (
                <span className="text-zinc-400">Thinking...</span>
              )}
              {isStreaming && <span className="ml-0.5 inline-block animate-pulse">▌</span>}
            </div>
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "thought") {
    return (
      <MessageContentWrapper>
        <div className="flex items-start gap-2">
          <AgentAvatar
            agentId={agentId}
            displayName={liveAgentName}
            iconId={agentIconId}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5 mb-1">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-zinc-200 px-1 py-0 text-[10px] font-medium text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="w-full max-w-[var(--content-max-width)] rounded-lg rounded-bl-sm bg-zinc-100 px-4 py-2.5 dark:bg-zinc-800 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              <ThinkBlock
                content={message.content}
                isStreaming={isStreaming}
                hasReplyStarted={!isStreaming}
                startTime={message.startTime}
                endTime={message.endTime}
              />
            </div>
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
          className="flex w-fit max-w-[var(--content-max-width)] items-start gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
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
            className="flex w-full max-w-[var(--content-max-width)] items-center gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
            onClick={() => setExpanded(!expanded)}
          >
            <Wrench className="h-3 w-3 shrink-0" />
            <span className="font-medium">{message.toolName}</span>
            <span className="text-zinc-400 dark:text-zinc-500">→ Result</span>
            <span className="ml-auto text-[10px] text-zinc-400 dark:text-zinc-500">Click to view</span>
            {expanded ? <ChevronDown className="ml-2 h-3 w-3 shrink-0" /> : <ChevronRight className="ml-2 h-3 w-3 shrink-0" />}
          </button>
          {expanded && (
            <pre className="mt-1 max-w-[var(--content-max-width)] overflow-x-auto rounded-lg bg-zinc-50 p-3 text-xs text-zinc-600 dark:bg-zinc-800/50 dark:text-zinc-400 select-text">
              {message.content}
            </pre>
          )}
        </div>
      </MessageContentWrapper>
    );
  }

  return null;
}

/** Add Key dialog — exact copy from SettingsPage with provider as dropdown */
function AddModelDialog({
  open,
  onClose,
  onSuccess,
}: {
  open: boolean;
  onClose: () => void;
  onSuccess: () => void;
}) {
  const [provider, setProvider] = useState("minimax");
  const [key, setKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelSearchTerm, setModelSearchTerm] = useState("");
  const [modelCapabilityFilter, setModelCapabilityFilter] = useState<string[]>([]);
  const [contextWindow, setContextWindow] = useState("");
  const [maxOutputTokens, setMaxOutputTokens] = useState("");
  const [supportsToolCalling, setSupportsToolCalling] = useState(true);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);
  const [dynamicProviders, setDynamicProviders] = useState<Array<{ id: string; name: string; api?: string }>>([]);
  const [providersLoading, setProvidersLoading] = useState(false);
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);


  // Fetch existing keys to check which models are already added
  useEffect(() => {
    if (!open) return;
    const loadKeys = async () => {
      try {
        const result = await invoke<VaultKeyEntry[]>("list_keys");
        setKeys(result);
      } catch {
        // Ignore errors
      }
    };
    loadKeys();
  }, [open]);

  // Fetch dynamic providers from Gateway cache on open
  useEffect(() => {
    if (!open) return;
    const loadProviders = async () => {
      setProvidersLoading(true);
      try {
        const CACHE_KEY = "rollball_models_cache";
        const cachedData = localStorage.getItem(CACHE_KEY);
        if (cachedData) {
          const parsed = JSON.parse(cachedData);
          const providers = (parsed.providers || []).map((p: any) => ({
            id: p.id,
            name: p.name || p.id,
            api: p.api,
          }));
          if (providers.length > 0) {
            setDynamicProviders(providers);
            setProvidersLoading(false);
            return;
          }
        }
        const providers = await fetchProviders();
        setDynamicProviders(providers);
      } catch {
        setDynamicProviders([]);
      }
      setProvidersLoading(false);
    };
    loadProviders();
  }, [open]);

  // Fetch models when provider changes
  useEffect(() => {
    if (!open || !provider) return;
    const loadModels = async () => {
      setModelsLoading(true);
      try {
        const data = await fetchProviderModels(provider);
        setAvailableModels(data.models ?? []);
      } catch {
        setAvailableModels([]);
      }
      setModelsLoading(false);
    };
    loadModels();
  }, [provider, open]);

  // Reset state when provider changes
  useEffect(() => {
    if (!provider) return;
    const dynamicProvider = dynamicProviders.find((p) => p.id === provider);
    setBaseUrl(dynamicProvider?.api ?? "");
    setModels([]);
    setModelSearchTerm("");
    setModelCapabilityFilter([]);
    setContextWindow("");
    setMaxOutputTokens("");
    setSupportsToolCalling(true);
  }, [provider]);

  if (!open) return null;

  const toggleModel = (modelId: string, currentList: string[], setList: (v: string[]) => void) => {
    if (currentList.includes(modelId)) {
      setList(currentList.filter((m) => m !== modelId));
    } else {
      setList([...currentList, modelId]);
    }
  };

  const handleSave = async () => {
    if (needsApiKey(provider) && !key.trim()) {
      setTestResult({ success: false, message: "Please enter an API Key first" });
      return;
    }
    
    setSaving(true);
    setTesting(true);
    setTestResult(null);
    
    try {
      // First test the API key
      if (needsApiKey(provider)) {
        // Temporarily add the key
        await invoke("add_key", {
          provider,
          key,
          baseUrl: baseUrl || undefined,
        });
        
        // Try to fetch models to verify the key works
        await fetchProviderModels(provider);
        
        setTestResult({ success: true, message: "API Key is valid!" });
        
        // Remove the temporary key
        await invoke("remove_key", { provider });
      }
    } catch (e: any) {
      const errorMsg = e?.message || e?.toString() || "Test failed";
      setTestResult({ success: false, message: errorMsg });
      setTesting(false);
      setSaving(false);
      return;
    }
    
    setTesting(false);
    
    // Test passed, proceed with saving
    try {
      // Get effective values (prefer models.dev data if available)
      const primaryModel = models.length > 0 ? models[0] : "";
      const modelInfo = availableModels.find(m => m.id === primaryModel);
      const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
      const effectiveContextWindow = hasModelsDevData 
        ? (modelInfo?.context_window?.toString() ?? contextWindow)
        : contextWindow;
      const effectiveMaxOutputTokens = hasModelsDevData 
        ? (modelInfo?.max_tokens?.toString() ?? maxOutputTokens)
        : maxOutputTokens;
      const effectiveSupportsToolCalling = hasModelsDevData 
        ? (modelInfo?.tool_call ?? supportsToolCalling)
        : supportsToolCalling;
      
      // Rust requires context_window to be present (u64, not Option)
      // Default to 128000 if not specified (safe default for most models)
      const ctxWindow = effectiveContextWindow ? parseInt(effectiveContextWindow) : 128000;
      const maxOutTokens = effectiveMaxOutputTokens ? parseInt(effectiveMaxOutputTokens) : 0;
      
      const modelCapabilities = models.length > 0 ? {
        context_window: ctxWindow,
        max_output_tokens: maxOutTokens,
        supports_tool_calling: effectiveSupportsToolCalling,
      } : undefined;
      
      await invoke("add_key", {
        provider,
        key,
        baseUrl: baseUrl || undefined,
        models: models.length > 0 ? models : undefined,
        modelCapabilities,
      });
      onSuccess();
      onClose();
    } catch (e) {
      alert(`Failed to add: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  const showBaseUrl = true;

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-[440px] max-h-[85vh] overflow-hidden rounded-lg bg-white shadow-xl dark:bg-zinc-800 flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="shrink-0 px-6 pt-6 pb-3 text-sm font-semibold">Add Model</h3>

        <div className="flex-1 overflow-y-auto px-6">
          <div className="space-y-2">
          {/* Provider dropdown */}
          <div>
            <label className="mb-1 block text-xs text-zinc-500">Provider</label>
            {providersLoading ? (
              <div className="flex items-center gap-2 rounded-md border border-zinc-200 px-3 py-2 text-xs text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900">
                <RefreshCw className="h-3 w-3 animate-spin" />
                Loading providers...
              </div>
            ) : (
              <select
                value={provider}
                onChange={(e) => setProvider(e.target.value)}
                className="w-full appearance-none rounded-md border border-zinc-200 bg-white px-3 py-2 pr-8 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                style={{
                  backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                  backgroundPosition: 'right 0.5rem center',
                  backgroundRepeat: 'no-repeat',
                  backgroundSize: '1.5em 1.5em',
                }}
              >
                <optgroup label="All Providers">
                  {dynamicProviders.map((p) => (
                    <option key={p.id} value={p.id}>{p.name}</option>
                  ))}
                </optgroup>
              </select>
            )}
          </div>

          {/* API Key */}
          {needsApiKey(provider) && (
            <div>
              <label className="mb-1 block text-xs text-zinc-500">API Key</label>
              <input
                type="password"
                value={key}
                onChange={(e) => setKey(e.target.value)}
                placeholder={keyPlaceholder(provider)}
                className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
              />
            </div>
          )}

          {showBaseUrl && (
            <div>
              <label className="mb-1 block text-xs text-zinc-500">Base URL</label>
              <input
                type="text"
                value={baseUrl}
                onChange={(e) => setBaseUrl(e.target.value)}
                placeholder="https://..."
                className="w-full rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
              />
            </div>
          )}

          {/* Model selection */}
          <div>
            <label className="mb-1 block text-xs text-zinc-500">
              Model {models.length > 0 && <span className="text-blue-500">({models.length} selected)</span>}
            </label>
            
            {/* Capability filters */}
            <div className="mb-2 flex gap-2">
              <button
                onClick={() => setModelCapabilityFilter(
                  modelCapabilityFilter.includes('tool_call') 
                    ? modelCapabilityFilter.filter(f => f !== 'tool_call')
                    : [...modelCapabilityFilter, 'tool_call']
                )}
                className={cn(
                  "rounded px-2 py-0.5 text-[10px] font-medium",
                  modelCapabilityFilter.includes('tool_call')
                    ? "bg-blue-100 text-blue-700 dark:bg-blue-900 dark:text-blue-300"
                    : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                )}
              >
                🔧 Tool Calling
              </button>
              <button
                onClick={() => setModelCapabilityFilter(
                  modelCapabilityFilter.includes('reasoning') 
                    ? modelCapabilityFilter.filter(f => f !== 'reasoning')
                    : [...modelCapabilityFilter, 'reasoning']
                )}
                className={cn(
                  "rounded px-2 py-0.5 text-[10px] font-medium",
                  modelCapabilityFilter.includes('reasoning')
                    ? "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300"
                    : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                )}
              >
                🧠 Reasoning
              </button>
            </div>
            
            {/* Selected models as tags */}
            {models.length > 0 && (
              <div className="mb-1 flex flex-wrap gap-1">
                {models.map((m) => (
                  <span key={m} className="inline-flex items-center gap-1 rounded bg-blue-100 px-2 py-0.5 text-xs text-blue-700 dark:bg-blue-900 dark:text-blue-300">
                    {m}
                    <button onClick={() => setModels(models.filter((x) => x !== m))} className="text-blue-400 hover:text-blue-600">×</button>
                  </span>
                ))}
              </div>
            )}
            
            {/* Search */}
            <input
              type="text"
              value={modelSearchTerm}
              onChange={(e) => setModelSearchTerm(e.target.value)}
              placeholder="Search models..."
              className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
            />
            
            {/* Model list */}
            <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
              {modelsLoading ? (
                <div className="px-3 py-2 text-xs text-zinc-400">Loading models...</div>
              ) : (
                availableModels
                  .filter((m) => {
                    const matchesSearch = !modelSearchTerm ||
                      m.id.toLowerCase().includes(modelSearchTerm.toLowerCase()) ||
                      m.name.toLowerCase().includes(modelSearchTerm.toLowerCase());
                    
                    const matchesCapabilities = modelCapabilityFilter.length === 0 ||
                      modelCapabilityFilter.every(filter => {
                        if (filter === 'tool_call') return m.tool_call === true;
                        if (filter === 'reasoning') return m.reasoning === true;
                        return true;
                      });
                    
                    return matchesSearch && matchesCapabilities;
                  })
                  .map((m) => (
                    <label
                      key={m.id}
                      className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                    >
                      <input
                        type="checkbox"
                        checked={models.includes(m.id)}
                        disabled={keys.some(k => k.provider === provider && k.models?.includes(m.id))}
                        onChange={() => toggleModel(m.id, models, setModels)}
                        className="accent-blue-600 disabled:opacity-50"
                      />
                      <div className="flex flex-1 flex-col gap-0.5">
                        <span className="truncate">{m.name || m.id}</span>
                        <div className="flex items-center gap-2 text-[10px] text-zinc-400">
                          {keys.some(k => k.provider === provider && k.models?.includes(m.id)) && (
                            <span className="text-green-600 dark:text-green-400">✓ Added</span>
                          )}
                          {m.context_window && (
                            <span>{(m.context_window / 1000).toFixed(0)}K context</span>
                          )}
                          {m.max_tokens && (
                            <span>{(m.max_tokens / 1000).toFixed(1)}K max output</span>
                          )}
                          {m.reasoning && <span>🧠 reasoning</span>}
                          {m.tool_call && <span>🔧 tools</span>}
                        </div>
                      </div>
                    </label>
                  ))
              )}
              {!modelsLoading && availableModels.length === 0 && (
                <div className="px-3 py-2 text-xs text-zinc-400">No models found. Select provider first.</div>
              )}
            </div>
            
            {/* Manual model input */}
            <div className="mt-2 flex gap-1">
              <input
                type="text"
                placeholder="Or type a custom model name..."
                className="flex-1 rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    const val = (e.target as HTMLInputElement).value.trim();
                    if (val && !models.includes(val)) {
                      setModels([...models, val]);
                      (e.target as HTMLInputElement).value = "";
                    }
                  }
                }}
              />
            </div>
          </div>

          {/* Model Capabilities */}
          {models.length > 0 && (() => {
            const primaryModel = models[0];
            const modelInfo = availableModels.find(m => m.id === primaryModel);
            const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
            const autoContextWindow = modelInfo?.context_window?.toString() ?? "";
            const autoMaxOutputTokens = modelInfo?.max_tokens?.toString() ?? "";
            const autoSupportsToolCalling = modelInfo?.tool_call ?? true;
            const displayContextWindow = hasModelsDevData ? autoContextWindow : contextWindow;
            const displayMaxOutputTokens = hasModelsDevData ? autoMaxOutputTokens : maxOutputTokens;
            const displaySupportsToolCalling = hasModelsDevData ? autoSupportsToolCalling : supportsToolCalling;
            return (
              <div>
                <label className="mb-1 block text-xs text-zinc-500">
                  Model Capabilities
                  {hasModelsDevData && <span className="ml-1 text-[10px] text-zinc-400">(from models.dev)</span>}
                  {!hasModelsDevData && <span className="ml-1 text-[10px] text-amber-500">(manual input required)</span>}
                </label>
                <div className="flex gap-2">
                  <div className="flex-1">
                    <label className="mb-0.5 block text-[10px] text-zinc-400">Context Window</label>
                    <input
                      type="number"
                      value={displayContextWindow}
                      onChange={(e) => setContextWindow(e.target.value)}
                      readOnly={hasModelsDevData}
                      placeholder="e.g. 128000"
                      className={cn(
                        "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                        hasModelsDevData
                          ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                          : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                      )}
                    />
                  </div>
                  <div className="flex-1">
                    <label className="mb-0.5 block text-[10px] text-zinc-400">Max Output Tokens</label>
                    <input
                      type="number"
                      value={displayMaxOutputTokens}
                      onChange={(e) => setMaxOutputTokens(e.target.value)}
                      readOnly={hasModelsDevData}
                      placeholder="e.g. 4096"
                      className={cn(
                        "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                        hasModelsDevData
                          ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                          : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                      )}
                    />
                  </div>
                </div>
                <div className="mt-1.5 flex items-center gap-2">
                  <label className="flex items-center gap-1.5 text-xs text-zinc-500">
                    <input
                      type="checkbox"
                      checked={displaySupportsToolCalling}
                      onChange={(e) => setSupportsToolCalling(e.target.checked)}
                      disabled={hasModelsDevData}
                      className="accent-blue-600"
                    />
                    Supports Tool Calling
                  </label>
                </div>
              </div>
            );
          })()}



          {/* Test result */}
          {testResult && (
            <div className={cn(
              "rounded-md px-3 py-2 text-xs",
              testResult.success
                ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                : "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400"
            )}>
              {testResult.message}
            </div>
          )}
        </div>
        </div>

        <div className="shrink-0 flex items-center justify-between gap-2 border-t border-zinc-100 dark:border-zinc-800 px-6 py-4">
          {/* Test result on the left */}
          <div className="flex-1 min-w-0">
            {testResult && (
              <div className={cn(
                "rounded-md px-3 py-1.5 text-xs truncate",
                testResult.success
                  ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                  : "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400"
              )}>
                {testResult.message}
              </div>
            )}
            {testing && (
              <div className="text-xs text-zinc-400">Testing...</div>
            )}
          </div>
          
          {/* Buttons on the right with equal width */}
          <div className="flex gap-2 shrink-0">
            <button
              onClick={onClose}
              className="w-20 rounded-md px-3 py-1.5 text-xs font-medium text-center text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
            >
              Cancel
            </button>
            <button
              onClick={handleSave}
              disabled={(needsApiKey(provider) ? !key.trim() : false) || saving}
              className="w-20 rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-center text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
            >
              {saving ? "Saving..." : "Save"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/** Popup-style model selector with provider shown in gray */
function ModelMenu({
  models,
  currentModel,
  currentProvider,
  onSelect,
}: {
  models: { name: string; provider: string }[];
  currentModel: string | null;
  currentProvider: string | null;
  onSelect: (model: string, provider: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Calculate menu width based on longest model name + provider
  const menuWidth = useMemo(() => {
    const CHAR_WIDTH = 7.5; // Approximate px per character for text-xs
    const PADDING = 30; // Left + right padding (12.5px each side)
    const GAP = 12; // Space between model and provider (~2 chars)
    let maxWidth = 0;
    
    for (const m of models) {
      const displayName = m.name.includes('/') && m.name.split('/')[0].length < m.name.split('/').slice(1).join('/').length
        ? m.name.split('/').slice(1).join('/')
        : m.name;
      const itemWidth = displayName.length * CHAR_WIDTH + m.provider.length * CHAR_WIDTH + GAP + PADDING;
      if (itemWidth > maxWidth) maxWidth = itemWidth;
    }
    
    return Math.max(maxWidth, 180); // Minimum 180px
  }, [models]);

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
          toolbarButton,
          open && toolbarButtonActive,
        )}
      >
        <Cpu size={14} />
        <span className="font-medium">
          {(() => {
            if (!currentModel || !currentModel.includes('/')) return currentModel ?? "Model";
            const parts = currentModel.split('/');
            const prefix = parts[0];
            const modelName = parts.slice(1).join('/');
            // Only strip if model name is longer than prefix (avoid stripping model/provider)
            return modelName.length > prefix.length ? modelName : currentModel;
          })()}
        </span>
        <ChevronDown className="h-3 w-3 text-zinc-400" />
      </button>

      {/* Popup menu */}
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-lg border shadow-lg",
            "border-zinc-200 bg-white dark:border-zinc-700 dark:bg-zinc-800",
          )}
          style={{ width: `${menuWidth}px` }}
        >
          {/* Model list */}
          <div className="max-h-[240px] overflow-y-auto">
            {models.map((m) => {
            const isActive = m.name === currentModel && m.provider === currentProvider;
            return (
              <button
                key={`${m.name}::${m.provider}`}
                type="button"
                onClick={() => {
                  onSelect(m.name, m.provider);
                  setOpen(false);
                }}
                className={cn(
                  "flex w-full items-center justify-between px-2.5 py-1.5 text-xs transition-colors",
                  isActive
                    ? "text-zinc-900 dark:text-white"
                    : "text-zinc-600 hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50",
                )}
              >
                <span className={cn("font-medium", isActive && "text-blue-600 dark:text-blue-400")}>
                  {/* Strip provider prefix from model name if format is provider/model and model is longer */}
                  {(() => {
                    if (!m.name.includes('/')) return m.name;
                    const parts = m.name.split('/');
                    const prefix = parts[0];
                    const modelName = parts.slice(1).join('/');
                    // Only strip if model name is longer than prefix (avoid stripping model/provider)
                    return modelName.length > prefix.length ? modelName : m.name;
                  })()}
                </span>
                <span className="text-[10px] text-zinc-400 dark:text-zinc-500 shrink-0">
                  {m.provider}
                </span>
              </button>
            );
          })}
          </div>

          {/* Divider */}
          <div className="border-t border-zinc-200 dark:border-zinc-700" />

          {/* Add Models button — same style as Install Agent */}
          <button
            type="button"
            onClick={() => {
              setShowAddDialog(true);
              setOpen(false);
            }}
            className="mx-1.5 mt-2 mb-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
          >
            <Plus className="h-3.5 w-3.5" />
            Add Model
          </button>
        </div>
      )}

      {/* Add Model Dialog */}
      <AddModelDialog
        open={showAddDialog}
        onClose={() => setShowAddDialog(false)}
        onSuccess={() => {
          // Trigger reload of models from parent component
          window.dispatchEvent(new Event('models-added'));
        }}
      />
    </div>
  );
}

