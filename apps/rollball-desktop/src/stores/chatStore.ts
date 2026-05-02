import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage, TokenUsage, ToolApprovalNeededEvent } from "../lib/types";
import { usePermissionStore } from "./permissionStore";
import { getGatewayUrl } from "../lib/config";

interface ChatStore {
  messages: ChatMessage[];
  streamingMessageId: string | null;
  sending: boolean;
  ws: WebSocket | null;
  tokenUsage: TokenUsage | null;
  /** Current active model for the selected agent */
  currentModel: string | null;
  /** Current active provider for the selected agent */
  currentProvider: string | null;
  /** Per-agent model memory: agent_id → model name */
  agentModels: Record<string, string>;
  availableModels: string[];
  /** Current agent ID for stop functionality */
  currentAgentId: string | null;
  /** Whether the agent loop is paused at iteration limit, awaiting user continue */
  iterationLimitPaused: { iteration: number; maxIterations: number; message: string } | null;

  connectStream: (agentId: string, gatewayUrl: string) => void;
  sendMessage: (content: string, agentId: string) => Promise<void>;
  stopCurrentMessage: () => Promise<void>;
  disconnectStream: () => void;
  clearMessages: () => void;
  setCurrentModel: (model: string, agentId: string) => void;
  setAvailableModels: (models: string[]) => void;
  /** Continue agent execution after iteration limit pause */
  continueExecution: (agentId: string) => Promise<void>;
  /** Load model for a specific agent from Gateway API, returns the model name */
  loadAgentModel: (agentId: string) => Promise<string | null>;
  /** Load conversation history for a specific agent from Gateway API */
  loadConversationHistory: (agentId: string) => Promise<void>;
}

/** Derive WebSocket URL from Gateway HTTP base URL */
function toWsUrl(httpUrl: string, agentId: string): string {
  return `${httpUrl.replace("http://", "ws://").replace("https://", "wss://")}/api/agents/${agentId}/stream`;
}

/** Reconnect state — tracked outside zustand to avoid re-render loops */
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempts = 0;
const MAX_RECONNECT_ATTEMPTS = 10;
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 30000;

function scheduleReconnect(agentId: string, gatewayUrl: string) {
  if (reconnectTimer) return; // already scheduled
  if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
    console.warn("[ChatStore] Max reconnect attempts reached, giving up");
    return;
  }
  const delay = Math.min(RECONNECT_BASE_MS * Math.pow(1.5, reconnectAttempts), RECONNECT_MAX_MS);
  reconnectAttempts++;
  console.log(`[ChatStore] Reconnecting in ${Math.round(delay)}ms (attempt ${reconnectAttempts}/${MAX_RECONNECT_ATTEMPTS})`);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    const store = useChatStore.getState();
    // Only reconnect if still on the same agent
    if (store.ws === null) {
      store.connectStream(agentId, gatewayUrl);
    }
  }, delay);
}

function resetReconnect() {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  reconnectAttempts = 0;
}

