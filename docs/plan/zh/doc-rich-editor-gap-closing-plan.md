# Doc 栏富文本编辑器差距抹平开发计划（P0/P1/P2）

> 版本：v0.1（草案）| 日期：2026-09-16
>
> 关联分析：[`docs/review/zh/35-doc-rich-editor-gap-analysis-vs-docflow.md`](../../review/zh/35-doc-rich-editor-gap-analysis-vs-docflow.md)（任务 t-eea8dbeb）
> 关联计划：[`docs/plan/zh/doc-dev-plan.md`](./doc-dev-plan.md)（doc 模块总计划；本计划是其编辑器增强补充，不影响 D0~D4 主线）
> 关联 ADR：[`ADR-079`](../../adr/zh/ADR-079-doc-realtime-collab-tiptap-yjs.md)（markdown 权威 P0 会话层；本计划全部能力受此约束）
>
> **一句话**：不换底层富文本库，按 P0（表格行列编辑 UI）→ P1（流程图 mermaid 插入 + NodeView 可视化）→ P2（表格样式美化）三批交付，预估总工期 **5-8 人日**（单人全职 1-1.5 周）。

---

## 0. 结论锚点（来自分析报告，已复核代码）

- 内核命令完备：`@tiptap/extension-table` ^3.31.3 提供 `addRowAfter/Before`、`addColumnAfter/Before`、`deleteRow/deleteColumn/deleteTable`、`toggleHeaderRow/Column/Cell`、`mergeCells/splitCell`、`fixTables`；`RichToolbar.tsx:115` 仅暴露 `insertTable`。
- mermaid 链路已通：`mermaid` ^11.15.0 已入依赖；预览走 `CodeBlock.tsx` → `MermaidBlock.tsx`；源码模式已有 mermaid 插入按钮（`MarkdownToolbar.tsx:150`）。缺的是**富文本模式内的插入 + 可视化**。
- markdown 权威存储（`.md` 落盘）决定约束：任何新节点必须能 `markdown ↔ Tiptap JSON` 往返；GFM 无法表达的（合并/拆分单元格）默认不暴露。
- 已具备 i18n 5 语言（`zh-CN/en/ja/ko/zh-TW`，`doc.*` namespace）与 round-trip 测试契约（`roundtrip.test.ts` 191 行）。

---

## 1. 排期假设

- **团队规模**：单人全职，与 doc-dev-plan 口径一致。
- **工时口径**：1d = 8h，含编码 + 单测 + i18n + 文档同步。
- **前置依赖**：P0 无；P1 无（可与 P0 并行，但建议 P0 先行验证 NodeView 挂载模式）；P2 的 `resizable` 随 P0 顺带完成。
- **并行机会**：P0 与 P1 代码路径独立（表格命令 vs codeBlock NodeView），单人按 P0 → P1 → P2 串行。
- **无新增依赖**：BubbleMenu 来自已装的 `@tiptap/react`；mermaid 已在依赖中。

---

## 2. 里程碑总览

