/**
 * Markdown → Tiptap JSONContent 转换（ADR-079 D5 读取链路）。
 *
 * 管道：.md（GFM）→ micromark → mdast → Tiptap JSON。
 * 纯函数、零 DOM 依赖，可在 Node/jsdom 下直接单测；round-trip 契约由
 * `roundtrip.test.ts` 锁定。
 *
 * 节点映射（与 `components/doc/editor/extension-kit.ts` 的 schema 对齐）：
 *
 * | mdast | Tiptap JSON |
 * |---|---|
 * | heading | heading(level) |
 * | paragraph | paragraph（行内 image 拆成独立块节点，对齐 ProseMirror block 模型） |
 * | text / strong / emphasis / delete / inlineCode / link | text + marks(bold/italic/strike/code/link) |
 * | image | image(src/alt/title)，链接内图片带 link mark |
 * | blockquote | blockquote |
 * | code | codeBlock(language) |
 * | list(ordered) | orderedList(start) |
 * | list | bulletList |
 * | listItem(checked) | taskItem(checked) 包在 taskList 内 |
 * | table(GFM) | table + tableRow + tableHeader/tableCell（单元格 align 逐列注入） |
 * | thematicBreak | horizontalRule |
 * | break | hardBreak |
 * | html（兜底） | paragraph(text) —— 不丢内容，round-trip 标记为 known-lossy |
 */

import type { JSONContent } from "@tiptap/core";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { gfm } from "micromark-extension-gfm";
import type { Content, Root } from "mdast";

/** 行内 → JSONContent 文本/图片节点数组（marks 沿嵌套累加）。 */
function inlineToJSON(
  children: Content[],
  marks: JSONContent["marks"] = [],
): JSONContent[] {
  const out: JSONContent[] = [];
  for (const child of children) {
    switch (child.type) {
      case "text":
        out.push({ type: "text", text: child.value, marks: marks.length ? marks : undefined });
        break;
      case "strong":
        out.push(...inlineToJSON(child.children, [...marks, { type: "bold" }]));
        break;
      case "emphasis":
        out.push(...inlineToJSON(child.children, [...marks, { type: "italic" }]));
        break;
      case "delete":
        out.push(...inlineToJSON(child.children, [...marks, { type: "strike" }]));
        break;
      case "inlineCode":
        out.push({ type: "text", text: child.value, marks: [...marks, { type: "code" }] });
        break;
      case "link": {
        const linkMark: JSONContent["marks"] = [
          ...marks,
          {
            type: "link",
            attrs: {
              href: child.url,
              ...(child.title ? { title: child.title } : {}),
            },
          },
        ];
        // 链接内图片：image 节点携带 link mark（导出时还原 [![alt](src)](href)）。
        // 注意：真正的块级拆分发生在 paragraphToJSON（链接含图片时整体提升为块）。
        for (const inner of child.children) {
          if (inner.type === "image") {
            out.push({
              type: "image",
              attrs: imageAttrs(inner.url, inner.alt ?? "", inner.title ?? undefined),
              marks: linkMark,
            });
            continue;
          }
          out.push(...inlineToJSON([inner], linkMark));
        }
        break;
      }
      case "image":
        out.push({
          type: "image",
          attrs: imageAttrs(child.url, child.alt ?? "", child.title ?? undefined),
          marks: marks.length ? marks : undefined,
        });
        break;
      case "break":
        out.push({ type: "hardBreak" });
        break;
      case "html":
        // 行内 HTML 降级为文本（rehype-raw 预览仍可渲染；编辑态原样可见）。
        out.push({ type: "text", text: child.value, marks: marks.length ? marks : undefined });
        break;
      default:
        if ("value" in child && typeof child.value === "string") {
          out.push({ type: "text", text: child.value, marks: marks.length ? marks : undefined });
        }
        break;
    }
  }
  return out;
}

function imageAttrs(src: string, alt: string, title?: string): JSONContent["attrs"] {
  return { src, alt, ...(title ? { title } : {}) };
}

