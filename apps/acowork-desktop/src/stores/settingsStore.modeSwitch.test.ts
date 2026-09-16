/**
 * Contract: switching gateway mode must NOT modify the Gateway URL.
 *
 * Single-topology rule (see
 * apps/acowork-desktop/src-tauri/src/commands/gateway.rs
 * `set_gateway_config`): both "local" and "remote" modes accept the
 * user-configured URL. The mode only decides whether Desktop spawns
 * a child Gateway when nothing answers at the URL. Auto-resetting the
 * URL on mode switch would break the legitimate "local mode + LAN
 * gateway URL" pattern (Desktop adopts a foreign Gateway on the LAN
 * via init_local_gateway's probe-then-spawn).
 *
 * The original incident was a user switching back to local while the
 * URL was still a stale LAN address (192.168.3.67). The fix in this
 * branch was to add a visual cue on the Settings page and a URL input
 * on the SplashScreen timeout view — NOT to silently reset the URL.
 *
 * These tests pin the contract so any future "let's just reset the URL
 * on mode change" refactor must explicitly revisit this design choice.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string) => mockInvoke(cmd),
}));

import { useSettingsStore } from "./settingsStore";

const REMOTE_URL = "http://192.168.3.67:19876";
const LOCAL_URL = "http://127.0.0.1:19876";

describe("settingsStore mode switch preserves URL (single-topology contract)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Always start the test from a known URL so setState can target
    // either direction of the switch without leaking localStorage
    // state from the host (the store reads localStorage on import).
    useSettingsStore.setState({
      gatewayMode: "local",
      gatewayUrl: LOCAL_URL,
    });
  });

  it("local → remote leaves the URL untouched", async () => {
    useSettingsStore.getState().setGatewayMode("remote");
    expect(useSettingsStore.getState().gatewayMode).toBe("remote");
    expect(useSettingsStore.getState().gatewayUrl).toBe(LOCAL_URL);
  });

  it("remote → local leaves the URL untouched (the original incident)", async () => {
    // Reproduce the original incident: user had set the URL to a remote
    // address, then switched mode back to local. The URL must remain the
    // remote one — single-topology — so the test pins this behavior.
    useSettingsStore.getState().setGatewayUrl(REMOTE_URL);
    expect(useSettingsStore.getState().gatewayUrl).toBe(REMOTE_URL);

    useSettingsStore.getState().setGatewayMode("local");

    expect(useSettingsStore.getState().gatewayMode).toBe("local");
    // The URL must NOT have been reset to the default — only the user
    // changes the URL via setGatewayUrl. This is the single-topology
    // contract.
    expect(useSettingsStore.getState().gatewayUrl).toBe(REMOTE_URL);
  });

  it("URL changes only via setGatewayUrl, never via setGatewayMode", async () => {
    // Bounce the mode a few times to make sure the URL is not touched
    // by mode transitions, only by explicit setGatewayUrl calls.
    useSettingsStore.getState().setGatewayUrl(REMOTE_URL);
    useSettingsStore.getState().setGatewayMode("remote");
    useSettingsStore.getState().setGatewayMode("local");
    useSettingsStore.getState().setGatewayMode("remote");
    expect(useSettingsStore.getState().gatewayUrl).toBe(REMOTE_URL);

    useSettingsStore.getState().setGatewayUrl(LOCAL_URL);
    expect(useSettingsStore.getState().gatewayUrl).toBe(LOCAL_URL);
    expect(useSettingsStore.getState().gatewayMode).toBe("remote");
  });
});
