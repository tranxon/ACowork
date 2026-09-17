/**
 * DocRichEditor — P0 富文本引擎组件测试（ADR-079 D5 / D6）。
 *
 * 覆盖四个关键契约：
 * 1. markdown 载入：contentMd → Tiptap JSON → 渲染（内容可见）。
 * 2. 编辑回写：editor 命令 → onUpdate → tiptapToMarkdown → onContentChange。
 * 3. 挂载不污染 dirty：初始回显不触发 onContentChange（无假 dirty）。
 * 4. 外部同步（审阅流联调，D6）：applyMergedUpdate / reload 推入新 contentMd
 *    → 编辑器重载；自己回写的 echo 不触发重载（无死循环）。
 * 5. readOnly 热切换：editor.setEditable 生效（DocFlow 只读链）。
 */

import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { Editor } from "@tiptap/react";
import { DocRichEditor } from "./DocRichEditor";

const mocks = vi.hoisted(() => ({
  onContentChange: vi.fn(),
  onSave: vi.fn(),
  t: (key: string): string => {
    const map: Record<string, string> = {
      "doc.richToolbar": "富文本工具栏",
      "doc.richPlaceholder": "开始输入…",
      "doc.charCount": "{{count}}/{{limit}}",
      "doc.tbH1": "一级标题",
      "doc.tbH2": "二级标题",
      "doc.tbH3": "三级标题",
      "doc.tbBold": "加粗",
      "doc.tbItalic": "斜体",
      "doc.tbStrike": "删除线",
      "doc.tbInlineCode": "行内代码",
      "doc.tbCodeBlock": "代码块",
      "doc.tbQuote": "引用",
      "doc.tbBulletList": "无序列表",
      "doc.tbOrderedList": "有序列表",
      "doc.tbTaskList": "任务列表",
      "doc.tbTable": "插入表格",
      "doc.tbAddRowBefore": "在上方插入行",
      "doc.tbAddRowAfter": "在下方插入行",
      "doc.tbAddColumnBefore": "在左侧插入列",
      "doc.tbAddColumnAfter": "在右侧插入列",
      "doc.tbDeleteRow": "删除当前行",
      "doc.tbDeleteColumn": "删除当前列",
      "doc.tbToggleHeaderRow": "切换表头行",
      "doc.tbToggleHeaderColumn": "切换表头列",
      "doc.tbDeleteTable": "删除表格",
      "doc.mermaidEdit": "编辑代码",
      "doc.mermaidPreview": "预览",
      "doc.mermaidRendering": "渲染中…",
      "doc.tbLink": "链接",
      "doc.tbImage": "图片",
      "doc.tbRule": "分割线",
      "doc.tbUndo": "撤销",
      "doc.tbRedo": "重做",
      "doc.linkPrompt": "链接地址",
      "doc.imagePrompt": "图片地址",
    };
    return map[key] ?? key;
  },
}));

