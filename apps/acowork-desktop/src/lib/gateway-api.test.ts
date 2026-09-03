/**
 * User profile API client tests.
 *
 * Regression coverage for the backend response contract:
 * - `POST /api/users` answers with an `OperationAck` (ADR-059 §7.3)
 * - `PUT  /api/users/{user_id}` answers with `UserResponse { user, version }`
 * - `POST /api/users/{user_id}/activate` answers with `ActivateResponse`
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
    createUser,
    updateUser,
    activateUser,
    fetchActiveUser,
    verifyAgentHealth,
} from "./gateway-api";
import type { BackendUserProfile } from "./types";

// ── Fixtures ─────────────────────────────────────────────────────────────

const PROFILE: BackendUserProfile = {
    user_id: "e8397df5-0000-4000-8000-000000000001",
    display_name: "大鱼",
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    city: "上海",
    created_at: "2026-08-01T00:00:00Z",
    updated_at: "2026-08-27T00:00:00Z",
    is_active: true,
};

const ENVELOPE = { user: PROFILE, version: 7 };

/** URLs + methods captured from the mocked global fetch. */
const calls: Array<{ url: string; init?: RequestInit }> = [];

function mockFetchOnce(body: unknown, status = 200) {
    vi.stubGlobal(
        "fetch",
        vi.fn((url: string | URL, init?: RequestInit) => {
            calls.push({ url: String(url), init });
            return Promise.resolve({
                ok: status >= 200 && status < 300,
                status,
                json: () => Promise.resolve(body),
            } as Response);
        }),
    );
}

// ── Tests ────────────────────────────────────────────────────────────────

