/**
 * DocEditor — ADR-073 wire-contract regression.
 *
 * The doc server persists the importing agent's **runtime instance_id**
 * (UUID, not package id) into `meta.import.instance_id`. The UI must
 * resolve that UUID to a human-readable display name via the agentStore
 * — never display the raw UUID as the badge text. This test exercises
 * the resolution path + the wire-contract guard against a regression
 * that re-introduces the legacy `agent_id` field name.
 */
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";

const mocks = vi.hoisted(() => ({
  editorState: {
    doc: {
      meta: {
        doc_id: "doc-1",
        name: "v1 设计纪要",
        version: 1,
        import: {
          // ADR-073: this is a runtime instance UUID (not a package id).
          instance_id: "inst-aaaa-bbbb-cccc-dddd-000000000001",
          workspace_path: "ws-main:notes/design.md",
        },
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
        deleted: false,
      },
      content: "# 设计纪要",
      path: "研发/设计纪要.md",
    } as unknown as Record<string, unknown> & {
      meta: { import?: { instance_id: string; workspace_path: string } | null };
    },
    content: "# 设计纪要",
    dirty: false,
    saving: false,
    loading: false,
    mode: "preview" as const,
    conflict: false,
    saveError: null,
    pendingOpenDocId: null,
    setMode: vi.fn(),
    setContent: vi.fn(),
    save: vi.fn(),
    reload: vi.fn(),
    confirmPendingOpen: vi.fn(),
    cancelPendingOpen: vi.fn(),
  },
  healthState: {
    healthy: true,
  },
  agentState: {
    // ADR-073: the store is keyed by **instance_id** (UUID).
    agents: {
      "inst-aaaa-bbbb-cccc-dddd-000000000001": {
        meta: {
          display_name: "资深工程师",
          name: "com.acowork.senior-engineer",
        },
      },
    },
  },
  t: (key: string, params?: Record<string, unknown>): string => {
    const translations: Record<string, string> = {
      "doc.importedBy": "由 {{ agent }} 导入",
      "doc.editorEmpty": "无文档",
      "doc.versionLabel": "版本",
      "doc.unsaved": "未保存",
      "doc.saved": "已保存",
    };
    let v = translations[key] ?? key;
    if (params) {
      for (const [k, val] of Object.entries(params)) {
        v = v.replace(`{{ ${k} }}`, String(val));
      }
    }
    return v;
  },
}));

vi.mock("../../stores/doc/editorStore", () => ({
  useDocEditorStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.editorState),
}));
vi.mock("../../stores/doc/healthStore", () => ({
  useDocHealthStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.healthState),
}));
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.agentState),
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

// Mock the children/modal that we don't exercise here.
vi.mock("./DocMarkdownView", () => ({
  DocMarkdownView: () => <div data-testid="markdown-view" />,
}));
vi.mock("../../components/common/ConfirmDialog", () => ({
  ConfirmDialog: () => null,
}));

import { DocEditor } from "./DocEditor";

describe("DocEditor — ADR-073 instance_id → display name", () => {
  it("renders the importer badge with the resolved display name, not the raw UUID", () => {
    render(<DocEditor />);
    // The badge text is the i18n string with `{{ agent }}` interpolated
    // to `resolveAgentName(agents, import.instance_id)`. We expect the
    // display_name, not the raw UUID, and not the package id.
    expect(screen.getByText("由 资深工程师 导入")).toBeTruthy();
    // The raw UUID must still appear in the tooltip for forensic
    // debuggability (so the actual instance can be cross-referenced
    // against the Gateway installed_agents log).
    const badge = screen.getByTitle(
      "inst-aaaa-bbbb-cccc-dddd-000000000001 · ws-main:notes/design.md",
    );
    expect(badge).toBeTruthy();
  });

  it("falls back to the raw instance_id when agentStore has no entry for that UUID", () => {
    mocks.agentState.agents = {}; // simulate unloaded store
    render(<DocEditor />);
    expect(
      screen.getByText("由 inst-aaaa-bbbb-cccc-dddd-000000000001 导入"),
    ).toBeTruthy();
  });

  it("does not display the package id `com.acowork.*` in the badge (ADR-073)", () => {
    render(<DocEditor />);
    // The agentStore entry has BOTH `display_name` (Chinese) and
    // `name` (package id reverse-DNS). The badge must pick the
    // display_name first; the package id should never appear in the
    // visible badge text.
    expect(screen.queryByText(/com\.acowork\.senior-engineer/)).toBeNull();
  });
});