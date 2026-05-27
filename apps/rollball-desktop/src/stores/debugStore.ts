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

// ── Session-level debug info ─────────────────────────────────────────
// Per-agent debug sessions: agent_id → DebugSessionStore
// This allows the debug store to remain static when switching agents.
// We multiplex the single WebSocket but keep state scoped.

const DEFAULT_DEBUG_PORT = 19878;

/** Retry timer handle for WebSocket connection retries. Module-level so
 *  it persists across connect() calls and can be cleared on disconnect. */
let connectRetryTimer: ReturnType<typeof setTimeout> | null = null;

// ── Store interface ────────────────────────────────────────────────────

interface DebugStore {
  // Connection
  socket: WebSocket | null;
  connected: boolean;
  connecting: boolean;

  // For the currently selected agent (should match the WebSocket target)
  debugAgentId: string | null;

  // Execution state (agent-scoped)
  iteration: number;
  phase: Phase;
  /** Execution state mirroring backend DebugState — single source of truth. */
  debugState: DebugState;
  paused: boolean;
  promptTokens: number;
  completionTokens: number;
  breakpoints: Breakpoint[];

  // Context snapshots (per-iteration metadata)
  snapshots: ContextSnapshotMeta[];
  // Lazy-loaded section contents: key = `${iteration}:${section}`
  sectionCache: Map<string, SectionContent>;

  // Pending RPC
  nextRequestId: number;
  pendingRequests: Map<number, { resolve: (r: unknown) => void; reject: (e: Error) => void }>;

  // Actions
  connect: (agentId: string, debugPort?: number) => void;
  disconnect: () => void;
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
  hasPendingPatches: boolean;
}

