import { create } from "zustand";
import { useChatStore } from "./chatStore";

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

interface BreakpointCondition {
  type: "on_phase" | "on_tool_call" | "on_iteration" | "on_tool_result";
  phase?: string;
  tool_name_pattern?: string;
  iteration?: number;
  is_error?: boolean;
}

interface Breakpoint {
  breakpoint_id: string;
  condition: BreakpointCondition;
  enabled: boolean;
}

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
  breakpoints: Breakpoint[];
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
    breakpoints: [],
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

/** Build the top-level "view" fields from a per-session state. */
function topLevelFromSession(st: PerSessionDebugState) {
  return {
    iteration: st.iteration,
    phase: st.phase,
    debugState: st.debugState,
    paused: st.paused,
    promptTokens: st.promptTokens,
    completionTokens: st.completionTokens,
    breakpoints: st.breakpoints,
    snapshots: st.snapshots,
    sectionCache: st.sectionCache,
    hasPendingPatches: st.hasPendingPatches,
  };
}

// ── Store interface ────────────────────────────────────────────────────

interface DebugStore {
  // Connection (shared — one WebSocket per agent)
  socket: WebSocket | null;
  connected: boolean;
  connecting: boolean;
  debugAgentId: string | null;

  // Current session ID — determines which per-session state is the live view
  currentSessionId: string | null;
  /** Per-session debug state map — preserved across session switches. */
  sessionStates: Record<string, PerSessionDebugState>;

  // Live view fields — reflect currentSessionId's state
  iteration: number;
  phase: Phase;
  debugState: DebugState;
  paused: boolean;
  promptTokens: number;
  completionTokens: number;
  breakpoints: Breakpoint[];
  snapshots: ContextSnapshotMeta[];
  sectionCache: Map<string, SectionContent>;
  hasPendingPatches: boolean;

  // Pending RPC (shared)
  nextRequestId: number;
  pendingRequests: Map<number, { resolve: (r: unknown) => void; reject: (e: Error) => void }>;

  // Actions
  connect: (agentId: string, debugPort?: number) => void;
  disconnect: () => void;
  setCurrentSessionId: (sessionId: string | null) => void;
  sendRequest: (method: string, params?: Record<string, unknown>) => Promise<unknown>;

  // Debug commands
  resume: () => Promise<void>;
  pause: () => Promise<void>;
  step: (granularity?: "iteration" | "phase") => Promise<void>;
  stop: () => Promise<void>;
  restart: () => Promise<void>;
  getState: () => Promise<void>;

  // Context commands
  getContextSnapshot: (iteration: number) => Promise<void>;
  getSection: (iteration: number, section: string) => Promise<SectionContent | null>;

  // Context editing commands (S2.8)
  rewind: (toIteration: number) => Promise<{ rewound_to_iteration: number; messages_trimmed_to: number }>;
  reExecute: () => Promise<{ has_patches: boolean }>;
  patchContext: (patches: Record<string, unknown>) => Promise<void>;
}

const initialTopLevel = topLevelFromSession(freshPerSessionState());

