# Doc 栏富文本编辑器功能差距分析（vs DocFlow）与抹平方案

**任务**：t-eea8dbeb（Doc栏 rich text 编辑器功能差距分析）
**作者**：Architect
**日期**：2026-09-16
**性质**：分析报告（非实现）。结论：**不需要换底层富文本库**，两项用户痛点（表格行列编辑 / 流程图）均可基于现有 Tiptap 3 内核抹平。

---

## 0. 执行摘要

| 用户痛点 | 结论 | 路径 | 成本 |
|---|---|---|---|
| 表格无法加/删行列 | **内核（@tiptap/extension-table v3）命令完备，纯 UI 未暴露** | 新增表格浮动工具条（TableBubbleMenu），调用既有命令 | 低（约 1–2 人日） |
| 流程图无法插入/编辑 | **无需换库**；markdown 权威存储决定了「mermaid 代码块 + 可视化渲染」是最优路径 | Tiptap NodeView 复用 chat 侧 MermaidBlock 渲染；工具栏加插入按钮；Monaco 模式已有 mermaid 按钮 | 中（约 3–5 人日） |
| 表格样式丑 | 样式 token / 主题层问题，与功能补齐解耦 | prose table 样式 token + TableKit `resizable` | 低（独立 P2） |

关键背景：
- 我们与 DocFlow **同为 Tiptap 3 内核**（DocFlow `@tiptap/core ^3.17.1`，我们 `^3.31.3`，同源同命令集）。
- 我们是 **markdown 权威存储**（`.md` 落盘 + PUT/base_version），DocFlow 是 **Yjs 权威 + Hocuspocus 持久化**。这是所有能力取舍的底层约束：任何新节点必须能往返 `markdown ↔ Tiptap JSON` 或明确标注 lossy。
- 实时协作（Yjs）已由 [ADR-079](../adr/zh/ADR-079-doc-realtime-collab-tiptap-yjs.md) 规划为 P1（gate 在 ADR-076 多用户账号之后），不在本次差距分析范围内（协作能力差异单列，见矩阵）。

---

## 1. 对标 DocFlow：能力清单（源码/官方 README 确认）

DocFlow（`D:\projects\tranxon\DocFlow`，Tiptap 3 + Yjs + Hocuspocus + Next.js）扩展集见 `apps/DocFlow/src/extensions/extension-kit.ts`，斜杠命令见 `extensions/SlashCommand/groups.ts`，菜单见 `components/menus/TextMenu/*`、`ContentItemMenu/*`。

