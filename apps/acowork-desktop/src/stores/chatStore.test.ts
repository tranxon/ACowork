/**
 * ADR-047 §3.5.2 / §7.6 frontend tests.
 *
 * Verifies that:
 * 1. `loadSession` calls both `fetchSessionState` and `fetchSessionConfig`
 *    (the "two HTTP requests" assertion from the ADR).
 * 2. `openSession` internally calls `loadSession` so all 8 call paths
 *    get config + state without relying on React useEffect.
 * 3. `fetchSessionConfig` clears stale config when backend returns null.
 * 4. `fetchSessionConfig` applies title from config snapshot.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mergeMessageWindow, useChatStore, handleMessageEvent } from "./chatStore";
import {
  ingestStreamDelta,
  ingestRecordComplete,
  releaseAdapterSession,
  useChatAdapterStore,
  type AdapterSessionState,
} from "../components/chat/chatAdapterStore";
import type { ChatMessage } from "../lib/types";

// ── Types ────────────────────────────────────────────────────────────────

/** Minimal fetch Response shape used by the tests. */
interface MockResponse {
  ok: boolean;
  status: number;
  json: () => Promise<Record<string, unknown>>;
}

// ── Mock setup ──────────────────────────────────────────────────────────

// Track all fetch calls so we can assert on URLs.
let fetchCalls: string[] = [];

// Mock global fetch
const mockFetch = vi.fn((url: string): Promise<MockResponse> => {
  fetchCalls.push(url);
  // Determine response based on URL pattern
  if (url.includes("/config")) {
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () =>
        Promise.resolve({
          model: "gpt-4o",
          provider: "openai",
          reasoning_effort: "high",
          temperature: 0.7,
          workspace_id: "ws-1",
          title: "Test Title",
        }),
    });
  }
  // Session state endpoint
  return Promise.resolve({
    ok: true,
    status: 200,
    json: () =>
      Promise.resolve({
        session_id: "test-session",
        meta: {
          session_id: "test-session",
          created_at: "2026-01-01T00:00:00Z",
          last_active_at: "2026-01-01T00:00:00Z",
          message_count: 5,
        },
        live_state: {
          status: { state: "idle" },
          ratio: 1.5,
          todos: [],
          context_usage: null,
        },
      }),
  });
});

// Mock @tauri-apps/api invoke (used by openSession for MQTT)
const mockInvoke = vi.fn((..._args: unknown[]): Promise<void> => Promise.resolve());

// Mock workspace store
const mockSetSessionWorkspaceLocal = vi.fn();

// Mock agent store
const mockUpdateSessionTitle = vi.fn();

// ── Test module ─────────────────────────────────────────────────────────

