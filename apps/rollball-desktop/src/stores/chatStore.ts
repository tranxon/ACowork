import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry } from "../lib/types";
import { usePermissionStore } from "./permissionStore";
import { useSessionStore } from "./sessionStore";
import { useAgentStore } from "./agentStore";
import { useUserProfileStore } from "./userProfileStore";
import { useAgentProfileStore } from "./agentProfileStore";
import { getGatewayUrl } from "../lib/config";

// ── Sender info helpers ────────────────────────────────────────────────

/** Fill senderDisplayName / senderRole for messages from the agent */
function getAgentSenderInfo(agentId: string): { senderDisplayName?: string; senderRole?: string } {
  // Priority: agentProfileStore custom name > agent display_name > agent name
  const agentProfile = useAgentProfileStore.getState().getProfile(agentId);
  const agents = useAgentStore.getState().agents;
  const agent = agents.find((a) => a.agent_id === agentId);
  return {
    senderDisplayName: agentProfile?.displayName ?? agent?.display_name ?? agent?.name,
    senderRole: agent?.role,
  };
}

/** Fill senderDisplayName for user messages */
function getUserSenderInfo(): { senderDisplayName?: string } {
  try {
    const profile = useUserProfileStore.getState().profile;
    return { senderDisplayName: profile.displayName };
  } catch {
    return { senderDisplayName: "我" };
  }
}

// ---------------------------------------------------------------------------
// Per-agent chat state — each agent owns an independent instance
// ---------------------------------------------------------------------------
interface AgentChatState {
  messages: ChatMessage[];
  streamingMessageId: string | null;
  streamBuffer: string;
  thinkingMessageId: string | null;
  isInThinkPhase: boolean;
  currentTurnId: string | null;
  tokenUsage: TokenUsage | null;
  contextUsage: ContextUsageInfo | null;
  hasMoreMessages: boolean;
  messageCursor: string | null;
  iterationLimitPaused: { iteration: number; maxIterations: number; message: string } | null;
  /** Whether initial session messages are being loaded for this agent */
  isLoadingSession: boolean;
  /** Error message when session message loading fails (null = no error) */
  loadError: string | null;
  /** LLM reasoning in progress — frontend shows pulsing "..." indicator */
  isReasoning: boolean;
}

const DEFAULT_AGENT_STATE: AgentChatState = {
  messages: [],
  streamingMessageId: null,
  streamBuffer: "",
  thinkingMessageId: null,
  isInThinkPhase: false,
  currentTurnId: null,
  tokenUsage: null,
  contextUsage: null,
  hasMoreMessages: false,
  messageCursor: null,
  iterationLimitPaused: null,
  isLoadingSession: false,
  loadError: null,
  isReasoning: false,
};

/** Get the chat state for a specific agent (returns default if not yet initialized) */
function getAgentState(state: ChatStore, agentId: string): AgentChatState {
  return state.agentStates[agentId] ?? DEFAULT_AGENT_STATE;
}

/** Produce a new agentStates patch that merges `patch` into the agent's current state */
function updateAgentState(
  state: ChatStore,
  agentId: string,
  patch: Partial<AgentChatState>,
): { agentStates: Record<string, AgentChatState> } {
  const current = getAgentState(state, agentId);
  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: { ...current, ...patch },
    },
  };
}

// ---------------------------------------------------------------------------
// ChatStore — global fields + per-agent agentStates
// ---------------------------------------------------------------------------
interface ChatStore {
  /** Per-agent chat states — the core of the per-agent model */
  agentStates: Record<string, AgentChatState>;

  // ---- Global (non-per-agent) fields ----
  sending: boolean;
  /** Per-agent WebSocket connections: agentId → WebSocket */
  wsMap: Record<string, WebSocket>;
  /** Current active model for the selected agent */
  currentModel: string | null;
  /** Current active provider for the selected agent */
  currentProvider: string | null;
  /** Per-agent model memory: agent_id → { model, provider } */
  agentModels: Record<string, { model: string; provider: string }>;
  availableModels: { name: string; provider: string }[];
  /** Current agent ID for stop functionality */
  currentAgentId: string | null;
  /** Whether more messages are being loaded */
  isLoadingMore: boolean;
  /** Load sequence number to prevent race conditions on fast session switches */
  loadSequence: number;
  /** AbortController for cancelling in-flight loadSessionMessages requests */
  abortController: AbortController | null;

  // ---- Actions ----
  connectStream: (agentId: string, gatewayUrl: string) => void;
  sendMessage: (content: string, agentId: string, command?: string) => Promise<void>;
  stopCurrentMessage: () => Promise<void>;
  /** Send interrupt without changing `sending` — keeps UI as Stop button.
   *  Used when the user queues a message then hits Stop — the loop is
   *  interrupted but the agent continues processing in the next turn. */
  sendInterrupt: () => void;
  /** Disconnect a specific agent's WebSocket, or all if no agentId provided */
  disconnectStream: (agentId?: string) => void;
  /** Clear messages and streaming state for a specific agent (or currentAgentId) */
  clearMessages: (agentId?: string) => void;
  /** Trim messages to the specified count (for debug rewind).
   *  Keeps the first `count` messages, discards the rest. */
  trimMessagesTo: (agentId: string, count: number) => void;
  setCurrentModel: (model: string, provider: string, agentId: string) => void;
  setAvailableModels: (models: { name: string; provider: string }[]) => void;
  /** Get the WebSocket for a specific agent */
  getWs: (agentId: string) => WebSocket | undefined;
  /** Load provider for a specific agent from per-agent cache */
  loadAgentProvider: (agentId: string) => string | null;
  /** Continue agent execution after iteration limit pause */
  continueExecution: (agentId: string) => Promise<void>;
  /** Load model for a specific agent from Gateway API, returns the model name */
  loadAgentModel: (agentId: string) => Promise<string | null>;
  /** Load conversation history for a specific agent from Gateway API */
  loadConversationHistory: (agentId: string) => Promise<void>;
  /** Load paginated messages for a specific session */
  loadSessionMessages: (
    agentId: string,
    sessionId: string,
    cursor?: string,
    limit?: number,
    direction?: string,
  ) => Promise<void>;
  /** Abort any in-flight loadSessionMessages request */
  abortSessionLoad: () => void;
  /** Load more older messages (triggered by scroll to top) */
  loadMoreMessages: (agentId: string, sessionId: string) => Promise<void>;
}

