import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import type { editor } from "monaco-editor";
import { MarkdownToolbar } from "./MarkdownToolbar";

const mocks = vi.hoisted(() => ({
  t: (key: string, params?: Record<string, unknown>): string => {
    const dict: Record<string, string> = {
      "doc.toolbarLabel": "Markdown 工具栏",
      "doc.tbH1": "一级标题",
      "doc.tbBold": "加粗",
      "doc.tbCodeBlock": "代码块",
      "doc.tbTable": "插入表格",
      "doc.tbTableRows": "行",
      "doc.tbTableCols": "列",
      "doc.tbTableInsert": "插入",
      "doc.tbColLabel": "列 {{n}}",
      "doc.tbMermaid": "Mermaid 流程图",
      "doc.tbRule": "分割线",
    };
    let v = dict[key] ?? key;
    if (params) {
      for (const [k, val] of Object.entries(params)) {
        v = v.replace(`{{${k}}}`, String(val));
      }
    }
    return v;
  },
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

/** Single-line fake editor/model pair. Column is 1-based; offset = column - 1. */
function makeEditor(text: string, anchor: number, active: number) {
  const executeEdits = vi.fn();
  const model = {
    getOffsetAt: (p: { column: number }) => p.column - 1,
    getPositionAt: (offset: number) => ({ lineNumber: 1, column: offset + 1 }),
    getValueInRange: (range: { startColumn: number; endColumn: number }) =>
      text.slice(range.startColumn - 1, range.endColumn - 1),
  };
  const ed = {
    getModel: () => model,
    getSelection: () => ({
      startLineNumber: 1,
      startColumn: anchor + 1,
      endLineNumber: 1,
      endColumn: active + 1,
    }),
    executeEdits,
    setPosition: vi.fn(),
    setSelection: vi.fn(),
    focus: vi.fn(),
  } as unknown as editor.IStandaloneCodeEditor;
  return { ed, executeEdits };
}

describe("MarkdownToolbar", () => {
  it("renders the toolbar with an accessible label", () => {
    const { ed } = makeEditor("", 0, 0);
    render(<MarkdownToolbar editor={ed} />);
    expect(screen.getByRole("toolbar")).toBeTruthy();
    expect(screen.getByRole("button", { name: "加粗" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Mermaid 流程图" })).toBeTruthy();
  });

  it("wraps the selection via executeEdits when Bold is clicked", () => {
    const { ed, executeEdits } = makeEditor("hello", 1, 4); // "ell"
    render(<MarkdownToolbar editor={ed} />);
    fireEvent.click(screen.getByRole("button", { name: "加粗" }));

    expect(executeEdits).toHaveBeenCalledTimes(1);
    const edit = executeEdits.mock.calls[0][1][0];
    expect(edit.text).toBe("**ell**");
    // getPositionAt is 1-based (column = offset + 1); offset 3 → column 4.
    expect(ed.setSelection).toHaveBeenCalledWith({
      startLineNumber: 1,
      startColumn: 4,
      endLineNumber: 1,
      endColumn: 7,
    });
    expect(ed.focus).toHaveBeenCalled();
  });

  it("inserts a heading at the cursor when nothing is selected", () => {
    const { ed, executeEdits } = makeEditor("abc", 2, 2);
    render(<MarkdownToolbar editor={ed} />);
    fireEvent.click(screen.getByRole("button", { name: "一级标题" }));

    const edit = executeEdits.mock.calls[0][1][0];
    expect(edit.text).toBe("# ");
    // offset 4 → column 5
    expect(ed.setPosition).toHaveBeenCalledWith({ lineNumber: 1, column: 5 });
  });

  it("inserts an n×m GFM table from the picker", () => {
    const { ed, executeEdits } = makeEditor("", 0, 0);
    render(<MarkdownToolbar editor={ed} />);

    fireEvent.click(screen.getByRole("button", { name: "插入表格" }));
    fireEvent.click(screen.getByRole("button", { name: "插入" }));

    expect(executeEdits).toHaveBeenCalledTimes(1);
    const edit = executeEdits.mock.calls[0][1][0];
    const lines = edit.text.split("\n");
    expect(lines[0]).toBe("| 列 1 | 列 2 | 列 3 |");
    expect(lines[1]).toBe("| --- | --- | --- |");
    expect(lines).toHaveLength(5); // header + separator + 3 body rows
  });

  it("disables all actions when no editor is attached", () => {
    render(<MarkdownToolbar editor={null} />);
    expect(screen.getByRole("button", { name: "加粗" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "插入表格" }).hasAttribute("disabled")).toBe(true);
  });
});