| 阶段 | 内容 | 估时 | 交付物 / 验收入口 |
|------|------|------|-------------------|
| **P0** | 表格行列编辑 UI（TableBubbleMenu） | 1-2d | 光标进表格浮出工具条，加/删行列、表头切换、删表；GFM round-trip 回归绿 |
| **P1** | 流程图（mermaid 插入 + NodeView 渲染） | 3-5d | 工具栏流程图按钮 → 插入 ```mermaid 围栏 → NodeView 可视化渲染，点击进编辑态 |
| **P2** | 表格样式美化 | ≤1d | prose table token（表头底色/斑马纹/边框/圆角/dark 适配）+ 列宽拖拽 |

---

## 3. 任务分解

### 3.1 P0 表格行列编辑 UI（1-2d）

| # | 任务 | 影响文件 | 说明 |
|---|------|---------|------|
| P0.1 | 新增 `TableBubbleMenu` 组件 | 新建 `src/components/doc/editor/TableBubbleMenu.tsx` | 用 `@tiptap/react` 的 `BubbleMenu`；`shouldShow` 判 `editor.isActive('table')` 或 `CellSelection`；按钮：上行加行 / 下行加行 / 左列加列 / 右列加列 / 删行 / 删列 / 表头行/列切换 / 删除表格；全部走 `editor.chain()` |
| P0.2 | 挂载到富文本编辑器 | `src/components/doc/editor/DocRichEditor.tsx` | 在 `EditorContent` 外挂 `<TableBubbleMenu editor={editor} />`；`readOnly` 时隐藏 |
| P0.3 | TableKit 启用列宽拖拽 | `src/components/doc/editor/extension-kit.ts` | `TableKit.configure({ resizable: true })`（对齐 DocFlow） |
| P0.4 | i18n keys | `src/i18n/locales/{zh-CN,en,ja,ko,zh-TW}.json` | `doc.tbAddRowAfter/Before`、`tbAddColBefore/After`、`tbDeleteRow/Col/Table`、`tbToggleHeaderRow/Col` 等 |
| P0.5 | 测试 | `DocRichEditor.test.tsx`、`roundtrip.test.ts` | 见 §5 |

**验收标准**：
- 表格内光标/选区浮出工具条；每次操作后可 Ctrl+Z 撤销（undo/redo 栈）。
- 加行/删行/加列/删列/表头切换后保存为 markdown，仍是合法 GFM 表格（`roundtrip.test.ts` 增补用例绿）。
- 只读模式不显示工具条；i18n 5 语言无缺 key。

**明确不做（YAGNI）**：`mergeCells` / `splitCell`（GFM lossy，用户未提）；合并降级提示弹窗（分析报告 §4.1 建议前者）。

### 3.2 P1 流程图（3-5d）

| # | 任务 | 影响文件 | 说明 |
|---|------|---------|------|
| P1.1 | 抽离 mermaid 渲染逻辑 | 新建 `src/components/doc/editor/mermaidRenderer.ts`（或直接复用导出） | 从 `src/components/chat/MermaidBlock.tsx` 抽离 `mermaid.initialize` + 渲染 + 尺寸自适应 + `isPlausibleMermaid` 守卫为共享模块，chat 侧改为引用（**不改 chat 行为**，仅消除复制） |
| P1.2 | 新增 `MermaidNodeView` | 新建 `src/components/doc/editor/MermaidNodeView.tsx` | 渲染态 = 复用共享渲染（SVG 图 + 尺寸自适应）；编辑态 = 点击/双击切换轻量 textarea 代码编辑（不引 codeMirror）；保留 codeBlock 数据模型，**不新增节点类型** |
| P1.3 | codeBlock 挂 NodeView | `src/components/doc/editor/extension-kit.ts` | StarterKit 的 codeBlock 裁剪后配置 `addNodeView`（仅 `language === 'mermaid'` 时路由到 `MermaidNodeView`，其余走默认渲染，避免影响普通代码块） |
| P1.4 | 工具栏流程图按钮 | `src/components/doc/editor/RichToolbar.tsx` | 新增按钮 → `chain().insertContent('```mermaid\n...\n```')` 或插入 codeBlock(language=mermaid)；i18n key |
| P1.5 | 测试 | `roundtrip.test.ts`（回归）、`DocRichEditor.test.tsx` | 见 §5 |

**验收标准**：
- 富文本工具栏点「流程图」→ 出现 mermaid 代码块，默认渲染为图（含合法 mermaid 语法示例）。
- 点击图进入编辑态，改代码后退出编辑态重新渲染；无效 mermaid 显示错误占位（复用 MermaidBlock 守卫）。
- 保存的 markdown 与源码模式插入的 ```mermaid 围栏完全一致，round-trip 不变（NodeView 不改变序列化）。
- 只读模式 NodeView 只读（不进入编辑态）。

**路径决策**：采纳分析报告方案 A（NodeView 内嵌渲染）；不做方案 B（Tiptap 内实时 SVG + 侧栏源码）；不碰方案 C（tldraw / LogicFlow / AntV X6 画布，P3+ 单独立项）。

### 3.3 P2 表格样式美化（≤1d）

| # | 任务 | 影响文件 | 说明 |
|---|------|---------|------|
| P2.1 | prose table 样式 token | `src/styles/*`（prose 表样式所在处） | 表头底色、边框色、斑马纹、单元格内边距、圆角；dark 主题适配 |
| P2.2 | 列宽拖拽收尾 | `extension-kit.ts` | `resizable: true` 已在 P0.3 完成；P2 验证样式联动 |

