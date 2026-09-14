/**
 * ADR-078 frontend tests — gitStore data layer.
 *
 * Covers the Desktop-side contract with the Gateway reverse proxy
 * (`/api/agents/{id}/git/status|diff|log`):
 *   1. URL construction (workspace_id param, `__agent_home__` elision).
 *   2. Request error surfacing (non-2xx → status.error carries status).
 *   3. In-flight dedup for concurrent fetchStatus calls.
 *   4. Expanded state (setExpanded / isExpanded) + collapse cancels a
 *      pending debounced fs-refresh.
 *   5. notifyFsChanged debounce (300ms) — only while the group is
 *      expanded; bursts coalesce into ONE refresh.
 *   6. invalidate / refresh semantics.
 *   7. DTO field passthrough — Runtime serializes camelCase
 *      (isRepo / shortHash / oldPath) and the store must surface them
 *      verbatim (the wire-contract bug fixed in f02b7b46).
 *
 * The 503-retry loop itself lives in httpRetry (covered by
 * httpRetry.test.ts); here we stub with503Retry as a transparent
 * passthrough so the store's own URL/state logic is what's under test.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// ── Mocks ────────────────────────────────────────────────────────────────

const mockWith503Retry = vi.fn(
  async (fetcher: (sig?: AbortSignal) => Promise<Response>) => fetcher(undefined),
);

vi.mock("../lib/config", () => ({
  getGatewayUrl: () => "http://gw.test",
}));

vi.mock("../lib/httpRetry", () => ({
  with503Retry: (fetcher: (sig?: AbortSignal) => Promise<Response>) =>
    mockWith503Retry(fetcher),
}));

vi.mock("../lib/logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { useGitStore, gitGroupKey, type GitChangeDto } from "./gitStore";

/** Capture URLs passed to the mocked global fetch. */
let fetchUrls: string[] = [];

function mockFetchOk(body: Record<string, unknown>) {
  return {
    ok: true,
    status: 200,
    statusText: "OK",
    json: () => Promise.resolve({ ...body }),
  } as Response;
}

function stubFetch() {
  fetchUrls = [];
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string | URL) => {
      fetchUrls.push(String(url));
      return Promise.resolve(
        mockFetchOk({
          isRepo: true,
          branch: "main",
          error: null,
          truncated: false,
          changes: [],
        }),
      );
    }),
  );
}

const SAMPLE_CHANGE: GitChangeDto = {
  path: "src/a.ts",
  oldPath: "src/a.old.ts",
  index: "renamed",
  worktree: "modified",
  staged: true,
};

function resetStore() {
  useGitStore.setState({
    expandedKey: null,
    status: {},
    _inflight: {},
    _fsTimer: {},
  });
  mockWith503Retry.mockClear();
}