export const useDebugStore = create<DebugStore>((set, get) => ({
  socket: null,
  connected: false,
  connecting: false,
  debugAgentId: null,
  iteration: 0,
  phase: "Idle" as Phase,
  debugState: "Stepping" as DebugState,
  paused: false,
  promptTokens: 0,
  completionTokens: 0,
  breakpoints: [],
  snapshots: [],
  sectionCache: new Map(),
  nextRequestId: 1,
  pendingRequests: new Map(),

  connect: (agentId: string, debugPort?: number) => {
    const state = get();
    // If already connected to this agent with an open socket, no-op.
    // Guard: only skip when the socket is actually open (WebSocket.OPEN === 1).
    // After a ResultsPanel remount (e.g. coming back from Settings), the
    // socket may be stale/closed while the store still thinks it's connected.
    if (state.connected && state.debugAgentId === agentId && state.socket?.readyState === WebSocket.OPEN) return;
    // If connecting to a different agent, disconnect first
    if (state.socket) {
      state.socket.close();
    }

    // Clear any pending retry timer
    if (connectRetryTimer) {
      clearTimeout(connectRetryTimer);
      connectRetryTimer = null;
    }

    // Reset debug state when starting a new connection.
    // Always clear — reconnect retries go through tryConnect() directly,
    // not through connect().
    set({
      iteration: 0,
      phase: "Idle" as Phase,
      debugState: "Stepping" as DebugState,
      paused: false,
      snapshots: [],
      sectionCache: new Map(),
      breakpoints: [],
      hasPendingPatches: false,
    });

    const port = debugPort ?? DEFAULT_DEBUG_PORT;

    // Retry state: attempts start at 0, max 10 retries with 1s delay each
    let retries = 0;
    const maxRetries = 10;
    const retryDelayMs = 1000;

    const tryConnect = () => {
      // If already connected, stop retrying
      if (get().connected && get().debugAgentId === agentId) return;

      set({ connecting: true, debugAgentId: agentId });

      const url = `ws://127.0.0.1:${port}`;
      const socket = new WebSocket(url);

      socket.onopen = () => {
        // Guard: only update state if this socket is still the current one.
        // React Strict Mode double-mount + retry logic can create multiple
        // sockets whose callbacks race against each other.
        if (get().socket !== socket) {
          console.log("[debugStore] onopen: socket is not current, ignoring");
          return;
        }
        set({ connected: true, connecting: false });
        // Tauri webview WebSocket quirk: onopen fires before the internal
        // send buffer is ready.  Defer the initial requests by one microtask
        // to let the WebSocket fully stabilize.
        setTimeout(() => {
          get().getState().catch(() => {});
          console.log("[debugStore] WebSocket connected, sending initial step");
          // Send step to enter stepping mode so first iteration auto-pauses
          get().sendRequest("debugger.step").catch((e) => console.warn("[debugStore] initial step failed:", e));
        }, 0);
      };

      socket.onmessage = (event: MessageEvent) => {
        // Guard: ignore messages from a socket that is no longer current.
        if (get().socket !== socket) return;
        try {
          const msg = JSON.parse(event.data) as JsonRpcResponse | JsonRpcEvent;
          const store = get();

          if ("id" in msg && msg.id !== undefined) {
            // Response to a request
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
            // Server-pushed event
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
        // Guard: only clear state if THIS socket is still the current one.
        // Stale socket onclose from a previous connect() cycle must not
        // overwrite the state of the current socket.
        const isCurrent = get().socket === socket;
        const willRetry = isCurrent && retries < maxRetries;
        if (isCurrent) {
          // Keep connecting=true if we're about to retry, so the UI
          // shows the spinner instead of a disconnected state flash.
          set({ connected: false, connecting: willRetry, socket: null });
        }
        // Retry only when this is the current socket that just closed,
        // and we aren't already connected through a newer socket.
        if (willRetry && !get().connected) {
          retries++;
          connectRetryTimer = setTimeout(tryConnect, retryDelayMs);
        }
      };

      socket.onerror = () => {
        // onclose will fire after onerror, handle retry there
      };

      set({ socket });
    };

    tryConnect();
  },

  disconnect: () => {
    // Clear any pending retry timer
    if (connectRetryTimer) {
      clearTimeout(connectRetryTimer);
      connectRetryTimer = null;
    }
    const { socket } = get();
    if (socket) {
      socket.close();
    }
    set({
      socket: null,
      connected: false,
      connecting: false,
      debugAgentId: null,
      iteration: 0,
      phase: "Idle" as Phase,
      debugState: "Stepping" as DebugState,
      paused: false,
      snapshots: [],
      sectionCache: new Map(),
      breakpoints: [],
    });
  },

  sendRequest: (method: string, params: Record<string, unknown> = {}): Promise<unknown> => {
    return new Promise((resolve, reject) => {
      const state = get();
      if (!state.socket || !state.connected) {
        reject(new Error("Not connected to debug WebSocket"));
        return;
      }

      const id = state.nextRequestId;
      set({ nextRequestId: id + 1 });

      const request: JsonRpcRequest = {
        jsonrpc: "2.0",
        id,
        method,
        params,
      };

      state.pendingRequests.set(id, { resolve, reject });

      // socket.send() may throw synchronously even when readyState reports
      // OPEN — this is a known quirk in some WebSocket implementations
      // (including Tauri's webview).  Catch the exception and convert it
      // to a proper rejection so callers can handle the error gracefully.
      try {
        state.socket.send(JSON.stringify(request));
      } catch (sendErr) {
        state.pendingRequests.delete(id);
        reject(new Error(`WebSocket send failed: ${sendErr}`));
      }
    });
  },

  // Event handler (internal, not exposed)
  _handleEvent(event: JsonRpcEvent) {
    switch (event.method) {
      case "debugger.onStep":
        set({
          iteration: (event.params.iteration as number) ?? 0,
          phase: (event.params.phase as Phase) ?? "Idle",
          debugState: "Stepping",
          paused: true,
        });
        break;
      case "debugger.onPaused":
        set({ debugState: "Paused", paused: true });
        break;
      case "debugger.onResumed":
        set({ debugState: "Running", paused: false });
        break;
      case "debugger.onContextBuilt": {
        const params = event.params as Record<string, unknown>;
        // Server sends flat fields: { iteration, sections, total_token_estimate }
        const iteration = (params.iteration as number) ?? 0;
        const sections = params.sections as ContextSnapshotMeta["sections"] | undefined;
        const total_token_estimate = (params.total_token_estimate as number) ?? 0;
        console.log("[debugStore] onContextBuilt: iteration=", iteration, "sections=", !!sections, "sectionsKeys=", sections ? Object.keys(sections) : null);
        if (sections) {
          // After a rewind, stale onContextBuilt events for iterations
          // beyond the rewind point may still arrive via WebSocket.
          // Discard any event whose iteration is more than one step
          // ahead of the last known snapshot — such events are from
          // before the rewind and must not be re-added.
          //
          // Only apply this guard when snapshots already exist.
          // When snapshots is empty (e.g., after frontend restart/reconnect
          // to an already-running debug session), accept events to bootstrap
          // the snapshot list.
          const currentSnapshots = get().snapshots;
          const maxExisting = currentSnapshots.length > 0
            ? Math.max(...currentSnapshots.map((sn) => sn.iteration))
            : 0;
          if (currentSnapshots.length > 0 && iteration > maxExisting + 1) {
            console.log("[debugStore] onContextBuilt: discarding stale event iteration=", iteration, "maxExisting=", maxExisting);
            break;
          }
          // Also skip duplicates (same iteration already in snapshots)
          if (currentSnapshots.some((sn) => sn.iteration === iteration)) {
            console.log("[debugStore] onContextBuilt: skipping duplicate iteration=", iteration);
            break;
          }
          const snapshot: ContextSnapshotMeta = {
            iteration,
            built_at: new Date().toISOString(),
            sections,
            total_token_estimate,
            phase: get().phase, // current phase from last state update
          };
          set((s) => ({
            snapshots: [...s.snapshots, snapshot],
          }));
          console.log("[debugStore] snapshot added, total snapshots:", get().snapshots.length);
        }
        break;
      }
      case "debugger.onBreakpoint": {
        set({ debugState: "Paused", paused: true });
        break;
      }
      case "debugger.onExecutionStateChange": {
        const newState = event.params.new_state as DebugState;
        if (newState) {
          set({
            debugState: newState,
            paused: newState === "Paused",
          });
        }
        break;
      }
    }
  },

  // ── Control commands ────────────────────────────────────────────────

  resume: async () => {
    await get().sendRequest("debugger.resume");
    set({ debugState: "Running", paused: false });
  },

  pause: async () => {
    await get().sendRequest("debugger.pause");
    set({ debugState: "Paused" });
    // Runtime sends onExecutionStateChange event later confirming the state
  },

  step: async (granularity = "iteration") => {
    await get().sendRequest("debugger.step", { granularity });
    set({ debugState: "Stepping" });
    // Runtime sends onExecutionStateChange(Paused) after auto-pause
  },

  stop: async () => {
    await get().sendRequest("debugger.stop");
    // Set both debugState and paused — the backend pushes an
    // onExecutionStateChange(Stopped) event which will confirm.
    set({ debugState: "Stopped", paused: true });
  },

  restart: async () => {
    await get().sendRequest("debugger.restart");
    set({
      iteration: 0,
      phase: "Idle" as Phase,
      debugState: "Stepping" as DebugState,
      paused: false,
      snapshots: [],
      sectionCache: new Map(),
    });
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
      set({
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
    const result = (await get().sendRequest("debugger.getContextSnapshot", {
      iteration,
    })) as ContextSnapshotMeta | undefined;
    if (result) {
      set((s) => {
        const existing = s.snapshots.findIndex((sn) => sn.iteration === iteration);
        if (existing >= 0) {
          const updated = [...s.snapshots];
          updated[existing] = result;
          return { snapshots: updated };
        }
        return { snapshots: [...s.snapshots, result] };
      });
    }
  },

  getSection: async (iteration: number, section: string): Promise<SectionContent | null> => {
    const cacheKey = `${iteration}:${section}`;
    const cached = get().sectionCache.get(cacheKey);
    if (cached) return cached;

    try {
      const result = (await get().sendRequest("debugger.getSection", {
        iteration,
        section,
      })) as SectionContent | undefined;
      if (result) {
        set((s) => {
          const updated = new Map(s.sectionCache);
          updated.set(cacheKey, result);
          return { sectionCache: updated };
        });
        return result;
      }
    } catch {
      // Section fetch failed — probably not built yet
    }
    return null;
  },

  // ── Context editing commands (S2.8) ────────────────────────────────

  hasPendingPatches: false,

  patchContext: async (patches: Record<string, unknown>) => {
    await get().sendRequest("debugger.patchContext", { patches });
    set({ hasPendingPatches: true });
  },

  rewind: async (toIteration: number) => {
    const result = (await get().sendRequest("debugger.rewind", {
      to_iteration: toIteration,
    })) as { rewound_to_iteration: number; messages_trimmed_to: number };
    // After rewind, clear cached sections for truncated snapshots
    set((s) => {
      const newCache = new Map(s.sectionCache);
      // Remove cached sections for snapshots beyond the rewind target
      const keysToDelete: string[] = [];
      newCache.forEach((_, key) => {
        const iter = parseInt(key.split(":")[0], 10);
        if (iter > toIteration) keysToDelete.push(key);
      });
      keysToDelete.forEach((k) => newCache.delete(k));
      return {
        sectionCache: newCache,
        snapshots: s.snapshots.filter((sn) => sn.iteration <= toIteration),
        hasPendingPatches: false, // rewind clears patches
        iteration: toIteration,
      };
    });
    // Trim chat messages to the rewind point so the chat UI reflects
    // the truncated conversation history.
    const agentId = get().debugAgentId;
    if (agentId && result.messages_trimmed_to > 0) {
      useChatStore.getState().trimMessagesTo(agentId, result.messages_trimmed_to);
    }
    return result;
  },

  reExecute: async () => {
    const result = (await get().sendRequest("debugger.reExecute", {})) as {
      has_patches: boolean;
    };
    // Re-execute consumed the flag — clear local tracking
    set({ hasPendingPatches: false, debugState: "Running", paused: false });
    return result;
  },
}));

// Augment the interface for the internal _handleEvent
declare module "zustand" {
  interface StoreMutators<S, A> {}
}

// Extend the store type
interface DebugStore {
  _handleEvent: (event: JsonRpcEvent) => void;
}