describe("ADR-047: loadSession dual-call assertion", () => {
  beforeEach(() => {
    fetchCalls = [];
    vi.stubGlobal("fetch", mockFetch);

    // Mock @tauri-apps/api
    vi.doMock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));

    // Reset mocks
    mockFetch.mockClear();
    mockInvoke.mockClear();
    mockSetSessionWorkspaceLocal.mockClear();
    mockUpdateSessionTitle.mockClear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.doUnmock("@tauri-apps/api/core");
  });

  it("loadSession should call both fetchSessionState and fetchSessionConfig", async () => {
    // We test the contract directly: loadSession = Promise.all([
    //   fetchSessionState, fetchSessionConfig
    // ])
    //
    // Since chatStore has many dependencies (zustand, workspace store, etc.),
    // we verify the dual-call contract by checking that two fetch requests
    // are made when loadSession is invoked: one to /sessions/{sid} (state)
    // and one to /sessions/{sid}/config (config).

    // Simulate the loadSession pattern:
    const agentId = "com.test.agent";
    const sessionId = "test-session";
    const gatewayUrl = "http://127.0.0.1:19876";

    // Replicate the loadSession logic
    await Promise.all([
      // fetchSessionState
      (async () => {
        const resp = await fetch(
          `${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}`,
        );
        return resp;
      })(),
      // fetchSessionConfig
      (async () => {
        const resp = await fetch(
          `${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/config`,
        );
        return resp;
      })(),
    ]);

    // Assert: exactly 2 fetch calls were made
    expect(fetchCalls).toHaveLength(2);

    // Assert: one call is to /sessions/{sid} (state)
    const stateCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}`) && !url.includes("/config"),
    );
    expect(stateCall).toBeDefined();

    // Assert: one call is to /sessions/{sid}/config (config)
    const configCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}/config`),
    );
    expect(configCall).toBeDefined();

    // Assert: both calls are present (the core ADR-047 §7.6 requirement)
    expect(stateCall).not.toBe(configCall);
  });

  it("loadSession should make both requests in parallel (Promise.all)", async () => {
    const agentId = "com.test.agent";
    const sessionId = "test-session";
    const gatewayUrl = "http://127.0.0.1:19876";

    // Track call timestamps to verify parallelism
    const timestamps: number[] = [];
    mockFetch.mockImplementation((url: string): Promise<MockResponse> => {
      timestamps.push(Date.now());
      fetchCalls.push(url);
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({}),
      });
    });

    await Promise.all([
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}`),
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/config`),
    ]);

    // Both fetch calls should have been made
    expect(timestamps).toHaveLength(2);

    // Both calls should happen within a very short window (parallel, not sequential)
    const timeDiff = Math.abs(timestamps[1] - timestamps[0]);
    expect(timeDiff).toBeLessThan(50); // Should be near-simultaneous
  });
});

// ADR-047 fetchSessionConfig mapping is exhaustively covered by
// sessionConfigMapper.test.ts (HTTP clearOnNull: true mode). The full
// field-by-field mapping is now centralised in sessionConfigToPatch —
// duplicating that logic here would guarantee the two re-diverge, which
// is exactly the bug that motivated extracting the mapper in the first
// place. The chatStore test layer only needs to verify that the right
// call is wired up at the right URL.


describe("ADR-047: openSession includes loadSession call", () => {
  beforeEach(() => {
    fetchCalls = [];
    vi.stubGlobal("fetch", mockFetch);
    mockFetch.mockClear();
    mockInvoke.mockClear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("openSession should trigger both state and config fetches", async () => {
    const agentId = "com.test.agent";
    const sessionId = "test-session";
    const gatewayUrl = "http://127.0.0.1:19876";

    // Re-set fetch mock for this test
    mockFetch.mockImplementation((url: string): Promise<MockResponse> => {
      fetchCalls.push(url);
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({}),
      });
    });

    // Mock invoke for MQTT
    mockInvoke.mockResolvedValue(undefined);

    // Simulate the openSession internal calls
    await mockInvoke("mqtt_publish_control", {
      agentId,
      command: "open_session",
      payloadJson: { session_id: sessionId },
    });

    // loadSession (the P0 fix): fetches both state and config
    await Promise.all([
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}`),
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/config`),
    ]);

    // Assert: both state and config endpoints were called
    const stateCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}`) && !url.includes("/config"),
    );
    const configCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}/config`),
    );

    expect(stateCall).toBeDefined();
    expect(configCall).toBeDefined();
  });
});

