import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useChatStore } from "./chatStore";
import { useAgentStore } from "./agentStore";
import { log } from "../lib/logger";

// ── Debug Protocol types ──────────────────────────────────────────────
//
// ADR-048 D6: transport switched from a direct WebSocket JSON-RPC
// connection (ws://127.0.0.1:19878) to the standard IPC pair:
//   - RPC  -> Tauri command `debug_rpc` (Gateway HTTP reverse proxy
//             `/api/agents/{agent_id}/debug/{path}` -> Runtime
//             `/api/debug/{path}`)
//   - push -> Tauri event `debug-event` (MQTT
//             `acowork/agents/{agent_id}/debug/events/{type}`, decoded
//             by the Rust MQTT client in `commands/chat_mqtt.rs`)

type Phase =
  | "BudgetCheck"
  | "BuildContext"
  | "LlmCall"
  | "ParseResponse"
  | "ToolExecution"
  | "AppendHistory"
  | "Idle";

/** Mirrors backend DebugState - the single source of truth for execution state. */
type DebugState = "Running" | "Paused" | "Stepping" | "Stopped";

const DEBUG_STATES: ReadonlySet<string> = new Set(["Running", "Paused", "Stepping", "Stopped"]);

interface SectionMeta {
  size_bytes: number;
  token_estimate: number;
  hash: string;
}

interface ContextSnapshotMeta {
  iteration: number;
  built_at: string;
  /**
   * Section metadata keyed by section name (system_prompt, ...).
   * Both sources use this shape: the `onContextBuilt` MQTT event
   * (proto map) and the `GET /api/debug/context/{iter}` RPC result
   * (ContextSections struct - same seven keys).
   */
  sections: Record<string, SectionMeta>;
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

// ── Debug event payloads (Rust MQTT client -> `debug-event`) ──────────
//
// Emitted by `commands/chat_mqtt.rs` after decoding the
// `DataEnvelope::Debug*Event` protobuf on
// `acowork/agents/{agent_id}/debug/events/{type}`. `type` mirrors the
// MQTT topic suffix.

interface DebugEventPayload {
  type: "onStep" | "onContextBuilt" | "onStateChange";
  agent_id: string;
  session_id: string;
  // onStep
  iteration?: number;
  phase?: string;
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  // onContextBuilt
  sections?: Record<string, SectionMeta>;
  total_token_estimate?: number;
  // onStateChange
  new_state?: string;
}

// ── Global `debug-event` listener ─────────────────────────────────────
//
// Registered lazily by `connect()` (DevMode only) and kept for the app
// lifetime. Events arriving before registration are dropped - same QoS 0
// semantics as the MQTT publisher (ADR-048 §2.4): after a (re)connect
// the panel re-syncs via `GET /api/debug/state`.

let _debugEventUnlisten: (() => void) | null = null;
let _debugEventInitPromise: Promise<void> | null = null;

async function ensureDebugEventListener(): Promise<void> {
  if (_debugEventUnlisten) return;
  if (_debugEventInitPromise) {
    await _debugEventInitPromise;
    return;
  }
  _debugEventInitPromise = (async () => {
    const unlisten = await listen<DebugEventPayload>("debug-event", (event) => {
      useDebugStore.getState()._handleDebugEvent(event.payload);
    });
    if (_debugEventUnlisten) {
      // A concurrent init won the race - drop this registration.
      unlisten();
      return;
    }
    _debugEventUnlisten = unlisten;
  })();
  await _debugEventInitPromise;
}

// ── Store interface ────────────────────────────────────────────────────

interface DebugStore {
  // Connection (shared - one debug session per agent)
  /**
   * Whether a debug session is attached (`debugAgentId` set). There is
   * no persistent connection any more: RPC is per-call HTTP through the
   * Gateway and events ride the shared MQTT subscription, so this flag
   * simply gates the DevMode UI.
   */
  connected: boolean;
  debugAgentId: string | null;

