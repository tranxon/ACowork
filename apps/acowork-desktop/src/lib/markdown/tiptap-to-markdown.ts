/**
 * Tiptap JSONContent → Markdown（GFM）转换（ADR-079 D5 保存链路）。
 *
 * 输出约定（决定 round-trip 字节稳定性）：
 * - 块之间以空行分隔；文档以单个 `\n` 结尾。
 * - 列表嵌套缩进 = 父项内容列（marker 宽度 + 1），CommonMark 兼容：
 *   `- a` 下子列表缩进 2，`1. a` 下缩进 3。
 * - GFM 表格分隔行按单元格 align 生成 `:---` / `---:` / `:---:`。
 * - 行首歧义字符（#、>、-、数字.、```、--- 等）按需转义，保证再解析语义不变。
 * - 已知-lossy：highlight mark（导出为 `<mark>` 行内 HTML，内容保留）、
 *   下划线 mark（GFM 无对应语法，导出为纯文本）。
 */

import type { JSONContent } from "@tiptap/core";

// ── 行内（marks → markdown 语法）──────────────────────────────────────

interface MarkSig {
  type: string;
  attrs?: Record<string, unknown>;
}

function markKey(m: MarkSig): string {
  return m.type + (m.attrs ? `:${JSON.stringify(m.attrs)}` : "");
}

/** 图片 → markdown（block 或 inline 上下文共用）。 */
function renderImageNode(node: JSONContent): string {
  const src = (node.attrs?.src as string) ?? "";
  const alt = (node.attrs?.alt as string) ?? "";
  const title = node.attrs?.title as string | undefined;
  const base = `![${escapeLinkText(alt)}](${escapeUrl(src)}${title ? ` "${title}"` : ""})`;
  // 链接包裹的图片（导入时 image 携带 link mark）
  const linkMark = (node.marks ?? []).find((m) => m.type === "link");
  if (linkMark) {
    const href = (linkMark.attrs?.href as string) ?? "";
    return `[${base}](${escapeUrl(href)})`;
  }
  return base;
}

/**
 * 行内序列化器（markdown-it 式状态机）见下方 `renderInlines`。
 * 说明：marks 作为 delimiter 跨相邻文本节点保持打开/关闭，避免 `**a** **`b`**`
 * 相邻产生 `****` 粘连（真实 ADR 文档 `**废止其 `0 = 无限制`**` 场景）。
 * `blockStart=true` 表示首个文本节点位于行首（段落块级上下文，需行首转义；
 * hardBreak 之后的行也总是需要）。heading/表格单元格等 inline-only 上下文
 * 传 false —— 内容跟在 `## ` / `| ` 前缀之后，无块级歧义。
 */