export const useChatStore = create<ChatStore>((set, get) => ({
  messages: [],
  streamingMessageId: null,
  sending: false,
  ws: null,
  tokenUsage: null,
  currentModel: null,
  currentProvider: null,
  agentModels: {},
  availableModels: [],
  currentAgentId: null,
  iterationLimitPaused: null,

  connectStream: (agentId: string, gatewayUrl: string = getGatewayUrl()) => {
    // Update currentAgentId for stop functionality
    set({ currentAgentId: agentId });
    
    // Cancel any pending reconnect
    resetReconnect();

    // Close existing connection and clear its handlers to prevent stale callbacks
    const existing = get().ws;
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
      set({ ws: null });
      scheduleReconnect(agentId, gatewayUrl);
      return;
    }

    ws.onopen = () => {
      console.log("[ChatStore] WebSocket connected for agent:", agentId);
      resetReconnect(); // successful connection resets retry counter
      // Re-set ws to trigger React re-render with OPEN readyState
      set({ ws });
    };

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data);
        handleMessageEvent(data, set, get);
      } catch (e) {
        console.error("[ChatStore] Failed to parse WS message:", e);
      }
    };

    ws.onclose = () => {
      // Defensive check: ignore stale callbacks from a replaced WebSocket
      if (get().ws !== ws) {
        console.log("[ChatStore] Stale WebSocket closed, ignoring");
        return;
      }
      console.log("[ChatStore] WebSocket closed, scheduling reconnect");
      set({ ws: null, sending: false, streamingMessageId: null });
      scheduleReconnect(agentId, gatewayUrl);
    };

    ws.onerror = (err) => {
      // Defensive check: ignore stale callbacks from a replaced WebSocket
      if (get().ws !== ws) {
        console.log("[ChatStore] Stale WebSocket error, ignoring");
        return;
      }
      console.warn("[ChatStore] WebSocket error:", err);
      // Don't set ws: null here — onclose will fire after onerror
    };

    set({ ws, streamingMessageId: null, tokenUsage: null, currentAgentId: agentId });
  },

  sendMessage: async (content: string, agentId: string) => {
    const { ws } = get();

    // Add user message
    const userMsg: ChatMessage = {
      id: `msg-user-${Date.now()}`,
      type: "user",
      content,
      timestamp: Date.now(),
    };
    set((state) => ({
      messages: [...state.messages, userMsg],
      sending: true,
    }));

    // Helper: send via WebSocket and set up streaming placeholder
    const sendViaWs = (socket: WebSocket) => {
      socket.send(JSON.stringify({ type: "message", content }));

      // Create placeholder for assistant streaming message
      const assistantMsgId = `msg-assistant-${Date.now()}`;
      const assistantMsg: ChatMessage = {
        id: assistantMsgId,
        type: "assistant",
        content: "",
        timestamp: Date.now(),
      };
      set((state) => ({
        messages: [...state.messages, assistantMsg],
        streamingMessageId: assistantMsgId,
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
        { agentId, content },
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
        messages: [...state.messages, replyMsg],
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
        messages: [...state.messages, errorMsg],
        sending: false,
      }));
    }
  },

  stopCurrentMessage: async () => {
    const { currentAgentId, streamingMessageId, ws } = get();
    if (!currentAgentId || !streamingMessageId) {
      console.warn("[ChatStore] No active streaming message to stop");
      return;
    }

    console.log("[ChatStore] Stopping current message for agent:", currentAgentId);

    // Send stop command via WebSocket if available
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "stop", agentId: currentAgentId }));
    }

    // Also send via HTTP API as fallback
    try {
      await fetch(`${getGatewayUrl()}/api/agents/${currentAgentId}/stop`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      });
    } catch (error) {
      console.warn("[ChatStore] HTTP stop request failed:", error);
    }

    // Update UI state immediately
    set({ sending: false, streamingMessageId: null });
  },

  disconnectStream: () => {
    resetReconnect(); // stop any pending reconnect
    const ws = get().ws;
    if (ws) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
    }
    set({ ws: null, sending: false, streamingMessageId: null });
  },

  clearMessages: () => {
    set({ messages: [], tokenUsage: null });
  },
  setCurrentModel: (model: string, agentId: string) => {
    // Optimistically update UI only — don't cache until confirmed by backend
    const prevModel = get().currentModel;
    set({ currentModel: model });
    // Send model_switch message to Agent via WebSocket when user changes model
    const ws = get().ws;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "model_switch", model, agentId, _prevModel: prevModel }));
    } else {
      // No WebSocket — revert immediately
      set({ currentModel: prevModel });
    }
  },
  setAvailableModels: (models: string[]) => {
    set({ availableModels: models, currentModel: models[0] ?? null });
  },
  continueExecution: async (agentId: string) => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/continue`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      });
      if (resp.ok) {
        set({ iterationLimitPaused: null, sending: true });
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
        set((state) => ({
          currentModel: data.model,
          currentProvider: data.provider,
          agentModels: { ...state.agentModels, [agentId]: data.model },
          availableModels: data.available_models?.length ? data.available_models : state.availableModels,
        }));
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
        type: msg.role === "user" ? "user" : msg.role === "assistant" ? "assistant" : "system",
        content: msg.content,
        timestamp: msg.timestamp * 1000, // Convert seconds to milliseconds
      }));

      set({ messages: historyMessages });
    } catch (e) {
      console.error("Failed to load conversation history:", e);
      // Silently fail — empty chat is acceptable
    }
  },
}));

/** Handle incoming WebSocket events */
function handleMessageEvent(
  data: Record<string, unknown>,
  set: (fn: Partial<ChatStore> | ((state: ChatStore) => Partial<ChatStore>)) => void,
  get: () => ChatStore,
) {
  const eventType = data.type as string;

  switch (eventType) {
    case "connected":
      // Initial connection acknowledgment
      break;

    case "ack":
      // Message received acknowledgment
      break;

    case "chunk": {
      // Streaming text chunk — append to current assistant message
      const delta = data.delta as string;
      set((state) => ({
        messages: state.messages.map((msg) =>
          msg.id === state.streamingMessageId ? { ...msg, content: msg.content + delta } : msg,
        ),
      }));
      break;
    }

    case "tool_call": {
      const toolName = data.name as string;
      const params = data.params as Record<string, unknown>;
      const toolMsg: ChatMessage = {
        id: `msg-tool-${Date.now()}`,
        type: "tool_call",
        content: JSON.stringify(params, null, 2),
        timestamp: Date.now(),
        toolName,
        toolData: params,
      };
      set((state) => ({
        messages: [...state.messages, toolMsg],
      }));
      break;
    }

    case "tool_result": {
      const toolName = data.name as string;
      const result = data.result as Record<string, unknown>;
      const resultMsg: ChatMessage = {
        id: `msg-result-${Date.now()}`,
        type: "tool_result",
        content: JSON.stringify(result, null, 2),
        timestamp: Date.now(),
        toolName,
        toolData: result,
        toolStatus: "success",
      };
      set((state) => ({
        messages: [...state.messages, resultMsg],
      }));
      break;
    }

    case "done": {
      // Streaming complete — or non-streaming response with full content
      const usage = data.usage as TokenUsage | undefined;
      const content = data.content as string | undefined;
      set((state) => {
        const messages = [...state.messages];
        // If there's a streaming message with empty content (non-streaming mode),
        // fill in the content from the done event.
        if (state.streamingMessageId && content) {
          const idx = messages.findIndex((m) => m.id === state.streamingMessageId);
          if (idx >= 0 && !messages[idx].content) {
            messages[idx] = { ...messages[idx], content };
          }
        }
        return {
          messages,
          streamingMessageId: null,
          sending: false,
          tokenUsage: usage ?? state.tokenUsage,
        };
      });
      break;
    }

    case "model_confirmed": {
      // Gateway confirms the model switch was forwarded to the Agent Runtime
      const confirmedModel = data.model as string;
      const confirmedAgentId = data.agentId as string | undefined;
      console.log("[ChatStore] Model switch confirmed:", confirmedModel);
      // Now persist to agentModels cache
      if (confirmedAgentId && confirmedModel) {
        set((state) => ({
          agentModels: { ...state.agentModels, [confirmedAgentId]: confirmedModel },
        }));
      }
      break;
    }

    case "error": {
      const errorMsg = data.message as string;
      console.error("[ChatStore] Server error:", errorMsg);
      // If model_switch failed, revert the optimistic currentModel update
      if (errorMsg && errorMsg.includes("cannot switch model")) {
        // Revert: reload the model from the backend
        const agentId = data.agentId as string | undefined;
        if (agentId) {
          get().loadAgentModel(agentId);
        }
      }
      // Add system error message
      const errMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "system",
        content: `Error: ${errorMsg}`,
        timestamp: Date.now(),
      };
      set((state) => ({
        messages: [...state.messages, errMsg],
        sending: false,
        streamingMessageId: null,
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

    case "iteration_limit_paused": {
      const { iteration, max_iterations, message } = data as {
        iteration: number;
        max_iterations: number;
        message: string;
      };
      set({
        iterationLimitPaused: {
          iteration,
          maxIterations: max_iterations,
          message,
        },
      });
      break;
    }

    default:
      console.log("[ChatStore] Unknown event type:", eventType, data);
  }
}