beforeEach(() => {
  stubFetch();
  resetStore();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("gitStore URL construction", () => {
  it("builds the status URL with workspace_id", async () => {
    await useGitStore.getState().fetchStatus("a1", "ws1");
    expect(fetchUrls).toHaveLength(1);
    expect(fetchUrls[0]).toBe(
      "http://gw.test/api/agents/a1/git/status?workspace_id=ws1",
    );
  });

  it("elides workspace_id for the default workspace (__agent_home__)", async () => {
    await useGitStore.getState().fetchStatus("a1", "__agent_home__");
    expect(fetchUrls[0]).toBe("http://gw.test/api/agents/a1/git/status");
  });

  it("builds the diff URL with path and cached params", async () => {
    await useGitStore.getState().fetchDiff("a1", "ws1", "src/a.ts", 1);
    const url = fetchUrls[0];
    expect(url).toContain("/api/agents/a1/git/diff");
    expect(url).toContain("workspace_id=ws1");
    expect(url).toContain("path=src%2Fa.ts");
    expect(url).toContain("cached=1");
  });

  it("builds the log URL with limit (clamped to 200) and optional path", async () => {
    await useGitStore.getState().fetchLog("a1", "ws1", "src/a.ts", 999);
    const url = fetchUrls[0];
    expect(url).toContain("/api/agents/a1/git/log");
    expect(url).toContain("workspace_id=ws1");
    expect(url).toContain("path=src%2Fa.ts");
    expect(url).toContain("limit=200");

    // No path → no path param; default limit 50.
    await useGitStore.getState().fetchLog("a1", "ws1");
    expect(fetchUrls[1]).toContain("limit=50");
    expect(fetchUrls[1]).not.toContain("path=");
  });

  it("passes every request through with503Retry", async () => {
    await useGitStore.getState().fetchStatus("a1", "ws1");
    await useGitStore.getState().fetchDiff("a1", "ws1", "x.ts");
    await useGitStore.getState().fetchLog("a1", "ws1");
    expect(mockWith503Retry).toHaveBeenCalledTimes(3);
  });
});

describe("gitStore error surfacing", () => {
  it("records a non-2xx response in status.error with the HTTP status", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve({
          ok: false,
          status: 404,
          statusText: "Not Found",
          json: () => Promise.resolve({}),
        } as Response),
      ),
    );
    const data = await useGitStore.getState().fetchStatus("a1", "ws1");
    expect(data).toBeNull();
    const entry = useGitStore.getState().status[gitGroupKey("a1", "ws1")];
    expect(entry?.error).toContain("404");
    expect(entry?.loading).toBe(false);
  });

  it("surfaces camelCase DTO fields verbatim (wire contract, f02b7b46)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          mockFetchOk({
            isRepo: true,
            branch: "feature/x",
            error: null,
            truncated: true,
            changes: [SAMPLE_CHANGE],
          }),
        ),
      ),
    );
    await useGitStore.getState().fetchStatus("a1", "ws1");
    const entry = useGitStore.getState().status[gitGroupKey("a1", "ws1")];
    expect(entry?.data?.isRepo).toBe(true);
    expect(entry?.data?.branch).toBe("feature/x");
    expect(entry?.data?.truncated).toBe(true);
    expect(entry?.data?.changes[0].oldPath).toBe("src/a.old.ts");
    expect(entry?.data?.changes[0].index).toBe("renamed");
    expect(entry?.data?.changes[0].staged).toBe(true);
  });
});

describe("gitStore in-flight dedup", () => {
  it("coalesces concurrent fetchStatus calls into one HTTP request", async () => {
    let resolveFetch!: (r: Response) => void;
    let calls = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(() => {
        calls++;
        if (calls === 1) {
          // First call stays pending so both concurrent callers share it.
          return new Promise<Response>((resolve) => {
            resolveFetch = resolve;
          });
        }
        // Subsequent calls (post-dedup) auto-resolve so awaiting them is safe.
        return Promise.resolve(
          mockFetchOk({ isRepo: true, branch: "main", error: null, truncated: false, changes: [] }),
        );
      }),
    );

    const p1 = useGitStore.getState().fetchStatus("a1", "ws1");
    const p2 = useGitStore.getState().fetchStatus("a1", "ws1");
    expect((fetch as unknown as ReturnType<typeof vi.fn>).mock.calls).toHaveLength(1);

    resolveFetch(mockFetchOk({ isRepo: true, branch: "main", error: null, truncated: false, changes: [] }));
    const [r1, r2] = await Promise.all([p1, p2]);
    expect(r1).not.toBeNull();
    expect(r2).not.toBeNull();

    // After completion the slot is cleared — a later call fetches again.
    await useGitStore.getState().fetchStatus("a1", "ws1");
    expect((fetch as unknown as ReturnType<typeof vi.fn>).mock.calls).toHaveLength(2);
  });
});