**验收标准**：表格在 light/dark 主题下样式统一、可读；列宽可拖拽；不影响 round-trip（纯 CSS）。

---

## 4. 横切约束（每阶段都要守）

1. **所有新 UI 操作走 `editor.chain()`**（进 undo/redo 栈）；组件不持有文档状态（对齐 `RichToolbar.tsx` 现状）。
2. **schema 变更必须同步 `roundtrip.test.ts` 契约**（`extension-kit.ts` 头部注释明示）。P0/P1 均**不新增节点类型**、不改变序列化 → round-trip 契约理论上零变更，测试只增回归用例。
3. **懒加载边界**：`extension-kit.ts`、`DocRichEditor.tsx` 只能被 `DocEditor` 动态 `import()` 引入；新增的 `TableBubbleMenu`/`MermaidNodeView` 属该模块内部，不得被首屏路径静态引用。
4. **不换库、不加依赖**：BubbleMenu 用 `@tiptap/react` 自带组件；mermaid 已存在；i18n 用现用 `useTranslation`。
5. **只读联动**：`readOnly` 时隐藏 TableBubbleMenu、NodeView 不可进入编辑态（对齐现有 `setEditable()` 热切换）。

---

## 5. 测试策略

| 层 | 位置 | 内容 |
|----|------|------|
| round-trip 回归 | `src/lib/markdown/roundtrip.test.ts` | 增补：加行/删行/加列/删列后的表格 → markdown 往返一致；mermaid 围栏往返（已有 92 行用例，补 NodeView 后回归确认不变） |
| 组件行为 | `src/components/doc/editor/DocRichEditor.test.tsx` | 增补：插入表格 → 触发各行列命令 → 断言输出 markdown 的行列数变化；NodeView 渲染态/编辑态切换（jsdom 内 mermaid 真实 SVG 渲染不可靠，断言「渲染态容器存在 + 编辑态 textarea 出现」，SVG 视觉效果走手动验收） |
| 手动验收 | 桌面应用 | 见 §6 验收清单 |

---

## 6. 风险与待确认

| 风险/待确认 | 影响 | 缓解 |
|------------|------|------|
| NodeView 与只读/外部 setContent 同步（P1） | 中 | P0 先合入验证扩展挂载链路；NodeView 不持有状态、仅渲染 props 传入的 node，规避同步问题 |
| mermaid 大图/长代码性能 | 低 | 复用 MermaidBlock 既有 debounce + 尺寸自适应；异常代码走 `isPlausibleMermaid` 守卫降级 |
| DocFlow「流程图」真实形态未确认（AI 生成 mermaid vs 画布） | 低 | 分析报告 §6.1：即使 DocFlow 是画布，本计划方案 A 在「人可编辑」上仍成立，不阻塞 |
| doc 服务图片上传端点（P2+ 图片） | 不阻塞本计划 | 沿用分析报告 §6.4，P2+ 单独评估 |
| 合并/拆分单元格（产品强需时） | 低 | 本次不暴露；需产品决策降级策略后另行排期 |

---

## 7. YAGNI 后置清单（本计划不做）

- 合并/拆分单元格、单元格对齐 UI（GFM lossy / 未提）
- 斜杠命令、文本样式（字号/字体/颜色/对齐）、查找替换、TOC、折叠块、块拖拽、图片上传（分析报告 §3：P2/P3，独立评估，不随本计划）
- 数学公式（KaTeX，P3 垂直场景）
- 实时协作（ADR-079 P1 轨道，gate ADR-076，独立排期）
- 专业画布引擎（方案 C，P3+ 单独立项）
- 导出 docx/pdf（用户未提）

---

## 8. 排期起点建议

1. P0.3（resizable）+ P0.1 + P0.2 一次性合入，先解锁「表格可加/删行列」强痛点（第 1-2 天）。
2. P1 以 P1.1（抽离渲染）起步，P1.2/P1.3 联调 NodeView，最后 P1.4 按钮 + 回归（第 3-7 天）。
3. P2 样式与 P1 并行或插队，≤1 天收尾。
