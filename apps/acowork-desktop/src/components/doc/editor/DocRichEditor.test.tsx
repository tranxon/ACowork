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
import { act, render, screen, waitFor } from "@testing-library/react";
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
});
