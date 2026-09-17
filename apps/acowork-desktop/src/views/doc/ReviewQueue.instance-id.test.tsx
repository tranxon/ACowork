/**
 * ReviewQueue — ADR-073 wire-contract regression.
 *
 * `UpdateRequest.submitted_by` on the wire is the runtime instance_id
 * (UUID, ADR-073) carried over the `X-MCP-Actor` header. The UI must
 * resolve it to a human display name via the agentStore, not display
 * the raw UUID. This test pins that contract.
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  healthState: { healthy: true },
  requestState: {
    requests: [
      {
        request_id: "req-1",
        doc_id: "doc-1",
        path: "研发/纪要.md",
        base_version: 3,
        content: "v2 提案",
        // ADR-073: this is the runtime instance identity, not the
        // package id (reverse-DNS). The UI must resolve it.
        submitted_by: "inst-aaaa-bbbb-cccc-dddd-000000000001",
        status: "pending" as const,
        created_at: "2026-01-01T00:00:00Z",
        reviewed_at: null,
        reviewed_by: null,
        review_note: null,
      },
    ],
    loading: false,
    error: null,
    loadPending: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(true),
    reject: vi.fn().mockResolvedValue(true),
  },
  editorState: {
    doc: null,
    applyMergedUpdate: vi.fn(),
  },
  agentState: {
    // ADR-073: store is keyed by instance_id (UUID).
    agents: {
      "inst-aaaa-bbbb-cccc-dddd-000000000001": {
        meta: {
          display_name: "资深工程师",
          name: "com.acowork.senior-engineer",
        },
      },
    },
  },
  toast: { addToast: vi.fn() },
  t: (key: string, params?: Record<string, unknown>): string => {
    const translations: Record<string, string> = {
      "doc.approve": "批准",
      "doc.reject": "拒绝",
      "doc.reviewOpenDoc": "打开文档",
      "doc.reviewApproved": "已批准 {{ name }}",
      "doc.reviewFailed": "审核失败",
      "doc.noteDialogTitle": "拒绝原因",
      "doc.noteDialogPlaceholder": "可选",
      "doc.noteDialogConfirm": "确认拒绝",
      "doc.noteDialogCancel": "取消",
      "doc.reviewEmpty": "无待审核请求",
      "doc.reviewRefresh": "刷新",
      "doc.reviewError": "加载失败",
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

vi.mock("../../stores/doc/requestStore", () => ({
  useDocRequestStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.requestState),
}));
vi.mock("../../stores/doc/editorStore", () => ({
  useDocEditorStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.editorState),
  // `getState()` is used for the merged-update shortcut in approve.
  useDocEditorStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector(mocks.editorState),
    { getState: () => mocks.editorState },
  ),
}));
vi.mock("../../stores/doc/healthStore", () => ({
  useDocHealthStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.healthState),
}));
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) =>
    selector(mocks.agentState),
}));
vi.mock("../../components/common/ToastProvider", () => ({
  useToast: () => mocks.toast,
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

import { ReviewQueue } from "./ReviewQueue";

describe("ReviewQueue — ADR-073 submitted_by → display name", () => {
  it("resolves the submitter to display_name, not the raw UUID", () => {
    const { container } = render(<ReviewQueue />);
    // Open the panel — the collapsed top bar only shows the pending count.
    fireEvent.click(screen.getByRole("button", { expanded: false }));
    // The submitter row contains "<resolved-name> · <timestamp>".
    // We grab it via class because the timestamp lives in the same text
    // node as the name, so `getByText("资深工程师")` won't match.
    const submitterRow = container.querySelector(
      ".text-\\[10px\\].text-text-tertiary\.truncate",
    ) as HTMLElement | null;
    expect(submitterRow).toBeTruthy();
    const rowText = submitterRow?.textContent ?? "";
    expect(rowText).toContain("资深工程师");
    expect(rowText).not.toContain("inst-aaaa-bbbb-cccc-dddd-000000000001");
    expect(rowText).not.toContain("com.acowork.senior-engineer");
  });

  it("falls back to the raw instance_id when agentStore has no entry for that UUID", () => {
    mocks.agentState.agents = {};
    const { container } = render(<ReviewQueue />);
    fireEvent.click(screen.getByRole("button", { expanded: false }));
    const submitterRow = container.querySelector(
      ".text-\\[10px\\].text-text-tertiary\.truncate",
    ) as HTMLElement | null;
    expect(submitterRow).toBeTruthy();
    const rowText = submitterRow?.textContent ?? "";
    expect(rowText).toContain("inst-aaaa-bbbb-cccc-dddd-000000000001");
  });
});