| # | 能力 | DocFlow 实现（源码证据） | 核心卖点 / 高频 |
|---|---|---|---|
| 1 | 实时协作编辑 | `@tiptap/extension-collaboration` + `-caret`，HocuspocusProvider（extension-kit.ts:1-3,336） | **核心卖点**（README 明示） |
| 2 | 块级结构 + 拖拽 | DraggableBlock / DragHandler / ContentItemMenu（块手柄：复制/删除/重复/插入） | **核心卖点**（README：20+ 内容类型、拖拽调序） |
| 3 | AI 辅助编辑 | AgentSuggestion（track-changes 式标记）+ AgentEditPanel（意图识别→定位→生成→插入模式）+ ContinueWriting/StreamAgent | **核心卖点**（README：头脑风暴/润色/续写） |
| 4 | 表格 | TableKit（`resizable: true`）+ SlashCommand 插入 3×3 | 高频（但**未见显式行列增删 UI 命令**，见 §4 待确认） |
| 5 | 图片上传/块级图/表格内图 | ImageUpload（上传服务）/ ImageBlock + Figcaption / TableImage / FileHandler（拖放+粘贴，base64 预览→后台上传） | 高频 |
| 6 | 斜杠命令 | SlashCommand（format 组 + insert 组：标题/列表/任务/折叠/引用/代码块/表格/数学/分割线/TOC/视频） | 高频 |
| 7 | 查找替换 | SearchAndReplace | 高频 |
| 8 | 目录 TOC | TableOfContents + TableOfContentsNode + FloatingToc 侧栏 | 高频 |
| 9 | 历史版本/快照恢复 | HistoryPanel + RestoreSnapshotConfirmDialog + useEditorHistory | 高频 |
| 10 | 文本样式 | TextStyle / FontSize / FontFamily / Color / Highlight(multicolor) / Underline / Subscript / Superscript / TextAlign / Typography | 高频（字体/字号/颜色/对齐） |
| 11 | 数学公式 | Mathematics（KaTeX）+ MathLiveExtension（可视化数学编辑器）+ SlashCommand 插入 | 中频（垂直场景） |
| 12 | 折叠块 | Details / DetailsContent / DetailsSummary | 中频 |
| 13 | 代码块 | 自定义 CodeBlock（SelectOnlyCode + 低亮） | 高频 |
| 14 | 提及 | Mention + mentionSuggestion（协作上下文） | 中频（协作后才有意义） |
| 15 | Emoji | Emoji + emojiSuggestion | 中频 |
| 16 | 视频嵌入 | Youtube 扩展（弹窗输入 URL） | 低频 |
| 17 | 导入导出 | `utils/export-doc/`（docx 库，converters：heading/table/list/task-list/details/code-block 等）+ `utils/document-export/`（docx/pdf） | 高频（导出 docx/pdf） |
| 18 | 粘贴增强 | JsonPaste / MarkdownPaste | 中频 |
| 19 | 其他 | UniqueID（协作稳定 ID）、Focus、ClearMarksOnEnter、TrailingNode、CharacterCount(50000)、Placeholder、Selection | 体验增强 |
| 20 | 流程图（diagram） | **前端无 mermaid 渲染/可视化画布**；仅 `services/ai/type.ts` 定义 `mermaidCode` / `GenerateDiagramParams`（AI 生成 mermaid 代码），**前端使用处未找到** | 待确认（见 §4） |

> 注：README 自述核心特性 = 块级编辑器 + 实时协作 + AI 功能；上表「核心卖点/高频」列按源码投入度 + README 定位推断，仅供参考。

---

## 2. 对标我们当前实现（apps/acowork-desktop）

### 2.1 编辑器内核与扩展集

- 内核：**Tiptap 3**（`@tiptap/core ^3.31.3`，比 DocFlow 的 ^3.17.1 更新）。
- 扩展集（`src/components/doc/editor/extension-kit.ts`）：StarterKit（裁剪，标题1–6 + link 自动）、Placeholder、CharacterCount（50_000 上限）、TaskList/TaskItem(nested)、**TableKit**、Highlight(multicolor)、Image（相对路径，不解析资源）。
- 明确与 `src/lib/markdown/*` round-trip 契约对齐（`roundtrip.test.ts` 覆盖：标题/列表/表格/任务列表/高亮/mermaid 围栏等）。
- 未启用：CodeBlock 语言选择 UI、字体/字号/颜色/对齐、折叠块、数学、协作、SlashCommand、图片上传、拖拽块、TOC、查找替换、导出 docx/pdf。

### 2.2 编辑体验（三层）

| 层 | 实现 | 能力 |
|---|---|---|
| 富文本编辑（默认） | `DocRichEditor.tsx`（懒加载）+ `RichToolbar.tsx` | B/I/删除线/行内码/代码块/引用/有序/无序/任务列表/**表格(仅插入 3×3)**/链接/图片(相对路径)/分割线/撤销/重做 |
| 源码编辑（保留） | Monaco + `MarkdownToolbar.tsx` | markdown 全量；**已有 mermaid 插入按钮**（`tbMermaid`，插入 ```mermaid 围栏）+ 表格插入 picker |
| 预览 | `DocMarkdownView.tsx`（react-markdown + remark-gfm + rehype-raw + CodeBlock） | **```mermaid 已可渲染为图**（CodeBlock 路由到 chat 的 `MermaidBlock.tsx`） |

