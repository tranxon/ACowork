import { create } from "zustand";
import { useChatStore } from "./chatStore";
import { useAgentStore } from "./agentStore";

// ── Debug Protocol types ──────────────────────────────────────────────

type Phase =
  | "BudgetCheck"
  | "BuildContext"
  | "LlmCall"
  | "ParseResponse"
  | "ToolExecution"
  | "AppendHistory"
  | "Idle";

/** Mirrors backend DebugState — the single source of truth for execution state. */
type DebugState = "Running" | "Paused" | "Stepping" | "Stopped";

interface SectionMeta {
  size_bytes: number;
  token_estimate: number;
  hash: string;
}

interface ContextSnapshotMeta {
  iteration: number;
  built_at: string;
  sections: {
    system_prompt: SectionMeta;
    workspace_context: SectionMeta;
    environment: SectionMeta;
    tool_definitions: SectionMeta;
    skill_instructions: SectionMeta;
    retrieved_memory: SectionMeta;
    identity_context: SectionMeta;
  };
  total_token_estimate: number;
  phase: Phase;
}

interface SectionContent {
  content: string;
  hash: string;
  token_count: number;
}

// ── Per-session debug state ───────────────────────────────────────────
// Each session gets its own independent copy preserved across session
// switches. The top-level fields (iteration, phase, snapshots, etc.) are
// a live view into the current session's state.

interface PerSessionDebugState {
  iteration: number;
  phase: Phase;
  debugState: DebugState;
  paused: boolean;
  promptTokens: number;
  completionTokens: number;
  snapshots: ContextSnapshotMeta[];
  sectionCache: Map<string, SectionContent>;
  hasPendingPatches: boolean;
}

function freshPerSessionState(): PerSessionDebugState {
  return {
    iteration: 0,
    phase: "Idle" as Phase,
    debugState: "Stepping" as DebugState,
    paused: false,
    promptTokens: 0,
    completionTokens: 0,
    snapshots: [],
    sectionCache: new Map(),
    hasPendingPatches: false,
  };
}

// ── JSON-RPC types ─────────────────────────────────────────────────────

interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: number;
  method: string;
  params: Record<string, unknown>;
}

interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number;
  result?: unknown;
  error?: { code: number; message: string };
}

interface JsonRpcEvent {
  jsonrpc: "2.0";
  method: string;
  params: Record<string, unknown>;
}

// ── Helpers ────────────────────────────────────────────────────────────

const DEFAULT_DEBUG_PORT = 19878;
let connectRetryTimer: ReturnType<typeof setTimeout> | null = null;

/** Get or create the per-session state entry for a session ID. */
function ensureSessionState(
  states: Record<string, PerSessionDebugState>,
  sid: string,
): PerSessionDebugState {
  if (!states[sid]) {
    states[sid] = freshPerSessionState();
  }
  return states[sid];
}

// ── Store interface ────────────────────────────────────────────────────

interface DebugStore {
  // Connection (shared — one WebSocket per agent)
  socket: WebSocket | null;
  connected: boolean;
  connecting: boolean;
  debugAgentId: string | null;

  /** Per-session debug state map — preserved across session switches. */
  sessionStates: Record<string, PerSessionDebugState>;

  // Pending RPC (shared)
  nextRequestId: number;
  pendingRequests: Map<number, { resolve: (r: unknown) => void; reject: (e: Error) => void }>;

  // Actions
  connect: (agentId: string, debugPort?: number) => void;
  disconnect: () => void;
  sendRequest: (sessionId: string | null, method: string, params?: Record<string, unknown>) => Promise<unknown>;

  // Debug commands
  resume: (sessionId: string | null) => Promise<void>;
  pause: (sessionId: string | null) => Promise<void>;
  step: (sessionId: string | null, granularity?: "iteration" | "phase") => Promise<void>;
  stop: (sessionId: string | null) => Promise<void>;
  restart: (sessionId: string | null) => Promise<void>;
  getState: (sessionId: string | null) => Promise<void>;

  // Context commands
  getContextSnapshot: (sessionId: string | null, iteration: number) => Promise<void>;
  getSection: (sessionId: string | null, iteration: number, section: string) => Promise<SectionContent | null>;

  // Context editing commands (S2.8)
  rewind: (sessionId: string | null, toIteration: number) => Promise<{ rewound_to_iteration: number; messages_trimmed_to: number }>;
  reExecute: (sessionId: string | null) => Promise<{ has_patches: boolean }>;
  patchContext: (sessionId: string | null, patches: Record<string, unknown>) => Promise<void>;
}