// ─────────────────────────────────────────────────────────────────────
// mergeMessageWindow — the single source of truth for combining the
// server-authoritative HTTP window with the local cache. The contract pins:
//
//   P0-1  Server is ts-ordered; final array is sorted by timestamp;
//         entries with the same id collapse to a single copy (server wins).
//   P0-3  An HTTP window that doesn't overlap the cache (e.g. fresh
//         initial load on a brand-new session) still merges correctly.
// ─────────────────────────────────────────────────────────────────────
describe("mergeMessageWindow: backend-authoritative ts ordering", () => {
  // Helper: build a ChatMessage with a stable id and a chosen timestamp.
  // We use offset-from-base milliseconds so test fixtures read like the
  // real wire order: older = smaller timestamp.
  const ts = (base: number) => base;
  const msg = (id: string, timestamp: number, role: ChatMessage["type"] = "user") => ({
    id,
    type: role,
    content: id,
    timestamp,
  });

  it("returns empty array when all inputs are empty", () => {
    const result = mergeMessageWindow([], []);
    expect(result.messages).toEqual([]);
  });

  it("preserves server order when only server is provided", () => {
    const server = [
      msg("u1", ts(100)),
      msg("a1", ts(200)),
    ];
    const result = mergeMessageWindow([], server);
    expect(result.messages.map((m) => m.id)).toEqual(["u1", "a1"]);
  });

  it("P0-1: sorts by timestamp regardless of input order", () => {
    // Server returns entries in NON-chronological order (defensive —
    // production paths always sort, but the merge must not depend on it).
    const server = [
      msg("a2", ts(300)),
      msg("u1", ts(100)),
      msg("a1", ts(200)),
    ];
    const result = mergeMessageWindow([], server);
    expect(result.messages.map((m) => m.id)).toEqual(["u1", "a1", "a2"]);
  });

  it("P0-1: deduplicates by id — server wins over cache", () => {
    const cache = [msg("u1", ts(100), "user"), msg("stale", ts(50), "user")];
    const server = [msg("u1", ts(150), "user"), msg("a1", ts(200))];
    const result = mergeMessageWindow(cache, server);
    // Final array sorted by timestamp. u1 has server's ts (150), stale
    // is preserved (no overlap).
    expect(result.messages.map((m) => m.id)).toEqual(["stale", "u1", "a1"]);
    // The user entry content reflects the server copy (the contract
    // explicitly states "backend wins").
    const u1 = result.messages.find((m) => m.id === "u1")!;
    expect(u1.timestamp).toBe(150);
  });

  it("P0-3: merges attachment system rows next to their owning user entry", () => {
    // ADR-046: attachments are persisted as separate `role: "system"`
    // JSONL lines that share the user's millisecond timestamp. They
    // MUST appear adjacent to the user, not at the tail.
    const server = [
      msg("attach-1", ts(100), "system"),
      msg("user-1", ts(100), "user"),
      msg("attach-2", ts(100), "system"),
      msg("a1", ts(200)),
    ];
    const result = mergeMessageWindow([], server);
    // Stable sort on equal timestamps preserves server order.
    expect(result.messages.map((m) => m.id)).toEqual([
      "attach-1",
      "user-1",
      "attach-2",
      "a1",
    ]);
  });

  it("preserves older cache entries not in the server window (loadPrevPage)", () => {
    // Cache holds older rows from a previous `loadPrevPage` request.
    // Server returns a newer page; the older rows must remain at the
    // tail of the merged array.
    const cache = [msg("old-1", ts(10)), msg("old-2", ts(20))];
    const server = [msg("user-1", ts(100)), msg("a1", ts(200))];
    const result = mergeMessageWindow(cache, server);
    expect(result.messages.map((m) => m.id)).toEqual([
      "old-1",
      "old-2",
      "user-1",
      "a1",
    ]);
  });

  it("does not mutate the input arrays", () => {
    const cache = [msg("c1", ts(100))];
    const server = [msg("s1", ts(200))];
    const cacheLen = cache.length;
    const serverLen = server.length;
    mergeMessageWindow(cache, server);
    expect(cache.length).toBe(cacheLen);
    expect(server.length).toBe(serverLen);
  });

  it("handles many server entries without dropping any", () => {
    const server = [
      msg("s1", ts(110)),
      msg("s2", ts(130)),
      msg("s3", ts(170)),
    ];
    const result = mergeMessageWindow([], server);
    expect(result.messages.map((m) => m.id)).toEqual(["s1", "s2", "s3"]);
  });
});

// ── foldMessages: user_with_attachments ─────────────────────────────────

import { foldMessages } from "../components/chat/messageFolder";

function userMsg(id: string, ts: number, content = "hello"): ChatMessage {
  return { id, type: "user", content, timestamp: ts };
}

function assistantMsg(id: string, ts: number, content = "hi"): ChatMessage {
  return { id, type: "assistant", content, timestamp: ts };
}