### 2.3 已具备但未暴露/未接入（内核支持或链路存在）

| 能力 | 现状 | 证据 |
|---|---|---|
| 表格行列增删/合并/表头 | **内核命令全部存在**，UI 只暴露了 `insertTable` | `node_modules/@tiptap/extension-table/dist/index.js`：`addRowAfter/Before`、`addColumnAfter/Before`、`deleteRow/deleteColumn/deleteTable`、`mergeCells/splitCell`、`toggleHeaderRow/Column/Cell`、`fixTables`；`RichToolbar.tsx:115` 仅 `insertTable({rows:3, cols:3})` |
| 表格列宽拖拽 | TableKit 默认 `resizable` 可配置（DocFlow 开了，我们没配） | `extension-kit.ts` TableKit 无 configure |
| mermaid 渲染 | 预览链路已通；富文本内无插入/可视化 | `CodeBlock.tsx:115` → `MermaidBlock`；`roundtrip.test.ts:92` mermaid 围栏往返 |
| 代码块语言 | round-trip 保留 `language` 属性，但富文本无语言选择 UI | `markdown-to-tiptap.ts:128` |
| 图片上传 | 仅相对路径 Image，无上传/拖放/粘贴 | `extension-kit.ts` Image |

---

## 3. 差距矩阵（能力 × 差距 × 抹平成本 × 风险）

> 成本：S=≤1人日，M=3–5人日，L=≥2周。风险：换库/格式破坏 = 高；纯 UI = 低。

| DocFlow 能力 | 我们的差距 | 抹平成本 | 风险 | 备注/优先级 |
|---|---|---|---|---|
| 表格（行列增删 UI） | 内核同源，缺 UI | **S** | 低 | **P0**（用户强痛点；纯 UI） |
| 表格样式 | 丑（prose 默认边框/无斑马纹/无列宽拖拽） | S | 低 | **P2**（样式 token 层，与功能解耦） |
| 流程图（diagram） | 富文本内不可插入/不可可视化编辑（源码+预览链路已有 mermaid） | **M** | 中（NodeView + round-trip 需保 mermaid 围栏） | **P1**；无需换库 |
| 图片上传/块级图/表格内图 | 仅相对路径图片，无上传服务/拖放粘贴 | M–L | 中（需要资源存储服务；当前 doc 服务无上传端点，待确认） | P2+（依赖 doc 服务端能力） |
| 斜杠命令 | 无 | M | 低 | P2（体验增强，可与表格菜单同批） |
| 文本样式（字体/字号/颜色/对齐/上下标） | 无（部分 round-trip 无法表达：字号/字体/颜色 → lossy） | M | 中（markdown 无法保真，需定义 lossy 策略） | P2（样式类合并评估） |
| 数学公式 | 无 | L | 高（markdown 无标准数学语法保真；需 KaTeX + 自定义序列化） | P3（垂直场景，先不动） |
| 实时协作 | 无（ADR-079 P1 规划，Yjs 后端自研） | L | 高 | 独立轨道：gate ADR-076，**不在本任务** |
| 历史版本/快照恢复 | 已有 base_version + 409 冲突提示，无协作快照 UI | M | 低 | P2+（随协作轨道） |
| 查找替换 | 无 | M | 低 | P2 |
| 目录 TOC | 无 | M | 中（markdown 标题可推导，但插入节点需 round-trip） | P2 |
| 折叠块 Details | 无（GFM 无对应语法 → lossy） | M | 中 | P2（需定义序列化约定，如 HTML details） |
| 块拖拽/块手柄 | 无 | M | 低 | P2 |
| 提及/Emoji/视频 | 无 | M | 低–中 | P3（协作后价值才大） |
| 导出 docx/pdf | 无 | L | 中 | P3（用户未提，暂缓） |

---

## 4. 两项重点问题的抹平路径（直接回答任务问题）

### 4.1 表格行列编辑 —— P0，无需换库，纯 UI 补齐

