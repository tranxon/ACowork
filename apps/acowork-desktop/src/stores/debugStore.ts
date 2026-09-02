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
  /** Section key ("system_prompt", "workspace_context", ...). */
  key: string;
  size_bytes: number;
  token_estimate: number;
  hash: string;
}

/** Mirrors backend `RequestParams` (ADR-054 step 2). */
export interface RequestParams {
  model: string;
  temperature?: number | null;
  max_tokens?: number | null;
  reasoning_effort?: string | null;
  thinking_mode?: string | null;
}

interface ContextSnapshotMeta {
  iteration: number;
  built_at: string;
  /**
   * Section metadata list, ordered by the backend's `build()` injection
   * order (ADR-054). Both sources converge to this shape:
   * - `GET /api/debug/context/{iter}` RPC result (ContextSections now a
   *   `Vec<SectionMeta>` — each element carries its `key`)
   * - `onContextBuilt` MQTT event (proto `map<string, SectionMeta>` is
   *   converted to the same list in `_handleDebugEvent`)
   */
  sections: SectionMeta[];
  total_token_estimate: number;
  phase: Phase;
  /** ADR-054 step 2: control params of the ChatRequest that built this snapshot. */
  request_params: RequestParams;
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
  /** ADR-054 step 2: request params of the current iteration (from getState). */
  requestParams: RequestParams | null;
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
    requestParams: null,
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
  // onContextBuilt — proto `map<string, SectionMeta>` (keys are the
  // section names; each value has no `key` field). Converted to
  // `SectionMeta[]` in `_handleDebugEvent` before storing.
  sections?: Record<string, Omit<SectionMeta, "key">>;
  total_token_estimate?: number;
  // ADR-054 step 2: control params of the ChatRequest that built this
  // snapshot (now carried on the MQTT event itself).
  request_params?: RequestParams | null;
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
  /**
   * Tear down DevMode for the currently-attached agent. Symmetric
   * counterpart to the "Enable Debug" button wired in
   * `ResultsPanel.tsx` (which calls the Tauri command directly +
   * then `connect()`s). Idempotent: if DevMode is already off this
   * is a no-op apart from a single `fetchAgents()` round-trip to
   * confirm the state.
   *
   * Order of operations mirrors the enable path inverted:
   *
   *   1. `invoke("disable_agent_debug", { agentId })` — hits
   *      Gateway's `/api/agents/{id}/debug/disable` proxy hook,
   *      which forwards to Runtime's `/api/debug/disable`. On a
   *      2xx the Gateway flips `running_agents[id].debug_state`
   *      to `Disabled`.
   *   2. `useAgentStore.getState().fetchAgents()` — refresh the
   *      agent list so `selectedAgent.debug_state === "disabled"`
   *      becomes visible to the UI. Without this, the next render
   *      would still see the stale `"enabled"` value and the Debug
   *      Panel would stay mounted.
   *   3. `get().disconnect()` — drop the local attach; the
   *      `debug-event` listener stays registered for the app
   *      lifetime but events for the just-detached agent are
   *      filtered by `debugAgentId === null`.
   *
   * Errors from step 1 are surfaced to the caller (the "Exit
   * Debug" button) so it can show a toast / keep the button
   * enabled. Steps 2-3 run only on success — a partial failure
   * (Runtime tore down but Gateway proxy flaked) leaves the local
   * store consistent with the Runtime's truth.
   */
  disableDebugMode: () => Promise<void>;
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
    log.debug("[debugStore] connect() called", {
      agentId,
      wasConnected: state.connected,
      wasDebugAgentId: state.debugAgentId,
    });
    if (state.connected && state.debugAgentId === agentId) {
      log.debug("[debugStore] connect() no-op: already attached to this agent");
      return;
    }

    // Events arrive on the global `debug-event` Tauri channel (MQTT
    // subscription owned by the Rust client); make sure it is listened
    // on. RPC needs no handshake - it is per-call HTTP via the Gateway.
    void ensureDebugEventListener();

    set({ connected: true, debugAgentId: agentId });
    log.debug("[debugStore] connect() set connected=true, debugAgentId=", agentId);