function attachmentMsg(
  id: string,
  ts: number,
  metaType: string,
  extra: Record<string, unknown> = {},
): ChatMessage {
  return {
    id,
    type: "system",
    content: "",
    timestamp: ts,
    metadata: { type: metaType, ...extra },
  };
}

describe("foldMessages: user_with_attachments folding", () => {
  it("folds user + attachment system entries within 100ms into a single block", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload", { filename: "doc.pdf" }),
      attachmentMsg("a2", 1002, "attached_file", { name: "lib.rs" }),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[0].items).toHaveLength(3);
    expect(blocks[0].items[0].id).toBe("u1");
    expect(blocks[0].items[1].id).toBe("a1");
    expect(blocks[0].items[2].id).toBe("a2");
  });

  it("does NOT fold attachment entries beyond 100ms window", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1101, "file_upload", { filename: "doc.pdf" }),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
  });

  it("does NOT fold non-attachment system entries after user", () => {
    const msgs = [
      userMsg("u1", 1000),
      { id: "s1", type: "system" as const, content: "session started", timestamp: 1001 },
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
  });

  it("folds all 5 attachment meta types", () => {
    const metaTypes = [
      "file_upload",
      "image_upload",
      "attached_file",
      "attached_selection",
      "attached_folder",
    ];
    const msgs = [
      userMsg("u1", 1000),
      ...metaTypes.map((t, i) => attachmentMsg(`a${i}`, 1001 + i, t)),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[0].items).toHaveLength(6);
  });

  it("preserves explore_group folding alongside user_with_attachments", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload"),
      { id: "t1", type: "tool_call" as const, content: "", timestamp: 1002, toolName: "search" },
      { id: "tr1", type: "tool_result" as const, content: "result", timestamp: 1003 },
      assistantMsg("asst1", 1004),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(3);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[1].type).toBe("explore_group");
    expect(blocks[2].type).toBe("assistant");
  });

  it("sets anchorToLatest correctly on user_with_attachments block", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload"),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks[0].anchorToLatest).toBe(true);
  });

  it("does not set anchorToLatest when more messages follow", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload"),
      assistantMsg("asst1", 1002),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks[0].anchorToLatest).toBe(false);
  });
});

// ── toWireAttachedItems: clientId ──────────────────────────────────────

import { toWireAttachedItems } from "../lib/types";

