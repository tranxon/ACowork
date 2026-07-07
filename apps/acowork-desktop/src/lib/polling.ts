//! PollingManager — ADR-021 Phase 3 / ADR-025 refactor
//!
//! Manages HTTP polling for session message data. Replaces the old WebSocket
//! streaming data channel (Delta/ReasoningDelta/ToolCall/ToolResult) with
//! incremental HTTP pulls triggered by `new_data_available` notifications
//! and a fallback interval with exponential backoff.
//!
//! ## Architecture (ADR-025)
//!
//! ```
//! WebSocket "new_data_available" ──→ notify() ──→ immediate poll
//! Fallback timer (500ms→…→5s)     ──→ scheduled poll
//! has_more=true (batch catch-up)  ──→ immediate re-poll
//!                                                  │
//!                                                  ▼
//!                              chatStore.loadSessionMessages()
//!                              ?incremental=true
//!                              (no coordinates — backend tracks cursor)
//! ```
//!
//! ## Backoff strategy (ADR-021 §难点 2)
//!
//! - Normal: 500ms interval, reset on new data
//! - Empty response: double interval (max 5s)
//! - Auto-stop: NEVER — the poller is stopped only by explicit stop()
//!   calls (done/error/stopped/session switch). LLM thinking phases can
//!   last 10-30 seconds with no data; auto-stopping on empty polls would
//!   kill the poller before the first token arrives.
//!
//! ## Batch catch-up (ADR-025)
//!
//! When a session was in the background for a long time, the backend's
//! delivery cursor may lag far behind the actual data. The backend returns
//! a batch of messages with `has_more=true`. The PollingManager immediately
//! re-polls (no delay) until `has_more=false`, then resumes normal polling.
//!
//! ## Lifecycle
//!
//! - `start()` — begin polling (called when session becomes active/streaming)
//! - `stop()`  — stop polling (called on done/error/stopped or session switch)
//! - `notify(intervalMs)` — trigger immediate poll (pure signal, no coordinates)

import { useChatStore } from "../stores/chatStore";

/** Polling interval when no `interval_ms` is provided by backend (fallback). */
const POLL_FALLBACK_MS = 500;
/** Maximum backoff interval in milliseconds */
const POLL_MAX_MS = 5000;
/** Backoff multiplier per empty response */
const POLL_BACKOFF_MULTIPLIER = 2.0;

/**
 * Per-session polling manager.
 *
 * Each active streaming session gets its own PollingManager instance.
 * The manager is stored in a module-level Map keyed by `agentId:sessionId`.
 *
 * ADR-025: The manager no longer maintains any delivery coordinates.
 * The backend tracks the delivery cursor per-session. The frontend simply
 * polls with `incremental=true` and appends whatever the backend returns.
 */
export class PollingManager {
  private agentId: string;
  private sessionId: string;
  private baseIntervalMs: number;
  private currentIntervalMs: number;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private running: boolean = false;
  /** Prevents overlapping polls (AbortController in store also guards, but
   *  this flag avoids unnecessary fetch attempts). */
  private polling: boolean = false;

  constructor(agentId: string, sessionId: string) {
    this.agentId = agentId;
    this.sessionId = sessionId;
    this.baseIntervalMs = POLL_FALLBACK_MS;
    this.currentIntervalMs = POLL_FALLBACK_MS;
  }

  /** Start the polling loop. Idempotent — safe to call multiple times. */
  start(): void {
    if (this.running) return;
    this.running = true;
    this.currentIntervalMs = this.baseIntervalMs;
    console.log(
      `[PollingManager] Starting poll for ${this.agentId}/${this.sessionId}`,
    );
    this.scheduleNext();
  }

  /** Stop the polling loop. Idempotent. */
  stop(): void {
    if (!this.running) return;
    this.running = false;
    this.clearTimer();
    console.log(
      `[PollingManager] Stopped poll for ${this.agentId}/${this.sessionId}`,
    );
  }

  /**
   * Called when a `new_data_available` WebSocket event arrives.
   * Triggers an immediate poll — no coordinates, pure signal.
   *
   * The Runtime already throttles these notifications to the configured
   * `interval_ms` (from DataFlowConfig), so this method always fires an
   * immediate fetch without additional frontend-side rate limiting.
   *
   * If the poller was stopped (e.g., by a previous done/error event but
   * the session was re-activated), it is restarted automatically.
   *
   * @param intervalMs - Notify throttle interval from backend (DataFlowConfig).
   *                     Used as the base polling interval.  When omitted,
   *                     POLL_FALLBACK_MS is used.
   */
  notify(intervalMs?: number): void {
    if (!this.running) {
      this.start();
    }

    // Update base interval from backend notification if provided.
    // This ensures the polling rate matches the Runtime's throttle rate.
    if (intervalMs != null && intervalMs > 0) {
      this.baseIntervalMs = intervalMs;
      this.currentIntervalMs = intervalMs;
    }

    console.log(
      `[PollingManager] notify: intervalMs=${intervalMs ?? "not set"}`,
    );

    // Trigger immediate poll (cancel any pending timer first)
    this.clearTimer();
    this.doPoll();
  }

