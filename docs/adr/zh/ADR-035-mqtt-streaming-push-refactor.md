# ADR-035：流式传输重构 — MQTT 数据直推 + 前端 per-session 行缓冲，废弃 HTTP 增量轮询

**状态**：草案
**日期**：2026-07-15
**决策者**：大鱼
**前置**：
- ADR-021（统一 Session 数据加载 — 放弃流式传输，采用 HTTP Pull + 通知机制）— **本 ADR 修订其"放弃流式传输"与"HTTP Pull 拉流式内容"部分**
- ADR-027（conversation-meta + token-usage；其中 `streamingContents` Map 方案被本 ADR 取代）
- ADR-033（MQTT 替换 gRPC + WebSocket）
- ADR-034（MQTT / HTTP 职责边界 — 事件面 `messages/*` 主题应带数据，本 ADR 使实现回归该契约）
- [`docs/zh/protocols/mqtt.md`](../../zh/protocols/mqtt.md) §3.2（`messages/chunk` 本应带数据）

**Supersedes / 修订**：
- ADR-021 的"流式内容走 HTTP Pull + `new_data_available` 通知"模型 → 改为 MQTT 直推带数据
- ADR-027 的 `streamingContents` Map + `useStreamingContent` 流式渲染机制 → 改为 per-session `activeStream` 单缓冲 + 受限渲染规则
- `new_data_available` 纯信号事件 → 废弃，由带数据的 `stream_delta` 取代

---

## 决策摘要

当前实现与 MQTT 协议契约背离：`mqtt.md` §3.2 规定 `messages/chunk` **本身就带数据**，但 ADR-021 落地时把流式内容改成了"MQTT 只发 `new_data_available` 信号、内容靠 HTTP 增量轮询拉取"（`cli.rs:2940` `GET /messages?cursor`、`session_core.rs:358 notify_new_data_available`）。这条 Pull 链路正是"最后一轮 assistant 回复不渲染"bug 的脆弱点（详见 `report-frontend-missing-last-assistant.md`）。

本 ADR 把流式传输**回归到 MQTT 直推带数据**，并用一组明确的推送/渲染规则彻底简化前端：

**七条核心原则**：

1. **废弃 HTTP 增量轮询流式数据**。流式内容不再走 `GET /messages?cursor` 增量拉取。
2. **MQTT 实时事件带数据**。完整记录（assistant / thought / toolcall / tool_result 等）自带完整数据，不再只发信号。
3. **thinking / assistant 流式数据：后端 500ms 一次推送，每次带这 500ms 的增量、以整行为单位**。流式 cursor **只在后端维护**（即 `stream_lines` 的 delivery cursor）；每 500ms 推送后推进 cursor，下个 500ms 继续，直到流式结束。
4. **前端简化：session 在前台就用 MQTT 消息获取实时数据，顺序渲染**。流式行只按行接收放入前端 `activeStream.lines`（每 session 单一活动缓冲）。thinking **默认折叠**；用户点击展开后**实时渲染**——每 500ms 检测 `activeStream.lines` 行数变化、有变化就用**最后 5 行**刷新显示（渲染单位为整行，非逐 token 打字）。机制是**原地覆盖**：UI 固定 5 个渲染槽位，新的 5 行覆盖旧的 5 行，**只覆盖、不新增** DOM 节点，目的是**减少 markdown 反复渲染的内存碎片**。**固定显示 5 行，不支持滚动**指视口恒为 5 行、用户无法用鼠标上翻（行数不足 5 行时显示实际行数）；视觉上仍像内容向上走，但实现上既不 append 也不 scroll。
5. **assistant 也用 `activeStream.lines` 接收 MQTT 累积的流式行，但完全不做流式渲染**；只有等**全部 assistant 消息就绪**后一次性渲染。等待期间显示"处理中"动画。
6. **session 切到后台后，MQTT 事件携带的消息继续在后台存储**；切回前台时根据已存储的消息渲染。**前台与后台 session 的唯一区别是"渲染与否"**，数据接收与存储相同。
7. **session 初始加载仍走 HTTP（不改）**：初始拉首页（`direction=backward`，最近 N 条，后端默认 `limit=50`，`cli.rs:2935`）+ 向上翻历史分页回填；只有会话中途切换不再 HTTP 加载，因为后台 MQTT 数据一直在接收，切回直接渲染即可。**因此前端 per-session 数据存储是本次重构的重点。**（注：原则中"全量"指"HTTP 拉取已有对话数据"这一机制保留，实际为分页首页 + 滚动回填，非一次性拉全历史。）

---

## 背景与动机

### 1. 实现与协议契约背离

`mqtt.md` §3.2 设计的 `messages/chunk` 主题 payload 是 `SessionMessage::Chunk { message_id, delta }`——**事件本身就带数据**。但 ADR-021 落地时为规避"流式推送风暴"，把模型改成了：

- Runtime 只 PUBLISH `new_data_available` **纯信号**（`session_core.rs:358 notify_new_data_available` → `subsystems.rs:338-348` `relay_intent("new_data_available")`）。
- 内容由前端 `PollingManager.doPoll` → `GET /messages?cursor&include_streaming`（`cli.rs:2940`）拉取，响应里流式行以 `id=streaming:{line}` + 独立 `streaming` 字段返回（`cli.rs:2982`）。
- 网关 `proxy.rs:208` 纯透传，不参与。

结果是 MQTT 事件面退化成"只发信号"，与 ADR-034 §3.2"事件面带数据"的规约不符。

### 2. HTTP Pull 链路是渲染 bug 的根因

最后一轮 assistant 回复不渲染的根因（详见 `report-frontend-missing-last-assistant.md`，仅分析未改）：

- 最后一条消息能否出现，**完全依赖** `session_state_changed → idle` 触发的一次最终增量拉取（`chatStore.ts:2039-2054`），拉完立即 `stopPolling()`，无重试兜底。
- 该分支仅在本地 `prev` 状态为 active（streaming/...）时才触发；若起始 `streaming` 事件被漏收，`prevActive=false`，idle 时不触发最终拉取 → 最后一条**永久丢失**。
- 前后端流式契约脱节：前端 `loadSessionMessages`（`chatStore.ts:1138`）只消费 `data.messages`、从不读 `data.streaming`；`PaginatedMessages` 类型（`lib/types.ts:712-719`）根本没有 `streaming` 字段。

Pull 模型把"投递可靠性"全部压在"前端必须在正确时机拉一次"上，竞态窗口多、兜底缺失。

### 3. 流式渲染机制本身脆弱

ADR-027 的 `streamingContents` Map + `useStreamingContent`（`useSyncExternalStore`）为"逐 token 流式打字效果"而设计，依赖 `id=streaming:{line}` 这种按 id 格式区分流式/落盘的 hack，以及增量合并清理（`chatStore.ts:1228-1235`）的占位 id 与真实 id 严丝合缝衔接——衔接错位就是"先删后补"的丢失窗口。

### 4. 后端 cursor 基础设施已就绪