vi.mock("../../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

// ProseMirror view 在 jsdom 下需要 getClientRects / getBoundingClientRect
// （focus() 的 scrollToSelection → coordsAtPos 依赖它们）。
beforeAll(() => {
  const rect = {
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
    width: 0,
    height: 0,
    x: 0,
    y: 0,
    toJSON: () => ({}),
  };
  const emptyRectList = {
    length: 0,
    item: () => null,
    [Symbol.iterator]: [][Symbol.iterator],
  };
  if (!Element.prototype.getClientRects) {
    Element.prototype.getClientRects = () => emptyRectList as unknown as DOMRectList;
  }
  if (!Element.prototype.getBoundingClientRect) {
    Element.prototype.getBoundingClientRect = () => rect as unknown as DOMRect;
  }
  if (!Range.prototype.getClientRects) {
    Range.prototype.getClientRects = () => emptyRectList as unknown as DOMRectList;
  }
  if (!Range.prototype.getBoundingClientRect) {
    Range.prototype.getBoundingClientRect = () => rect as unknown as DOMRect;
  }
});

function renderEditor(props: Partial<Parameters<typeof DocRichEditor>[0]> = {}) {
  const onEditorReady = props.onEditorReady ?? (() => {});
  const editorRef: { current: Editor | null } = { current: null };
  const utils = render(
    <DocRichEditor
      contentMd="# 标题\n\n正文段落"
      readOnly={false}
      onContentChange={mocks.onContentChange}
      onSave={mocks.onSave}
      onEditorReady={(ed) => {
        editorRef.current = ed;
        onEditorReady(ed);
      }}
      {...props}
    />,
  );
  return { ...utils, editorRef };
}

async function waitEditor(editorRef: { current: Editor | null }) {
  await waitFor(() => {
    expect(editorRef.current).not.toBeNull();
  });
  return editorRef.current!;
}

describe("DocRichEditor", () => {
  beforeEach(() => {
    mocks.onContentChange.mockClear();
    mocks.onSave.mockClear();
  });

  it("将 markdown 载入 Tiptap 并渲染内容", async () => {
    const { editorRef } = renderEditor();
    await waitEditor(editorRef);
    const el = document.querySelector(".doc-rich-content");
    expect(el?.textContent).toContain("标题");
    expect(el?.textContent).toContain("正文段落");
    expect(screen.getByRole("toolbar")).toBeTruthy();
  });

  it("挂载回显不触发 onContentChange（不污染 dirty 标志）", async () => {
    const { editorRef } = renderEditor();
    await waitEditor(editorRef);
    expect(mocks.onContentChange).not.toHaveBeenCalled();
  });

  it("编辑触发 onContentChange 回写 markdown（store 事实源）", async () => {
    const { editorRef } = renderEditor();
    const editor = await waitEditor(editorRef);
    act(() => {
      editor.chain().insertContent(" 追加文本").focus().run();
    });
    await waitFor(() => {
      expect(mocks.onContentChange).toHaveBeenCalled();
    });
    const md = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    expect(md).toContain("追加文本");
  });

  it("外部内容推送（reload / applyMergedUpdate）时重载编辑器", async () => {
    const { rerender, editorRef } = renderEditor();
    await waitEditor(editorRef);
    expect(mocks.onContentChange).not.toHaveBeenCalled();

    rerender(
      <DocRichEditor
        contentMd="# 新标题\n\n审核合并后的内容"
        readOnly={false}
        onContentChange={mocks.onContentChange}
        onSave={mocks.onSave}
      />,
    );
    await waitFor(() => {
      expect(document.querySelector(".doc-rich-content")?.textContent).toContain("审核合并后的内容");
    });
    // 外部推送不触发 onContentChange（非用户编辑）
    expect(mocks.onContentChange).not.toHaveBeenCalled();
  });

  it("readOnly 热切换：editor 变为不可编辑", async () => {
    const { rerender, editorRef } = renderEditor();
    const editor = await waitEditor(editorRef);
    rerender(
      <DocRichEditor
        contentMd="# 标题"
        readOnly={true}
        onContentChange={mocks.onContentChange}
        onSave={mocks.onSave}
      />,
    );
    await waitFor(() => {
      expect(editor.isEditable).toBe(false);
    });
    expect(document.querySelector(".doc-rich-content")?.getAttribute("contenteditable")).toBe("false");
  });

  it("表格：光标入表浮出工具条；行列增删按钮输出合法 GFM", async () => {
    const { editorRef } = renderEditor();
    const editor = await waitEditor(editorRef);
    mocks.onContentChange.mockClear();

    // 插入 2×2 表格（withHeaderRow），insertTable 后光标落在首单元格。
    act(() => {
      editor.chain().focus().insertTable({ rows: 2, cols: 2, withHeaderRow: true }).run();
    });
    expect(editor.isActive("table")).toBe(true);

    // 浮动工具条可见（shouldShow：光标在表格内且可编辑）。
    await waitFor(() => {
      const menu = document.querySelector('[data-testid="table-bubble-menu"]') as HTMLElement | null;
      expect(menu?.style.visibility).toBe("visible");
    });

    // 下行加行 + 右列加列（真实按钮点击，走 editor.chain() 进 undo 栈）。
    act(() => {
      fireEvent.click(screen.getByLabelText("在下方插入行"));
    });
    act(() => {
      fireEvent.click(screen.getByLabelText("在右侧插入列"));
    });
    await waitFor(() => {
      expect(mocks.onContentChange).toHaveBeenCalled();
    });

    const pipeLines = (md: string) => md.split("\n").filter((l) => l.trim().startsWith("|"));
    const cellCount = (line: string) => line.split("|").length - 2;
    let md = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    // 2 行 + 1 行 = 3 行（表头 + 分隔符 + 2 body），每行 3 列。
    expect(pipeLines(md)).toHaveLength(4);
    expect(cellCount(pipeLines(md)[0])).toBe(3);

    // 删列（按钮）→ 每行回到 2 列；删行（命令）→ 回到 3 行。
    act(() => {
      fireEvent.click(screen.getByLabelText("删除当前列"));
    });
    md = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    expect(cellCount(pipeLines(md)[0])).toBe(2);
    act(() => {
      editor.chain().focus().deleteRow().run();
    });
    md = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    expect(pipeLines(md)).toHaveLength(3);

    // 撤销恢复上一状态（命令进了 undo 栈）。
    act(() => {
      editor.chain().focus().undo().run();
    });
    md = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    expect(pipeLines(md)).toHaveLength(4);

    // 删除表格 → 输出不再含表格。
    act(() => {
      editor.chain().focus().deleteTable().run();
    });
    md = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    expect(pipeLines(md)).toHaveLength(0);
  });

  it("表格：readOnly 时工具条隐藏（不可编辑不浮出）", async () => {
    const { rerender, editorRef } = renderEditor();
    const editor = await waitEditor(editorRef);
    act(() => {
      editor.chain().focus().insertTable({ rows: 2, cols: 2, withHeaderRow: true }).run();
    });
    await waitFor(() => {
      const menu = document.querySelector('[data-testid="table-bubble-menu"]') as HTMLElement | null;
      expect(menu?.style.visibility).toBe("visible");
    });

    // 用编辑器当前内容 rerender（避免外部同步 setContent 移除表格），只切 readOnly。
    const currentMd = mocks.onContentChange.mock.calls.at(-1)?.[0] as string;
    rerender(
      <DocRichEditor
        contentMd={currentMd}
        readOnly={true}
        onContentChange={mocks.onContentChange}
        onSave={mocks.onSave}
      />,
    );
    await waitFor(() => {
      expect(editor.isEditable).toBe(false);
    });
    // hide() 会 element.remove() → 菜单从 DOM 消失。
    await waitFor(() => {
      expect(document.querySelector('[data-testid="table-bubble-menu"]')).toBeNull();
    });
  });

  it("mermaid 代码块：NodeView 可视化 + 点击切换编辑态", async () => {
    const { editorRef } = renderEditor({
      contentMd: "```mermaid\ngraph TD\n  A --> B\n```",
    });
    await waitEditor(editorRef);

    // mermaid language → 挂 MermaidNodeView（渲染态：编辑按钮在，代码内容隐藏）。
    await waitFor(() => {
      expect(document.querySelector('[data-mermaid-nodeview]')).not.toBeNull();
    });
    expect(screen.getByLabelText("编辑代码")).toBeTruthy();
    const contentEl = () =>
      document.querySelector('[data-mermaid-nodeview] [data-node-view-content]') as HTMLElement | null;
    // NodeViewContent 恒挂载，渲染态 wrapper 带 hidden（contentDOM 稳定）。
    expect(contentEl()?.closest(".hidden")).not.toBeNull();

    // 点击「编辑代码」→ 编辑态：预览按钮出现，编辑按钮消失，代码可见。
    act(() => {
      fireEvent.click(screen.getByLabelText("编辑代码"));
    });
    await waitFor(() => {
      expect(screen.getByLabelText("预览")).toBeTruthy();
      expect(screen.queryByLabelText("编辑代码")).toBeNull();
    });
    expect(contentEl()?.textContent).toContain("graph TD");
    expect(contentEl()?.closest(".hidden")).toBeNull();

    // 点击「预览」→ 回渲染态。
    act(() => {
      fireEvent.click(screen.getByLabelText("预览"));
    });
    await waitFor(() => {
      expect(screen.getByLabelText("编辑代码")).toBeTruthy();
      expect(screen.queryByLabelText("预览")).toBeNull();
    });
  });

  it("非 mermaid 代码块：不走 NodeView，默认代码块渲染", async () => {
    const { editorRef } = renderEditor({
      contentMd: "```rust\nfn main() {}\n```",
    });
    await waitEditor(editorRef);
    await waitFor(() => {
      expect(document.querySelector(".doc-rich-content pre")).not.toBeNull();
    });
    expect(document.querySelector('[data-mermaid-nodeview]')).toBeNull();
    expect(document.querySelector(".doc-rich-content pre")?.textContent).toContain("fn main()");
  });
});