export const useDebugStore = create<DebugStore>((set, get) => ({
  // Connection
  socket: null,
  connected: false,
  connecting: false,
  debugAgentId: null,
  currentSessionId: null,
  sessionStates: {},

  // Live view fields
  ...initialTopLevel,

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
          get().getState().catch(() => {});
          console.log("[debugStore] WebSocket connected, sending initial step");
          get().sendRequest("debugger.step").catch((e) => console.warn("[debugStore] initial step failed:", e));
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
              store.pendingRequests.delete(msg.id);
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
    set({ socket: null, connected: false, connecting: false, debugAgentId: null, currentSessionId: null });
  },

  // ── Session switching ──────────────────────────────────────────────

  setCurrentSessionId: (sessionId: string | null) => {
    const prev = get().currentSessionId;
    if (prev === sessionId) return;
    console.log("[debugStore] setCurrentSessionId:", prev, "→", sessionId);

    // Switch the live view to the new session's state (or fresh if not yet created).
    const liveView = sessionId
      ? topLevelFromSession(ensureSessionState(get().sessionStates, sessionId))
      : initialTopLevel;

    set({
      currentSessionId: sessionId,
      ...liveView,
    });
  },

  // ── RPC ────────────────────────────────────────────────────────────

  sendRequest: (method: string, params: Record<string, unknown> = {}): Promise<unknown> => {
    return new Promise((resolve, reject) => {
      const state = get();
      if (!state.socket || !state.connected) {
        reject(new Error("Not connected to debug WebSocket"));
        return;
      }
      const id = state.nextRequestId;
      set({ nextRequestId: id + 1 });
      const request: JsonRpcRequest = { jsonrpc: "2.0", id, method, params };
      state.pendingRequests.set(id, { resolve, reject });
      try {
        state.socket.send(JSON.stringify(request));
      } catch (sendErr) {
        state.pendingRequests.delete(id);
        reject(new Error(`WebSocket send failed: ${sendErr}`));
      }
    });
  },

  // ── Event handler ──────────────────────────────────────────────────

  _handleEvent: function (event: JsonRpcEvent) {
    // Route events by session_id so background sessions' state is
    // updated correctly even when not currently displayed.
    const eventSessionId = event.params.session_id as string | undefined;
    const targetSid = eventSessionId ?? get().currentSessionId;
    if (!targetSid) return;

    const isCurrentSession = targetSid === get().currentSessionId;

    // Helper: patch per-session state, and sync to live view if current.
    const patchSession = (patch: Partial<PerSessionDebugState>) => {
      set((s) => {
        const updated = { ...ensureSessionState(s.sessionStates, targetSid), ...patch };
        const result: Partial<DebugStore> = {
          sessionStates: { ...s.sessionStates, [targetSid]: updated },
        };
        if (isCurrentSession) {
          Object.assign(result, topLevelFromSession(updated));
        }
        return result;
      });
    };

    // Helper: full setter for session state (e.g. for snapshot array updates).
    const setSession = (fn: (current: PerSessionDebugState) => PerSessionDebugState) => {
      set((s) => {
        const updated = fn(ensureSessionState(s.sessionStates, targetSid));
        const result: Partial<DebugStore> = {
          sessionStates: { ...s.sessionStates, [targetSid]: updated },
        };
        if (isCurrentSession) {
          Object.assign(result, topLevelFromSession(updated));
        }
        return result;
      });
    };

    switch (event.method) {
      case "debugger.onStep":
        patchSession({
          iteration: (event.params.iteration as number) ?? 0,
          phase: (event.params.phase as Phase) ?? "Idle",
          debugState: "Stepping",
          paused: true,
        });
        break;

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

      case "debugger.onBreakpoint":
        patchSession({ debugState: "Paused", paused: true });
        break;

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

  resume: async () => {
    await get().sendRequest("debugger.resume");
    patchCurrent({ debugState: "Running", paused: false });
  },

  pause: async () => {
    await get().sendRequest("debugger.pause");
    patchCurrent({ debugState: "Paused" });
  },

  step: async (granularity = "iteration") => {
    await get().sendRequest("debugger.step", { granularity });
    patchCurrent({ debugState: "Stepping" });
  },

  stop: async () => {
    await get().sendRequest("debugger.stop");
    patchCurrent({ debugState: "Stopped", paused: true });
  },

  restart: async () => {
    await get().sendRequest("debugger.restart");
    resetCurrent();
  },

  // ── State query ─────────────────────────────────────────────────────

  getState: async () => {
    const result = (await get().sendRequest("debugger.getState")) as {
      iteration: number;
      phase: Phase;
      state: DebugState;
      breakpoints: Breakpoint[];
      usage: { prompt_tokens: number; completion_tokens: number };
      paused?: boolean;
    };
    if (result) {
      const debugState = result.state ?? "Running";
      patchCurrent({
        iteration: result.iteration ?? 0,
        phase: result.phase ?? "Idle",
        debugState,
        breakpoints: result.breakpoints ?? [],
        promptTokens: result.usage?.prompt_tokens ?? 0,
        completionTokens: result.usage?.completion_tokens ?? 0,
        paused: debugState === "Paused",
      });
    }
  },

  // ── Context commands ────────────────────────────────────────────────

  getContextSnapshot: async (iteration: number) => {
    const result = (await get().sendRequest("debugger.getContextSnapshot", { iteration })) as
      | ContextSnapshotMeta
      | undefined;
    if (result) {
      applyCurrent((s) => {
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

  getSection: async (iteration: number, section: string): Promise<SectionContent | null> => {
    const cacheKey = `${iteration}:${section}`;
    const current = get().sectionCache;
    const cached = current.get(cacheKey);
    if (cached) return cached;
    try {
      const result = (await get().sendRequest("debugger.getSection", { iteration, section })) as
        | SectionContent
        | undefined;
      if (result) {
        applyCurrent((s) => {
          const updated = new Map(s.sectionCache);
          updated.set(cacheKey, result);
          return { ...s, sectionCache: updated };
        });
        return result;
      }
    } catch {
      // Section fetch failed — probably not built yet
    }
    return null;
  },

  // ── Context editing commands (S2.8) ────────────────────────────────

  patchContext: async (patches: Record<string, unknown>) => {
    await get().sendRequest("debugger.patchContext", { patches });
    patchCurrent({ hasPendingPatches: true });
  },

  rewind: async (toIteration: number) => {
    const result = (await get().sendRequest("debugger.rewind", { to_iteration: toIteration })) as {
      rewound_to_iteration: number;
      messages_trimmed_to: number;
    };
    applyCurrent((s) => {
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

  reExecute: async () => {
    const result = (await get().sendRequest("debugger.reExecute", {})) as { has_patches: boolean };
    patchCurrent({ hasPendingPatches: false, debugState: "Running", paused: false });
    return result;
  },
}));

// ── Internal helpers (called inside store actions) ────────────────────

/** Patch the current session's state AND sync to live view. */
function patchCurrent(patch: Partial<PerSessionDebugState>) {
  useDebugStore.setState((s) => {
    const sid = s.currentSessionId;
    if (!sid) return s;
    const updated = { ...ensureSessionState(s.sessionStates, sid), ...patch };
    return {
      sessionStates: { ...s.sessionStates, [sid]: updated },
      ...topLevelFromSession(updated),
    };
  });
}

/** Apply a transformation to the current session's state AND sync to live view. */
function applyCurrent(fn: (current: PerSessionDebugState, sid: string) => PerSessionDebugState) {
  useDebugStore.setState((s) => {
    const sid = s.currentSessionId;
    if (!sid) return s;
    const updated = fn(ensureSessionState(s.sessionStates, sid), sid);
    return {
      sessionStates: { ...s.sessionStates, [sid]: updated },
      ...topLevelFromSession(updated),
    };
  });
}

/** Reset the current session's state to fresh AND sync to live view. */
function resetCurrent() {
  useDebugStore.setState((s) => {
    const sid = s.currentSessionId;
    if (!sid) return { ...initialTopLevel };
    const fresh = freshPerSessionState();
    return {
      sessionStates: { ...s.sessionStates, [sid]: fresh },
      ...topLevelFromSession(fresh),
    };
  });
}

// Augment the interface for the internal _handleEvent
declare module "zustand" {
  interface StoreMutators<S, A> {}
}

interface DebugStore {
  _handleEvent: (event: JsonRpcEvent) => void;
}