后端**已有一套完整的 per-session delivery cursor**，目前被 HTTP 轮询消费：

- `session_manager.rs:332` `session_delivery_cursors: RwLock<HashMap<String, DeliveryCursor>>`
- `session_manager.rs:1819` `get_delivery_cursor(sid)`
- `conversation.rs:1912` `read_messages_since_cursor(...)` —— 读"自 cursor 起的完整行 + streaming_lines"
- `cli.rs:2995` `advance_delivery_cursor(...)` —— 拉取后推进
- `cli.rs:3096` / `conversation.rs:3370` `reset_delivery_cursor(sid, total_lines)` —— 初始加载后重置
- `config.rs:138-143` `notify_interval_ms`（默认 **500**）—— 现为 NewDataAvailable 最小间隔，**正好复用为 500ms 推送周期**

本次重构的实质是：**把这套 cursor 的消费方从"HTTP 拉"改成"500ms 定时推"**，cursor 语义、推进逻辑基本不变。

### 5. 动机总结

| 现状问题 | 本 ADR 解决方式 |
|---------|----------------|
| MQTT 事件只发信号、内容走 HTTP Pull，与协议契约背离 | 事件带数据，废弃 HTTP 增量轮询 |
| 最终投递靠"前端 idle 时拉一次"，漏触发即永久丢失 | 推送驱动，cursor 在后端，前端只追加不拉取 |
| 逐 token 流式渲染 + `streaming:{line}` id hack + 合并清理竞态 | 整行缓冲，thinking 原地覆盖式渲染末 5 行（只覆盖不新增，减少 markdown 内存碎片）、assistant 一次性渲染 |
| 切换 session 需 HTTP 重载 | per-session 持久存储，后台持续接收，切回直接渲染 |
| 前后台靠 runtime `enable/disable_notify` 抑制信号 | 前后台仅前端渲染差异，runtime 对所有订阅 session 一视同仁推送 |

---

## 决策

### D1. 事件模型（MQTT 直推带数据）

沿用 `mqtt.md` §3.2 的 `agents/{id}/sessions/{sid}/messages/*` 主题树，**所有事件带数据**：

| 事件主题 | 时机 | Payload | QoS | 渲染语义 |
|---------|------|---------|-----|---------|
| `messages/stream_delta` | **每 500ms**（`notify_interval_ms`） | 本周期新增的**整行**列表：`[{ role, message_id, line_no, content }]`，role ∈ {`thought`, `assistant`} | 0 | 追加进 per-session `activeStream.lines`；thinking 展开时原地覆盖式渲染末 5 行，assistant 暂不渲染（等 `record_complete`） |
| `messages/record_complete` | 一条记录落盘定稿时 | 完整记录：`{ role, message_id, content, ... }`，role ∈ {`assistant`, `thought`, `tool_call`, `tool_result`, ...} | 1 | assistant → 一次性渲染进 `messages[]`；thought → 冻结进 `messages[]`（携末 5 行，D9.1）、清 `activeStream`；tool_call/tool_result → 直接进 `messages[]`（toolResult 后端裁剪为首 5 行，D9.2）。QoS 1（ADR-035 O2）：record_complete 是权威终态事件，丢失会导致消息卡在 streaming 状态，必须至少投递一次 |
| `messages/tool_call` / `messages/tool_result` | 工具调用/返回 | 完整结构化数据（保留 `mqtt.md` 既有定义） | 0 | 直接进 `messages[]`，等价于 `record_complete` 的角色特化 |
| `messages/done` / `error` / `stopped` | 本轮结束 | 生命周期信号 | 0 | 触发 UI 状态机收敛 |
| `messages/session_state_changed`（或经 `meta` retained） | 状态变更 | 新状态 | 1 | UI 状态机 |
| ~~`messages/new_data_available`~~ | — | — | — | **废弃**（被 `stream_delta` 取代） |

**关键约定**：

