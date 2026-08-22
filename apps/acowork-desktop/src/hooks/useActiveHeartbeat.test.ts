/**
 * useActiveHeartbeat test — verifies the frontend contract for the
 * presence pulse that backs the Runtime's idle-watcher heartbeat.
 *
 * The hook is intentionally minimal:
 *   - When agentId is non-null, publish an `active_heartbeat` MQTT
 *     control command immediately on mount and every
 *     ACTIVE_HEARTBEAT_INTERVAL_MS thereafter.
 *   - When agentId changes, the previous interval is torn down and a
 *     fresh one is started for the new ID (with an immediate pulse).
 *   - When agentId is null, no calls are made and no timer leaks.
 *
 * Tests use vitest fake timers to advance time deterministically
 * without waiting real wall-clock seconds, and jest-mock-style spies
 * on `invoke` (the Tauri command bridge).
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";

// Mock the Tauri invoke bridge BEFORE importing the hook so the hook
// captures the spy at module-load time.
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

// Mock the agent store with a controllable selector so
// useActiveHeartbeatForSelection can be driven from tests.
const selectedAgentIdRef: { current: string | null } = { current: null };
vi.mock("../stores/agentStore", () => ({
  useAgentStore: (selector: (s: { selectedAgentId: string | null }) => unknown) =>
    selector({ selectedAgentId: selectedAgentIdRef.current }),
}));

import {
  useActiveHeartbeat,
  useActiveHeartbeatForSelection,
  ACTIVE_HEARTBEAT_INTERVAL_MS,
} from "./useActiveHeartbeat";

// ── Setup ──────────────────────────────────────────────────────────────

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  selectedAgentIdRef.current = null;
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// ── Helpers ────────────────────────────────────────────────────────────

function getHeartbeatCalls(): Array<{ agentId: string; command: string; payloadJson: unknown }> {
  // Walk the raw mock.calls directly so we never lose positional
  // alignment between the filtered command name and its argument
  // object (an earlier version of this helper re-indexed after a
  // .filter and produced silently wrong tuples).
  return invokeMock.mock.calls
    .filter((c) => c[0] === "mqtt_publish_control")
    .map((c) => c[1] as { agentId: string; command: string; payloadJson: unknown });
}

// ── Tests ──────────────────────────────────────────────────────────────

describe("useActiveHeartbeat", () => {
  it("publishes one heartbeat immediately on mount with non-null agentId", () => {
    renderHook(() => useActiveHeartbeat("com.acowork.test"));

    // One immediate pulse; the interval has not fired yet.
    expect(getHeartbeatCalls()).toEqual([
      { agentId: "com.acowork.test", command: "active_heartbeat", payloadJson: {} },
    ]);
  });

  it("publishes additional heartbeats at ACTIVE_HEARTBEAT_INTERVAL_MS cadence", () => {
    renderHook(() => useActiveHeartbeat("com.acowork.test"));

    // First pulse is synchronous on mount.
    expect(getHeartbeatCalls().length).toBe(1);

    // Advance to just before the next interval tick — still one pulse.
    act(() => {
      vi.advanceTimersByTime(ACTIVE_HEARTBEAT_INTERVAL_MS - 1);
    });
    expect(getHeartbeatCalls().length).toBe(1);

    // Cross the tick boundary — second pulse fires.
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(getHeartbeatCalls().length).toBe(2);

    // Two more intervals → four total.
    act(() => {
      vi.advanceTimersByTime(2 * ACTIVE_HEARTBEAT_INTERVAL_MS);
    });
    expect(getHeartbeatCalls().length).toBe(4);
  });

  it("is a no-op when agentId is null (no invoke, no timer leak)", () => {
    renderHook(() => useActiveHeartbeat(null));

    expect(invokeMock).not.toHaveBeenCalled();

    // Advance time well past the cadence — still nothing fires.
    act(() => {
      vi.advanceTimersByTime(10 * ACTIVE_HEARTBEAT_INTERVAL_MS);
    });

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("stops publishing when unmounted (no leaked interval)", () => {
    const { unmount } = renderHook(() => useActiveHeartbeat("com.acowork.test"));
    expect(getHeartbeatCalls().length).toBe(1);

    unmount();

    // After unmount, advancing the fake clock must NOT produce new
    // invokes — this is the crash-safety property the Runtime relies
    // on (frontend gone → heartbeats stop).
    act(() => {
      vi.advanceTimersByTime(10 * ACTIVE_HEARTBEAT_INTERVAL_MS);
    });
    expect(getHeartbeatCalls().length).toBe(1);
  });

  it("restarts the interval cleanly when agentId changes", () => {
    let currentId: string | null = "agent-a";
    const { rerender } = renderHook(({ id }: { id: string | null }) => useActiveHeartbeat(id), {
      initialProps: { id: currentId },
    });

    // Initial pulse for agent-a.
    expect(getHeartbeatCalls()).toEqual([
      { agentId: "agent-a", command: "active_heartbeat", payloadJson: {} },
    ]);

    // Switch to agent-b — React re-runs the effect; old interval
    // cleared, new immediate pulse for agent-b.
    currentId = "agent-b";
    rerender({ id: currentId });

    const calls = getHeartbeatCalls();
    expect(calls.length).toBe(2);
    expect(calls[1]).toEqual({
      agentId: "agent-b",
      command: "active_heartbeat",
      payloadJson: {},
    });

    // After switching to agent-b, the agent-a interval MUST be torn
    // down (cleanup runs because the effect's `agentId` dep changed).
    // Advance past a full interval — only agent-b's pulse fires.
    act(() => {
      vi.advanceTimersByTime(ACTIVE_HEARTBEAT_INTERVAL_MS);
    });

    const finalCalls = getHeartbeatCalls();
    // Total: agent-a mount (1) + agent-b rerender immediate (1) +
    //        agent-b interval tick (1) = 3.
    expect(finalCalls.length).toBe(3);
    // Crash-safety assertion: every heartbeat AFTER the agent-a
    // mount must be for agent-b. The agent-a interval must not
    // still be running.
    const callsAfterSwitch = finalCalls.slice(1);
    expect(callsAfterSwitch.every((c) => c.agentId === "agent-b")).toBe(true);
  });

  it("transitions to no-op when agentId changes to null", () => {
    let currentId: string | null = "agent-a";
    const { rerender } = renderHook(({ id }: { id: string | null }) => useActiveHeartbeat(id), {
      initialProps: { id: currentId },
    });
    expect(getHeartbeatCalls().length).toBe(1);

    currentId = null;
    rerender({ id: currentId });

    // No new invoke on transition to null.
    expect(getHeartbeatCalls().length).toBe(1);

    // No leaked timer — advancing time produces no further heartbeats.
    act(() => {
      vi.advanceTimersByTime(5 * ACTIVE_HEARTBEAT_INTERVAL_MS);
    });
    expect(getHeartbeatCalls().length).toBe(1);
  });

  it("does not crash when invoke rejects (transient publish failure)", async () => {
    invokeMock.mockRejectedValueOnce(new Error("broker disconnected"));

    renderHook(() => useActiveHeartbeat("agent-x"));

    // The immediate pulse rejects, but the hook swallows it (logged
    // at debug, not rethrown). Interval must still be armed.
    expect(getHeartbeatCalls().length).toBe(1);

    // The next interval tick must still fire.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(ACTIVE_HEARTBEAT_INTERVAL_MS);
    });
    expect(getHeartbeatCalls().length).toBe(2);
  });
});

describe("useActiveHeartbeatForSelection", () => {
  it("drives useActiveHeartbeat from selectedAgentId", () => {
    selectedAgentIdRef.current = "com.acowork.system";
    renderHook(() => useActiveHeartbeatForSelection());

    expect(getHeartbeatCalls()).toEqual([
      { agentId: "com.acowork.system", command: "active_heartbeat", payloadJson: {} },
    ]);
  });

  it("publishes nothing while selectedAgentId is null", () => {
    selectedAgentIdRef.current = null;
    renderHook(() => useActiveHeartbeatForSelection());

    expect(invokeMock).not.toHaveBeenCalled();
  });
});