**结论**：差距**可以抹平**，路径 = 暴露 Tiptap TableKit 既有命令，**不需要换底层富文本库**。

- 内核证据：`@tiptap/extension-table` v3 提供完整命令（§2.3 列表）；GFM 表格 round-trip 已实现（`markdown-to-tiptap.ts:238` tableToJSON / `tiptap-to-markdown.ts:343` tableToMarkdown），加行/删行/加列/删列/表头切换后输出仍是合法 GFM 表格。
- 实现形态：**表格浮动工具条（TableBubbleMenu）**——光标进入表格（`editor.isActive('table')`）或单元格选区（`CellSelection`）时浮出：上行加行 / 下行加行 / 左列加列 / 右列加列 / 删行 / 删列 / 表头行/列切换 / 删除表格。
- **架构决策点（重要）**：
  - `mergeCells` / `splitCell`（合并/拆分单元格）**GFM 无法表达**，round-trip 会漂移。P0 **不暴露合并/拆分**，或在触发时提示「保存为 markdown 后会降级为独立单元格」——建议前者（YAGNI，用户未提）。
  - `toggleHeaderRow/Column` GFM 可表达（首行列即为表头），可安全暴露。
  - 单元格对齐（`align`）round-trip 已支持，无需处理。
- DocFlow 侧参考：DocFlow TableKit 只配了 `resizable: true`，**前端源码未发现行列增删命令调用**（§4.3 待确认）。即「行不能加」在 DocFlow 大概率同样受限——我们的 P0 是**超越对标产品的加分项**。

### 4.2 流程图 —— P1，无需换库，mermaid 代码块 + NodeView 可视化

**结论**：差距**可以抹平**（达到「插入 + 可编辑 + 所见即所得预览」），**不需要换底层富文本库**；但**不做拖拽式可视化画布**（那是换数据模型的远期选项）。