beforeEach(() => {
    calls.length = 0;
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("updateUser response unwrapping", () => {
    it("unwraps { user, version } so the profile is returned directly", async () => {
        mockFetchOnce(ENVELOPE);

        const result = await updateUser(PROFILE.user_id, { city: "上海" }, "http://gw");

        expect(result).toEqual(PROFILE);
        expect(result.user_id).toBe(PROFILE.user_id);
        // The envelope's version must NOT leak into the profile.
        expect((result as unknown as { version?: unknown }).version).toBeUndefined();
        expect(calls[0].url).toBe(`http://gw/api/users/${PROFILE.user_id}`);
    });

    it("throws on non-OK responses", async () => {
        mockFetchOnce({ error: "User not found" }, 404);

        await expect(updateUser("missing", { city: "x" }, "http://gw")).rejects.toThrow(
            /User not found/,
        );
    });
});

describe("chained update after a previous save (ProfileTab saveField pattern)", () => {
    it("uses the returned profile's user_id for the next PUT — never /users/undefined", async () => {
        // First save returns the envelope; the client must unwrap it so the
        // caller's cached profile keeps a valid user_id.
        mockFetchOnce(ENVELOPE);

        const saved = await updateUser(PROFILE.user_id, { city: "上海" }, "http://gw");

        // Second save: same flow as ProfileTab.saveField(user_id, ...).
        mockFetchOnce(ENVELOPE);
        await updateUser(saved.user_id, { language: "zh-CN" }, "http://gw");

        expect(calls).toHaveLength(2);
        // Regression: the old code stored the envelope, user_id became
        // undefined, and this PUT went to /api/users/undefined → 404.
        expect(calls[1].url).not.toContain("undefined");
        expect(calls[1].url).toBe(`http://gw/api/users/${PROFILE.user_id}`);
    });
});

describe("createUser response shape (ADR-059 §7.3)", () => {
    it("returns OperationAck as-is — not the unwrapped profile", async () => {
        const ack = {
            operation_id: "op-1234",
            state: "committed",
            resource_version: 42,
        };
        mockFetchOnce(ack);

        const result = await createUser(
            { display_name: "大鱼", language: "zh-CN", timezone: "Asia/Shanghai" },
            "http://gw",
        );

        // OperationAck must be returned verbatim, with no unwrapping.
        expect(result).toEqual(ack);
        expect(result.operation_id).toBe("op-1234");
        expect(result.state).toBe("committed");
        expect((result as unknown as { user?: unknown }).user).toBeUndefined();
        expect(calls[0].url).toBe("http://gw/api/users");
        expect(calls[0].init?.method).toBe("POST");
    });
});

describe("activateUser response shape", () => {
    it("returns ActivateResponse { active_user_id, version } as-is", async () => {
        mockFetchOnce({ active_user_id: PROFILE.user_id, version: 9 });

        const result = await activateUser(PROFILE.user_id, "http://gw");

        expect(result).toEqual({ active_user_id: PROFILE.user_id, version: 9 });
        expect(calls[0].url).toBe(`http://gw/api/users/${PROFILE.user_id}/activate`);
        expect(calls[0].init?.method).toBe("POST");
    });
});

describe("fetchActiveUser", () => {
    it("picks the active user from the list response", async () => {
        mockFetchOnce({
            version: 3,
            users: [
                { ...PROFILE, user_id: "other", is_active: false },
                PROFILE,
            ],
        });

        const active = await fetchActiveUser("http://gw");

        expect(active?.user_id).toBe(PROFILE.user_id);
    });
});

// ── verifyAgentHealth (distributed liveness double-check) ────────────────
//
// Regression: 2026-09-02 09:12 incident. MQTT disconnection during
// system sleep marked agents as offline and the desktop rendered them
// as sleeping (zzz). The fix probes the Runtime's `/health` endpoint
// through the Gateway reverse-proxy on every MQTT `online=false` event;
// a 2xx answer overrides back to online.

describe("verifyAgentHealth (HTTP double-check on MQTT disconnect)", () => {
    it("returns true on a 2xx response and targets the Gateway health proxy", async () => {
        mockFetchOnce({ status: "ok" }, 200);
        const alive = await verifyAgentHealth("com.acowork.architect", 3000, "http://gw");
        expect(alive).toBe(true);
        expect(calls).toHaveLength(1);
        expect(calls[0].url).toBe("http://gw/api/agents/com.acowork.architect/health");
        expect(calls[0].init?.method).toBeUndefined(); // GET is the fetch default
    });

    it("returns false on a 503 response (Runtime not registered)", async () => {
        mockFetchOnce({ error: "Runtime HTTP endpoint not registered" }, 503);
        const alive = await verifyAgentHealth("com.acowork.never-registered", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false on a 502 BAD_GATEWAY (Runtime died after registry populated)", async () => {
        mockFetchOnce({ error: "upstream unreachable" }, 502);
        const alive = await verifyAgentHealth("com.acowork.dead", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false on a 404 (route not configured — would be a Gateway bug)", async () => {
        mockFetchOnce({ error: "not found" }, 404);
        const alive = await verifyAgentHealth("com.acowork.architect", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false when fetch itself throws (network unreachable, DNS, etc.)", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() => Promise.reject(new Error("ECONNREFUSED"))),
        );
        const alive = await verifyAgentHealth("com.acowork.architect", 3000, "http://gw");
        expect(alive).toBe(false);
    });

    it("returns false when the request is aborted by the timeout (Runtime slow)", async () => {
        // fetch returns a promise that never resolves before the AbortController
        // fires — the function must catch that and return false, never hang.
        vi.stubGlobal(
            "fetch",
            vi.fn((_url: string | URL, init?: RequestInit) => {
                return new Promise((_resolve, reject) => {
                    const signal = init?.signal as AbortSignal | undefined;
                    signal?.addEventListener("abort", () => {
                        reject(new DOMException("Aborted", "AbortError"));
                    });
                });
            }),
        );
        // Use a short timeout so the test doesn't actually wait 3s.
        const alive = await verifyAgentHealth("com.acowork.architect", 50, "http://gw");
        expect(alive).toBe(false);
    });

    it("URL-encodes the agent_id so reverse-domain dots don't confuse the path", async () => {
        mockFetchOnce({ status: "ok" }, 200);
        await verifyAgentHealth("com.acowork.senior-engineer", 3000, "http://gw");
        expect(calls[0].url).toBe(
            "http://gw/api/agents/com.acowork.senior-engineer/health",
        );
    });
});
