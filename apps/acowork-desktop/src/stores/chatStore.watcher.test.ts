/**
 * Watchdog tick-path integration tests (M-2 of the unified-diagnostics
 * review).
 *
 * `chatStore.test.ts` pins the pure helpers; this file drives the REAL
 * `initMqttListener` → `startMqttPoll` → `setInterval` wiring with fake
 * timers and a mocked Tauri IPC surface:
 *
 *   - B-2: a Rust snapshot with `connecting: true` must NOT be
 *     downgraded to `disconnected` by the watchdog poll.
 *   - B-1: a stuck connecting episode escalates to `stale` at the 30s
 *     deadline and fires exactly one `force_reconnect_mqtt`, then
 *     retries on the next 30s window.
 *   - H-1 + F-3: a non-connected `mqtt-status` event after a connected
 *     episode re-arms the watchdog; a later `connected` snapshot
 *     self-heals and stops it.
 *   - H-2 (data side): a terminal `disconnected` snapshot leaves
 *     `staleSince` null (no bogus countdown) and never force-reconnects.
 *
 * These are the ONLY tests that exercise the setInterval path — the
 * exact place where the P0 regressions hid. If the watchdog wiring is
 * refactored, this file is the canary.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

/** Channel → handlers registry filled by the mocked `listen`. */
const { mockInvoke, mockListen, registeredListeners } = vi.hoisted(() => {
  type Handler = (event: { payload: unknown }) => void;
  const registeredListeners = new Map<string, Handler[]>();
  const mockInvoke = vi.fn(
    async (..._args: unknown[]): Promise<unknown> => undefined,
  );
  const mockListen = vi.fn(async (channel: string, handler: Handler) => {
    const arr = registeredListeners.get(channel) ?? [];
    arr.push(handler);
    registeredListeners.set(channel, arr);
    return () => {
      registeredListeners.set(
        channel,
        (registeredListeners.get(channel) ?? []).filter((h) => h !== handler),
      );
    };
  });
  return { mockInvoke, mockListen, registeredListeners };
});

vi.mock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mockListen }));

import {
  disposeMqttListener,
  initMqttListener,
  STUCK_FORCE_RECONNECT_MS,
  useChatStore,
} from "./chatStore";

/** Watchdog interval — module-private in `chatStore.ts`. */
const WATCHDOG_INTERVAL_MS = 5_000;

const T0 = new Date("2026-01-01T00:00:00Z");

/** Emit an `mqtt-status` event to every registered listener. */
function emitMqttStatus(payload: {
  connected: boolean;
  connecting?: boolean;
  reconnecting?: boolean;
  reason?: string;
}): void {
  for (const handler of registeredListeners.get("mqtt-status") ?? []) {
    handler({ payload });
  }
}

function resetConnectionState(): void {
  useChatStore.setState({
    mqttConnected: false,
    lastMqttError: null,
    effectiveConnection: "idle",
    staleSince: null,
    transitionLog: [],
  });
}

/** Rust snapshot for a client that is stuck in the initial connect. */
function rustConnectingSnapshot(): Record<string, unknown> {
  return { known: true, connected: false, connecting: true, reason: null };
}

beforeEach(() => {
  disposeMqttListener();
  vi.useFakeTimers();
  vi.setSystemTime(T0);
  registeredListeners.clear();
  mockInvoke.mockReset();
  mockListen.mockClear();
  resetConnectionState();
});

afterEach(() => {
  disposeMqttListener();
  vi.useRealTimers();
});

describe("watchdog tick path (B-2): Rust `connecting` survives the poll", () => {
  it("keeps `connecting` while Rust reports connecting:true — never downgrades to disconnected", async () => {
    let statusPolls = 0;
    mockInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === "get_mqtt_status") {
        statusPolls += 1;
        return rustConnectingSnapshot();
      }
      return undefined;
    });

    await initMqttListener();
    // Init snapshot is passed through verbatim → `connecting`, not
    // `disconnected` (the pre-fix collapse overwrote the flag).
    expect(useChatStore.getState().effectiveConnection).toBe("connecting");

    // Two watchdog ticks poll Rust — each must preserve the verdict.
    await vi.advanceTimersByTimeAsync(WATCHDOG_INTERVAL_MS * 2);
    expect(statusPolls).toBe(3); // init + tick@5s + tick@10s
    expect(useChatStore.getState().effectiveConnection).toBe("connecting");
    expect(useChatStore.getState().mqttConnected).toBe(false);
  });
});