- 现状链路：Monaco 模式已有 mermaid 按钮（`MarkdownToolbar.tsx:150`）→ 插入 ```mermaid 围栏 → round-trip 保真（`roundtrip.test.ts:92`）→ 预览渲染（`CodeBlock.tsx` → `MermaidBlock`）。**缺的是富文本模式内的插入与可视化**。
- 推荐路径（A：NodeView 内嵌渲染）：
  1. 工具栏加「流程图」按钮 → `insertContent('```mermaid\n...\n```')` 或新建 codeBlock(language=mermaid)。
  2. 为 mermaid 代码块挂 **NodeView**（`MermaidNodeView`）：渲染态 = 复用/抽离 chat 的 `MermaidBlock` 渲染逻辑（mermaid.initialize + render + 尺寸自适应，`MermaidBlock.tsx` 已有现成实现）；编辑态 = 点击切换为代码编辑（textarea/codeMirror 轻量）或双击进入源码。
  3. 保持 markdown round-trip 不变（NodeView 不改变节点数据模型，仍是 codeBlock + language）。
- 备选路径（B：Tiptap 内实时 SVG 预览 + 侧栏源码编辑）：体验类似 Typora 的图模式，成本略高，收益有限，不建议先行。
- 远期选项（C：专业画布引擎 tldraw / LogicFlow / AntV X6）：可视化拖拽节点、连线；**与 markdown 权威存储冲突**（画布数据需自定义序列化为 mermaid/DSL，round-trip 复杂度高、丢失风险大）。评估结论：**P3+ 单独立项**，本次不碰，不换库。
- DocFlow 侧参考：DocFlow 前端**没有** mermaid 渲染/画布；仅 AI 服务类型定义了 `mermaidCode`（AI 生成图），前端使用处未找到（待确认）。若 DocFlow 的「流程图」= AI 生成 mermaid 代码，则我们的方案 A 在「人可编辑」上**强于 DocFlow**。

### 4.3 表格样式丑 —— P2，样式 token / 主题层

- 归类为**样式层问题，与功能补齐解耦**（任务原话）。
- 内容：prose table 样式 token（表头底色、边框色、斑马纹、单元格内边距、圆角、dark 主题适配）+ TableKit `resizable: true`（列宽拖拽，DocFlow 同款）。
- 不动内核、不动数据模型，风险低。

---

## 5. 建议实现优先级与影响文件清单

| 优先级 | 事项 | 影响文件（apps/acowork-desktop） | 备注 |
|---|---|---|---|
| **P0** | 表格行列编辑 UI | 新增 `src/components/doc/editor/TableBubbleMenu.tsx`；修改 `src/components/doc/editor/DocRichEditor.tsx`（挂载菜单）；修改 `src/components/doc/editor/extension-kit.ts`（TableKit.configure({ resizable: true })）；i18n `zh-CN/en/ja/ko/zh-TW.json`（tbAddRowAfter 等 key）；测试 `src/components/doc/editor/DocRichEditor.test.tsx`、`src/lib/markdown/roundtrip.test.ts` | 纯 UI；命令内核已有；约 1–2 人日 |
| **P1** | 流程图（mermaid 插入 + NodeView 渲染） | 新增 `src/components/doc/editor/MermaidNodeView.tsx`（抽离/复用 `src/components/chat/MermaidBlock.tsx` 渲染逻辑）；修改 `extension-kit.ts`（codeBlock NodeView）；修改 `RichToolbar.tsx`（流程图按钮）；i18n keys；测试 | 保持 codeBlock 数据模型，round-trip 不变；约 3–5 人日 |
| **P2** | 表格样式美化 | `src/styles/*`（prose table token）、`extension-kit.ts`（resizable）、dark 主题 | 独立交付，可随时插队 |

**约束/前提**：
- 所有新 UI 操作必须走 `editor.chain()`（进 undo/redo 栈），组件不持有文档状态（对齐 `RichToolbar.tsx` 现状）。
- 修改 schema 需同步 `roundtrip.test.ts` 契约（`extension-kit.ts` 头部注释明示）。
- 若 P0/P1 涉及换库或 schema 级变更，先同步方案再动手（本次结论为都不涉及）。

---

## 6. 待确认项 / 开放问题

1. **DocFlow 表格行列增删 UI**：前端源码未发现 `addRowAfter/deleteRow` 等命令调用，仅 `resizable: true`。DocFlow 实际产品中表格行列操作交互未知（可能依赖 Tiptap 默认 Tab 移动行为、右键、或产品侧未实现）→ **待确认**（不影响我们的 P0 决策，我们的内核命令是确定的）。
2. **DocFlow 流程图**：`services/ai/type.ts` 定义了 `mermaidCode` / `GenerateDiagramParams`，但前端使用处未找到 → **待确认** DocFlow 的「流程图」是否就是 AI 生成 mermaid 代码块、是否已上线。
3. **markdown 权威 vs Yjs 权威**：我们是 markdown 落盘 + Tiptap 会话层（ADR-079 D3 P0 态），DocFlow 是 Yjs 权威。本报告所有 lossy 判断都基于此；若未来 ADR-079 演进到 P2（Yjs 权威），表格合并/折叠块等能力可放开，届时需重新评估。
4. **图片上传**：当前 doc 服务（`core/acowork-doc`）是否有上传端点/静态资源服务 → 影响 P2+ 图片上传方案（待确认，本次不阻塞）。
5. **合并/拆分单元格**：本次 P0 建议不暴露（GFM lossy）；若产品强需，需定义降级策略（如导出 markdown 时拆分为独立单元格并警告）。

---

*参考：我们的 `extension-kit.ts` / `RichToolbar.tsx` / `DocRichEditor.tsx` / `DocMarkdownView.tsx` / `lib/markdown/*`；DocFlow `apps/DocFlow/src/extensions/extension-kit.ts` / `extensions/SlashCommand/groups.ts` / `components/menus/*` / `utils/export-doc/*` / README；ADR-079。*