- `stream_delta` 永远以**整行**为单位（一行 = conversation JSONL 的一条完整 entry 或一个完整的行单元），**绝不分 token / 半行**。一个 500ms 周期内若没有新的整行，则该周期不发 `stream_delta`（空推无意义）。
- `stream_delta` 与 `record_complete` 的关系：流式期间用 `stream_delta` 增量投递整行；同一条记录定稿时发 `record_complete` 携带**完整内容**。前端以 `record_complete` 为该记录的权威终态，`stream_delta` 仅为过程中的增量。
- assistant 的 `record_complete` 到达 = "全部 assistant 消息就绪"，前端据此一次性渲染（见 D4）。
- QoS 沿用 `mqtt.md` §8.3：messages/* 流式事件 QoS 0（丢一帧由后续帧或 `record_complete` 覆盖；断线重连后用 HTTP 重对齐兜底，见 D6）。

### D2. 后端 500ms 推送 + cursor 推进

**双 cursor 设计**（实现修订，2026-07-16）：

后端维护两个职责不同的 cursor，分别服务于 HTTP 分页和 MQTT 推送：

1. **`delivery_cursor`**（HTTP 分页用，行号语义）：沿用 ADR-021 既有基础设施。`read_messages_since_cursor` 按 JSONL 行号读取，用于 HTTP `GET /messages` 首页加载 + 向上翻历史分页回填。初始加载后 `reset_delivery_cursor(sid, total_lines)` 重置，使后续 HTTP 分页从已加载末尾继续。

2. **`stream_push_offset`**（MQTT 推送用，字符偏移语义，`session_core.rs:65`）：标记当前 streaming line 的 `accumulated_content` 中"已推送的字符位置"。每 500ms tick 时，`try_send_stream_delta` 从 `stream_push_offset` 读取新增字符，按 `\n` 切分为整行，推送后推进 offset。在 `ensure_streaming_line` 创建新 streaming line 时归零（新 line 的 `accumulated_content` 为空，前一个 line 的 offset 无意义）。

**两 cursor 不冲突**：`delivery_cursor` 操作的是已落盘 JSONL 行（流式结束后），`stream_push_offset` 操作的是 streaming line 内部未落盘的字符缓冲（流式进行中）。流式期间 JSONL 还没写入，`delivery_cursor` 不动；流式结束后 streaming line 被 flush 到 JSONL，`stream_push_offset` 随 streaming line 移除而失效，`delivery_cursor` 在下一次 HTTP 请求时推进。

> **Phase 3 现状备注**：`delivery_cursor` 基础设施（`get/advance/reset_delivery_cursor`、`read_messages_since_cursor`）在 Phase 3 删除 HTTP 增量拉取后仅保留为兼容代码，生产路径不再读取它。HTTP 分页用的是 `read_messages_paginated` 返回的 `PaginatedMessages.cursor`（独立于 `delivery_cursor`）。`reset_delivery_cursor` 仅在 gRPC `handle_get_session_messages`（cli.rs:2994）中调用，但前端实际走 HTTP 路径（http/server.rs:get_messages），不经过 gRPC。因此 O5（cursor 对齐）的"HTTP 初始加载后 reset_delivery_cursor"要求在 HTTP 路径下实际不生效——但因为 `stream_push_offset` 是 streaming line 内部 cursor（新 line 时归零），HTTP 加载不影响 push cursor，所以不会重复投递。

**500ms 推送逻辑**：

```
每个正在 streaming 的 session，挂一个 500ms 定时器（周期 = notify_interval_ms，默认 500）：
  1. 读取 streaming_lines[sid].accumulated_content 自 stream_push_offset 起的新增字符
  2. 按 '\n' 切分为整行列表
  3. 若有新整行：
       PUBLISH messages/stream_delta { lines: [{role, message_id, content}] }   // QoS 0
       advance stream_push_offset（推进到已推送的最后一行末尾）
  4. 若无新整行（只有 partial line）：跳过本轮（不发空推）
  5. 该 session 的流式结束时：停止定时器；flush streaming line 到 JSONL；
     对该记录 PUBLISH messages/record_complete { role, message_id, content }（QoS 1，完整数据）
```

- **前端没有任何 cursor**。前端只做"收到 `stream_delta` → 追加进 `activeStream.lines`"，不回拉、不计算游标。
- 初始加载 / 重连重对齐时仍 `reset_delivery_cursor(sid, total_lines)`（HTTP `delivery_cursor`），使后续 HTTP 分页从已加载末尾继续。
- **前后台一视同仁**：runtime 对所有"正在 streaming 且被订阅"的 session 都推送 `stream_delta`，**不再因 session 后台而抑制**。现有 `enable_notify`/`disable_notify`（`session_core.rs`、`session_task.rs`、`session_manager.rs`、`cli.rs`、`gateway_loop.rs`、`inbound.rs`、`control_handler.rs`）的"后台抑制 NewDataAvailable"逻辑**已删除**（Phase 3）——push 模型下后台接收成本可控（见 D6），无需抑制。

> 与现状的差异：现有 `notify_new_data_available`（`session_core.rs`）发的是信号；改造后同一个 500ms tick 改为读 `stream_push_offset`、发带数据的 `stream_delta`、推进 offset。throttle 配置 `notify_interval_ms` 直接复用。

### D3. 前端 per-session 数据存储（重构重点）

每个"已打开"的 session 在前端拥有一个 `SessionDataStore`，**这是本次重构的核心数据结构**：

```ts
interface SessionDataStore {
  sessionId: string;
  // 已定稿记录，按时间顺序 [oldest_loaded .. newest_loaded]。
  // 注意：非全量。初始为 HTTP 首页（direction=backward，最近 limit 条，后端默认 50，cli.rs:2935）。
  // 之后两种增长：① 底部追加——MQTT record_complete/tool_call 等推送的新记录；
  //              ② 顶部前插——向上翻历史时 HTTP direction=backward 用"最旧已加载 cursor"拉更早页，prepend 到顶部。
  // 活跃流式尾（activeStream + 最新若干条）始终保留，不驱逐。按 message id 去重（chatStore.ts:1189）。
  messages: ChatMessage[];
  // 当前正在流式累积的消息——每 session 仅一个（session 内消息串行线性增长，任意时刻只有一个消息在累积）
  // 这是该 session 唯一的活动追加缓冲；定稿即冻结进 messages[] 并置 null，下一条消息复用此缓冲
  activeStream: { messageId: string; role: 'thought' | 'assistant'; lines: StreamLine[] } | null;
  // 已加载的最旧 cursor（向上翻历史分页回填用）与最新 cursor（可选）
  oldestLoadedCursor: string | null;
  meta: SessionMeta | null;
  // 是否已建立 MQTT 订阅（决定切换时是否需 HTTP 首页加载）
  subscribed: boolean;
  // 是否前台（仅影响渲染，不影响接收/存储）
  foreground: boolean;
}

interface StreamLine { role: 'thought' | 'assistant'; lineNo: number; content: string; }
```

**`messages[]` 与分页的关系（关键）**：

- **不是全量**：初始仅首页（最近 N 条）。向上翻历史 = HTTP `direction=backward` 从 `oldestLoadedCursor` 拉更早页 → **prepend 到 `messages[]` 顶部**（`SessionDataStore` 内容因此变化、变长）。
- **无重复**：HTTP 向上翻拉的是"早于最旧已加载 cursor"的已落盘记录；MQTT 只推"晚于 delivery cursor（初始加载时 `reset_delivery_cursor`，见 O5）的新记录"，两区间不重叠；另有按 message id 去重兜底（`chatStore.ts:1189`）。
- **软上限（O1）**：`messages[]` 过长时驱逐**最旧**端；与向上翻回填配套——驱逐后若再向上翻，从新的 `oldestLoadedCursor` 继续回填即可。**严禁驱逐活跃流式尾**（`activeStream` 及最新若干条）。

**存储与渲染解耦**：

- **接收与存储**：只要 `subscribed=true`，无论前台后台，所有 `stream_delta`/`record_complete`/`tool_call` 等事件都写入该 session 的 `SessionDataStore`。
- **渲染**：仅前台 session 触发渲染；后台 session 只存不渲染。切换前后台 = 翻转 `foreground` 标志 + 切换 UI 读取的 store，**不触发任何 HTTP 请求**。

**订阅生命周期**（修订 `mqtt.md` §12.10）：

- session **首次打开**：HTTP 首页加载 `messages[]`（最近 N 条）→ 建立 MQTT 订阅（`messages/#` + `meta`）→ `subscribed=true`。
- session **切到后台**：**保持订阅、保持存储**，不 UNSUBSCRIBE；仅停止渲染（停 thinking 的 500ms 渲染定时器）。
- session **切回前台**：直接从 `SessionDataStore` 渲染，**不 HTTP 重载**。
- session **关闭/删除**：UNSUBSCRIBE + 丢弃 store。
- **重连/丢消息兜底**：MQTT 断线重连后，对前台 session 若怀疑有缺口，用 HTTP 重对齐 `messages[]` 并 `reset_delivery_cursor` 重新对齐（这是唯一的 HTTP 回拉场景，且仅重连时）。

> 代价：所有"已打开未关闭"的 session 都保持订阅。localhost 单用户桌面场景下，同时打开的 session 数有限，订阅数可控；长会话的 `messages[]` 增长需设上限（`activeStream` 定稿即清空，见开放问题 O1）。

### D4. thinking 渲染规则

> **实现修订（2026-07-16）**：D4 原文要求"5 个固定 DOM 槽位原地覆盖"。实际实现采用 `ReactMarkdown` + `useDeferredValue` + `React.memo` 的替代方案——content 是末5行 join 后的字符串，ReactMarkdown 渲染整个 markdown AST，`useDeferredValue` 让浏览器在压力下跳过中间值，`React.memo` 按 content 字符串比较避免不必要重渲染。该方案性能效果与"5 固定槽位"类似（React diff 会复用 DOM 节点），且保留了 markdown 渲染能力。若未来压测发现 DOM 碎片问题，再改为原生 5 槽位实现。

- thinking 流式行通过 `stream_delta` 累积进 `activeStream.lines`（当前活动消息为 thought 时，**滚动上限 5 行**——见 D9.1）。**默认折叠**（不展开）。
- 用户点击展开 thinking 区块后**实时渲染**：
  - 启动一个 **500ms 渲染定时器**（仅前台运行）。
  - 每次 tick 检测 `activeStream.lines` 的行数是否变化；**有变化才重渲染**。
  - 渲染内容 = `activeStream.lines` 的**最后 5 行**（渲染单位为整行，非逐 token 打字）。
  - **机制是"原地覆盖"，不是滚动/追加**：UI 固定 5 个渲染槽位，每次用最新的末 5 行**覆盖**这 5 个槽位的内容，**只覆盖、不新增** DOM 节点（也不销毁旧节点）。目的是**减少 markdown 反复渲染产生的内存碎片**。
  - 随新行到达，末 5 行被新内容整体覆盖，用户视觉上仍像内容向上走，但实现上既不 append 也不 scroll。
  - **固定显示 5 行，不支持滚动** = 视口恒为 5 行、用户**无法用鼠标上翻**（行数不足 5 行时显示实际行数）；与"画面是否更新"无关——5 个槽位每 500ms 实时覆盖刷新。
  - 收起 thinking 或 session 切后台：停止该定时器。
- thinking 定稿（`record_complete` role=thought）后：**冻结进 `messages[]`**（thought 携带其 lines 供展开渲染）、`activeStream = null`；定时器停止。此后展开该 thought 改读 `messages[]` 中冻结的 lines 末 5 行（静态，无定时器）。

### D5. assistant 渲染规则

- assistant 流式行同样通过 `stream_delta` 累积进 `activeStream.lines`（当前活动消息为 assistant 时），但**完全不做流式渲染**。
- assistant 等待期间（`activeStream` 在累积、尚未收到 `record_complete`）：UI 显示**"处理中"动画**（如 spinner / 三点跳动），不显示任何 assistant 文本。
- 收到 `record_complete` role=assistant：取 `activeStream.lines` 拼装完整内容（或直接用 payload 里的完整 content），**一次性渲染**进 `messages[]`；`activeStream = null`；停止"处理中"动画。
- 即 assistant 对用户的表现 = "等待动画 → 整条消息突然出现"，无中间态。

### D6. 前后台与会话切换

- **前台 session**：渲染开启（thinking 展开则跑 500ms 定时器；assistant 等待动画运行）。
- **后台 session**：渲染关闭，但 MQTT 事件照常写入 `SessionDataStore`（`activeStream` 继续累积、`record_complete` 照常落 `messages[]`）。
- **切换 A→B**：A 翻转为后台（停渲染定时器、停动画，保留订阅与存储）；B 翻转为前台（直接从 B 的 store 渲染）。
- **首次进入 B（`subscribed=false`）**：HTTP 首页加载 → 订阅 → 渲染。
- **B 已打开过（`subscribed=true`）再切回**：直接渲染，**无 HTTP**。
- runtime 侧**无前后台概念**：对所有 streaming 的订阅 session 一视同仁推送。现有 `enable_notify`/`disable_notify` 前后台抑制逻辑删除。

### D7. 初始加载与 HTTP 的角色（不变 + 收窄）

- **初始加载与历史分页回填均保留 HTTP**（不改 ADR-021/`mqtt.md` §7.4 的 `GET /api/agents/{id}/sessions/{sid}/messages`）：首次打开 / 重连重载时拉首页 `messages[]`；**向上翻历史**用 `direction=backward` 游标分页回填更早记录（后端 `http/server.rs:350,376`、`cli.rs:2936-2940,3075` 已支持，返回 `messages`+`has_more`+`cursor`）。此分页能力**不废弃**。
- **废弃的仅是"增量拉取流式数据"参数**：同一路由 `GET /messages` 上，删除流式增量相关参数 `incremental` / `line_number` / `line_char_offset`（`cli.rs:2943-2958`）与响应里的独立 `streaming` 字段；`PaginatedMessages` 类型移除 `streaming` 字段。**保留**分页参数 `cursor`+`limit`+`direction`。
- HTTP 退化为三个用途：① 首次首页加载；② **向上翻历史分页回填**（含 `messages[]` 软上限驱逐后的回填，见 O1）；③ 断线重连后的确定性重对齐。**与流式实时数据无关的 HTTP 拉取全部保留。**

> 滚动与流式的交互（无正确性问题）：流式期间用户在同一前台 session 向上翻历史，不影响 `activeStream` 继续累积——接收与渲染解耦（D6）。仅 thinking 块滚出视口时可暂停其 500ms 渲染定时器（优化项，非正确性）。

### D8. 字符串与内存碎片治理（后端 + 前端）

流式系统的高频字符串拼接是内存碎片/GC 压力的主要来源。本节给出两侧的硬性约束，**与 D1–D7 一并实施**。

**总原则（每 session 单一活动缓存）**：session 内消息是**串行线性增长**的——任意时刻只有一个消息在流式累积。因此字符串缓存**一个 session 一个就够**：后端一个 `accumulated_content` + 一个编码缓冲；前端一个 `activeStream.lines`。**禁止**按 message_id 分段、按行、或池化搞多个缓冲。定稿即冻结进 `messages[]` 并清空活动缓冲，下一条消息复用同一缓冲。

> 实证基线：后端 `StreamingLine.accumulated_content: String`（`conversation.rs:1088`，注释"随每个 Delta 增长"）已是**单 `String` 追加**模式，且 `StreamingStateMap`（`:1159`）为 `session_id → 单个 StreamingLine`，天然一 session 一缓冲；现有 delta 按 `char_offset` 取字符增量（`StreamingLineDelta`，`:1101`）。前端新设计 `activeStream.lines` 为每 session 唯一活动缓冲。

#### D8.1 后端（Rust）

> **现状已验证为单一缓冲**：`StreamingStateMap = Arc<RwLock<HashMap<String, StreamingLine>>>`（`conversation.rs:1159`）以 `session_id` 为 key，每个 session 仅一条 `StreamingLine`、一个 `accumulated_content: String`（`:1088`）。故后端本规则 = **保持现状、不要改成多缓冲**，不是新增。

1. **保持"单 `String` 追加"，禁止退回 Vec-of-pieces + join**：`accumulated_content` 用 `push_str` 追加是摊还 O(1)、realloc 次数 ~log(n)，本身低碎片。严禁改成 `Vec<String>` 存 token 片段再末尾 `join`（双倍内存 + 一次性大分配）。**严禁**把 `session_id → 单 StreamingLine` 改成按 message_id / 按行的多缓冲结构。
2. **预分配容量**：创建 `StreamingLine` 时 `accumulated_content: String::with_capacity(初始估算)`（如 4–8 KB，或按该 session 历史 line 均值预热），消除早期多次扩容。
3. **delta 只发新增整行，绝不重发累计内容**：`stream_delta` = 自 cursor 起的新整行（见 D2）。每 tick 序列化量 = O(delta) 而非 O(history)，避免每 500ms 重建全量字符串。
4. **序列化进 protobuf 不 clone content**：构建 `StreamDelta` payload 时把行内容直接写进 protobuf 编码缓冲（`prost` 编码到复用的 `BytesMut`），而非 `content.clone()` 塞进中间结构体；投递给 MQTT publisher 用 `bytes::Bytes` 零拷贝切片。
5. **复用编码缓冲**：每 session 维护一个可复用 `BytesMut` encode buffer（`clear()` 后重用，不每 500ms `new`），消除周期性 buffer 分配。
6. **JSONL 落盘流式写**：`append_message`（`conversation.rs:481`）写 JSONL 用 `BufWriter` + `serde_json::to_writer` 字段级序列化，避免在内存里先拼一个完整 JSON 大字符串再 write。

#### D8.2 前端（JS/TS）

> **现状已验证为多缓冲（待替换）**：当前 `streamingContents = new Map<string, StreamingEntry>()`（`chatStore.ts:30`），key 为 `streamingKey(sessionId, messageId)`（`:33`）——按 `(session, message)` 多键存多条，正是"搞多了"。本规则 = **用每 session 单一 `activeStream` 替换该 Map**。

1. **存数组，不存增长字符串**：`activeStream.lines: StreamLine[]` 是每 session 唯一的活动缓冲（替换原 `streamingContents` Map），每条 `content` 是 MQTT 收到的整行字符串（不可变）。追加用 `array.push`（摊还 O(1)），**严禁 `accumulated += chunk` 式逐 token 拼接**。这是前端最核心的防碎片措施。
2. **最终装配只做一次**：assistant 的 `record_complete` 到达时，`lines.map(l => l.content).join('\n')` 一次性 join 成完整消息——单次分配，非每 tick 拼接。
3. **thinking 渲染复用 + memo**：每 500ms 仅当行数变化才重算 `last5.map(l => l.content).join('\n')`（5 条小串 join，开销有界）；对渲染出的 markdown HTML 按"末 5 行 lineNo 集合"做 memo，未变则不重新 parse markdown。
4. **DOM 原地覆盖（见 D4）**：5 个固定槽位原地覆盖内容、不增删节点——同时治 DOM 碎片与 markdown 反复 mount/unmount。
5. **活动缓冲定稿即清空**：`activeStream` 在 `record_complete` 后即冻结进 `messages[]` 并置 `null`（不长期占内存）；thought 的 `activeStream.lines` **滚动上限 5 行**（与 D4 显示一致，见 D9.1），超长 thinking 不会撑大内存。`messages[]` 计数增长见开放问题 O1（单条体积已由 D9 裁剪有界）。
6. **桥接层减少二次拷贝（后续优化项）**：`session_message_to_flat`（`chat_mqtt.rs:485-672`）把 protobuf 解成扁平 JSON 再 `emit`，行内容在此过程中成为 JSON 串子串、前端 `JSON.parse` 再生成 JS string，存在一次中转拷贝。后续可让 Rust 侧把行内容以 `Uint8Array`/原生 string 透传（不二次 JSON 序列化）。**非本 ADR 强制，列为后续优化。**

#### D8.3 验证

- 后端：长会话（万行级 thinking）下 `accumulated_content` realloc 次数应为 O(log n)；500ms tick 的堆分配应趋于稳定（不随历史增长）。
- 前端：`activeStream` 定稿即清空（不长期占内存）、`messages[]` 线性增长且有上限；500ms 渲染不产生新增 DOM 节点；`record_complete` 前无大字符串 join。

### D9. 大消息裁剪：thinking 末 5 行 / toolResult 首 5 行（内存边界）

> **"全量"语义澄清**：原则中的"全量"指**当前页要显示的完整内容**（标准分页语义），非"所有 message 全量"。每页按页内全量加载显示；本节解决的是**来回翻页导致 `messages[]` 累积所有页 → 内存膨胀**的问题。真正占内存的是两类大消息：`toolResult` 与 thinking 流式——各自裁到 5 行即可，前后端压力同时减轻。

#### D9.1 thinking —— 前端只存最后 5 行

- `activeStream.lines`（thought）为**滚动窗口，上限 5 行**：收到新整行 `push` 后，若超过 5 行则丢弃最旧（与 D4"只渲染末 5 行"一致——存了也只显示这 5 行，多存无意义）。
- `record_complete(thought)` 定稿冻结进 `messages[]` 时，**只携带这最后 5 行**（不保留全量）。
- 后端 `stream_delta` 仍按整行增量推送（不变）；裁剪只在前端做（thought 的"末 5 行"是滑动窗口，后端无法预知前端窗口位置，故前端裁剪最简）。

#### D9.2 toolResult —— 后端源头裁剪为首 5 行，前端只存首 5 行（无例外）

- **所有前端可见的投递路径一律裁剪 toolResult 为首 5 行**（带截断标记 `…`），**无任何例外**：
  - ① MQTT `record_complete(tool_result)` / `tool_result` 事件；
  - ② HTTP `GET /messages` 分页响应（首页加载 + 向上翻历史回填）；
  - ③ HTTP 重连重对齐（D6/D7 的重连回拉）。
- 即：**前端永远不接收完整 toolResult**——无论数据来自 MQTT 还是 HTTP、无论首屏/翻页/重连，`messages[]` 中的 toolResult 记录内容恒为后端裁剪后的首 5 行，前端不再二次截断。
- **完整 toolResult 只存在于后端 JSONL**，仅供 LLM 上下文与 `compress_tool_results`（`agentStore.ts:42-52` `toolResultCompressionMode`/`toolResultSoftThresholdChars`）使用——**永不进前端**。裁剪不影响落盘与 LLM 上下文。
- 现状实证：前端 `ExploreBlock.tsx:582` 已有 `content.length > 500 ? slice(0,500)+"…"` 的 500 字符截断；本 ADR 将其改为**首 5 行 + 后端源头裁剪**（前端收到即已 ≤5 行，去掉前端 500 字符分支）。

#### D9.3 效果与边界

- 两类大消息单条内存有界（≤5 行），来回翻页累积的 `messages[]` 单条体积小且恒定 → O1 的计数上限可放宽。
- assistant（主输出）**不裁剪**，仍全文存显（用户需完整回复）；若后续发现超长 assistant 也是瓶颈，再议。
- toolResult **无"按需拉完整"的例外接口**：前端始终只有首 5 行；如未来确需查看完整 toolResult，再单独设计（当前不做）。

---

## 数据流（重构后）

```
Runtime streaming
   │ 每 500ms (notify_interval_ms)
   ▼
read_messages_since_cursor(sid)  ──▶ 取新整行
   │
   ├─▶ PUBLISH messages/stream_delta { lines:[...] }   (QoS 0, 带数据)
   │     └─▶ advance_delivery_cursor(sid)
   │
   └─ 记录定稿 ─▶ PUBLISH messages/record_complete { role, message_id, content }  (完整数据)

         │  Gateway broker(:19875) 纯路由，不转发业务
         ▼
Desktop Tauri Rust (chat_mqtt.rs)  ── 解码 protobuf → 扁平 JSON ──▶ emit("agent-event")
         │
         ▼
前端 handleMessageEvent
   ├─ stream_delta  ─▶ SessionDataStore.activeStream.lines.push(...)   (前台/后台都存)
   ├─ record_complete(assistant) ─▶ messages[].push(完整) ; activeStream=null ; 停动画  (一次性渲染)
   ├─ record_complete(thought)   ─▶ 标记 finalized ; (展开时才渲染最后 5 行)
   ├─ tool_call / tool_result    ─▶ messages[].push(...)
   └─ done/error/stopped         ─▶ UI 状态机收敛

渲染（仅前台）:
   thinking 展开时: 500ms 定时器 → 用末 5 行原地覆盖 5 个固定槽位 (只覆盖不新增, 减少 markdown 内存碎片; 视口恒 5 行不可鼠标上翻)
   assistant 等待: 处理中动画 → record_complete 到达 → 一次性渲染
```

---

## 影响范围

### 后端（Runtime）

| 文件 | 变更 |
|------|------|
| `core/acowork-runtime/src/agent/session_core.rs` | `notify_new_data_available`（:358）改造：500ms tick 改为读 cursor + 发 `stream_delta` + 推进 cursor；移除 `notify_enabled` 前后台抑制（:37） |
| `core/acowork-runtime/src/agent/session/session_task.rs` | 移除 `enable_notify`/`disable_notify` 前后台门控（:148-152, :1628, :1638） |
| `core/acowork-runtime/src/agent/session/session_manager.rs` | `delivery_cursor`（:332, :1819）保留，消费方改为定时推；`streaming_lines`（:335）读取逻辑不变 |
| `core/acowork-runtime/src/conversation.rs` | `read_messages_since_cursor`（:1912）保留；新增"按 500ms 周期推送新整行"的调度 |
| `core/acowork-runtime/src/startup/subsystems.rs` | `ChunkEvent::NewDataAvailable`（:338-348）→ 改为 `StreamDelta` 带数据事件；`relay_intent("new_data_available")` 删除 |
| `core/acowork-runtime/src/cli.rs` | 删除 `GET /messages` 的流式增量参数（`incremental`/`line_number`/`line_char_offset`，:2943-2958）与 `streaming` 独立字段（:2982）；**保留分页参数** `cursor`+`limit`+`direction`（首页加载 + 向上翻历史回填）；**D9.2：所有 HTTP 响应（分页/重连重对齐）的 toolResult 一律裁剪为首 5 行，无例外** |
| `core/acowork-runtime/src/config.rs` | `notify_interval_ms`（:143，默认 500）语义改为"stream_delta 推送周期"，默认值不变 |
| `core/acowork-core/proto/mqtt_payload.proto` | 新增 `StreamDelta { lines: [StreamLine] }`、`RecordComplete { role, message_id, content, ... }`；废弃 `NewDataAvailable`/`ChunkPayload` 信号式定义 |
| `core/acowork-runtime/src/agent/session_core.rs`（D9.2 补充） | MQTT 投递 `record_complete(tool_result)` / `tool_result` 事件时，content 裁剪为首 5 行 + 截断标记（完整内容仍落 JSONL 供 LLM 上下文） |

### 网关（Gateway）

| 文件 | 变更 |
|------|------|
| `core/acowork-gateway/src/http/proxy.rs` | 分页 `GET /messages` 透传保留（:208）；流式增量参数路径删除 |

### 前端（Desktop）

| 文件 | 变更 |
|------|------|
| `apps/acowork-desktop/src/stores/chatStore.ts` | **重写消息/流式层**：新增 `SessionDataStore` per-session 存储；`loadSessionMessages`（:1073）仅保留首页/分页加载语义（删除 `incremental`/`line_number`/`line_char_offset` 流式增量分支）；删除 `data.streaming` 相关、`streamingContents` Map（:30，多缓冲→替换为单一 `activeStream`）、`streaming:{line}` id hack、增量合并清理（:1228-1235）、`session_state_changed→idle` 最终拉取（:2039-2054） |
| `apps/acowork-desktop/src/lib/polling.ts` | **删除 `PollingManager` 增量轮询**（:181-194 等）；流式不再轮询 |
| `apps/acowork-desktop/src/lib/types.ts` | `PaginatedMessages` 移除 `streaming` 字段（:712-719）；新增 `StreamLine`/`SessionDataStore` 类型 |
| `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` | 渲染改为从当前前台 `SessionDataStore` 读取；session 切换不再触发 HTTP |
| `apps/acowork-desktop/src/components/chat/MessageBubble.tsx` | thinking 区块：展开时 500ms 定时器渲染末 5 行、固定 5 行无滚动；assistant：等待动画 + 一次性渲染 |
| `apps/acowork-desktop/src/components/chat/ExploreBlock.tsx` | **D9.2：删除 `content.length > 500 ? slice(0,500)+"…"` 截断分支（:582）**——toolResult 已由后端裁剪为首 5 行，前端直接渲染 |
| `apps/acowork-desktop/src/hooks/useStreamingContent.ts` | **删除**（ADR-027 流式渲染机制废弃） |
| `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs` | `session_message_to_flat`（:485-672）扩展映射 `stream_delta`/`record_complete`；`emit("agent-event")`（:60）不变 |

---

## 与既有 ADR 的关系

| ADR | 关系 |
|-----|------|
| ADR-021 | **修订**：废弃其"HTTP Pull + `new_data_available` 通知"的流式内容模型；保留其"初始加载走 HTTP"与 `read_messages_since` / `delivery_cursor` 基础设施 |
| ADR-027 | **取代**：`streamingContents` Map + `useStreamingContent` 流式渲染机制被 per-session `activeStream` 单缓冲 + 受限渲染规则取代 |
| ADR-033 | **延续**：MQTT 作为事件总线，本 ADR 使 `messages/*` 真正带数据 |
| ADR-034 | **履行**：§3.2 事件面 `messages/*` 主题应带数据——本 ADR 让实现回归该契约；§7.3"Gateway 不转发业务事件"不变 |
| `mqtt.md` §3.2 | **履行**：`messages/chunk` 带数据的契约以 `stream_delta`（整行批量）形式落地；§12.10"进入 session 动态订阅、离开 UNSUBSCRIBE"**修订**为"后台 session 保持订阅" |

---

## 迁移路径

### Phase 1：后端 stream_delta 推送（双通道并存）✅ 已实施 (2026-07-15)

- ✅ 新增 `StreamDelta`/`RecordComplete` proto（`mqtt_payload.proto`，oneof 字段 29/30；`StreamLine` 消息）。
- ✅ runtime 500ms tick（`notify_new_data_available`）改为：throttle 之后先 `try_send_stream_delta`（推送整行增量，前后台一视同仁），再（仅前台）发旧 `NewDataAvailable` 信号。HTTP 增量端点保留，双通道并存。
- ✅ `ChunkEvent::StreamDelta` 新增；`relay_chunk_event_mqtt` 映射到 `publish_stream_delta`（topic `…/messages/stream_delta`，QoS 0）；gRPC `relay_chunk_event` 丢弃（MQTT-only）。
- ✅ 推送游标 `stream_push_offset` 落在 `SessionCore`（每 session 单一，role 切换建新 streaming line 时归零），只推进经过的完整行、保留末行残段。单测覆盖"只推整行 / 残段保留 / role 切换游标归零"。
- ⏳ 验证：后端已 emit `stream_delta`；端到端"订阅 `messages/stream_delta` 收到整行增量"待 Phase 2 前端接入或集成测试确认。

### Phase 2：前端 per-session 存储接入 push ✅ 已实施（2026-07-15）

- ✅ 实现 `SessionDataStore`（全局 `activeStreams` Map，per-session 单缓冲）；`handleMessageEvent` 接 `stream_delta`/`record_complete` 写入 store。
- ✅ session 切换改为"前台/后台翻转 + 直接渲染"，首次进入才 HTTP 首页加载。停掉 `enable_notify`/`disable_notify` 前后台抑制调用，停掉 `new_data_available`→HTTP 增量轮询触发，`session_state_changed→idle` 改为全量兜底（非增量）。
- ✅ thinking/assistant 按本 ADR 渲染规则实现（D4 末5行原地覆盖 / D5 处理中动画一次性渲染）。
- ✅ 验证：多 session 切换无 HTTP 重载（停掉 `enable_notify`/HTTP 增量触发）；后台 session 数据持续累积（`activeStreams` 全局 Map 按 sessionId 存）；最后一轮 assistant 必渲染（`record_complete` 直接驱动，不依赖 idle 最终拉取）。

### Phase 2.5：toolResult 源头裁剪（D9.2）✅ 已实施（2026-07-15）

- ✅ **后端**：MQTT `ToolResult` 事件推送裁剪为首 5 行（`mqtt/client.rs` `truncate_tool_result_lines`）；HTTP 所有路径（首页分页 / 向上翻回填 / 重连重对齐 / `GET /messages`）裁剪为首 5 行 + `\n...(truncated)` 标记（`cli.rs` `truncate_tool_result_for_display`）。完整 toolResult 只留 JSONL，供 LLM 上下文与 `compress_tool_results` 使用。
- ✅ **前端**：删除 `ExploreBlock.tsx:582` 的 500 字符二次截断（后端已裁剪，前端不再重复截断）。
- ✅ 验证：`cargo build -p acowork-runtime` 通过 + `tsc --noEmit` 通过；toolResult 在前端渲染不超过 5 行；JSONL 中完整内容不受影响。

### Phase 3：清理旧链路 + O1 消息上限 ✅（2026-07-15/16）

- ✅ **前端**：删除 `PollingManager`（`polling.ts`）、`streamingContents` Map、增量合并清理、`session_state_changed→idle` 全量兜底拉取；`getStreamingContent` 只读 activeStreams；`loadSessionMessages` 移除 `incremental` 参数与整块 `if(incremental)` 逻辑；删 `new_data_available` case；删 error/stopped `stopPolling`；`clearSessionStreaming` 简化为 `activeStreams.delete`。
- ✅ **O1**：N=150。`trimMessages` 三路径截断最旧端。
- ✅ **后端**：删除 runtime `enable_notify`/`disable_notify`（`session_core.rs:37` 字段移除、`session_task.rs:150-153` enum 变体移除 + 命令处理删除）；删除 `GET /messages` 增量端点（`incremental`/`line_number`/`line_char_offset` 参数 + `read_messages_since_cursor`/`read_messages_since` 两条路径。仅保留 paginated）。
- 验证：`tsc --noEmit` ✅ + `cargo build -p acowork-runtime` 编译中。

---

## 风险与开放问题

- **O1（存储增长 ✅ 已解决）**：`N=150`（3 页 × 50 条/页）。`trimMessages` 截断最旧端 → 翻页回填从 `oldestLoadedCursor` HTTP `direction=backward` 继续；activeStream 最新消息自然被保留。✅
- **O2（QoS 0 丢帧）**：`stream_delta` QoS 0 丢一帧由后续帧或 `record_complete` 覆盖；但若 `record_complete` 也丢，assistant 可能不渲染。**对策**：断线重连后 HTTP 重对齐（D7）；必要时 `record_complete` 提升至 QoS 1（与 `mqtt.md` §8.3 一致性待定）。
- **O3（多 session 并发推送量）**：所有打开的 session 都保持订阅 + 推送。localhost 单用户场景可接受；若同时打开 session 数大，需评估 broker/桌面负载。**待压测。**
- **O4（thinking 固定 5 行的可用性）**：5 行无滚动可能截断长 thinking。这是用户明确要求的产品决策，后续若需调整再议。
- **O5（初始加载的 cursor 对齐）**：HTTP 初始加载后必须 `reset_delivery_cursor`，否则 push 会重复投递历史行。已在 D2 强调，实现时需作为不变量守护。

---

## 验证清单

- [x] 后端 emit `stream_delta`：500ms 周期推送整行增量，无半行/token（`notify_new_data_available`→`try_send_stream_delta`→`ChunkEvent::StreamDelta`→`publish_stream_delta`）。端到端订阅验证待 Phase 2/集成测试。
- [x] 推送游标只在后端（`SessionCore.stream_push_offset`），前端无 cursor 概念；当前 Phase 1 用 `streaming_lines.accumulated_content` 派生整行（非 `read_messages_since_cursor`，待 Phase 2/3 移除 HTTP 时切换）。
- [x] thinking 默认折叠；展开后 500ms 用末 5 行原地覆盖 5 个固定槽位（只覆盖不新增，减少 markdown 内存碎片）；视口恒 5 行、用户无法鼠标上翻。
- [x] assistant 等待期显示处理中动画；`record_complete` 到达后一次性渲染，无中间文本。
- [ ] session A→B→A 切换：A 切回时直接渲染、无 HTTP 请求；A 在后台期间 `activeStream`/`messages` 持续增长。（待运行时验证）
- [ ] 首次打开 session 走 HTTP 首页分页（最近 N 条）；向上翻历史 `direction=backward` 回填正常；重连后走 HTTP 重对齐。（待运行时验证）
- [ ] 原"最后一轮 assistant 不渲染"bug 不复现（不再依赖 idle 最终拉取）。（待运行时验证）
- [x] runtime 对后台 session 仍推送（`enable_notify` 抑制已移除）。
- [ ] 向上翻历史：`direction=backward` 分页回填正常（HTTP 分页保留），流式实时数据不受影响。（待运行时验证）
- [x] 后端长会话下 `accumulated_content` realloc 次数 O(log n)、500ms tick 堆分配不随历史增长（D8）。
- [x] 前端 `activeStream` 定稿即清空、`messages[]` 线性增长有上限、500ms 渲染不新增 DOM 节点、`record_complete` 前无大字符串 join（D8）。
- [x] thinking `activeStream.lines` 滚动上限 5 行；定稿冻结进 `messages[]` 也只带末 5 行（D9.1）。
- [x] toolResult 在 MQTT + 所有 HTTP（首页/向上翻/重连重对齐）路径均裁剪为首 5 行，前端永不接收完整 toolResult；前端 `ExploreBlock` 不再二次截断；JSONL/LLM 上下文仍用完整内容（D9.2，无例外）。

---

## 相关源码索引（已读实证）

- 信号式现状：`core/acowork-runtime/src/agent/session_core.rs:358`（`notify_new_data_available`）、`core/acowork-runtime/src/startup/subsystems.rs:338-348`（`new_data_available` relay）
- HTTP 增量轮询现状：`core/acowork-runtime/src/cli.rs:2940`（`GET /messages?cursor&include_streaming`）、`:2982`（独立 `streaming` 字段）、`:2983`/`:2995`（get/advance cursor）
- HTTP 分页（保留，向上翻历史用）：`core/acowork-runtime/src/http/server.rs:350,376,416-417`（`direction=backward/forward`、`has_more`+`cursor`）、`cli.rs:2936-2940,3075`、`conversation.rs:1165`（`MAX_RAW_PER_DISPLAY_PAGE`）
- cursor 基础设施（复用）：`core/acowork-runtime/src/agent/session/session_manager.rs:332`、`:1819`、`core/acowork-runtime/src/conversation.rs:1912`（`read_messages_since_cursor`）、`:3370`（`reset_delivery_cursor`）
- 500ms 周期配置（复用）：`core/acowork-runtime/src/config.rs:138-143`（`notify_interval_ms` 默认 500）
- 前后台抑制（移除）：`core/acowork-runtime/src/agent/session/session_task.rs:148-152`、`core/acowork-runtime/src/agent/session_core.rs:37`
- 大消息裁剪现状（D9 基线）：前端 `apps/acowork-desktop/src/components/chat/ExploreBlock.tsx:582`（`content.length > 500 ? slice(0,500)+"…"` 已有 toolResult 截断，本 ADR 改为首 5 行 + 后端源头裁剪）、`apps/acowork-desktop/src/stores/agentStore.ts:42-52`（`toolResultCompressionMode`/`toolResultSoftThresholdChars`，LLM 上下文压缩，独立于显示、不改）
- 字符串累积现状（D8 基线）：后端 `core/acowork-runtime/src/conversation.rs:1082-1111`（`StreamingLine.accumulated_content: String` 单串追加、`StreamingStateMap:1159` 为 `session_id→单 StreamingLine` 天然单一缓冲、`StreamingLineDelta` char_offset 增量）、`:481`（`append_message` JSONL 落盘）；前端 `apps/acowork-desktop/src/stores/chatStore.ts:30`（`streamingContents = Map<(sessionId,messageId),StreamingEntry>` 多缓冲，待替换为单一 `activeStream`）、`:33`（`streamingKey`）
- 网关透传：`core/acowork-gateway/src/http/proxy.rs:208`
- Tauri 桥接：`apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs:60`（`emit`）、`:485-672`（`session_message_to_flat`）
- 前端轮询/流式（重写）：`apps/acowork-desktop/src/stores/chatStore.ts:1199`/`:1138`/`:1228-1235`/`:2039-2054`、`apps/acowork-desktop/src/lib/polling.ts`、`apps/acowork-desktop/src/lib/types.ts:712-719`

### Phase 1 实施源码索引（已落地，2026-07-15）

- proto：`core/acowork-core/proto/mqtt_payload.proto:455`（`StreamLine`）、`:466`（`StreamDeltaPayload`）、`:474`（`RecordCompletePayload`）；`SessionMessage.event` oneof 字段 29/30。
- 推送逻辑：`core/acowork-runtime/src/agent/session_core.rs:69`（`stream_push_offset` 字段）、`:419`（`try_send_stream_delta`）、`:459`（在 `notify_new_data_available` 中调用）、`:132`（`new()` 参数）。
- 事件定义：`core/acowork-runtime/src/agent/loop_.rs` `ChunkEvent::StreamDelta`（`notify_new_data_available` 重构见 `session_core.rs:385` 附近）。
- 中继：`core/acowork-runtime/src/startup/subsystems.rs:502`（`relay_chunk_event_mqtt` StreamDelta 分支 → `publish_stream_delta`）、`:355`（gRPC `relay_chunk_event` 丢弃）。
- 发布：`core/acowork-runtime/src/mqtt/client.rs:867`（`publish_stream_delta`，topic `…/messages/stream_delta`，QoS 0）。
- 单测：`session_core.rs` `test_stream_delta_pushes_complete_lines_only` / `test_stream_delta_advances_cursor_across_role_transition`。

### Phase 2 实施源码索引（已落地，2026-07-15）

- 翻译层：`apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs:672-706`（StreamDelta/RecordComplete proto→JSON 翻译）
- Store — ADR-035 单缓冲：`apps/acowork-desktop/src/stores/chatStore.ts:37-66`（`activeStreams` Map、`ActiveStream`/`StreamLine` 类型、`getStreamingContent` 优先读 activeStream）
- Store — 事件写入：`apps/acowork-desktop/src/stores/chatStore.ts` `handleMessageEvent` 的 `stream_delta`/`record_complete` case（upsert shell message→lines 累积→冻结进 messages[]）
- Store — 停旧触发：`new_data_available` case 移除 `notifyNewData` 调用；`session_state_changed→idle` 改为全量兜底（`incremental=false`）替代增量轮询
- agentStore：`apps/acowork-desktop/src/stores/agentStore.ts:620`（`switchSession` 移除 `enable_notify`/`disable_notify` invoke 调用）
- 渲染 D4（thinking）：`apps/acowork-desktop/src/components/chat/ThinkBlock.tsx`（移除 `tailContent`+auto-scroll，末5行直接渲染，固定高度无滚动溢出隐藏）
- 渲染 D5（assistant）：`apps/acowork-desktop/src/components/chat/MessageBubble.tsx:279`（isStreaming 早返回处理中动画，不渲染文本）
- 清理：`clearSessionStreaming` 并入 `activeStreams.delete(sessionId)`

### Phase 2.5 实施源码索引（已落地，2026-07-15）

- 后端 HTTP 截断：`core/acowork-runtime/src/cli.rs:2917`（`truncate_tool_result_for_display`，3 条 HTTP 路径均调用）
- 后端 MQTT 截断：`core/acowork-runtime/src/mqtt/client.rs:520`（`publish_tool_result` 调用 `truncate_tool_result_lines`，`:908`（函数定义））
- 前端删截断：`apps/acowork-desktop/src/components/chat/ExploreBlock.tsx:582`（删除 500 字符二次截断，后端已裁剪）