/** Derive WebSocket URL from Gateway HTTP base URL */
function toWsUrl(httpUrl: string, agentId: string): string {
  return `${httpUrl.replace("http://", "ws://").replace("https://", "wss://")}/api/agents/${agentId}/stream`;
}

/** Per-agent reconnect state — tracked outside zustand to avoid re-render loops */
const reconnectState: Record<string, { timer: ReturnType<typeof setTimeout> | null; attempts: number }> = {};
const MAX_RECONNECT_ATTEMPTS = 10;
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 30000;

function scheduleReconnect(agentId: string, gatewayUrl: string) {
  if (!reconnectState[agentId]) {
    reconnectState[agentId] = { timer: null, attempts: 0 };
  }
  const rs = reconnectState[agentId];
  if (rs.timer) return; // already scheduled
  if (rs.attempts >= MAX_RECONNECT_ATTEMPTS) {
    console.warn(`[ChatStore] Max reconnect attempts reached for agent ${agentId}, giving up`);
    return;
  }
  const delay = Math.min(RECONNECT_BASE_MS * Math.pow(1.5, rs.attempts), RECONNECT_MAX_MS);
  rs.attempts++;
  console.log(`[ChatStore] Reconnecting agent ${agentId} in ${Math.round(delay)}ms (attempt ${rs.attempts}/${MAX_RECONNECT_ATTEMPTS})`);
  rs.timer = setTimeout(() => {
    rs.timer = null;
    const store = useChatStore.getState();
    // Only reconnect if this agent has no active ws
    if (!store.wsMap[agentId]) {
      store.connectStream(agentId, gatewayUrl);
    }
  }, delay);
}

function resetReconnect(agentId: string) {
  const rs = reconnectState[agentId];
  if (rs?.timer) {
    clearTimeout(rs.timer);
    rs.timer = null;
  }
  if (rs) rs.attempts = 0;
}

function resetAllReconnects() {
  for (const agentId of Object.keys(reconnectState)) {
    resetReconnect(agentId);
  }
}