  /** Per-session debug state map - preserved across session switches. */
  sessionStates: Record<string, PerSessionDebugState>;

  // Actions
  connect: (agentId: string) => void;
  disconnect: () => void;
  rpc: (
    method: "GET" | "POST",
    path: string,
    opts?: { query?: Record<string, string>; body?: Record<string, unknown> },
  ) => Promise<unknown>;

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
  rewind: (sessionId: string | null, toIteration: number) => Promise<{ rewound_to_iteration: number; messages_trimmed_to: number } | undefined>;
  reExecute: (sessionId: string | null) => Promise<{ has_patches: boolean } | undefined>;
  patchContext: (sessionId: string | null, patches: Record<string, unknown>) => Promise<void>;

  // Internal
  _handleDebugEvent: (event: DebugEventPayload) => void;
}

export const useDebugStore = create<DebugStore>((set, get) => ({
  // Connection
  connected: false,
  debugAgentId: null,
  sessionStates: {},

  // ── Connection ─────────────────────────────────────────────────────

  connect: (agentId: string) => {
    const state = get();
    if (state.connected && state.debugAgentId === agentId) return;

    // Events arrive on the global `debug-event` Tauri channel (MQTT
    // subscription owned by the Rust client); make sure it is listened
    // on. RPC needs no handshake - it is per-call HTTP via the Gateway.
    void ensureDebugEventListener();

    set({ connected: true, debugAgentId: agentId });

    // Re-sync the active session, mirroring the legacy WS `onopen`
    // behaviour.
    setTimeout(() => {
      const sessionId = useChatStore.getState().getActiveSessionId(agentId);
      get().getState(sessionId).catch(() => { });
    }, 0);
  },

  disconnect: () => {
    // No socket to close: just detach from the agent. The
    // `debug-event` listener stays registered for the app lifetime -
    // events are filtered by `debugAgentId` in `_handleDebugEvent`.
    set({ connected: false, debugAgentId: null });
  },

  // ── RPC ────────────────────────────────────────────────────────────

  rpc: async (
    method: "GET" | "POST",
    path: string,
    opts?: { query?: Record<string, string>; body?: Record<string, unknown> },
  ): Promise<unknown> => {
    const agentId = get().debugAgentId;
    if (!agentId) {
      throw new Error("No debug session attached (agent not in DevMode?)");
    }
    return invoke<unknown>("debug_rpc", {
      agentId,
      method,
      path,
      query: opts?.query ?? null,
      body: opts?.body ?? null,
    });
  },

  // ── Event handler ──────────────────────────────────────────────────

  _handleDebugEvent: function (event: DebugEventPayload) {
    // Only track the agent currently attached via connect(). Debug
    // events flow for every dev-mode agent on the shared MQTT
    // subscription; sessions of other agents are irrelevant here.
    const store = get();
    if (!event.agent_id || event.agent_id !== store.debugAgentId) return;

    // Route events by session_id so background sessions' state is
    // updated correctly even when not currently displayed.
    const targetSid = event.session_id;
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

    switch (event.type) {
      case "onStep": {
        patchSession({
          iteration: event.iteration ?? 0,
          phase: (event.phase as Phase) ?? "Idle",
          promptTokens: event.prompt_tokens ?? 0,
          completionTokens: event.completion_tokens ?? 0,
        });
        break;
      }

      case "onContextBuilt": {
        const iteration = event.iteration ?? 0;
        const sections = event.sections;
        const total_token_estimate = event.total_token_estimate ?? 0;
        log.debug("[debugStore] onContextBuilt: sid=", targetSid, "iteration=", iteration, "sections=", !!sections);
        if (sections) {
          setSession((current) => {
            const currentSnapshots = current.snapshots;
            const maxExisting = currentSnapshots.length > 0
              ? Math.max(...currentSnapshots.map((sn) => sn.iteration))
              : 0;
            if (currentSnapshots.length > 0 && iteration > maxExisting + 1) {
              log.debug("[debugStore] onContextBuilt: discarding stale event sid=", targetSid, "iteration=", iteration);
              return current;
            }
            if (currentSnapshots.some((sn) => sn.iteration === iteration)) {
              log.debug("[debugStore] onContextBuilt: skipping duplicate sid=", targetSid, "iteration=", iteration);
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

      case "onStateChange": {
        const newState = event.new_state;
        if (!newState) break;
        if (DEBUG_STATES.has(newState)) {
          // DebugState transition (Running/Paused/Stepping/Stopped) -
          // covers the legacy onExecutionStateChange /
          // onPaused / onResumed notifications.
          patchSession({ debugState: newState as DebugState, paused: newState === "Paused" });
        } else {
          // The Runtime maps DebugPhase changes onto the same topic
          // (mqtt/debug_events.rs encode_event) - phase names arrive
          // here as `new_state`.
          patchSession({ phase: newState as Phase });
        }
        break;
      }
    }
  },

  // ── Control commands ────────────────────────────────────────────────

  resume: async (sessionId: string | null) => {
    if (!sessionId) return;
    await get().rpc("POST", "resume", { body: { session_id: sessionId } });
  },

  pause: async (sessionId: string | null) => {
    if (!sessionId) return;
    await get().rpc("POST", "pause", { body: { session_id: sessionId } });
  },

  step: async (sessionId: string | null, granularity = "iteration") => {
    if (!sessionId) return;
    await get().rpc("POST", "step", { body: { session_id: sessionId, granularity } });
  },

  stop: async (sessionId: string | null) => {
    if (!sessionId) return;
    await get().rpc("POST", "stop", { body: { session_id: sessionId } });
  },

  restart: async (sessionId: string | null) => {
    const agentId = get().debugAgentId;
    if (!agentId) {
      log.warn("[debugStore] restart: no debugAgentId, skipping");
      return;
    }
    // Route through Gateway HTTP restart-debug (the debugger.restart RPC
    // was removed when restart-to-debug was refactored to be processless).
    try {
      await useAgentStore.getState().restartAgentInDebug(agentId);
    } catch (e) {
      log.error("[debugStore] restart: restartAgentInDebug failed:", e);
      throw e;
    }
    await get().getState(sessionId).catch(() => { });
  },

  // ── State query ─────────────────────────────────────────────────────

  getState: async (sessionId: string | null) => {
    if (!sessionId) return;
    const result = (await get().rpc("GET", "state", { query: { session_id: sessionId } })) as {
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
    if (!sessionId) return;
    const result = (await get().rpc("GET", `context/${iteration}`, {
      query: { session_id: sessionId },
    })) as
      | (ContextSnapshotMeta & { sections: Record<string, SectionMeta> })
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
    if (!sessionId) return null;
    const cacheKey = `${iteration}:${section}`;
    const current = get().sessionStates[sessionId]?.sectionCache;
    const cached = current?.get(cacheKey);
    if (cached) return cached;
    try {
      const result = (await get().rpc("GET", `context/${iteration}/sections/${section}`, {
        query: { session_id: sessionId },
      })) as
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
    if (!sessionId) return;
    // Body shape matches Runtime `DebugRpcBody` (http/debug.rs):
    // `patches` is a `PatchContextParams { patches: PatchSet }`, i.e.
    // the patch set is nested one level under a `patches` key.
    await get().rpc("POST", "context/patch", {
      body: { session_id: sessionId, patches: { patches } },
    });
    patchSessionDebug(sessionId, { hasPendingPatches: true });
  },

  rewind: async (sessionId: string | null, toIteration: number) => {
    if (!sessionId) return undefined;
    const result = (await get().rpc("POST", "context/rewind", {
      body: { session_id: sessionId, to_iteration: toIteration },
    })) as {
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
    if (!sessionId) return undefined;
    const result = (await get().rpc("POST", "context/re-execute", {
      body: { session_id: sessionId },
    })) as { has_patches: boolean };
    patchSessionDebug(sessionId, { hasPendingPatches: false });
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
