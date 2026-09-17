/**
 * DocRichEditor 的 ExtensionKit —— 裁剪版扩展集（ADR-079 D1 / P0）。
 *
 * 对齐 DocFlow `extension-kit.ts` 的核心集，去掉协作（Collaboration/Caret）、
 * 图片上传、SlashCommand、数学等 P1/P2 或非必需扩展；保留 markdown
 * round-trip 可表达的节点/标记：
 *
 * - StarterKit（v3 内置 link/underline）：段落/标题1-6/粗斜删除线/行内码/
 *   引用/列表/分割线/换行/撤销重做/拖拽光标（codeBlock 拆出，见下）
 * - CodeBlock（独立扩展 + mermaid NodeView）：代码块；language=mermaid →
 *   MermaidNodeView（可视化渲染/编辑），其余语言默认渲染（P1）
 * - Placeholder：空文档占位文案
 * - CharacterCount：50_000 字上限（DocFlow 同款，ADR-079 §7 大文档性能）
 * - TaskList/TaskItem：GFM 任务列表（nested）
 * - TableKit（v3，resizable 列宽拖拽）：table/tableRow/tableHeader/tableCell
 *   （单元格 align 属性与 GFM 列对齐互转）
 * - Highlight：高亮（导出为 `<mark>`，known-lossy）
 * - Image：相对路径图片（保持 markdown 相对路径语义，不解析资源）
 *
 * 与 `src/lib/markdown/*` 的 JSON 节点名/属性一一对应 —— 修改 schema 必须
 * 同步更新 `roundtrip.test.ts` 契约。
 */

import { CharacterCount } from "@tiptap/extension-character-count";
import { CodeBlock } from "@tiptap/extension-code-block";
import { Highlight } from "@tiptap/extension-highlight";
import { Image } from "@tiptap/extension-image";
import { Placeholder } from "@tiptap/extension-placeholder";
import { StarterKit } from "@tiptap/starter-kit";
import { TableKit } from "@tiptap/extension-table";
import { TaskItem } from "@tiptap/extension-task-item";
import { TaskList } from "@tiptap/extension-task-list";
import type { Extensions } from "@tiptap/core";
import { ReactNodeViewRenderer } from "@tiptap/react";
import { MermaidNodeView } from "./MermaidNodeView";

/** 字符上限（DocFlow 同款；ADR-079 §7：大文档性能控制）。 */
export const RICH_EDITOR_CHAR_LIMIT = 50000;

export interface ExtensionKitOptions {
  /** 空文档占位文案（i18n）。 */
  placeholder?: string;
}

/** 构建扩展数组（placeholder 文案依赖运行时 i18n，故用工厂函数）。 */
export function buildExtensionKit({ placeholder }: ExtensionKitOptions = {}): Extensions {
  return [
    StarterKit.configure({
      heading: { levels: [1, 2, 3, 4, 5, 6] },
      link: {
        openOnClick: false,
        autolink: true,
        linkOnPaste: true,
      },
      // codeBlock 拆出单独扩展（挂 mermaid NodeView，P1；StarterKit 内置
      // 版本无法通过 configure 注入 addNodeView —— 实证见 v3 源码）。
      codeBlock: false,
    }),
    // codeBlock 独立扩展：mermaid 语言 → MermaidNodeView（可视化 + 编辑态），
    // 其余语言 → 默认渲染（MermaidNodeView 内部分流）。数据模型/序列化不变。
    CodeBlock.extend({
      addNodeView() {
        return ReactNodeViewRenderer(MermaidNodeView);
      },
    }),
    Placeholder.configure({
      placeholder,
    }),
    CharacterCount.configure({ limit: RICH_EDITOR_CHAR_LIMIT }),
    TaskList,
    TaskItem.configure({ nested: true }),
    TableKit.configure({ table: { resizable: true } }),
    Highlight.configure({ multicolor: true }),
    Image,
  ];
}
