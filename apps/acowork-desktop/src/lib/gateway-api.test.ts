/**
 * User profile API client tests.
 *
 * Regression coverage for the backend response contract mismatch:
 * POST/PUT /api/users return `UserResponse { user, version }`, NOT a bare
 * profile. The client used to return the raw envelope, so callers stored
 * `{ user, version }` as the profile — `user_id` became undefined and every
 * subsequent PUT hit `/api/users/undefined` (404) → "save failed".
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
    createUser,
    updateUser,
    activateUser,
    fetchActiveUser,
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

describe("createUser response unwrapping", () => {
    it("unwraps { user, version } and returns the created profile", async () => {
        mockFetchOnce(ENVELOPE);

        const result = await createUser(
            { display_name: "大鱼", language: "zh-CN", timezone: "Asia/Shanghai" },
            "http://gw",
        );

        expect(result).toEqual(PROFILE);
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