describe("watchdog tick path (B-1): stuck episode force-reconnects", () => {
  it("escalates to `stale` and fires exactly one force_reconnect at the 30s deadline", async () => {
    const forceCalls: number[] = [];
    mockInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === "get_mqtt_status") return rustConnectingSnapshot();
      if (cmd === "force_reconnect_mqtt") {
        forceCalls.push(Date.now());
        return undefined;
      }
      return undefined;
    });

    await initMqttListener();
    expect(useChatStore.getState().effectiveConnection).toBe("connecting");

    // 29s: five ticks have polled, the deadline has not been crossed.
    await vi.advanceTimersByTimeAsync(STUCK_FORCE_RECONNECT_MS - 1_000);
    expect(forceCalls).toHaveLength(0);
    expect(useChatStore.getState().effectiveConnection).toBe("connecting");

    // Cross the 30s deadline: the same tick promotes to `stale` AND
    // fires the reconnect. (The pre-fix code could NEVER reach this
    // branch — the 10s stale-upgrade reset the clock that the 30s
    // force-check read.)
    await vi.advanceTimersByTimeAsync(2_000);
    expect(useChatStore.getState().effectiveConnection).toBe("stale");
    expect(forceCalls).toHaveLength(1);

    // The episode clock restarted after the force: the NEXT reconnect
    // happens exactly one window later (periodic retry, not single shot).
    await vi.advanceTimersByTimeAsync(STUCK_FORCE_RECONNECT_MS);
    expect(forceCalls).toHaveLength(2);
    expect(forceCalls[1]).toBe(T0.getTime() + 2 * STUCK_FORCE_RECONNECT_MS);
  });
});

describe("watchdog tick path (H-1 + F-3): re-arm on degradation, self-heal", () => {
  it("re-arms the watchdog on a non-connected event and self-heals a lost `connected`", async () => {
    let statusPolls = 0;
    mockInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === "get_mqtt_status") {
        statusPolls += 1;
        // Rust is healthy by the time the watchdog polls: the
        // `connected` event was "lost" — exactly the F-3 scenario.
        return { known: true, connected: true };
      }
      return undefined;
    });

    await initMqttListener();
    // Connected init snapshot → the watchdog is NOT armed.
    expect(useChatStore.getState().effectiveConnection).toBe("connected");
    expect(statusPolls).toBe(1);

    // Runtime degradation: the event arrives → watchdog re-arms (H-1).
    emitMqttStatus({ connected: false, reconnecting: true });
    expect(useChatStore.getState().effectiveConnection).toBe("reconnecting");

    // First re-armed tick observes the Rust truth → self-heal (F-3) + stop.
    await vi.advanceTimersByTimeAsync(WATCHDOG_INTERVAL_MS);
    expect(statusPolls).toBe(2);
    expect(useChatStore.getState().effectiveConnection).toBe("connected");
    expect(useChatStore.getState().mqttConnected).toBe(true);

    // Watchdog stopped after recovery: no further polls.
    mockInvoke.mockClear();
    await vi.advanceTimersByTimeAsync(WATCHDOG_INTERVAL_MS * 3);
    const pollsAfterHeal = mockInvoke.mock.calls.filter(
      ([cmd]) => cmd === "get_mqtt_status",
    ).length;
    expect(pollsAfterHeal).toBe(0);
  });
});

describe("watchdog tick path (H-2 data side): terminal `disconnected`", () => {
  it("leaves staleSince null (no countdown) and never force-reconnects", async () => {
    const forceCalls: number[] = [];
    mockInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === "get_mqtt_status") {
        return { known: true, connected: false, reason: "bootstrap failed" };
      }
      if (cmd === "force_reconnect_mqtt") {
        forceCalls.push(Date.now());
        return undefined;
      }
      return undefined;
    });

    await initMqttListener();
    expect(useChatStore.getState().effectiveConnection).toBe("disconnected");
    // The banner keys its countdown on `staleSince` — a terminal state
    // must leave it null (no "0s until auto-reconnect" lie).
    expect(useChatStore.getState().staleSince).toBeNull();

    await vi.advanceTimersByTimeAsync(STUCK_FORCE_RECONNECT_MS * 2);
    expect(forceCalls).toHaveLength(0);
    expect(useChatStore.getState().effectiveConnection).toBe("disconnected");
  });
});