describe("gitStore expanded state", () => {
  it("setExpanded(true) marks the group expanded and pulls initial status", async () => {
    useGitStore.getState().setExpanded("a1", "ws1", true);
    expect(useGitStore.getState().isExpanded("a1", "ws1")).toBe(true);
    // Expand → refresh → one status fetch.
    await vi.waitFor(() => {
      expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(true);
    });
  });

  it("setExpanded(false) collapses and does not refetch", async () => {
    useGitStore.getState().setExpanded("a1", "ws1", true);
    await vi.waitFor(() => expect(fetchUrls).toHaveLength(1));
    useGitStore.getState().setExpanded("a1", "ws1", false);
    expect(useGitStore.getState().isExpanded("a1", "ws1")).toBe(false);
    expect(fetchUrls).toHaveLength(1);
  });

  it("repeat setExpanded(true) is a no-op (no duplicate fetch)", async () => {
    useGitStore.getState().setExpanded("a1", "ws1", true);
    await vi.waitFor(() => expect(fetchUrls).toHaveLength(1));
    useGitStore.getState().setExpanded("a1", "ws1", true);
    expect(fetchUrls).toHaveLength(1);
  });

  it("collapse cancels a pending debounced fs-refresh", async () => {
    vi.useFakeTimers();
    useGitStore.getState().setExpanded("a1", "ws1", true);
    // Let the initial refresh's microtasks settle.
    await vi.advanceTimersByTimeAsync(0);

    useGitStore.getState().notifyFsChanged("a1", "ws1"); // schedules 300ms refresh
    useGitStore.getState().setExpanded("a1", "ws1", false); // cancels it
    await vi.advanceTimersByTimeAsync(500);

    const statusFetches = fetchUrls.filter((u) => u.includes("/git/status"));
    expect(statusFetches).toHaveLength(1); // only the initial expand fetch
  });
});

describe("gitStore notifyFsChanged debounce", () => {
  it("ignores fs-changed events for a collapsed group", async () => {
    vi.useFakeTimers();
    useGitStore.getState().notifyFsChanged("a1", "ws1"); // not expanded
    await vi.advanceTimersByTimeAsync(500);
    expect(fetchUrls.filter((u) => u.includes("/git/status"))).toHaveLength(0);
  });

  it("coalesces a burst of fs-changed events into ONE refresh", async () => {
    vi.useFakeTimers();
    useGitStore.getState().setExpanded("a1", "ws1", true);
    await vi.advanceTimersByTimeAsync(0);

    // Three events inside the 300ms window → one debounced refresh.
    useGitStore.getState().notifyFsChanged("a1", "ws1");
    useGitStore.getState().notifyFsChanged("a1", "ws1");
    useGitStore.getState().notifyFsChanged("a1", "ws1");
    await vi.advanceTimersByTimeAsync(100);
    expect(fetchUrls.filter((u) => u.includes("/git/status"))).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(250);
    const statusFetches = fetchUrls.filter((u) => u.includes("/git/status"));
    expect(statusFetches).toHaveLength(2); // expand fetch + one debounced refresh
  });

  it("refreshes only the expanded group when events hit a different workspace", async () => {
    vi.useFakeTimers();
    useGitStore.getState().setExpanded("a1", "ws1", true);
    await vi.advanceTimersByTimeAsync(0);

    useGitStore.getState().notifyFsChanged("a1", "ws2"); // different group
    await vi.advanceTimersByTimeAsync(500);
    expect(fetchUrls.filter((u) => u.includes("/git/status"))).toHaveLength(1);
  });
});

describe("gitStore invalidate / refresh", () => {
  it("invalidate drops the cached entry for the group", async () => {
    await useGitStore.getState().fetchStatus("a1", "ws1");
    expect(useGitStore.getState().status[gitGroupKey("a1", "ws1")]?.data).not.toBeNull();

    useGitStore.getState().invalidate("a1", "ws1");
    expect(useGitStore.getState().status[gitGroupKey("a1", "ws1")]).toBeUndefined();
  });

  it("refresh bypasses the cache and fetches again", async () => {
    await useGitStore.getState().fetchStatus("a1", "ws1");
    const before = useGitStore.getState().status[gitGroupKey("a1", "ws1")]?.fetchedAt;
    await useGitStore.getState().refresh("a1", "ws1");
    const after = useGitStore.getState().status[gitGroupKey("a1", "ws1")]?.fetchedAt;
    expect(after).toBeGreaterThanOrEqual(before ?? 0);
    expect(fetchUrls.filter((u) => u.includes("/git/status"))).toHaveLength(2);
  });
});