export const useChatStore = create<ChatStore>((set, get) => ({
  agentStates: {},
  sending: false,
  wsMap: {},
  currentModel: null,
  currentProvider: null,
  agentModels: {},
  availableModels: [],
  currentAgentId: null,
  isLoadingMore: false,
  loadSequence: 0,
  abortController: null,

  getWs: (agentId: string) => get().wsMap[agentId],

  connectStream: (agentId: string, gatewayUrl: string = getGatewayUrl()) => {
    // Update currentAgentId for stop functionality
    set({ currentAgentId: agentId });

    // Cancel any pending reconnect for this agent
    resetReconnect(agentId);

    // If this agent already has an OPEN ws, reuse it
    const existing = get().wsMap[agentId];
    if (existing && existing.readyState === WebSocket.OPEN) {
      console.log("[ChatStore] Reusing existing WebSocket for agent:", agentId);
      return;
    }

    // Close any stale connection for this specific agent only
    if (existing) {
      existing.onopen = null;
      existing.onmessage = null;
      existing.onclose = null;
      existing.onerror = null;
      existing.close();
    }

    const wsUrl = toWsUrl(gatewayUrl, agentId);
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      console.warn("[ChatStore] WebSocket creation failed, will retry:", e);
      set((state) => {
        const newMap = { ...state.wsMap };
        delete newMap[agentId];
        return { wsMap: newMap };
      });
      scheduleReconnect(agentId, gatewayUrl);
      return;
    }

    ws.onopen = () => {
      console.log("[ChatStore] WebSocket connected for agent:", agentId);
      resetReconnect(agentId); // successful connection resets retry counter
      // Re-set wsMap entry to trigger React re-render with OPEN readyState
      set((state) => ({ wsMap: { ...state.wsMap, [agentId]: ws } }));
    };

    ws.onmessage = (event) => {
      // Process ALL incoming messages regardless of which agent is currently displayed.
      // Per-agent state ensures each agent's data stays independent.
      try {
        const data = JSON.parse(event.data);
        handleMessageEvent(data, set, get, agentId);
      } catch (e) {
        console.error("[ChatStore] Failed to parse WS message:", e);
      }
    };

    ws.onclose = () => {
      // Defensive check: ignore stale callbacks from a replaced WebSocket
      if (get().wsMap[agentId] !== ws) {
        console.log("[ChatStore] Stale WebSocket closed, ignoring");
        return;
      }
      console.log("[ChatStore] WebSocket closed for agent:", agentId);
      set((state) => {
        const newMap = { ...state.wsMap };
        delete newMap[agentId];
        return {
          wsMap: newMap,
          // Clear streaming state for the agent that lost its connection
          ...updateAgentState(state, agentId, {
            streamingMessageId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
            isReasoning: false,
          }),
          // Only clear global sending if this was the active agent
          ...(state.currentAgentId === agentId ? { sending: false } : {}),
        };
      });
      scheduleReconnect(agentId, gatewayUrl);
    };

    ws.onerror = (err) => {
      // Defensive check: ignore stale callbacks from a replaced WebSocket
      if (get().wsMap[agentId] !== ws) {
        console.log("[ChatStore] Stale WebSocket error, ignoring");
        return;
      }
      console.warn("[ChatStore] WebSocket error:", err);
      // Don't remove from wsMap here — onclose will fire after onerror
    };

    set((state) => ({
      wsMap: { ...state.wsMap, [agentId]: ws },
      currentAgentId: agentId,
      sending: false,
      ...updateAgentState(state, agentId, {
        streamingMessageId: null,
        streamBuffer: "",
        thinkingMessageId: null,
        isInThinkPhase: false,
        isReasoning: false,
        tokenUsage: null,
        contextUsage: null,
      }),
    }));
  },

  sendMessage: async (content: string, agentId: string, command?: string) => {
    const ws = get().wsMap[agentId];

    // Add user message to the agent's per-agent state
    const userMsg: ChatMessage = {
      id: `msg-user-${Date.now()}`,
      type: "user",
      content,
      timestamp: Date.now(),
      ...getUserSenderInfo(),
    };
    set((state) => ({
      sending: true,
      ...updateAgentState(state, agentId, {
        messages: [...getAgentState(state, agentId).messages, userMsg],
        currentTurnId: null, // Reset turn tracking for new conversation turn
      }),
    }));

    // Update session title immediately when first message is sent
    updateSessionTitleFromMessages(get().agentStates[agentId]?.messages ?? [], agentId);

    // Helper: send via WebSocket and set up streaming state
    const sendViaWs = (socket: WebSocket) => {
      socket.send(JSON.stringify({ type: "message", content, command }));

      // Reset streaming state for this agent — messages will be created on first chunk
      set((state) => updateAgentState(state, agentId, {
        streamBuffer: "",
        streamingMessageId: null,
        thinkingMessageId: null,
        isInThinkPhase: false,
        isReasoning: false,
      }));
    };

    // If WebSocket exists, try to use it
    if (ws) {
      if (ws.readyState === WebSocket.OPEN) {
        // Already connected — send immediately
        sendViaWs(ws);
        return;
      }

      if (ws.readyState === WebSocket.CONNECTING) {
        // Wait for connection to open (max 2 seconds)
        const connected = await new Promise<boolean>((resolve) => {
          const timeout = setTimeout(() => resolve(false), 2000);
          const onOpen = () => {
            clearTimeout(timeout);
            ws.removeEventListener("open", onOpen);
            ws.removeEventListener("error", onError);
            resolve(true);
          };
          const onError = () => {
            clearTimeout(timeout);
            ws.removeEventListener("open", onOpen);
            ws.removeEventListener("error", onError);
            resolve(false);
          };
          ws.addEventListener("open", onOpen);
          ws.addEventListener("error", onError);
        });

        if (connected) {
          sendViaWs(ws);
          return;
        }
        // Connection timed out or failed — fall through to HTTP
      }
    }

    // Fallback: send via Tauri HTTP command
    try {
      const result = await invoke<{ message_id: string; status: string }>(
        "send_message",
        { agentId, content, command },
      );
      console.log("[ChatStore] Message sent via HTTP:", result);
      // Show a system message since we can't stream the response
      const replyMsg: ChatMessage = {
        id: `msg-assistant-${Date.now()}`,
        type: "system",
        content: "Message sent. Waiting for agent response... (streaming not available)",
        timestamp: Date.now(),
      };
      set((state) => ({
        ...updateAgentState(state, agentId, {
          messages: [...getAgentState(state, agentId).messages, replyMsg],
        }),
        sending: false,
      }));
    } catch (error) {
      console.error("[ChatStore] HTTP message send failed:", error);
      const errorMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "system",
        content: `Failed to send message: Agent may not be connected yet. Please wait and try again.`,
        timestamp: Date.now(),
      };
      set((state) => ({
        ...updateAgentState(state, agentId, {
          messages: [...getAgentState(state, agentId).messages, errorMsg],
        }),
        sending: false,
      }));
    }
  },

  stopCurrentMessage: async () => {
    const { currentAgentId } = get();
    if (!currentAgentId) {
      console.warn("[ChatStore] No active agent to stop");
      return;
    }

    console.log("[ChatStore] Stopping current message for agent:", currentAgentId);

    // Send stop command via WebSocket if available.
    // This sends an Interrupt signal (soft stop) — the agent's inference
    // is cancelled but the agent process stays alive.
    const ws = get().wsMap[currentAgentId];
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "stop", agentId: currentAgentId }));
    } else {
      console.warn("[ChatStore] WebSocket not available for stop, skipping");
    }

    // Update UI state immediately
    set((state) => ({
      sending: false,
      ...updateAgentState(state, currentAgentId, {
        streamingMessageId: null,
        streamBuffer: "",
        thinkingMessageId: null,
        isInThinkPhase: false,
      }),
    }));
  },

  // Lightweight interrupt — only sends WebSocket stop without touching `sending`.
  // The `done` event from WebSocket will set `sending: false` naturally.
  sendInterrupt: () => {
    const { currentAgentId } = get();
    if (!currentAgentId) return;
    const ws = get().wsMap[currentAgentId];
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "stop", agentId: currentAgentId }));
    }
  },

  disconnectStream: (agentId?: string) => {
    if (agentId) {
      // Disconnect a specific agent's WebSocket
      resetReconnect(agentId);
      const ws = get().wsMap[agentId];
      if (ws) {
        ws.onopen = null;
        ws.onmessage = null;
        ws.onclose = null;
        ws.onerror = null;
        ws.close();
      }
      set((state) => {
        const newMap = { ...state.wsMap };
        delete newMap[agentId];
        return {
          wsMap: newMap,
          ...updateAgentState(state, agentId, {
            streamingMessageId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
          }),
          ...(state.currentAgentId === agentId ? { sending: false } : {}),
        };
      });
    } else {
      // Disconnect all agents' WebSockets
      resetAllReconnects();
      const allWs = get().wsMap;
      for (const id of Object.keys(allWs)) {
        const ws = allWs[id];
        ws.onopen = null;
        ws.onmessage = null;
        ws.onclose = null;
        ws.onerror = null;
        ws.close();
      }
      // Clear streaming state for all agents
      const clearedAgentStates: Record<string, AgentChatState> = {};
      for (const id of Object.keys(get().agentStates)) {
        clearedAgentStates[id] = {
          ...get().agentStates[id],
          streamingMessageId: null,
          streamBuffer: "",
          thinkingMessageId: null,
          isInThinkPhase: false,
        };
      }
      set({
        wsMap: {},
        sending: false,
        agentStates: clearedAgentStates,
      });
    }
  },

  clearMessages: (agentId?: string) => {
    const targetId = agentId ?? get().currentAgentId;
    if (!targetId) return;
    set((state) => ({
      ...updateAgentState(state, targetId, {
        messages: [],
        streamingMessageId: null,
        streamBuffer: "",
        thinkingMessageId: null,
        isInThinkPhase: false,
        tokenUsage: null,
        contextUsage: null,
        hasMoreMessages: false,
        messageCursor: null,
        iterationLimitPaused: null,
        currentTurnId: null,
        loadError: null,
      }),
      // Only clear global sending if this is the active agent
      ...(state.currentAgentId === targetId ? { sending: false } : {}),
    }));
  },

  trimMessagesTo: (agentId: string, count: number) => {
    set((state) => {
      const agentState = getAgentState(state, agentId);
      if (agentState.messages.length <= count) return {}; // nothing to trim
      return updateAgentState(state, agentId, {
        messages: agentState.messages.slice(0, count),
      });
    });
  },

  setCurrentModel: (model: string, provider: string, agentId: string) => {
    // Optimistically update UI only — don't cache until confirmed by backend
    const prevModel = get().currentModel;
    const prevProvider = get().currentProvider;
    set({ currentModel: model, currentProvider: provider });
    // Send model_switch message to Agent via WebSocket when user changes model
    const ws = get().wsMap[agentId];
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "model_switch", model, provider, agentId, _prevModel: prevModel }));
    } else {
      // No WebSocket — revert immediately
      set({ currentModel: prevModel, currentProvider: prevProvider });
    }
  },
  setAvailableModels: (models: { name: string; provider: string }[]) => {
    set((state) => {
      // Check if current model+provider combo exists in new list
      const currentModelExists = state.currentModel && state.currentProvider
        ? models.some(m => m.name === state.currentModel && m.provider === state.currentProvider)
        : false;

      return {
        availableModels: models,
        // Only fallback to first model if current selection doesn't exist in new list
        currentModel: currentModelExists ? state.currentModel : (models[0]?.name ?? null),
        currentProvider: currentModelExists ? state.currentProvider : (models[0]?.provider ?? null),
      };
    });
  },
  loadAgentProvider: (agentId: string) => {
    const cached = get().agentModels[agentId];
    return cached?.provider ?? null;
  },
  continueExecution: async (agentId: string) => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/continue`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      });
      if (resp.ok) {
        set((state) => ({
          sending: true,
          ...updateAgentState(state, agentId, { iterationLimitPaused: null }),
        }));
      }
    } catch (error) {
      console.error("[ChatStore] Failed to send continue signal:", error);
    }
  },
  loadAgentModel: async (agentId: string): Promise<string | null> => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/model`);
      if (!resp.ok) return null;
      const data = await resp.json() as { provider: string; model: string; available_models: string[] };
      if (data.model) {
        set((state) => {
          // The backend now returns the saved provider from .agent_model.json,
          // so data.provider is the correct per-agent provider.
          // Only fall back to the cached provider if the backend didn't return one
          // (shouldn't happen with the fix, but kept for robustness).
          const cached = state.agentModels[agentId];
          let provider = data.provider;
          if (!provider && cached && cached.model === data.model && cached.provider) {
            provider = cached.provider;
          }
          return {
            currentModel: data.model,
            currentProvider: provider,
            agentModels: {
              ...state.agentModels,
              [agentId]: { model: data.model, provider },
            },
          };
        });
      }
      return data.model ?? null;
    } catch {
      return null;
    }
  },
  loadConversationHistory: async (agentId: string) => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/conversations/latest`);
      if (!resp.ok) return;
      const data = await resp.json() as { session_id?: string; messages?: Array<{ role: string; content: string; timestamp: number; turn_index: number }> };

      if (!data.messages || data.messages.length === 0) return;

      // Convert Episode messages to ChatMessage format
      const historyMessages: ChatMessage[] = data.messages.map((msg) => ({
        id: `history-${msg.turn_index}-${msg.role}-${msg.timestamp}`,
        type: (msg.role === "user"
          ? "user"
          : msg.role === "assistant"
            ? "assistant"
            : msg.role === "think" || msg.role === "thought"
              ? "thought"
              : "system") as ChatMessage["type"],
        content: msg.content,
        timestamp: msg.timestamp * 1000, // Convert seconds to milliseconds
      }));

      set((state) => updateAgentState(state, agentId, { messages: historyMessages }));
    } catch (e) {
      console.error("[ChatStore] Failed to load conversation history:", e);
      set((state) => updateAgentState(state, agentId, { messages: [] }));
    }
  },

  loadSessionMessages: async (
    agentId: string,
    sessionId: string,
    cursor?: string,
    limit: number = 50,
    direction: string = "backward",
  ) => {
    // Skip loading if this agent is currently streaming — don't overwrite live data
    const agentState = getAgentState(get(), agentId);
    if (agentState.streamingMessageId != null) {
      console.log(`[ChatStore] Skipping loadSessionMessages — agent ${agentId} is streaming`);
      return;
    }

    // Increment loadSequence — stale responses with old sequence will be discarded
    const seq = get().loadSequence + 1;
    set({ loadSequence: seq });

    // Abort any in-flight request before starting a new one
    const oldController = get().abortController;
    if (oldController) {
      oldController.abort();
    }
    const controller = new AbortController();
    set({ abortController: controller });

    if (!cursor) {
      set((state) => ({
        ...updateAgentState(state, agentId, { isLoadingSession: true, loadError: null }),
      }));
    }

    try {
      const params = new URLSearchParams();
      params.set("limit", String(limit));
      params.set("direction", direction);
      if (cursor) params.set("cursor", cursor);

      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?${params}`,
        { signal: controller.signal },
      );

      // Discard stale response: if loadSequence has changed, this response is outdated
      if (get().loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale loadSessionMessages response (seq ${seq} vs current ${get().loadSequence})`);
        return;
      }

      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);

      const data = (await resp.json()) as PaginatedMessages;

      // Check again after await — another request may have started
      if (get().loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale response after json parse (seq ${seq} vs current ${get().loadSequence})`);
        return;
      }

      console.log(`[ChatStore] Loaded ${data.messages?.length ?? 0} messages for session ${sessionId}`);

      const converted = (data.messages ?? []).map((e) => convertConversationEntry(e, agentId));

      set((state) => {
        // Discard if sequence changed while we were building state
        if (state.loadSequence !== seq) {
          console.log(`[ChatStore] Discarding state update — sequence changed`);
          return {};
        }

        if (cursor) {
          // Loading more: prepend older messages
          const existingIds = new Set(getAgentState(state, agentId).messages.map((m) => m.id));
          const newMessages = converted.filter((m) => !existingIds.has(m.id));
          return {
            ...updateAgentState(state, agentId, {
              messages: [...newMessages, ...getAgentState(state, agentId).messages],
              hasMoreMessages: data.has_more,
              messageCursor: data.cursor,
              isLoadingSession: false,
              loadError: null,
            }),
            isLoadingMore: false,
          };
        }

        // Initial load: replace messages for this agent
        return {
          ...updateAgentState(state, agentId, {
            messages: converted,
            hasMoreMessages: data.has_more,
            messageCursor: data.cursor,
            isLoadingSession: false,
            loadError: null,
          }),
          isLoadingMore: false,
        };
      });
    } catch (e: unknown) {
      // Discard errors from stale/aborted requests
      if (get().loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale error response (seq ${seq})`);
        return;
      }
      // AbortError is expected — don't log as error
      if (e instanceof DOMException && e.name === "AbortError") {
        console.log(`[ChatStore] loadSessionMessages aborted (seq ${seq})`);
        set((state) => ({
          ...updateAgentState(state, agentId, { isLoadingSession: false }),
          isLoadingMore: false,
        }));
        return;
      }
      console.error("[ChatStore] Failed to load session messages:", e);
      set((state) => ({
        ...updateAgentState(state, agentId, {
          messages: [],
          hasMoreMessages: false,
          messageCursor: null,
          isLoadingSession: false,
          loadError: `消息加载失败: ${e instanceof Error ? e.message : String(e)}`,
        }),
        isLoadingMore: false,
      }));
    } finally {
      // Clear the controller if it's still the one we created
      const currentController = get().abortController;
      if (currentController === controller) {
        set({ abortController: null });
      }
    }
  },

  abortSessionLoad: () => {
    const controller = get().abortController;
    if (controller) {
      controller.abort();
      set({ abortController: null });
    }
    // Also bump loadSequence so pending responses are discarded
    set((state) => ({ loadSequence: state.loadSequence + 1 }));
  },

  loadMoreMessages: async (agentId: string, sessionId: string) => {
    const { isLoadingMore } = get();
    const agentState = getAgentState(get(), agentId);
    if (isLoadingMore || !agentState.hasMoreMessages || !agentState.messageCursor) return;
    set({ isLoadingMore: true });
    await get().loadSessionMessages(agentId, sessionId, agentState.messageCursor, 50, "backward");
  },
}));

/** Compute session title from first user message (mirrors backend set_title logic) */
function makeSessionTitle(content: string): string {
  return content.replace(/\n/g, " ").trim().substring(0, 30);
}

/** Tracks which session titles have already been persisted to backend,
 *  preventing redundant PUT API calls that trigger unnecessary metadata rewrites. */
const persistedTitles: Set<string> = new Set();

/** Find first user message and update session title if not yet set.
 *  Updates both local state (sessionStore) AND backend JSONL metadata.
 *  Avoids redundant API calls — the backend's `set_title` already writes the
 *  title after the first user message via AgentLoop. */
function updateSessionTitleFromMessages(messages: ChatMessage[], agentId?: string) {
  const firstUserMsg = messages.find((m) => m.type === "user");
  if (!firstUserMsg || !firstUserMsg.content) return;
  const sessionId = useSessionStore.getState().currentSessionId;
  if (!sessionId) return;
  const title = makeSessionTitle(firstUserMsg.content);

  // Update local sessionStore
  useSessionStore.getState().updateSessionTitle(sessionId, title);

  // Skip backend API if title was already persisted for this session
  const cacheKey = `${sessionId}::${title}`;
  if (persistedTitles.has(cacheKey)) return;
  persistedTitles.add(cacheKey);

  // Persist to backend (best-effort, non-blocking)
  if (agentId) {
    fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/title`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title }),
    }).catch((e) => {
      console.warn("[ChatStore] Failed to persist title to backend:", e);
    });
  }
}

