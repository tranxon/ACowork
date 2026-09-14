/**
 * ADR-078 frontend tests — git root path derivation in the watch set.
 *
 * ADR-078 decision 8 (Major-1 revision): the Git Status panel must be
 * watched even when the right-side workspace panel is hidden — the
 * GitStatusBar sits at the bottom of the editor panel, so its fs-watch
 * subscription is INDEPENDENT of `activePanelTab === "workspace"` and
 * `rightPanelCollapsed`. The git root (`""`) derivation lives OUTSIDE
 * the workspace-panel visibility guard in `deriveWatchGroups`.
 *
 * Covers:
 *   1. Expanded git group → root path even with the workspace panel
 *      hidden (activePanelTab ≠ "workspace" AND collapsed).
 *   2. Collapsed git panel → no git-derived group.
 *   3. Root path merges with open editor tabs for the same group.
 *   4. Group key parsing (`agent\u0000workspace`).
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

// ── Mocks ────────────────────────────────────────────────────────────────

/** Mutable fixtures read by the mocked store `getState()`s. */
const mocks = {
  openFiles: [] as Array<Record<string, unknown>>,
  layout: { activePanelTab: "chat", rightPanelCollapsed: true },
  chat: {
    getActiveSessionId: vi.fn(() => "sess1"),
    agentStates: {} as Record<string, unknown>,
  },
  selectedAgentId: "a1",
  getSessionWorkspaceId: vi.fn(() => "ws1"),
  expandedKey: null as string | null,
};

vi.mock("../stores/fileEditorStore", () => ({
  useFileEditorStore: { getState: () => ({ openFiles: mocks.openFiles }) },
}));

vi.mock("../stores/layoutStore", () => ({
  useLayoutStore: { getState: () => mocks.layout },
}));

vi.mock("../stores/chatStore", () => ({
  useChatStore: { getState: () => mocks.chat },
}));

vi.mock("../stores/agentStore", () => ({
  useAgentStore: { getState: () => ({ selectedAgentId: mocks.selectedAgentId }) },
}));

vi.mock("../stores/workspaceStore", () => ({
  useWorkspaceStore: { getState: () => ({ getSessionWorkspaceId: mocks.getSessionWorkspaceId }) },
}));

vi.mock("../stores/gitStore", () => ({
  useGitStore: { getState: () => ({ expandedKey: mocks.expandedKey }) },
}));

vi.mock("./config", () => ({
  getGatewayUrl: () => "http://gw.test",
}));

vi.mock("./logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { deriveWatchGroups } from "./workspaceFsWatch";

function groupPaths(): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const [k, v] of deriveWatchGroups()) out.set(k, v);
  return out;
}

beforeEach(() => {
  mocks.openFiles = [];
  mocks.layout = { activePanelTab: "chat", rightPanelCollapsed: true };
  mocks.chat.getActiveSessionId.mockReturnValue("sess1");
  mocks.chat.agentStates = {};
  mocks.selectedAgentId = "a1";
  mocks.expandedKey = null;
});

describe("workspaceFsWatch git root derivation (ADR-078 decision 8)", () => {
  it("watches the workspace root for an expanded git panel even when the workspace panel is hidden", () => {
    // Workspace panel is NOT visible (tab on chat + collapsed) — the old
    // guard would have skipped the root entirely.
    mocks.layout = { activePanelTab: "chat", rightPanelCollapsed: true };
    mocks.expandedKey = "a1\u0000ws1";

    const groups = groupPaths();
    expect(groups.get("a1\u0000ws1")).toEqual([""]);
  });

  it("keeps the root even when the right panel is collapsed but tab is workspace", () => {
    mocks.layout = { activePanelTab: "workspace", rightPanelCollapsed: true };
    mocks.expandedKey = "a1\u0000ws1";

    const groups = groupPaths();
    expect(groups.get("a1\u0000ws1")).toEqual([""]);
  });

  it("derives nothing for a collapsed git panel", () => {
    mocks.expandedKey = null;
    const groups = groupPaths();
    expect(groups.has("a1\u0000ws1")).toBe(false);
  });

  it("parses agent/workspace out of the expanded group key", () => {
    mocks.expandedKey = "agent-9\u0000ws-7";
    const groups = groupPaths();
    expect(groups.has("agent-9\u0000ws-7")).toBe(true);
    expect(groups.get("agent-9\u0000ws-7")).toEqual([""]);
  });

  it("merges the git root with open editor tabs for the same group", () => {
    mocks.expandedKey = "a1\u0000ws1";
    mocks.openFiles = [
      {
        id: "a1:ws1:notes.md",
        agentId: "a1",
        workspaceId: "ws1",
        relPath: "notes.md",
        kind: "file",
      },
    ];

    const groups = groupPaths();
    expect(new Set(groups.get("a1\u0000ws1"))).toEqual(new Set(["", "notes.md"]));
  });

  it("does not derive a git root from URL-preview tabs (kind !== file)", () => {
    mocks.expandedKey = "a1\u0000ws1";
    mocks.openFiles = [
      {
        id: "a1:url:https://x",
        agentId: "a1",
        workspaceId: "ws1",
        relPath: "https://x",
        kind: "url",
      },
    ];

    const groups = groupPaths();
    // URL tab contributes no path; git root still present.
    expect(groups.get("a1\u0000ws1")).toEqual([""]);
  });
});