/** 块级 mdast 节点 → JSONContent（返回 null 丢弃 / 数组表示拆分）。 */
function blockToJSON(node: Content): JSONContent | JSONContent[] | null {
  switch (node.type) {
    case "heading":
      return {
        type: "heading",
        attrs: { level: node.depth },
        content: inlineToJSON(node.children),
      };
    case "paragraph":
      return paragraphToJSON(node.children);
    case "blockquote":
      return {
        type: "blockquote",
        content: node.children.flatMap(blockToJSON).filter((n): n is JSONContent => Boolean(n)),
      };
    case "code":
      return {
        type: "codeBlock",
        attrs: node.lang ? { language: node.lang } : undefined,
        content: node.value ? [{ type: "text", text: node.value }] : undefined,
      };
    case "list": {
      const rawItems = node.children.filter((c) => c.type === "listItem");
      const hasChecked = rawItems.some((item) => item.checked !== null && item.checked !== undefined);
      if (hasChecked) {
        // 任一 item 带 checked → 整表转为 taskList；无 checked 的 item 视为未勾选。
        const items: JSONContent[] = rawItems.map((item) => ({
          type: "taskItem",
          attrs: { checked: item.checked === true },
          content: (item.children ?? [])
            .flatMap(blockToJSON)
            .filter((n): n is JSONContent => Boolean(n)),
        }));
        return { type: "taskList", content: items };
      }
      return {
        type: node.ordered ? "orderedList" : "bulletList",
        attrs:
          node.ordered && node.start != null && node.start !== 1
            ? { start: node.start }
            : undefined,
        content: rawItems
          .map((item) => listItemToJSON(item))
          .filter((n): n is JSONContent => Boolean(n)),
      };
    }
    case "thematicBreak":
      return { type: "horizontalRule" };
    case "table":
      return tableToJSON(node);
    case "html":
      // 块级 HTML 降级为段落文本（内容不丢；known-lossy 样式）。
      return { type: "paragraph", content: [{ type: "text", text: node.value }] };
    default:
      if ("children" in node && Array.isArray(node.children)) {
        const content = node.children.flatMap(blockToJSON).filter((n): n is JSONContent => Boolean(n));
        return content.length ? content : null;
      }
      if ("value" in node && typeof node.value === "string" && node.value.trim()) {
        return { type: "paragraph", content: [{ type: "text", text: node.value }] };
      }
      return null;
  }
}

/** 段落：把行内 image（及包裹 image 的 link）拆成独立块（PM image 是 block 节点）。 */
function paragraphToJSON(children: Content[]): JSONContent[] {
  const out: JSONContent[] = [];
  let inline: Content[] = [];
  const flush = () => {
    if (inline.length > 0) {
      const content = inlineToJSON(inline);
      if (content.length) out.push({ type: "paragraph", content });
      inline = [];
    }
  };
  for (const child of children) {
    if (child.type === "image" || (child.type === "link" && linkContainsImage(child))) {
      flush();
      const linkMark: JSONContent["marks"] =
        child.type === "link"
          ? [
              {
                type: "link",
                attrs: {
                  href: child.url,
                  ...(child.title ? { title: child.title } : {}),
                },
              },
            ]
          : undefined;
      const img = child.type === "link" ? child.children.find((c) => c.type === "image") : child;
      if (img && img.type === "image") {
        out.push({
          type: "image",
          attrs: imageAttrs(img.url, img.alt ?? "", img.title ?? undefined),
          ...(linkMark ? { marks: linkMark } : {}),
        });
      }
    } else {
      inline.push(child);
    }
  }
  flush();
  return out;
}

/** 判断链接的子孙中是否含 image（决定是否提升为块级图片）。 */
function linkContainsImage(node: Content & { type: "link" }): boolean {
  return node.children.some((c) => (c.type === "image" ? true : "children" in c ? linkContainsImage(c as Content & { type: "link" }) : false));
}

function listItemToJSON(node: Content & { type: "listItem" }): JSONContent | null {
  const children = node.children ?? [];
  if (node.checked !== null && node.checked !== undefined) {
    return {
      type: "taskItem",
      attrs: { checked: node.checked },
      content: children.flatMap(blockToJSON).filter((n): n is JSONContent => Boolean(n)),
    };
  }
  return {
    type: "listItem",
    content: children.flatMap(blockToJSON).filter((n): n is JSONContent => Boolean(n)),
  };
}

/** GFM 表格：首行 → tableHeader，其余 → tableCell；逐列注入 align。 */
function tableToJSON(node: Content & { type: "table" }): JSONContent {
  const align = node.align ?? [];
  const rows = node.children
    .filter((c) => c.type === "tableRow")
    .map((row, rowIdx) => {
      const cells = (row.children ?? [])
        .filter((c) => c.type === "tableCell")
        .map((cell, colIdx) => {
          const colAlign = align[colIdx] ?? null;
          const inlineContent = inlineToJSON(cell.children ?? []);
          return {
            type: rowIdx === 0 ? "tableHeader" : "tableCell",
            ...(colAlign ? { attrs: { align: colAlign } } : {}),
            content: [{ type: "paragraph", content: inlineContent.length ? inlineContent : undefined }],
          };
        });
      return { type: "tableRow", content: cells };
    });
  return { type: "table", content: rows };
}

/**
 * 将 Markdown 字符串转换为 Tiptap JSONContent。
 * 空/纯空白输入返回空文档（单空段落，与 DocFlow 一致）。
 */
export function markdownToTiptapJSON(markdown: string): JSONContent {
  if (!markdown || markdown.trim() === "") {
    return { type: "doc", content: [{ type: "paragraph" }] };
  }
  const tree = fromMarkdown(markdown, {
    extensions: [gfm()],
    mdastExtensions: [gfmFromMarkdown()],
  }) as Root;
  const content = tree.children.flatMap(blockToJSON).filter((n): n is JSONContent => Boolean(n));
  return { type: "doc", content: content.length ? content : [{ type: "paragraph" }] };
}
