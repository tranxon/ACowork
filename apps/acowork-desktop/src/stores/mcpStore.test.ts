//! Unit tests for the per-server reconcile-pending spinner state on
//! `useMcpStore`. The flag is set by `toggleServer`, cleared by
//! `clearServerLoading` (idempotent), and rolled back by a failed PUT
//! so a stuck spinner cannot survive a network error.

import { describe, it, expect, beforeEach, vi } from "vitest";
import { useMcpStore } from "./mcpStore";

const AGENT = "com.test.Agent";

describe("mcpStore perServerLoading", () => {
  beforeEach(() => {
    // Reset the slice under test; other fields keep their defaults.
    useMcpStore.setState({
      activeServers: {},
      activationLoading: {},
      perServerLoading: {},
      error: null,
    } as never);
    vi.restoreAllMocks();
  });

  it("toggleServer marks the targeted server as reconcile-pending", async () => {
    // Stub the PUT so it resolves 200 without touching the network.
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ active_servers: ["pm"] }),
        }),
      ),
    );

    await useMcpStore.getState().toggleServer(AGENT, "pm");

    expect(
      useMcpStore.getState().perServerLoading[AGENT]?.["pm"],
    ).toBe(true);
    // Active list reflects the optimistic write.
    expect(useMcpStore.getState().activeServers[AGENT]).toEqual(["pm"]);
  });

  it("clearServerLoading drops the flag and is idempotent", () => {
    useMcpStore.setState((s) => ({
      perServerLoading: {
        ...s.perServerLoading,
        [AGENT]: { pm: true },
      },
    }));

    useMcpStore.getState().clearServerLoading(AGENT, "pm");
    expect(
      useMcpStore.getState().perServerLoading[AGENT]?.["pm"],
    ).toBe(false);

    // Second call is a no-op — does not produce a fresh state object.
    const before = useMcpStore.getState();
    useMcpStore.getState().clearServerLoading(AGENT, "pm");
    const after = useMcpStore.getState();
    expect(after).toBe(before);
  });

  it("clearServerLoading on a non-pending server is a no-op", () => {
    useMcpStore.setState((s) => ({
      perServerLoading: {
        ...s.perServerLoading,
        [AGENT]: { other: true },
      },
    }));
    const before = useMcpStore.getState();

    useMcpStore.getState().clearServerLoading(AGENT, "pm");

    expect(useMcpStore.getState()).toBe(before);
  });

  it("PUT failure rolls back both activeServers and perServerLoading", async () => {
    useMcpStore.setState((s) => ({
      activeServers: { ...s.activeServers, [AGENT]: ["seed"] },
    }));

    // 500 → server-side rejection.
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve({
          ok: false,
          status: 500,
          json: () => Promise.resolve({ error: "boom" }),
        }),
      ),
    );

    await useMcpStore.getState().toggleServer(AGENT, "pm");

    // Active list rolled back to the pre-toggle snapshot.
    expect(useMcpStore.getState().activeServers[AGENT]).toEqual(["seed"]);
    // And the spinner flag for this toggle was cleared so the UI does
    // not look stuck on a request that never even reached the reconcile
    // phase.
    expect(
      useMcpStore.getState().perServerLoading[AGENT]?.["pm"],
    ).toBe(false);
    expect(useMcpStore.getState().error).toBe("boom");
  });
});