  /** Return the current base polling interval in ms. */
  getIntervalMs(): number {
    return this.baseIntervalMs;
  }

  private scheduleNext(): void {
    if (!this.running) return;
    this.timer = setTimeout(() => {
      this.doPoll();
    }, this.currentIntervalMs);
  }

  /** Clear the fallback timer */
  private clearTimer(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  /**
   * Execute a single poll cycle.
   *
   * Delegates the actual HTTP fetch to `chatStore.loadSessionMessages()`
   * with `incremental=true`. The backend uses its own delivery cursor to
   * determine what to return — no coordinates are sent from the frontend.
   *
   * If the response has `has_more=true` (batch catch-up), immediately
   * re-polls without waiting for the timer.
   */
  private async doPoll(): Promise<void> {
    if (!this.running || this.polling) return;
    this.polling = true;

    try {
      const store = useChatStore.getState();

      const agent = store.agentStates[this.agentId];
      if (!agent) { this.stop(); return; }

      const session = agent.sessionStates[this.sessionId];
      if (!session) { this.stop(); return; }

      // Don't poll if session is not in an active state
      const status = session.sessionStatus?.status;
      if (
        status !== "streaming" &&
        status !== "waiting_approval" &&
        status !== "paused"
      ) {
        // DEBUG: log why poll was skipped
        console.log(
          `[PollingManager:DEBUG] doPoll SKIPPED for ${this.agentId}/${this.sessionId}: ` +
          `status=${status}, messageCount=${session.messages.length}`,
        );
        this.stop();
        return;
      }

      // Delegate fetch to loadSessionMessages — single source of truth
      // for HTTP request + store update. ADR-025: no coordinates.
      const result = await store.loadSessionMessages(
        this.agentId,
        this.sessionId,
        undefined, // cursor — not used with incremental
        50,
        "backward",
        true,      // incremental = true (ADR-025)
      );

      // Batch catch-up: has_more=true → immediately re-poll
      if (result?.hasMore) {
        this.clearTimer();
        this.doPoll();
        return;
      }

      // Backoff: if no new data this cycle, double interval (max 5s).
      // Do NOT auto-stop — LLM thinking phases can last 10-30 seconds
      // with no data. The poller is stopped only by explicit stop()
      // calls (done/error/stopped/session_state_changed to idle).
      if (result?.noNewData) {
        this.currentIntervalMs = Math.min(
          this.currentIntervalMs * POLL_BACKOFF_MULTIPLIER,
          POLL_MAX_MS,
        );
      } else {
        // New data — reset backoff
        this.currentIntervalMs = this.baseIntervalMs;
      }

      this.scheduleNext();
    } catch (e) {
      console.warn(
        `[PollingManager] Poll error for ${this.agentId}/${this.sessionId}:`,
        e,
      );
      this.scheduleNext();
    } finally {
      this.polling = false;
    }
  }
}

// ── Module-level registry ────────────────────────────────────────────────

/** Active polling managers keyed by "agentId:sessionId" */
const managers = new Map<string, PollingManager>();

function managerKey(agentId: string, sessionId: string): string {
  return `${agentId}:${sessionId}`;
}

/**
 * Start polling for a session. If a manager already exists for this session,
 * it is restarted. Safe to call multiple times.
 */
export function startPolling(
  agentId: string,
  sessionId: string,
): PollingManager {
  const key = managerKey(agentId, sessionId);
  let mgr = managers.get(key);
  if (!mgr) {
    // ADR-025: No initial coordinates — backend manages the cursor.
    mgr = new PollingManager(agentId, sessionId);
    managers.set(key, mgr);
  }
  mgr.start();
  return mgr;
}

/**
 * Stop polling for a session. Safe to call even if no manager exists.
 */
export function stopPolling(agentId: string, sessionId: string): void {
  const key = managerKey(agentId, sessionId);
  const mgr = managers.get(key);
  if (mgr) {
    mgr.stop();
    managers.delete(key);
  }
}

/**
 * Notify a session's PollingManager of new data available.
 * Creates a manager if one doesn't exist yet.
 *
 * ADR-025: This is a pure signal — no `totalLines` parameter.
 * The backend maintains all delivery state.
 *
 * @param intervalMs - Notify throttle interval from backend (DataFlowConfig).
 *                     Used as the polling interval base.
 */
export function notifyNewData(
  agentId: string,
  sessionId: string,
  intervalMs?: number,
): void {
  const key = managerKey(agentId, sessionId);
  let mgr = managers.get(key);
  if (!mgr) {
    // ADR-025: No initial coordinates — backend manages the cursor.
    mgr = new PollingManager(agentId, sessionId);
    managers.set(key, mgr);
  }
  mgr.notify(intervalMs);
}

/**
 * Get the current polling interval for a session.
 * Returns undefined if no manager exists (no streaming in progress).
 */
export function getPollingIntervalMs(
  agentId: string,
  sessionId: string,
): number | undefined {
  const key = managerKey(agentId, sessionId);
  return managers.get(key)?.getIntervalMs();
}

/**
 * Stop all polling managers. Used during global cleanup.
 */
export function stopAllPolling(): void {
  for (const mgr of managers.values()) {
    mgr.stop();
  }
  managers.clear();
}