/** Convert a ConversationEntry from Gateway to UI ChatMessage */
function convertConversationEntry(entry: ConversationEntry, agentId: string): ChatMessage {
  const base: ChatMessage = {
    id: entry.id,
    type: (entry.role === "think" ? "thought" : entry.role) as ChatMessage["type"],
    content: entry.content,
    timestamp: new Date(entry.ts).getTime(),
  };

  // Fill sender info based on role
  if (entry.role === "user") {
    const userInfo = getUserSenderInfo();
    base.senderDisplayName = userInfo.senderDisplayName;
  } else if (entry.role === "assistant" || entry.role === "think" || entry.role === "thought" || entry.role === "tool_call" || entry.role === "tool_result") {
    const agentInfo = getAgentSenderInfo(agentId);
    base.senderDisplayName = agentInfo.senderDisplayName;
    base.senderRole = agentInfo.senderRole;
  }

  const meta = entry.metadata;
  if (!meta) return base;

  if (entry.role === "tool_call" || entry.role === "tool_result") {
    base.toolName = meta.tool_name as string | undefined;
    base.toolData = meta as Record<string, unknown>;
    if (entry.role === "tool_result") {
      base.toolStatus = meta.success === false ? "error" : "success";
    }
  }

  if (entry.role === "think" || entry.role === "thought") {
    base.startTime = (meta.startTime as number) ?? undefined;
    base.endTime = (meta.endTime as number) ?? undefined;
  }

  return base;
}

