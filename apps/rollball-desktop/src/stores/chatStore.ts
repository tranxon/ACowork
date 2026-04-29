import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage, TokenUsage } from "../lib/types";

interface ChatStore {
  messages: ChatMessage[];
  streamingMessageId: string | null;
  sending: boolean;
  ws: WebSocket | null;
  tokenUsage: TokenUsage | null;
  currentModel: string | null;
  availableModels: string[];

  connectStream: (agentId: string, gatewayUrl: string) => void;
  sendMessage: (content: string, agentId: string) => void;
  disconnectStream: () => void;
  clearMessages: () => void;
  setCurrentModel: (model: string) => void;
  setAvailableModels: (models: string[]) => void;
}

/** Derive WebSocket URL from Gateway HTTP base URL */
function toWsUrl(httpUrl: string, agentId: string): string {
  return `${httpUrl.replace("http://", "ws://").replace("https://", "wss://")}/api/agents/${agentId}/stream`;
}

const DEFAULT_GATEWAY_URL = "http://127.0.0.1:19876";

export const useChatStore = create<ChatStore>((set, get) => ({
  messages: [],
  streamingMessageId: null,
  sending: false,
  ws: null,
  tokenUsage: null,
  currentModel: null,
  availableModels: [],

  connectStream: (agentId: string, gatewayUrl: string = DEFAULT_GATEWAY_URL) => {
    // Close existing connection
    const existing = get().ws;
    if (existing) {
      existing.close();
    }

    const wsUrl = toWsUrl(gatewayUrl, agentId);
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      console.warn("[ChatStore] WebSocket creation failed, will use HTTP fallback:", e);
      set({ ws: null });
      return;
    }

    ws.onopen = () => {
      console.log("[ChatStore] WebSocket connected for agent:", agentId);
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
      console.log("[ChatStore] WebSocket closed");
      set({ ws: null, sending: false });
    };

    ws.onerror = (err) => {
      console.warn("[ChatStore] WebSocket error (will fall back to HTTP):", err);
      set({ ws: null, sending: false });
    };

    set({ ws, messages: [], streamingMessageId: null, tokenUsage: null });
  },

  sendMessage: (content: string, agentId: string) => {
    const ws = get().ws;

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

    // Try WebSocket first (streaming)
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "message", content }));

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
      return;
    }

    // Fallback: send via Tauri HTTP command
    (async () => {
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
      } catch (e) {
        const errMsg: ChatMessage = {
          id: `msg-error-${Date.now()}`,
          type: "system",
          content: `Error: ${e}`,
          timestamp: Date.now(),
        };
        set((state) => ({
          messages: [...state.messages, errMsg],
          sending: false,
        }));
      }
    })();
  },

  disconnectStream: () => {
    const ws = get().ws;
    if (ws) {
      ws.close();
    }
    set({ ws: null, sending: false, streamingMessageId: null });
  },

  clearMessages: () => {
    set({ messages: [], tokenUsage: null });
  },
  setCurrentModel: (model: string) => {
    set({ currentModel: model });
  },
  setAvailableModels: (models: string[]) => {
    set({ availableModels: models, currentModel: models[0] ?? null });
  },
}));

/** Handle incoming WebSocket events */
function handleMessageEvent(
  data: Record<string, unknown>,
  set: (fn: Partial<ChatStore> | ((state: ChatStore) => Partial<ChatStore>)) => void,
  _get: () => ChatStore,
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

    case "error": {
      const errorMsg = data.message as string;
      console.error("[ChatStore] Server error:", errorMsg);
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

    default:
      console.log("[ChatStore] Unknown event type:", eventType, data);
  }
}
