/**
 * ADR-078 frontend tests — openVirtualFile + save guard.
 *
 * The Git Status Bar opens read-only "virtual" tabs (`kind: "diff" |
 * "log"`). These are addressed as `git:${agentId}:${workspaceId}:${kind}:${relPath}`
 * so they never collide with real file tabs, and they are never saved.
 *
 * Covers:
 *   1. openVirtualFile(diff) → tab shape (id / relPath Monaco prefix /
 *      fileName / content / originalContent / gitDiffKind / non-dirty).
 *   2. Re-opening the same virtual file activates instead of duplicating.
 *   3. openVirtualFile(log) → log tab shape.
 *   4. saveFile on a virtual tab short-circuits (no fetch, no saving flag).
 *   5. closeFile removes the virtual tab.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

// ── Mocks (the store imports settingsStore → Tauri invoke) ───────────────

vi.mock("../lib/logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

vi.mock("../lib/config", () => ({
  DEFAULT_GATEWAY_URL: "http://gw.test",
  getGatewayUrl: () => "http://gw.test",
}));

vi.mock("./settingsStore", () => ({
  useSettingsStore: () => ({ gatewayUrl: "http://gw.test" }),
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { useFileEditorStore } from "./fileEditorStore";

function resetStore() {
  useFileEditorStore.setState({ openFiles: [], activeFileId: null });
}

beforeEach(() => {
  resetStore();
});

describe("openVirtualFile (ADR-078)", () => {
  it("opens a diff virtual tab with a URI-safe Monaco path", () => {
    useFileEditorStore.getState().openVirtualFile({
      agentId: "a1",
      workspaceId: "ws1",
      kind: "diff",
      relPath: "src/a.ts",
      content: "modified body",
      original: "HEAD body",
      gitDiffKind: "modified",
      language: "typescript",
    });

    const files = useFileEditorStore.getState().openFiles;
    expect(files).toHaveLength(1);
    const f = files[0];
    expect(f.id).toBe("git:a1:ws1:diff:src/a.ts");
    expect(f.kind).toBe("diff");
    // Monaco model path is scheme-prefixed so it never collides with the
    // real file's model (`diff:src/a.ts` parses as scheme "diff").
    expect(f.relPath).toBe("diff:src/a.ts");
    expect(f.fileName).toBe("diff: a.ts");
    expect(f.content).toBe("modified body");
    expect(f.originalContent).toBe("HEAD body");
    expect(f.gitDiffKind).toBe("modified");
    expect(f.language).toBe("typescript");
    expect(f.dirty).toBe(false);
    expect(f.mode).toBe("edit");
    expect(useFileEditorStore.getState().activeFileId).toBe(f.id);
  });

  it("activating an already-open virtual tab does not duplicate it", () => {
    const opts = {
      agentId: "a1",
      workspaceId: "ws1",
      kind: "diff",
      relPath: "src/a.ts",
      content: "v1",
      original: "orig",
      gitDiffKind: "modified",
      language: "typescript",
    };
    useFileEditorStore.getState().openVirtualFile(opts);
    useFileEditorStore.getState().openVirtualFile({ ...opts, content: "v2" });

    const files = useFileEditorStore.getState().openFiles;
    expect(files).toHaveLength(1);
    // First-opened content wins; the second call only activated the tab.
    expect(files[0].content).toBe("v1");
    expect(useFileEditorStore.getState().activeFileId).toBe("git:a1:ws1:diff:src/a.ts");
  });

  it("opens a log virtual tab with plaintext content", () => {
    useFileEditorStore.getState().openVirtualFile({
      agentId: "a1",
      workspaceId: "ws1",
      kind: "log",
      relPath: "src/a.ts",
      content: "abc123  me  2025-01-01\n    init",
      language: "plaintext",
    });

    const f = useFileEditorStore.getState().openFiles[0];
    expect(f.id).toBe("git:a1:ws1:log:src/a.ts");
    expect(f.kind).toBe("log");
    expect(f.relPath).toBe("log:src/a.ts");
    expect(f.fileName).toBe("log: a.ts");
    expect(f.dirty).toBe(false);
  });

  it("virtual tabs for the same path but different kind do not collide", () => {
    const base = {
      agentId: "a1",
      workspaceId: "ws1",
      relPath: "src/a.ts",
      language: "typescript",
    };
    useFileEditorStore.getState().openVirtualFile({
      ...base,
      kind: "diff",
      content: "m",
      original: "o",
    });
    useFileEditorStore.getState().openVirtualFile({
      ...base,
      kind: "log",
      content: "log",
    });
    expect(useFileEditorStore.getState().openFiles).toHaveLength(2);
  });
});

describe("saveFile guard (ADR-078 decision 7)", () => {
  it("short-circuits on virtual tabs — no fetch, no saving flag", async () => {
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);

    useFileEditorStore.getState().openVirtualFile({
      agentId: "a1",
      workspaceId: "ws1",
      kind: "diff",
      relPath: "src/a.ts",
      content: "body",
      original: "orig",
      language: "typescript",
    });
    const id = useFileEditorStore.getState().openFiles[0].id;

    const result = await useFileEditorStore.getState().saveFile(id);
    expect(result).toBeUndefined();
    expect(fetchSpy).not.toHaveBeenCalled();
    const f = useFileEditorStore.getState().openFiles[0];
    expect(f.saving).toBe(false);
    expect(f.saveError).toBeUndefined();

    vi.unstubAllGlobals();
  });
});

describe("closeFile on virtual tabs", () => {
  it("removes the tab and clears the active id", () => {
    useFileEditorStore.getState().openVirtualFile({
      agentId: "a1",
      workspaceId: "ws1",
      kind: "log",
      relPath: "src/a.ts",
      content: "log",
      language: "plaintext",
    });
    const id = useFileEditorStore.getState().openFiles[0].id;

    const closed = useFileEditorStore.getState().closeFile(id);
    expect(closed).toBe(true);
    expect(useFileEditorStore.getState().openFiles).toHaveLength(0);
    expect(useFileEditorStore.getState().activeFileId).toBeNull();
  });
});
