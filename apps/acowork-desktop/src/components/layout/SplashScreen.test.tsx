/**
 * Self-check for the remote-gateway timeout recovery (SplashScreen):
 *
 * When the Gateway is unreachable at startup, the timeout view must let
 * the user edit the remote Gateway URL and retry — the Settings page is
 * unreachable until a Gateway connects, so a stale address otherwise
 * bricks the app. Two properties matter:
 *
 *  1. Editing the address and clicking Retry persists the new URL via
 *     `setGatewayUrl` BEFORE the health probe, so the retry (and every
 *     later command) targets the new address.
 *  2. A failed retry returns to the editable timeout view instead of
 *     leaving the UI stuck on the "Retrying..." spinner forever.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, act } from "@testing-library/react";
import { SplashScreen } from "./SplashScreen";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";

vi.mock("@tauri-apps/api/core", () => ({
    invoke: vi.fn(async () => ({})),
}));

const OLD_URL = "http://192.168.1.10:19876";
const NEW_URL = "http://192.168.1.11:19876";
// Must match MAX_WAIT_MS in SplashScreen.tsx
const MAX_WAIT_MS = 30_000;

/** Flush chained microtasks (bootGateway awaits several resolved mocks). */
async function flushMicrotasks() {
    for (let i = 0; i < 10; i++) {
        await act(async () => {});
    }
}

describe("SplashScreen remote-gateway timeout recovery", () => {
    beforeEach(() => {
        vi.useFakeTimers();
        vi.stubGlobal("requestAnimationFrame", () => 0);
        vi.stubGlobal(
            "fetch",
            vi.fn(() => Promise.reject(new Error("unreachable"))),
        );
        useSettingsStore.setState({
            gatewayMode: "remote",
            gatewayUrl: OLD_URL,
        });
        useGatewayStore.setState({
            status: "disconnected",
            health: null,
            localState: "idle",
            localOwnership: "none",
        });
    });

    afterEach(() => {
        vi.useRealTimers();
        vi.unstubAllGlobals();
    });

    it("pre-fills the address, persists an edited address on retry, and returns to the timeout view when the retry fails", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();

        // Drive past the connection timeout.
        await act(async () => {
            vi.advanceTimersByTime(MAX_WAIT_MS + 1_000);
        });
        await flushMicrotasks();

        // Timeout view shows the address input pre-filled with the old URL.
        expect(screen.getByText("Retry Connection")).toBeTruthy();
        const input = screen.getByDisplayValue(OLD_URL) as HTMLInputElement;

        // User edits the address and retries.
        fireEvent.change(input, { target: { value: NEW_URL } });
        fireEvent.click(screen.getByText("Retry Connection"));
        await flushMicrotasks();

        // New URL was persisted to the settings store before the probe…
        expect(useSettingsStore.getState().gatewayUrl).toBe(NEW_URL);

        // …and since the gateway is still unreachable, the editable
        // timeout view is shown again (not an endless spinner).
        expect(screen.getByText("Retry Connection")).toBeTruthy();
        expect((screen.getByDisplayValue(NEW_URL) as HTMLInputElement).value).toBe(NEW_URL);
    });
});
