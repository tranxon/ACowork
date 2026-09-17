/**
 * Markdown ↔ Tiptap JSON round-trip 回归测试（ADR-079 D5 / §7 风险：转换保真度）。
 *
 * 契约（`.md` 是权威存储，见 ADR-079 D3 P0 会话层）：
 * 1. **字节稳定**：md → json → md 对常见 GFM 输入逐字节一致（保存不产生
 *    diff 噪音 —— 这是「未改动文档不被重写」的硬约束）。
 * 2. **语义稳定**：对无法字节稳定的输入（含转义/规范化），mdast AST 必须等价
 *    （round-trip 后语义不变）。
 *
 * 已知-lossy（显式列出，不允许静默扩大）：
 * - highlight mark（无 GFM 语法）→ 行内 `<mark>`，再导入变纯文本
 * - underline mark → 纯文本
 * - 块级/行内 HTML → 段落文本
 */

import { describe, expect, it } from "vitest";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { gfm } from "micromark-extension-gfm";
import type { Root } from "mdast";
import { markdownToTiptapJSON, tiptapToMarkdown } from "./index";

/** 剥离 mdast position 字段，比较纯语义 AST。 */
function ast(md: string): unknown {
  const tree = fromMarkdown(md, {
    extensions: [gfm()],
    mdastExtensions: [gfmFromMarkdown()],
  }) as Root;
  const strip = (n: unknown): unknown => {
    if (Array.isArray(n)) return n.map(strip);
    if (n && typeof n === "object") {
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(n as Record<string, unknown>)) {
        if (k === "position") continue;
        out[k] = strip(v);
      }
      return out;
    }
    return n;
  };
  return strip(tree);
}

function roundtrip(md: string): string {
  return tiptapToMarkdown(markdownToTiptapJSON(md));
}

/** 归一化输入尾部空白：转换契约输出总是以单个 `\n` 结尾（POSIX）。 */
function withEol(md: string): string {
  return md === "" ? "" : md.replace(/\s*$/, "") + "\n";
}

/** 字节稳定用例：roundtrip 输出必须与输入完全一致。 */
const BYTE_STABLE_CASES: Array<[string, string]> = [
  ["空文档", ""],
  ["纯文本", "hello world"],
  ["单标题", "# 标题"],
  ["多级标题", "# H1\n\n## H2\n\n### H3\n\n#### H4\n\n##### H5\n\n###### H6"],
  ["标题+段落", "# 标题\n\n这是一段正文。\n\n另一段。"],
  ["加粗斜体删除线", "**bold** and *italic* and ~~strike~~ and `code`"],
  ["相邻强调", "**a** **b** *c* *d*"],
  ["链接", "[文档](https://example.com) 和 [带标题](https://x.io \"title\")"],
  ["图片", "![logo](./assets/logo.png)"],
  ["图片标题", "![alt](./img/a.png \"截图\")"],
  ["链接包图片", "[![架构](./arch.png)](./arch.md)"],
  ["无序列表", "- a\n- b\n- c"],
  ["有序列表", "1. one\n2. two\n3. three"],
  ["有序非 1 起始", "3. three\n4. four"],
  ["嵌套列表（2 空格）", "- a\n  - a1\n  - a2\n- b"],
  ["有序嵌套无序", "1. one\n   - n1\n   - n2\n2. two"],
  ["任务列表", "- [x] 已完成\n- [ ] 待办"],
  ["引用", "> 引用第一行\n> 引用第二行"],
  ["多段引用", "> 段一\n>\n> 段二"],
  ["代码块带语言", "```rust\nfn main() {\n    println!(\"hi\");\n}\n```"],
  ["代码块无语言", "```\nplain code\n```"],
  ["表格", "| a | b |\n| --- | --- |\n| 1 | 2 |"],
  ["表格对齐", "| left | center | right |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |"],
  ["表格单行", "| h |\n| --- |\n| v |"],
  ["表格3行3列（行列命令产物）", "| a | b | c |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |"],
  ["分割线", "---"],
  ["分割线上下文", "# t\n\n---\n\n正文"],
  ["换行（soft break）", "第一行\n第二行"],
  ["中文混合", "# 项目计划\n\n- 阶段一：调研\n- 阶段二：实现\n\n> 注意：保持向后兼容"],
  // 状态机：bold 跨 text+code 运行（真实 ADR 文档模式 `**废止其 `0 = 无限制`**`）
  ["粗体含代码", "**废止其 `0 = 无限制`**，见 §6"],
  ["粗体含代码与文本", "**a `code` b** 和 **c**"],
  ["链接内粗体", "[**链接内粗体**](https://x.com) 与 [文本](https://y.com)"],
  ["粗体链接代码嵌套", "**`code` in bold**"],
];