    // Re-sync the active session, mirroring the legacy WS `onopen`
    // behaviour.
    setTimeout(() => {
      const sessionId = useChatStore.getState().getActiveSessionId(agentId);
      log.debug("[debugStore] connect() deferred getState for session", sessionId);
      get().getState(sessionId).catch((e) => {
        log.warn("[debugStore] connect() deferred getState failed:", e);
      });
    }, 0);

    // ADR-054 step 2 fallback: re-sync existing context snapshots so the
    // request-params metadata bar and section list are complete even if
    // MQTT events were dropped before the listener attached (QoS 0
    // semantics, ADR-048 §2.4). Idempotent — getContextSnapshot replaces
    // the entry in place.
    setTimeout(async () => {
      const sessionId = useChatStore.getState().getActiveSessionId(agentId);
      if (!sessionId) return;
      const s = useDebugStore.getState().sessionStates[sessionId];
      for (const snap of s?.snapshots ?? []) {
        await get()
          .getContextSnapshot(sessionId, snap.iteration)
          .catch((e) => {
            log.warn("[debugStore] connect() snapshot re-sync failed:", e);
          });
      }
    }, 0);
  },

  disconnect: () => {
    // No socket to close: just detach from the agent. The
    // `debug-event` listener stays registered for the app lifetime -
    // events are filtered by `debugAgentId` in `_handleDebugEvent`.
    //
    // Also drop every per-session debug cache (snapshots, section
    // payloads, prompt tokens, etc.). Product semantics: leaving
    // DevMode erases the debug history; the next time the operator
    // enters DevMode the iteration list starts empty. Without this
    // clear the desktop's `sessionStates` would keep stale snapshot
    // metadata fed by MQTT `onContextBuilt` events from a previous
    // enable cycle - Runtime's `DebugController` is recreated empty
    // on every `enable_debug_mode` (session_manager.rs), so loading
    // any section via `getSection` would 404 and the panel would spin
    // on "Loading section..." forever. Clearing here keeps the local
    // store aligned with the Runtime's source of truth.
    set({
      connected: false,
      debugAgentId: null,
      sessionStates: {},
    });
  },

  // ── DevMode lifecycle ────────────────────────────────────────────────
  //
  // `connect()` is purely local: it registers a debug-event listener
  // and attaches to a running agent that is *already* in DevMode.
  // `disableDebugMode()` is the inverse across the full stack: it
  // asks the Runtime to tear down DevMode, refreshes the agent
  // list so the UI sees the new `debug_state = "disabled"`, and
  // detaches locally. Together with the existing
  // `enable_agent_debug` Tauri command invoked from
  // `ResultsPanel.tsx`, these two actions form the symmetric
  // enable/disable pair the "Exit Debug" button needs.

  disableDebugMode: async () => {
    const agentId = get().debugAgentId;
    if (!agentId) {
      log.warn(
        "[debugStore] disableDebugMode() no-op: no debug session attached",
      );
      return;
    }
    log.debug("[debugStore] disableDebugMode() calling Tauri command", {
      agentId,
    });
    try {
      // Step 1: ask the Runtime to tear DevMode down. The Tauri
      // command is `disable_agent_debug` (see
      // `apps/acowork-desktop/src-tauri/src/commands/debug.rs`),
      // which routes through the same Gateway wildcard proxy as the
      // generic `debug_rpc` and unwraps the Runtime's
      // `{ disabled, already_disabled }` envelope.
      await invoke<{ disabled: boolean; already_disabled: boolean }>(
        "disable_agent_debug",
        { agentId },
      );
    } catch (err) {
      // Surface the error to the caller; do NOT continue with the
      // local disconnect — that would leave the UI thinking DevMode
      // is off while the Runtime is still in DevMode. The caller
      // (the "Exit Debug" button) can retry.
      log.error(
        "[debugStore] disableDebugMode() Tauri command failed:",
        err,
      );
      throw err;
    }

    log.debug(
      "[debugStore] disableDebugMode() Tauri command ok, refreshing agents",
    );
    // Step 2: refresh the agent list so `selectedAgent.debug_state`
    // flips to `"disabled"` on the next render. Without this the
    // store still reports the stale `"enabled"` value and the
    // Debug Panel stays mounted until the next natural refresh
    // cycle.
    try {
      await useAgentStore.getState().fetchAgents();
    } catch (err) {
      // The Runtime side is the source of truth — the Gateway's
      // `proxy_debug_rpc` hook already flipped `debug_state` to
      // `Disabled` when step 1's 2xx response landed. A failed
      // `fetchAgents` only means the agentStore cache is stale, so
      // patch it locally: `ResultsPanel`'s auto-connect effect runs
      // off `debug_state === "enabled"` and would otherwise see the
      // stale value, re-attach the Debug Panel to an agent whose
      // DevMode is already gone, and leave the user with a panel
      // where every RPC 503s. The next periodic refresh overwrites
      // this local patch with the same value anyway. Log loudly so
      // the on-call engineer notices the transient network failure.
      log.warn(
        "[debugStore] disableDebugMode() fetchAgents failed — patching local debug_state=disabled:",
        err,
      );
      useAgentStore.getState().patchAgentMeta(agentId, {
        debug_state: "disabled",
      });
    }

    // Step 3: detach locally. Reuses `disconnect()` so the
    // semantics are identical to the natural "agent stopped or
    // debug_state flipped externally" path.
    get().disconnect();
    log.info(
      "[debugStore] disableDebugMode() complete: DevMode torn down, agent list refreshed, local session detached",
      { agentId },
    );
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
          // ADR-054: proto map -> list. Order is unspecified by the map;
          // the panel sorts by SECTION_ORDER at render time.
          const sectionsList: SectionMeta[] = Object.entries(sections).map(([key, meta]) => ({
            key,
            ...meta,
          }));
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
                {
                  iteration,
                  built_at: new Date().toISOString(),
                  sections: sectionsList,
                  total_token_estimate,
                  phase: current.phase,
                  // ADR-054 step 2: request params now ride on the MQTT
                  // event itself; the fallback covers older/edge payloads
                  // (a later getState / getContextSnapshot re-sync will
                  // overwrite the entry with the full values).
                  request_params: event.request_params ?? { model: "", temperature: null, max_tokens: null, reasoning_effort: null, thinking_mode: null },
                },
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
      request_params?: RequestParams | null;
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
        requestParams: result.request_params ?? null,
      });
    }
  },

  // ── Context commands ────────────────────────────────────────────────

  getContextSnapshot: async (sessionId: string | null, iteration: number) => {
    if (!sessionId) return;
    // Backend `GetContextSnapshotResult` now carries
    // `sections: { sections: SectionMeta[] }` (ADR-054).
    const result = (await get().rpc("GET", `context/${iteration}`, {
      query: { session_id: sessionId },
    })) as
      | (Omit<ContextSnapshotMeta, "sections"> & {
          sections: { sections: SectionMeta[] };
        })
      | undefined;
    if (result) {
      const normalized: ContextSnapshotMeta = {
        iteration: result.iteration,
        built_at: result.built_at,
        sections: result.sections.sections,
        total_token_estimate: result.total_token_estimate,
        phase: result.phase,
        request_params: result.request_params ?? { model: "", temperature: null, max_tokens: null, reasoning_effort: null, thinking_mode: null },
      };
      applySessionDebug(sessionId, (s) => {
        const idx = s.snapshots.findIndex((sn) => sn.iteration === iteration);
        if (idx >= 0) {
          const updated = [...s.snapshots];
          updated[idx] = normalized;
          return { ...s, snapshots: updated };
        }
        return { ...s, snapshots: [...s.snapshots, normalized] };
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
    // ADR-054: PatchSet is now `HashMap<String, PatchValue>` with a tagged
    // `{ type: "text" | "json", value }` payload per section. Normalize
    // plain JS values into the tagged wire form here so callers (the
    // panel's onSaveEdit) don't need to know each section's value kind.
    const normalized: Record<string, { type: "text" | "json"; value: unknown }> = {};
    for (const [key, value] of Object.entries(patches)) {
      normalized[key] =
        typeof value === "string"
          ? { type: "text", value }
          : { type: "json", value };
    }
    // Body shape matches Runtime `DebugRpcBody` (http/debug.rs):
    // `patches` is a `PatchContextParams { patches: PatchSet }`, i.e.
    // the patch set is nested one level under a `patches` key.
    await get().rpc("POST", "context/patch", {
      body: { session_id: sessionId, patches: { patches: normalized } },
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
