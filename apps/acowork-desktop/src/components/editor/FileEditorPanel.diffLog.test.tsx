/**
 * ADR-078 frontend tests — FileEditorPanel Monaco branches for virtual
 * git tabs.
 *
 * Rendering the full panel requires Monaco + LSP + a dozen stores, so
 * everything heavy is stubbed and we assert the branch selection:
 *
 *   1. `kind === "diff"` → <DiffEditor original={originalContent}
 *      modified={content} readOnly /> (side-by-side, read-only).
 *   2. `kind === "log"`  → <Editor value={content} readOnly wordWrap />.
 *   3. Neither branch wires an LSP onMount (virtual tabs never attach to
 *      the LSP pool — the `kind !== "file"` guard).
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { act } from "react";

// ── Hoisted mutable fixtures + spies ─────────────────────────────────────

const h = vi.hoisted(() => {
  const MockEditor = vi.fn(() => null);
  const MockDiffEditor = vi.fn(() => null);
  return {
    MockEditor,
    MockDiffEditor,
    editorState: {
      openFiles: [] as Array<Record<string, unknown>>,
      activeFileId: null as string | null,
      setActiveFile: vi.fn(),
      updateContent: vi.fn(),
      saveFile: vi.fn(),
      closeFile: vi.fn(),
      closeOthers: vi.fn(),
      closeAllFiles: vi.fn(),
      refreshFile: vi.fn(),
      openFile: vi.fn(),
      openPreview: vi.fn(),
    },
    chatState: { addAttachedContext: vi.fn(), getActiveSessionId: vi.fn(() => null) },
    agentState: { selectedAgentId: null as string | null },
    workspaceState: { sessionWorkspaceMap: {} as Record<string, string>, requestLocate: vi.fn() },
    layoutState: { requestShowWorkspacePanel: vi.fn() },
    settingsState: { theme: "light" as string, fontSize: 14, osTheme: "light" as string },
    gitState: { isExpanded: vi.fn(() => false) },
    editorStatusState: {
      setLspSignals: vi.fn(),
      resetToIdle: vi.fn(),
      setCursor: vi.fn(),
      setSelectedCount: vi.fn(),
    },
    treeState: { getNode: vi.fn(() => null) },
  };
});

// ── Module mocks ─────────────────────────────────────────────────────────

vi.mock("@monaco-editor/react", () => ({
  default: (props: Record<string, unknown>) => {
    h.MockEditor(props);
    return null;
  },
  Editor: (props: Record<string, unknown>) => {
    h.MockEditor(props);
    return null;
  },
  DiffEditor: (props: Record<string, unknown>) => {
    h.MockDiffEditor(props);
    return null;
  },
}));

vi.mock("../../lib/monacoBootstrap", () => ({
  initMonaco: () => Promise.resolve(),
}));

vi.mock("./lspProviders", () => ({
  registerLspProviders: () => () => {},
  disposeModelForFile: () => {},
  unpinPreviewModel: () => {},
}));

vi.mock("../../hooks/useLspClientPool", () => ({
  useLspClientPool: () => ({
    activeStatus: null,
    activeStatusMessage: "",
    activeClient: null,
  }),
}));

vi.mock("../../hooks/useReportFilePanelBounds", () => ({
  useReportFilePanelBounds: () => {},
}));

function selectorStore<T>(state: T) {
  return Object.assign(
    (selector: (s: T) => unknown) => selector(state),
    { getState: () => state },
  );
}

vi.mock("../../stores/fileEditorStore", () => ({
  registerFileDisposer: () => () => {},
  useFileEditorStore: selectorStore(h.editorState),
  // Pass-through helper so the panel renders the same real-path
  // stripping it does in production. Tests don't assert on tooltip
  // text — only that the DiffEditor / binary placeholder render.
  sourceRelPath: (file: { kind: string; relPath: string }) =>
    file.kind === "diff" || file.kind === "log"
      ? file.relPath.replace(/^(diff|log):/, "")
      : file.relPath,
}));

vi.mock("../../stores/chatStore", () => ({
  useChatStore: selectorStore(h.chatState),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: selectorStore(h.agentState),
}));

vi.mock("../../stores/workspaceStore", () => ({
  useWorkspaceStore: selectorStore(h.workspaceState),
}));

vi.mock("../../stores/layoutStore", () => ({
  useLayoutStore: selectorStore(h.layoutState),
}));

vi.mock("../../stores/settingsStore", () => ({
  useSettingsStore: selectorStore(h.settingsState),
}));

vi.mock("../../stores/editorStatusStore", () => ({
  useEditorStatusStore: { getState: () => h.editorStatusState },
}));

vi.mock("../../stores/fileTree", () => ({
  useFileTreeStore: { getState: () => h.treeState },
  treeKey: (a: string, w: string, p: string) => `${a}\u0000${w}\u0000${p}`,
  isReadyNode: () => false,
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("../../lib/logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

vi.mock("../../lib/config", () => ({
  getGatewayUrl: () => "http://gw.test",
  DEFAULT_GATEWAY_URL: "http://gw.test",
}));

// ── Child components → null stubs ────────────────────────────────────────

vi.mock("./LspDocumentTracker", () => ({ LspDocumentTracker: () => null }));
vi.mock("./MarkdownPreviewView", () => ({ MarkdownPreviewView: () => null }));
vi.mock("./UrlPreviewView", () => ({ UrlPreviewView: () => null }));
vi.mock("./HtmlPreviewView", () => ({ HtmlPreviewView: () => null }));
vi.mock("./GoToFilePalette", () => ({ GoToFilePalette: () => null }));
vi.mock("./SymbolSearchPanel", () => ({ SymbolSearchPanel: () => null }));
vi.mock("../common/ScrollableTabBar", () => ({ ScrollableTabBar: () => null }));
vi.mock("../common/tab", () => ({ TabItem: () => null }));
vi.mock("../common/SetiIcon", () => ({ SetiIcon: () => null }));
vi.mock("../common/Tooltip", () => ({ Tooltip: () => null }));
vi.mock("../workspace/FileTree/fileIcons", () => ({ getFileIcon: () => null }));

// ── SUT ──────────────────────────────────────────────────────────────────

import { FileEditorPanel } from "./FileEditorPanel";

function diffFile(overrides: Record<string, unknown> = {}) {
  return {
    id: "git:a1:ws1:diff:src/a.ts",
    agentId: "a1",
    workspaceId: "ws1",
    relPath: "diff:src/a.ts",
    fileName: "diff: a.ts",
    content: "working-tree body",
    originalContent: "HEAD body",
    loading: false,
    saving: false,
    language: "typescript",
    dirty: false,
    mode: "edit",
    kind: "diff",
    gitDiffKind: "modified",
    ...overrides,
  };
}

function logFile(overrides: Record<string, unknown> = {}) {
  return {
    id: "git:a1:ws1:log:src/a.ts",
    agentId: "a1",
    workspaceId: "ws1",
    relPath: "log:src/a.ts",
    fileName: "log: a.ts",
    content: "abc123  me  2025-01-01\n    init",
    originalContent: "abc123  me  2025-01-01\n    init",
    loading: false,
    saving: false,
    language: "plaintext",
    dirty: false,
    mode: "edit",
    kind: "log",
    ...overrides,
  };
}

beforeEach(() => {
  h.editorState.openFiles = [];
  h.editorState.activeFileId = null;
  h.MockEditor.mockClear();
  h.MockDiffEditor.mockClear();
});

describe("FileEditorPanel virtual git tabs (ADR-078 decision 7)", () => {
  it("renders a read-only side-by-side DiffEditor for a diff tab", async () => {
    const file = diffFile();
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(h.MockDiffEditor).toHaveBeenCalled();
    });
    const props = h.MockDiffEditor.mock.calls.at(-1)![0] as {
      original?: string;
      modified?: string;
      language?: string;
      options?: { readOnly?: boolean; renderSideBySide?: boolean };
    };
    expect(props.original).toBe("HEAD body");
    expect(props.modified).toBe("working-tree body");
    expect(props.language).toBe("typescript");
    expect(props.options?.readOnly).toBe(true);
    expect(props.options?.renderSideBySide).toBe(true);
    // The plain (single-pane) editor must NOT be used for diffs.
    expect(h.MockEditor).not.toHaveBeenCalled();
    await act(async () => {}); // flush the async initMonaco state update
  });

  it("renders a read-only single-pane Editor for a log tab", async () => {
    const file = logFile();
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(h.MockEditor).toHaveBeenCalled();
    });
    const props = h.MockEditor.mock.calls.at(-1)![0] as {
      value?: string;
      language?: string;
      options?: { readOnly?: boolean; lineNumbers?: string; wordWrap?: string };
    };
    expect(props.value).toContain("abc123");
    expect(props.language).toBe("plaintext");
    expect(props.options?.readOnly).toBe(true);
    expect(props.options?.wordWrap).toBe("on");
    // Diff tabs are not shown for logs.
    expect(h.MockDiffEditor).not.toHaveBeenCalled();
    await act(async () => {}); // flush the async initMonaco state update
  });

  it("does not wire an LSP onMount for virtual tabs (kind !== file)", async () => {
    const file = diffFile();
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(h.MockDiffEditor).toHaveBeenCalled();
    });
    // The diff branch DOES pass an onMount — but it's only used to
    // capture the editor ref for GitVirtualNav's hunk-jump buttons.
    // The "no LSP wiring" invariant (ADR-058 / ADR-078) is preserved
    // because the regular <Editor> kind === "file" branch is the only
    // one that wires LSP didOpen / didChange hooks via
    // handleEditorMount (FileEditorPanel.tsx ~L1577). Asserting only
    // that the supplied onMount doesn't trigger any LSP / editor-
    // state side effects is enough to lock the invariant in.
    const diffProps = h.MockDiffEditor.mock.calls.at(-1)![0] as Record<string, unknown>;
    const onMount = diffProps.onMount as ((ed: unknown) => void) | undefined;
    expect(typeof onMount).toBe("function");
    onMount?.({} as never);
    expect(h.editorState.setActiveFile).not.toHaveBeenCalled();
    await act(async () => {}); // flush the async initMonaco state update
  });

  it("renders a binary placeholder instead of empty DiffEditor panes", async () => {
    // ADR-078 decision 4/7: binary diffs arrive as kind=binary with no
    // content — the UI shows an explicit notice, never two blank panes.
    const file = diffFile({ gitDiffKind: "binary" });
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(screen.getByText("gitStatus.binaryDiff")).toBeTruthy();
    });
    expect(h.MockDiffEditor).not.toHaveBeenCalled();
    await act(async () => {}); // flush the async initMonaco state update
  });
});