/** 语义稳定用例：roundtrip 后 mdast AST 必须等价（字节允许规范化）。 */
const SEMANTIC_CASES: Array<[string, string]> = [
  ["mermaid 围栏", "```mermaid\ngraph TD\n  A --> B\n```"],
  ["列表内代码块", "- item\n\n  ```sh\n  echo hi\n  ```"],
  ["行内代码含反引号", "`` `x` `` 原样"],
  ["特殊字符转义", "3.14 * 2 不是强调"],
  ["下划线边界", "_开头 与 结尾_ 与 a_b"],
  ["行首井号文本", "\\# 不是标题"],
  ["行首列表符文本", "\\- 不是列表"],
  ["表格内管道转义", "| a | b |\n| --- | --- |\n| x \\| y | z |"],
  ["空行多余", "# t\n\n\n\n正文\n\n\n"],
  ["列表连续段落", "- a\n\n  b 是续段\n- c"],
  ["引用内列表", "> 说明：\n>\n> - 一\n> - 二"],
  ["表格内行内格式", "| 名称 | 说明 |\n| --- | --- |\n| **粗体** | `code` 与 [链接](https://x.com) |"],
  // 近似真实 ADR 文档形态的复合用例
  [
    "复合真实文档",
    [
      "# ADR-079：文档实时协作编辑器（Tiptap + Yjs）",
      "",
      "**状态**：提议",
      "",
      "## 1. 背景与目标",
      "",
      "- 多人实时协作编辑（当前是「写后冲突、手动刷新」）",
      "- 富文本（所见即所得）编辑体验",
      "- 本地优先 / 离线缓存",
      "",
      "### 1.1 业务诉求",
      "",
      "> 用户希望参考 DocFlow 的 Tiptap + Yjs 实现。",
      "",
      "```mermaid",
      "graph TD",
      "    A[Input] --> B[Process]",
      "    B --> C[Output]",
      "```",
      "",
      "| 协作能力 | 依赖 ADR-076 的哪一部分 | 说明 |",
      "| --- | --- | --- |",
      "| 在线用户 / 实时光标归属 | `UserAccount` + token payload | awareness 里广播真实 `user_id` |",
      "| WS 认证 | Phase 2 `/api/auth/*` | y-websocket 连接携带 access_token |",
      "",
      "## 2. 现状分析",
      "",
      "- [x] P0：编辑器替换（单用户）",
      "- [ ] P1：实时协作（待 ADR-076）",
      "",
      "![架构图](./assets/arch.png \"目标架构\")",
      "",
    ].join("\n"),
  ],
];

/** 已知-lossy 契约：内容保留、格式降级（不允许静默丢内容）。 */
const KNOWN_LOSSY_CASES: Array<[string, string, string]> = [
  // [name, input, roundtrip 输出必须包含的内容片段]
  ["块级 HTML 降级为段落", "<div class=\"note\">注意</div>", "注意"],
  ["行内 HTML 保留原文", "请按 <kbd>Ctrl</kbd> + <kbd>S</kbd> 保存", "请按"],
];

describe("markdown ↔ tiptap 转换", () => {
  it("字节稳定：md → json → md 与输入一致", () => {
    for (const [name, md] of BYTE_STABLE_CASES) {
      expect(roundtrip(md), name).toBe(withEol(md));
    }
  });

  it("语义稳定：mdast AST 等价", () => {
    for (const [name, md] of SEMANTIC_CASES) {
      expect(ast(roundtrip(md)), name).toEqual(ast(md));
    }
  });

  it("幂等：roundtrip 后再次 roundtrip 不变", () => {
    for (const [name, md] of [...BYTE_STABLE_CASES, ...SEMANTIC_CASES]) {
      const once = roundtrip(md);
      expect(roundtrip(once), name).toBe(once);
    }
  });

  it("已知-lossy：内容保留（格式降级可接受，静默丢内容不可接受）", () => {
    for (const [name, md, mustContain] of KNOWN_LOSSY_CASES) {
      const out = roundtrip(md);
      expect(out, name).toContain(mustContain);
    }
  });

  it("空输入 → 空文档（单空段落）", () => {
    expect(markdownToTiptapJSON("")).toEqual({
      type: "doc",
      content: [{ type: "paragraph" }],
    });
  });

  it("空白输入 → 空文档", () => {
    expect(markdownToTiptapJSON("   \n  ")).toEqual({
      type: "doc",
      content: [{ type: "paragraph" }],
    });
  });
});