/** Handle incoming WebSocket events — operates on per-agent state via agentId */
function handleMessageEvent(
  data: Record<string, unknown>,
  set: (fn: Partial<ChatStore> | ((state: ChatStore) => Partial<ChatStore>)) => void,
  get: () => ChatStore,
  agentId: string,
) {
  const eventType = data.type as string;

  switch (eventType) {
    case "connected":
      // Initial connection acknowledgment
      break;

    case "ack":
      // Message received acknowledgment
      break;

    case "reasoning_started":
      // LLM reasoning phase started — show pulsing "..." indicator.
      // Cleared on first chunk / tool_call / done / error.
      set((state) => updateAgentState(state, agentId, { isReasoning: true }));
      break;

    case "chunk": {
      // Streaming text chunk — split think/reply at store level
      // Clear reasoning indicator on first token arrival
      set((state) => updateAgentState(state, agentId, { isReasoning: false }));
      // Fallback to `content` field for backward compatibility with older Gateway versions
      const delta = (data.delta ?? data.content) as string;
      const reasoningDelta = data.reasoning_content as string | undefined;

      set((state) => {
        const as = getAgentState(state, agentId);

        // DeepSeek-style reasoning_content (independent field, not wrapped in <think>)
        // NOTE: Do NOT set isInThinkPhase here — that flag is exclusively for <think> tag state machine.
        // Do NOT set streamingMessageId — so when regular content arrives, it creates a new assistant message.
        if (reasoningDelta) {
          if (as.thinkingMessageId) {
            return updateAgentState(state, agentId, {
              messages: as.messages.map((msg) =>
                msg.id === as.thinkingMessageId
                  ? { ...msg, content: msg.content + reasoningDelta }
                  : msg,
              ),
            });
          } else {
            const thinkMsgId = `msg-think-${Date.now()}`;
            const thinkMsg: ChatMessage = {
              id: thinkMsgId,
              type: "thought",
              content: reasoningDelta,
              timestamp: Date.now(),
              startTime: Date.now(),
              ...getAgentSenderInfo(agentId),
            };
            return updateAgentState(state, agentId, {
              messages: [...as.messages, thinkMsg],
              thinkingMessageId: thinkMsgId,
            });
          }
        }

        // If we had an active reasoning_content think message and now receive a regular delta,
        // finalize the think message by setting endTime to stop the timer.
        let messages = as.messages;
        if (as.thinkingMessageId && delta) {
          const now = Date.now();
          messages = messages.map((msg) =>
            msg.id === as.thinkingMessageId
              ? { ...msg, endTime: now }
              : msg,
          );
        }

        const newBuffer = as.streamBuffer + delta;

        // Already in think phase — check for </think> close
        if (as.isInThinkPhase && as.thinkingMessageId) {
          const closeIdx = newBuffer.indexOf("</think>");
          if (closeIdx >= 0) {
            // Think phase ends — extract think content and reply content
            const thinkStart = newBuffer.indexOf("<think>");
            const thinkContent = newBuffer.substring(thinkStart + 7, closeIdx);
            const replyContent = newBuffer.substring(closeIdx + 8).trimStart();

            // Finalize think message (set endTime to stop the timer)
            const now = Date.now();
            const finalMessages = messages.map((msg) =>
              msg.id === as.thinkingMessageId
                ? { ...msg, content: thinkContent, endTime: now }
                : msg,
            );

            // Create assistant message for reply
            const assistantMsgId = `msg-assistant-${Date.now()}`;
            const assistantMsg: ChatMessage = {
              id: assistantMsgId,
              type: "assistant",
              content: replyContent,
              timestamp: Date.now(),
              ...getAgentSenderInfo(agentId),
            };

            return updateAgentState(state, agentId, {
              messages: [...finalMessages, assistantMsg],
              streamBuffer: "",
              isInThinkPhase: false,
              thinkingMessageId: null,
              streamingMessageId: assistantMsgId,
            });
          } else {
            // Still in think phase — append to think message
            const thinkStart = newBuffer.indexOf("<thinking");
            const thinkContent = newBuffer.substring(thinkStart + 10);
            return updateAgentState(state, agentId, {
              messages: as.messages.map((msg) =>
                msg.id === as.thinkingMessageId
                  ? { ...msg, content: thinkContent }
                  : msg,
              ),
              streamBuffer: newBuffer,
            });
          }
        }

        // Not in think phase — check if entering
        const trimmed = newBuffer.trimStart();
        const THINK_OPEN = "<thinking>";

        if (trimmed.startsWith(THINK_OPEN)) {
          const thinkStart = newBuffer.indexOf("<thinking>");
          const thinkMsgId = `msg-think-${Date.now()}`;
          const thinkMsg: ChatMessage = {
            id: thinkMsgId,
            type: "thought",
            content: newBuffer.substring(thinkStart + 10),
            timestamp: Date.now(),
            startTime: Date.now(),
            ...getAgentSenderInfo(agentId),
          };
          return updateAgentState(state, agentId, {
            messages: [...as.messages, thinkMsg],
            streamBuffer: newBuffer,
            isInThinkPhase: true,
            thinkingMessageId: thinkMsgId,
            streamingMessageId: thinkMsgId,
          });
        }

        // If buffer is non-empty and definitely not starting with <thinking>,
        // create or append to assistant message
        if (trimmed.length > 0) {
          const definitelyNotThink =
            trimmed[0] !== "<" ||
            (trimmed.length >= THINK_OPEN.length && !trimmed.startsWith(THINK_OPEN));

          if (definitelyNotThink) {
            if (as.streamingMessageId && !as.isInThinkPhase) {
              return updateAgentState(state, agentId, {
                messages: messages.map((msg) =>
                  msg.id === as.streamingMessageId
                    ? { ...msg, content: msg.content + delta }
                    : msg,
                ),
                streamBuffer: newBuffer,
                thinkingMessageId: null,
              });
            } else {
              const assistantMsgId = `msg-assistant-${Date.now()}`;
              const assistantMsg: ChatMessage = {
                id: assistantMsgId,
                type: "assistant",
                content: newBuffer,
                timestamp: Date.now(),
              };
              return updateAgentState(state, agentId, {
                messages: [...messages, assistantMsg],
                streamBuffer: newBuffer,
                streamingMessageId: assistantMsgId,
                thinkingMessageId: null,
              });
            }
          }
        }

        // Buffer is empty or starts with `<` but too short to tell — keep buffering
        return updateAgentState(state, agentId, { streamBuffer: newBuffer });
      });
      break;
    }

    case "tool_call": {
      // Clear reasoning indicator
      set((state) => updateAgentState(state, agentId, { isReasoning: false }));
      const toolName = data.name as string;
      const params = data.params as Record<string, unknown>;

      // Generate or reuse turnId: group tools that happen after a think/assistant message
      const state = get();
      const as = getAgentState(state, agentId);
      let turnId = as.currentTurnId;

      // If no current turnId, create one
      if (!turnId) {
        turnId = `turn-${Date.now()}`;
      }

      const toolMsg: ChatMessage = {
        id: `msg-tool-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        type: "tool_call",
        content: JSON.stringify(params, null, 2),
        timestamp: Date.now(),
        toolName,
        toolData: params,
        turnId,
        startTime: Date.now(),
      };
      set((state) => {
        const as = getAgentState(state, agentId);
        // Finalize any active think message (set endTime to stop the timer)
        let messages = as.messages;
        if (as.thinkingMessageId) {
          const now = Date.now();
          messages = messages.map((msg) =>
            msg.id === as.thinkingMessageId
              ? { ...msg, endTime: now }
              : msg,
          );
        }
        return {
          ...updateAgentState(state, agentId, {
            messages: [...messages, toolMsg],
            // End current streaming phase: thinking/reply ends when tools start executing.
            // This ensures the next iteration's content creates new messages.
            streamingMessageId: null,
            thinkingMessageId: null,
            isInThinkPhase: false,
            streamBuffer: "",
            currentTurnId: turnId,
          }),
        };
      });
      break;
    }

    case "tool_result": {
      const toolName = data.name as string;
      const result = data.result as Record<string, unknown>;
      const state = get();
      const as = getAgentState(state, agentId);

      const resultMsg: ChatMessage = {
        id: `msg-result-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        type: "tool_result",
        content: JSON.stringify(result, null, 2),
        timestamp: Date.now(),
        toolName,
        toolData: result,
        toolStatus: "success",
        turnId: as.currentTurnId || undefined,
      };
      set((state) => updateAgentState(state, agentId, {
        messages: [...getAgentState(state, agentId).messages, resultMsg],
      }));
      break;
    }

    case "done": {
      // Streaming complete — or non-streaming response with full content
      set((state) => updateAgentState(state, agentId, { isReasoning: false }));
      const usage = data.usage as TokenUsage | undefined;
      const content = data.content as string | undefined;
      const reasoningContent = data.reasoning_content as string | undefined;
      set((state) => {
        const as = getAgentState(state, agentId);
        let messages = [...as.messages];

        // If there's a streaming message with empty content (non-streaming mode),
        // fill in the content from the done event.
        if (content) {
          if (as.streamingMessageId) {
            const idx = messages.findIndex((m) => m.id === as.streamingMessageId);
            if (idx >= 0 && !messages[idx].content) {
              messages[idx] = { ...messages[idx], content };
            }
          } else {
            // Fallback: create a new assistant message if no streaming message exists.
            // This handles the case where sendViaWs no longer pre-creates an empty
            // placeholder and no chunk arrived to create one.
            const assistantMsgId = `msg-assistant-${Date.now()}`;
            const assistantMsg: ChatMessage = {
              id: assistantMsgId,
              type: "assistant",
              content,
              timestamp: Date.now(),
            };
            messages = [...messages, assistantMsg];
          }
        }

        // If there's reasoning_content in done event (DeepSeek non-streaming),
        // create a think message with endTime (thinking is already complete)
        if (reasoningContent && !as.thinkingMessageId) {
          const thinkMsgId = `msg-think-${Date.now()}`;
          const now = Date.now();
          const thinkMsg: ChatMessage = {
            id: thinkMsgId,
            type: "thought",
            content: reasoningContent,
            timestamp: now,
            startTime: now,
            endTime: now,
          };
          messages = [...messages, thinkMsg];
        }

        // Set endTime on the currently streaming think message (if any)
        // This stops the ThinkBlock timer when the LLM call finishes.
        if (as.thinkingMessageId) {
          const endTime = Date.now();
          messages = messages.map((msg) =>
            msg.id === as.thinkingMessageId && !msg.endTime
              ? { ...msg, endTime }
              : msg,
          );
        }

        return {
          sending: false,
          ...updateAgentState(state, agentId, {
            messages,
            streamingMessageId: null,
            tokenUsage: usage ?? as.tokenUsage,
            currentTurnId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
          }),
        };
      });
      // Update session title from first user message (only if not yet set)
      const doneAgentState = getAgentState(get(), agentId);
      updateSessionTitleFromMessages(doneAgentState.messages, agentId);
      break;
    }

    case "model_confirmed": {
      // Gateway confirms the model switch was forwarded to the Agent Runtime
      const confirmedModel = data.model as string;
      const confirmedProvider = data.provider as string | undefined;
      const confirmedAgentId = data.agentId as string | undefined;
      console.log("[ChatStore] Model switch confirmed:", confirmedModel, confirmedProvider);
      // Now persist to agentModels cache (model + provider)
      if (confirmedAgentId && confirmedModel) {
        set((state) => ({
          agentModels: {
            ...state.agentModels,
            [confirmedAgentId]: {
              model: confirmedModel,
              provider: confirmedProvider ?? state.currentProvider ?? "",
            },
          },
        }));
      }
      break;
    }

    case "error": {
      set((state) => updateAgentState(state, agentId, { isReasoning: false }));
      const errorMsg = data.message as string;
      console.error("[ChatStore] Server error:", errorMsg);
      // If model_switch failed, revert the optimistic currentModel update
      if (errorMsg && errorMsg.includes("cannot switch model")) {
        // Revert: reload the model from the backend
        const errorAgentId = data.agentId as string | undefined;
        if (errorAgentId) {
          get().loadAgentModel(errorAgentId);
        }
      }
      // Add system error message to the agent's per-agent state
      const errMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "system",
        content: `Error: ${errorMsg}`,
        timestamp: Date.now(),
      };
      set((state) => ({
        sending: false,
        ...updateAgentState(state, agentId, {
          messages: [...getAgentState(state, agentId).messages, errMsg],
          streamingMessageId: null,
          streamBuffer: "",
          thinkingMessageId: null,
          isInThinkPhase: false,
        }),
      }));
      break;
    }

    case "tool_approval_needed":
      usePermissionStore.getState().showApprovalDialog(data as unknown as ToolApprovalNeededEvent);
      break;

    case "memory_updated":
      // Trigger refresh of memory data if panel is open
      console.log("[WS] Memory updated event:", data);
      break;

    case "skill_executed":
      console.log("[WS] Skill executed event:", data);
      break;

    case "context_usage": {
      const usage = data as unknown as ContextUsageInfo;
      console.log("[ChatStore] context_usage RECEIVED for agent:", agentId, usage);
      set((state) => updateAgentState(state, agentId, { contextUsage: usage }));
      break;
    }

    case "iteration_limit_paused": {
      const { iteration, max_iterations, message } = data as {
        iteration: number;
        max_iterations: number;
        message: string;
      };
      set((state) => updateAgentState(state, agentId, {
        iterationLimitPaused: {
          iteration,
          maxIterations: max_iterations,
          message,
        },
      }));
      break;
    }

    default:
      console.log("[ChatStore] Unknown event type:", eventType, data);
  }
}