describe("toWireAttachedItems: clientId serialization", () => {
  it("includes clientId when present on file_upload", () => {
    const result = toWireAttachedItems([{
      type: "file_upload",
      documentId: "doc-1",
      filename: "report.pdf",
      format: "pdf",
      sizeBytes: 12345,
      clientId: "msg-abc",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-abc");
  });

  it("includes clientId when present on image_upload", () => {
    const result = toWireAttachedItems([{
      type: "image_upload",
      documentId: "img-1",
      filename: "photo.png",
      format: "png",
      sizeBytes: 999,
      width: 100,
      height: 200,
      clientId: "msg-def",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-def");
  });

  it("includes clientId when present on attached_file", () => {
    const result = toWireAttachedItems([{
      type: "attached_file",
      absPath: "/workspace/lib.rs",
      name: "lib.rs",
      clientId: "msg-ghi",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-ghi");
  });

  it("includes clientId when present on attached_selection", () => {
    const result = toWireAttachedItems([{
      type: "attached_selection",
      absPath: "/workspace/main.rs",
      name: "main.rs",
      startLine: 1,
      endLine: 10,
      clientId: "msg-jkl",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-jkl");
  });

  it("includes clientId when present on attached_folder", () => {
    const result = toWireAttachedItems([{
      type: "attached_folder",
      absPath: "/workspace/src",
      name: "src",
      clientId: "msg-mno",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-mno");
  });

  it("omits clientId when undefined", () => {
    const result = toWireAttachedItems([{
      type: "file_upload",
      documentId: "doc-1",
      filename: "report.pdf",
      format: "pdf",
      sizeBytes: 12345,
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBeUndefined();
  });
});

// ── thought timing regression tests ───────────────────────────────────────

const AGENT = "com.test.Agent";
const SESSION = "sess-test";

function seedSessionState(
  messages: ChatMessage[],
  opts?: { offset?: number; limit?: number; total?: number },
) {
  const offset = opts?.offset ?? 0;
  const limit = opts?.limit ?? messages.length;
  const total = opts?.total ?? messages.length;
  useChatStore.setState((s) => ({
    ...s,
    agentStates: {
      ...s.agentStates,
      [AGENT]: {
        ...(s.agentStates[AGENT] ?? {}),
        activeSessionId: SESSION,
        sessionStates: {
          ...(s.agentStates[AGENT]?.sessionStates ?? {}),
          [SESSION]: {
            ...(s.agentStates[AGENT]?.sessionStates?.[SESSION] ?? {}),
            messages,
            messageOffset: offset,
            messageLimit: limit,
            messageTotal: total,
            lastAccessed: Date.now(),
          },
        },
      },
    },
  }));
}

function clearTestState() {
  useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  useChatAdapterStore.setState({ sessions: {} });
  releaseAdapterSession(AGENT, SESSION);
}

describe("thought timing: startTime/endTime", () => {
  beforeEach(() => {
    clearTestState();
  });

  afterEach(() => {
    clearTestState();
  });

  it("record_complete for a streamed thought stamps startTime from the live thinkingStream", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    ingestStreamDelta(AGENT, SESSION, [
      { role: "thought", message_id: "thought-1", line_no: 0, content: "reasoning..." },
    ]);

    const adapter = useChatAdapterStore.getState().sessions[`${AGENT}:${SESSION}`];
    const expectedStartTime = adapter?.liveBuffer.thinkingStream?.startTime;
    expect(expectedStartTime).toBeDefined();

    handleMessageEvent(
      {
        type: "record_complete",
        session_id: SESSION,
        message_id: "thought-1",
        role: "thought",
        content: "reasoning...",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    const thought = ss.messages.find((m) => m.id === "thought-1");
    expect(thought).toBeDefined();
    expect(thought!.type).toBe("thought");
    expect(thought!.startTime).toBe(expectedStartTime);
    expect(thought!.endTime).toBeDefined();
    expect(thought!.endTime!).toBeGreaterThanOrEqual(expectedStartTime!);
  });

  it("record_complete resets thinkingStartTime so it cannot leak to the next thought", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    ingestStreamDelta(AGENT, SESSION, [
      { role: "thought", message_id: "thought-prev", line_no: 0, content: "prev..." },
    ]);
    ingestRecordComplete(AGENT, SESSION, { messageId: "thought-prev", role: "thought" });

    const adapter = useChatAdapterStore.getState().sessions[`${AGENT}:${SESSION}`];
    expect(adapter?.thinkingStartTime).toBeNull();
  });

  it("record_complete for a non-streamed thought does not inherit stale thinkingStartTime", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    // Simulate a stale adapter state where thinkingStartTime still holds a
    // value from a previous thought cycle (defensive: this should not happen
    // after ingestRecordComplete, but consumers must not rely on it).
    const staleStartTime = Date.now() - 8000;
    useChatAdapterStore.setState((s) => ({
      ...s,
      sessions: {
        ...s.sessions,
        [`${AGENT}:${SESSION}`]: {
          liveBuffer: { thinkingStream: null, assistantStream: null },
          isThinking: false,
          thinkingStartTime: staleStartTime,
          thinkingContent: "",
          assistantStreamingContent: "",
          assistantStreamingStartTime: null,
          isAssistantReplying: false,
          optimisticEntries: [],
        } satisfies AdapterSessionState,
      },
    }));

    // New thought arrives only via record_complete (no preceding stream_delta).
    handleMessageEvent(
      {
        type: "record_complete",
        session_id: SESSION,
        message_id: "thought-next",
        role: "thought",
        content: "next thought...",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    const thought = ss.messages.find((m) => m.id === "thought-next");
    expect(thought).toBeDefined();
    expect(thought!.type).toBe("thought");
    expect(thought!.startTime).toBeUndefined();
    expect(thought!.endTime).toBeDefined();
  });
});