/** 行首转义：避免 `# foo`、`- x`、`1. x`、`> x`、``` ``` ``` 被再解析为块语法。 */
function escapeLineStart(text: string): string {
  return text.replace(/^([#>*+-]|\d+\.|\d+\)|\s*```|---|\+\+\+|\*\*\*|===)(?=\s|$)/, "\\$1");
}

function escapeText(text: string, opts: { lineStart?: boolean } = {}): string {
  let s = text;
  if (opts.lineStart) s = escapeLineStart(s);
  // 行尾反斜杠会被解析为 hard break → 转义。
  s = s.replace(/\\$/, "\\\\");
  // 星号/下划线仅在文本边界时可能形成强调标记 → 边界转义。
  s = s.replace(/^([*_]+)/, "\\$1").replace(/([*_]+)$/, "\\$1");
  return s;
}

/** 转义链接文本/图片 alt 中的括号类字符。 */
function escapeLinkText(text: string): string {
  return text.replace(/([\\[\]])/g, "\\$1");
}

/**
 * 行内序列化器（markdown-it 式状态机）：marks 作为 delimiter 跨相邻文本节点
 * 保持「打开」状态，避免 `**a** **`b`**` 相邻产生 `****` 粘连。
 *
 * 例如 `[bold] "废止其 "` + `[bold, code] "0 = 无限制"` 输出
 * `**废止其 `0 = 无限制`**`（bold 跨节点，code 自闭合围栏）。
 *
 * mark 优先级（开闭顺序）：link 最外层 → bold/italic/strike/highlight →
 * code 内容自闭合（围栏内字面量，不参与跨节点）。
 */
const MARK_PRIORITY: Record<string, number> = {
  link: 0,
  bold: 1,
  italic: 2,
  strike: 3,
  highlight: 4,
};

function renderInlines(
  nodes: JSONContent[] | undefined,
  opts: { blockStart?: boolean } = {},
): string {
  if (!nodes) return "";
  let out = "";
  let atLineStart = opts.blockStart ?? false;
  let i = 0;

  /** 当前打开的 span marks（按优先级升序，即外层在前）。 */
  let open: NonNullable<JSONContent["marks"]> = [];

  const closeMarks = (until: Set<string>) => {
    for (let idx = open.length - 1; idx >= 0; idx--) {
      const m = open[idx];
      if (until.has(m.type)) continue;
      out += markCloser(m);
      open = open.slice(0, idx);
      // 关闭最内层一个后重新检查外层（避免跳层关闭）
      return closeMarks(until);
    }
  };

  const flushRun = (run: JSONContent[]) => {
    const marks = (run[0].marks ?? []) as NonNullable<JSONContent["marks"]>;
    const targetTypes = new Set(marks.map((m) => m.type));
    // 1) 关闭 target 之外的已开 marks（从内向外）
    closeMarks(targetTypes);
    // 2) 打开 target 中未开的 marks（外层优先）
    const sorted = [...marks].sort((a, b) => (MARK_PRIORITY[a.type] ?? 9) - (MARK_PRIORITY[b.type] ?? 9));
    for (const m of sorted) {
      if (!open.some((o) => o.type === m.type && markKey(o) === markKey(m))) {
        out += markOpener(m);
        open = [...open, m];
      }
    }
    // 3) 渲染文本（code 内容自闭合围栏；其余需转义）
    const hasCode = marks.some((m) => m.type === "code");
    const text = run.map((n) => n.text ?? "").join("");
    let rendered = hasCode ? fenceInlineCode(text) : escapeText(text, { lineStart: atLineStart });
    if (open.some((o) => o.type === "link")) rendered = escapeLinkText(rendered);
    out += rendered;
    atLineStart = false;
  };

  while (i < nodes.length) {
    const node = nodes[i];
    if (node.type === "hardBreak") {
      flushEnd();
      out += "\\\n";
      atLineStart = true;
      i += 1;
      continue;
    }
    if (node.type === "image") {
      // 防御：Tiptap UI 可能把图片放入行内上下文（PM 正常会置为块）。
      flushEnd();
      out += renderImageNode(node);
      atLineStart = false;
      i += 1;
      continue;
    }
    if (node.type !== "text") {
      if (typeof node.text === "string") {
        flushRun([node]);
      }
      i += 1;
      continue;
    }
    // 收集相同 marks 签名的连续文本节点
    const sig = (node.marks ?? []).map(markKey).join("|");
    let j = i;
    const run: JSONContent[] = [];
    while (j < nodes.length && nodes[j].type === "text" && (nodes[j].marks ?? []).map(markKey).join("|") === sig) {
      run.push(nodes[j]);
      j += 1;
    }
    flushRun(run);
    i = j;
  }
  flushEnd();
  return out;

  function flushEnd() {
    closeMarks(new Set());
  }
}

function markOpener(mark: NonNullable<JSONContent["marks"]>[number]): string {
  switch (mark.type) {
    case "bold":
      return "**";
    case "italic":
      return "*";
    case "strike":
      return "~~";
    case "highlight":
      return "<mark>";
    case "link":
      return `[`;
    default:
      return "";
  }
}

function markCloser(mark: NonNullable<JSONContent["marks"]>[number]): string {
  switch (mark.type) {
    case "bold":
      return "**";
    case "italic":
      return "*";
    case "strike":
      return "~~";
    case "highlight":
      return "</mark>";
    case "link": {
      const href = (mark.attrs?.href as string | undefined) ?? "";
      const title = mark.attrs?.title as string | undefined;
      return `](${escapeUrl(href)}${title ? ` "${title}"` : ""})`;
    }
    default:
      return "";
  }
}

/** 行内代码：内容含反引号时用变长围栏；首/尾为空格或反引号时用 GFM 双空格填充。 */
function fenceInlineCode(code: string): string {
  const backticks = code.match(/`+/g)?.map((m) => m.length) ?? [];
  const maxRun = backticks.length ? Math.max(...backticks) : 0;
  const fenceLen = Math.max(1, maxRun + 1);
  const fence = "`".repeat(fenceLen);
  const pad = /^[ `]|[ `]$/.test(code) ? " " : "";
  return `${fence}${pad}${code}${pad}${fence}`;
}

function escapeUrl(url: string): string {
  return url.replace(/\(/g, "%28").replace(/\)/g, "%29");
}

// ── 块级（node → markdown lines）──────────────────────────────────────

const EMPTY_LINE = "";

function blockToMarkdown(node: JSONContent, ctx: { indent: number } = { indent: 0 }): string {
  switch (node.type) {
    case "paragraph": {
      const inline = renderInlines(node.content, { blockStart: true });
      if (inline === "") return EMPTY_LINE;
      // 段落内的 hardBreak 已产出 `\\\n`；soft-break 文本换行也需要行首转义。
      const lines = inline.split("\n");
      return lines
        .map((ln, idx) => (idx === 0 ? ln : escapeLineStart(ln)))
        .join("\n");
    }
    case "heading": {
      const level = Math.min(Math.max((node.attrs?.level as number) ?? 1, 1), 6);
      // inline-only 上下文：无行首转义；hardBreak 在 heading 内无 markdown 表达，
      // 降级为空格（避免输出损坏的跨行 heading）。
      const inline = renderInlines(node.content).replace(/\n/g, " ");
      return `${"#".repeat(level)} ${inline}`.trimEnd();
    }
    case "blockquote": {
      const inner = (node.content ?? [])
        .map((b) => blockToMarkdown(b))
        .join("\n\n");
      return inner
        .split("\n")
        .map((ln) => (ln === EMPTY_LINE ? ">" : `> ${ln}`))
        .join("\n");
    }
    case "codeBlock": {
      const lang = (node.attrs?.language as string | undefined) ?? "";
      const code = (node.content ?? []).map((c) => c.text ?? "").join("");
      const fence = pickFence(code);
      return `${fence}${lang}\n${code}${code.endsWith("\n") ? "" : "\n"}${fence}`;
    }
    case "horizontalRule":
      return "---";
    case "image":
      return renderImageNode(node);
    case "bulletList":
    case "orderedList":
    case "taskList":
      return listToMarkdown(node, ctx.indent);
    case "table":
      return tableToMarkdown(node);
    case "hardBreak":
      return "\\";
    default:
      // 未知块：降级为段落文本。
      return renderInlines(node.content);
  }
}

/** 代码围栏：内容含 ``` 时升级为更长的围栏。 */
function pickFence(code: string): string {
  const runs = code.match(/`{3,}/g)?.map((m) => m.length) ?? [];
  const maxRun = runs.length ? Math.max(...runs) : 0;
  return "`".repeat(Math.max(3, maxRun + 1));
}

function listToMarkdown(node: JSONContent, indent: number): string {
  const ordered = node.type === "orderedList";
  const taskList = node.type === "taskList";
  const start = ordered ? ((node.attrs?.start as number | undefined) ?? 1) : null;
  const items = node.content ?? [];
  const lines: string[] = [];

  items.forEach((item, idx) => {
    let marker: string;
    if (taskList) {
      const checked = item.attrs?.checked === true;
      marker = checked ? "- [x] " : "- [ ] ";
    } else {
      marker = ordered ? `${start! + idx}. ` : "- ";
    }
    const prefix = " ".repeat(indent) + marker;
    const contentIndent = indent + marker.length;
    const children = item.content ?? [];

    if (children.length === 0) {
      lines.push(prefix.trimEnd());
      return;
    }
    const [head, ...rest] = children;
    if (head.type === "paragraph") {
      const inline = renderInlines(head.content);
      lines.push(prefix + inline);
    } else {
      lines.push(prefix.trimEnd());
      rest.unshift(head);
    }
    for (const block of rest) {
      // 嵌套列表保持紧列表（无空行）；其余块（代码/段落等）前补空行 → loose list，
      // 与 CommonMark 输入约定一致（mdast `spread` 语义等价）。
      const isList = ["bulletList", "orderedList", "taskList"].includes(block.type ?? "");
      if (!isList) lines.push(EMPTY_LINE);
      const rendered = blockToMarkdown(block, { indent: contentIndent });
      if (isList) {
        // blockToMarkdown 已按 ctx.indent 处理列表标记缩进，无需再缩进。
        lines.push(rendered);
      } else {
        const indented = rendered
          .split("\n")
          .map((ln) => (ln === EMPTY_LINE ? EMPTY_LINE : " ".repeat(contentIndent) + ln))
          .join("\n");
        lines.push(indented);
      }
    }
  });
  return lines.join("\n");
}

/** GFM 表格：| 单元格 | 对齐分隔行 | 数据行 |；单元格内 `|` 转义。 */
function tableToMarkdown(node: JSONContent): string {
  const rows = node.content ?? [];
  if (rows.length === 0) return "";
  const headerCells = rows[0].content ?? [];
  const alignOf = (cell: JSONContent | undefined): string | null => {
    const a = cell?.attrs?.align as string | null | undefined;
    return a && ["left", "right", "center"].includes(a) ? a : null;
  };
  const align = headerCells.map((_cell, i) => {
    // 取该列首个非空 align（导入时逐列一致；Tiptap 手工建表无 align → null）
    for (const row of rows) {
      const cell = (row.content ?? [])[i];
      const a = alignOf(cell);
      if (a) return a;
    }
    return null;
  });
  const cellText = (cell: JSONContent | undefined): string => {
    if (!cell) return "";
    const inline = renderInlines((cell.content ?? []).find((c) => c.type === "paragraph")?.content);
    return inline.replace(/\|/g, "\\|").replace(/\n/g, " ");
  };
  const sep = align.map((a) => (a === "left" ? ":---" : a === "right" ? "---:" : a === "center" ? ":---:" : "---"));
  const line = (cells: JSONContent[]) => `| ${cells.map((c) => cellText(c)).join(" | ")} |`;
  const out = [line(headerCells), `| ${sep.join(" | ")} |`];
  for (const row of rows.slice(1)) out.push(line(row.content ?? []));
  return out.join("\n");
}

/** 文档级入口：块间空行分隔，末尾单个换行。 */
export function tiptapToMarkdown(json: JSONContent): string {
  const blocks = (json.content ?? [])
    .map((b) => blockToMarkdown(b))
    .filter((b) => b !== EMPTY_LINE);
  const body = blocks.join("\n\n");
  return body === "" ? "" : body + "\n";
}
