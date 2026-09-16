/**
 * Markdown ↔ Tiptap JSON 双向转换（ADR-079 D5）。
 *
 * - `markdownToTiptapJSON`：.md（GFM）→ Tiptap JSONContent（读取链路）
 * - `tiptapToMarkdown`：Tiptap JSONContent → .md（GFM）（保存链路）
 *
 * round-trip 契约（`.md` 是权威存储）：md → json → md 对常见 GFM 输入
 * 应字节稳定；语义（mdast AST）必须稳定。见 `roundtrip.test.ts`。
 */

export { markdownToTiptapJSON } from "./markdown-to-tiptap";
export { tiptapToMarkdown } from "./tiptap-to-markdown";