export const useDebugStore = create<DebugStore>((set, get) => ({
  // Connection
  socket: null,
  connected: false,
  connecting: false,
  debugAgentId: null,
  sessionStates: {},

  // Pending RPC
  nextRequestId: 1,
  pendingRequests: new Map(),

  // ── Connection ─────────────────────────────────────────────────────

  connect: (agentId: string, debugPort?: number) => {
    const state = get();
    if (state.connected && state.debugAgentId === agentId && state.socket?.readyState === WebSocket.OPEN) return;
    if (state.socket) {
      state.socket.close();
    }
    if (connectRetryTimer) {
      clearTimeout(connectRetryTimer);
      connectRetryTimer = null;
    }

    const port = debugPort ?? DEFAULT_DEBUG_PORT;
    let retries = 0;
    const maxRetries = 10;
    const retryDelayMs = 1000;

    const tryConnect = () => {
      if (get().connected && get().debugAgentId === agentId) return;
      set({ connecting: true, debugAgentId: agentId });

      const url = `ws://127.0.0.1:${port}`;
      const socket = new WebSocket(url);

      socket.onopen = () => {
        if (get().socket !== socket) {
          console.log("[debugStore] onopen: socket is not current, ignoring");
          return;
        }
        set({ connected: true, connecting: false });
        setTimeout(() => {
          const sessionId = useChatStore.getState().getActiveSessionId(agentId);
          get().getState(sessionId).catch(() => { });
          console.log("[debugStore] WebSocket connected, sending initial step");
          get().sendRequest(sessionId, "debugger.step").catch((e) => console.warn("[debugStore] initial step failed:", e));
        }, 0);
      };

      socket.onmessage = (event: MessageEvent) => {
        if (get().socket !== socket) return;
        try {
          const msg = JSON.parse(event.data) as JsonRpcResponse | JsonRpcEvent;
          const store = get();

          if ("id" in msg && msg.id !== undefined) {
            const pending = store.pendingRequests.get(msg.id);
            if (pending) {
              const nextPending = new Map(store.pendingRequests);
              nextPending.delete(msg.id);
              set({ pendingRequests: nextPending });
              if (msg.error) {
                pending.reject(new Error(msg.error.message));
              } else {
                pending.resolve(msg.result);
              }
            }
          } else if ("method" in msg) {
            console.log("[debugStore] received event:", msg.method, msg.params);
            store._handleEvent(msg as JsonRpcEvent);
          } else {
            console.warn("[debugStore] unexpected message format:", msg);
          }
        } catch (e) {
          console.warn("[debugStore] failed to parse message:", e);
        }
      };

      socket.onclose = () => {
        const isCurrent = get().socket === socket;
        const willRetry = isCurrent && retries < maxRetries;
        if (isCurrent) {
          set({ connected: false, connecting: willRetry, socket: null });
        }
        if (willRetry && !get().connected) {
          retries++;
          connectRetryTimer = setTimeout(tryConnect, retryDelayMs);
        }
      };

      socket.onerror = () => { /* onclose handles retry */ };
      set({ socket });
    };

    tryConnect();
  },

  disconnect: () => {
    if (connectRetryTimer) {
      clearTimeout(connectRetryTimer);
      connectRetryTimer = null;
    }
    const { socket } = get();
    if (socket) socket.close();
    set({ socket: null, connected: false, connecting: false, debugAgentId: null });
  },

  // ── RPC ────────────────────────────────────────────────────────────

  sendRequest: (sessionId: string | null, method: string, params: Record<string, unknown> = {}): Promise<unknown> => {
    return new Promise((resolve, reject) => {
      set((state) => {
        if (!state.socket || !state.connected) {
          reject(new Error("Not connected to debug WebSocket"));
          return {};
        }
        // Auto-inject current session_id so the backend knows which
        // DebugController to query.  Callers can override by passing
        // session_id explicitly — their value wins because ...params
        // comes after the default.
        const finalParams: Record<string, unknown> = {
          session_id: sessionId,
          ...params,
        };
        const id = state.nextRequestId;
        const newPending = new Map(state.pendingRequests);
        newPending.set(id, { resolve, reject });
        const request: JsonRpcRequest = { jsonrpc: "2.0", id, method, params: finalParams };
        try {
          state.socket.send(JSON.stringify(request));
        } catch (sendErr) {
          reject(new Error(`WebSocket send failed: ${sendErr}`));
          return {};
        }
        return { nextRequestId: id + 1, pendingRequests: newPending };
      });
    });
  },

  // ── Event handler ──────────────────────────────────────────────────

  _handleEvent: function (event: JsonRpcEvent) {
    // Route events by session_id so background sessions' state is
    // updated correctly even when not currently displayed.
    const targetSid = event.params.session_id as string | undefined;
    if (!targetSid) return;

    const patchSession = (patch: Partial<PerSessionDebugState>) => {
      set((s) => {
        const updated = { ...ensureSessionState(s.sessionStates, targetSid), ...patch };
        return {
          sessionStates: { ...s.sessionStates, [targetSid]: updated },
        };
      });
    };

    const setSession = (fn: (current: PerSessionDebugState) => PerSessionDebugState) => {
      set((s) => {
        const updated = fn(ensureSessionState(s.sessionStates, targetSid));
        return {
          sessionStates: { ...s.sessionStates, [targetSid]: updated },
        };
      });
    };

    switch (event.method) {
      case "debugger.onStep": {
        const usage = (event.params as Record<string, unknown>).usage as
          | { prompt_tokens: number; completion_tokens: number }
          | undefined;
        patchSession({
          iteration: (event.params.iteration as number) ?? 0,
          phase: (event.params.phase as Phase) ?? "Idle",
          debugState: "Stepping",
          paused: true,
          promptTokens: usage?.prompt_tokens ?? 0,
          completionTokens: usage?.completion_tokens ?? 0,
        });
        break;
      }

      case "debugger.onPaused":
        patchSession({ debugState: "Paused", paused: true });
        break;

      case "debugger.onResumed":
        patchSession({ debugState: "Running", paused: false });
        break;

      case "debugger.onContextBuilt": {
        const params = event.params as Record<string, unknown>;
        const iteration = (params.iteration as number) ?? 0;
        const sections = params.sections as ContextSnapshotMeta["sections"] | undefined;
        const total_token_estimate = (params.total_token_estimate as number) ?? 0;
        console.log("[debugStore] onContextBuilt: sid=", targetSid, "iteration=", iteration, "sections=", !!sections);
        if (sections) {
          setSession((current) => {
            const currentSnapshots = current.snapshots;
            const maxExisting = currentSnapshots.length > 0
              ? Math.max(...currentSnapshots.map((sn) => sn.iteration))
              : 0;
            if (currentSnapshots.length > 0 && iteration > maxExisting + 1) {
              console.log("[debugStore] onContextBuilt: discarding stale event sid=", targetSid, "iteration=", iteration);
              return current;
            }
            if (currentSnapshots.some((sn) => sn.iteration === iteration)) {
              console.log("[debugStore] onContextBuilt: skipping duplicate sid=", targetSid, "iteration=", iteration);
              return current;
            }
            return {
              ...current,
              snapshots: [
                ...currentSnapshots,
                { iteration, built_at: new Date().toISOString(), sections, total_token_estimate, phase: current.phase },
              ],
            };
          });
        }
        break;
      }

      case "debugger.onExecutionStateChange": {
        const newState = event.params.new_state as DebugState;
        if (newState) {
          patchSession({ debugState: newState, paused: newState === "Paused" });
        }
        break;
      }
    }
  },

  // ── Control commands ────────────────────────────────────────────────

  resume: async (sessionId: string | null) => {
    await get().sendRequest(sessionId, "debugger.resume");
    patchSessionDebug(sessionId, { debugState: "Running", paused: false });
  },

  pause: async (sessionId: string | null) => {
    await get().sendRequest(sessionId, "debugger.pause");
    patchSessionDebug(sessionId, { debugState: "Paused" });
  },

  step: async (sessionId: string | null, granularity = "iteration") => {
    await get().sendRequest(sessionId, "debugger.step", { granularity });
    patchSessionDebug(sessionId, { debugState: "Stepping" });
  },

  stop: async (sessionId: string | null) => {
    await get().sendRequest(sessionId, "debugger.stop");
    patchSessionDebug(sessionId, { debugState: "Stopped", paused: true });
  },

  restart: async (sessionId: string | null) => {
    const agentId = get().debugAgentId;
    if (!agentId) {
      console.warn("[debugStore] restart: no debugAgentId, skipping");
      return;
    }
    // Route through Gateway gRPC EnableDebugMode (the debugger.restart RPC
    // was removed when restart-to-debug was refactored to be processless).
    try {
      await useAgentStore.getState().restartAgentInDebug(agentId);
    } catch (e) {
      console.error("[debugStore] restart: restartAgentInDebug failed:", e);
      throw e;
    }
    // Reset UI state; the backend will push fresh events after reconnect.
    patchSessionDebug(sessionId, {
      iteration: 0,
      phase: "Idle",
      debugState: "Stepping",
      paused: false,
      snapshots: [],
      sectionCache: new Map(),
      hasPendingPatches: false,
    });
  },

  // ── State query ─────────────────────────────────────────────────────

  getState: async (sessionId: string | null) => {
    const result = (await get().sendRequest(sessionId, "debugger.getState")) as {
      iteration: number;
      phase: Phase;
      state: DebugState;
      usage: { prompt_tokens: number; completion_tokens: number };
      paused?: boolean;
    };
    if (result) {
      const debugState = result.state ?? "Running";
      patchSessionDebug(sessionId, {
        iteration: result.iteration ?? 0,
        phase: result.phase ?? "Idle",
        debugState,
        promptTokens: result.usage?.prompt_tokens ?? 0,
        completionTokens: result.usage?.completion_tokens ?? 0,
        paused: debugState === "Paused",
      });
    }
  },

  // ── Context commands ────────────────────────────────────────────────

  getContextSnapshot: async (sessionId: string | null, iteration: number) => {
    const result = (await get().sendRequest(sessionId, "debugger.getContextSnapshot", { iteration })) as
      | ContextSnapshotMeta
      | undefined;
    if (result) {
      applySessionDebug(sessionId, (s) => {
        const idx = s.snapshots.findIndex((sn) => sn.iteration === iteration);
        if (idx >= 0) {
          const updated = [...s.snapshots];
          updated[idx] = result;
          return { ...s, snapshots: updated };
        }
        return { ...s, snapshots: [...s.snapshots, result] };
      });
    }
  },

  getSection: async (sessionId: string | null, iteration: number, section: string): Promise<SectionContent | null> => {
    const cacheKey = `${iteration}:${section}`;
    const current = sessionId ? get().sessionStates[sessionId]?.sectionCache : undefined;
    const cached = current?.get(cacheKey);
    if (cached) return cached;
    try {
      const result = (await get().sendRequest(sessionId, "debugger.getSection", { iteration, section })) as
        | SectionContent
        | undefined;
      if (result) {
        applySessionDebug(sessionId, (s) => {
          const updated = new Map(s.sectionCache);
          updated.set(cacheKey, result);
          return { ...s, sectionCache: updated };
        });
        return result;
      }
    } catch {
    }
    return null;
  },

  // ── Context editing commands (S2.8) ────────────────────────────────

  patchContext: async (sessionId: string | null, patches: Record<string, unknown>) => {
    await get().sendRequest(sessionId, "debugger.patchContext", { patches });
    patchSessionDebug(sessionId, { hasPendingPatches: true });
  },

  rewind: async (sessionId: string | null, toIteration: number) => {
    const result = (await get().sendRequest(sessionId, "debugger.rewind", { to_iteration: toIteration })) as {
      rewound_to_iteration: number;
      messages_trimmed_to: number;
    };
    applySessionDebug(sessionId, (s) => {
      const newCache = new Map(s.sectionCache);
      const keysToDelete: string[] = [];
      newCache.forEach((_, key) => {
        if (parseInt(key.split(":")[0], 10) > toIteration) keysToDelete.push(key);
      });
      keysToDelete.forEach((k) => newCache.delete(k));
      return {
        ...s,
        sectionCache: newCache,
        snapshots: s.snapshots.filter((sn) => sn.iteration <= toIteration),
        hasPendingPatches: false,
        iteration: toIteration,
      };
    });
    const agentId = get().debugAgentId;
    if (agentId && result.messages_trimmed_to > 0) {
      useChatStore.getState().trimMessagesTo(agentId, result.messages_trimmed_to);
    }
    return result;
  },

  reExecute: async (sessionId: string | null) => {
    const result = (await get().sendRequest(sessionId, "debugger.reExecute", {})) as { has_patches: boolean };
    patchSessionDebug(sessionId, { hasPendingPatches: false, debugState: "Running", paused: false });
    return result;
  },
}));

// ── Internal helpers (called inside store actions) ────────────────────

function patchSessionDebug(sessionId: string | null, patch: Partial<PerSessionDebugState>) {
  if (!sessionId) return;
  useDebugStore.setState((s) => {
    const updated = { ...ensureSessionState(s.sessionStates, sessionId), ...patch };
    return {
      sessionStates: { ...s.sessionStates, [sessionId]: updated },
    };
  });
}

function applySessionDebug(sessionId: string | null, fn: (current: PerSessionDebugState, sid: string) => PerSessionDebugState) {
  if (!sessionId) return;
  useDebugStore.setState((s) => {
    const updated = fn(ensureSessionState(s.sessionStates, sessionId), sessionId);
    return {
      sessionStates: { ...s.sessionStates, [sessionId]: updated },
    };
  });
}

// Augment the interface for the internal _handleEvent
declare module "zustand" {
  interface StoreMutators<S, A> { }
}

interface DebugStore {
  _handleEvent: (event: JsonRpcEvent) => void;
}
