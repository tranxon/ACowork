# ADR-049：Session 状态细化 — 从粗粒度 `Streaming` 到 6 变体业务状态机

**状态**：提案
**日期**：2026-07-15
**决策者**：大鱼
**前置**：
- ADR-014（Session 状态由 Runtime 持有，前端只读 — `SessionStateChanged` 事件）
- ADR-021（Session 数据加载统一 — HTTP Pull + 通知机制）
- ADR-035（MQTT 流式传输重构 — `activeStream` 缓冲 + `stream_delta` 推送），尤其 D9.2『StreamingSourceBlock `<pre>` DOM 复用模式』
- ADR-043（Session 配置与运行时状态拆分 — `SessionState` 协议结构）
- **ADR-050 §3.3（liveBuffer 设计）+ §16（post-C5 修订记录）** — 流式数据归属 `chatAdapterStore`，`messages[]` 为已确认消息唯一容器。本 ADR 的"流式字段"全部在 `chatAdapterStore` 而非 `chatStore` 中；trailing virtual item 已废弃，改用 `adapter.blocks` 中 `isLive: true` block 路由。

**影响范围**：
- `core/acowork-runtime/src/agent/session_state.rs` — `SessionStatus` enum 定义
- `core/acowork-runtime/src/agent/` 6 个 loop 模块 — 状态迁移点
- `core/acowork-runtime/src/providers/reliable.rs` — 429 重试后恢复状态
- `core/acowork-core/src/protocol.rs` — `SessionStatusDto` 结构体
- `apps/acowork-desktop/src/lib/types.ts` — 前端 `SessionStatus` 类型 + `StreamLine`/`ActiveStream`；`isProcessing()` 替代 `isSessionActive()`
- `apps/acowork-desktop/src/stores/chatStore.ts` — `record_complete` 主路径写入 `messages[]`；`sendMessage` 乐观写入 `messages[]`；不再持有 `assistantStreamingContent` 等流式字段（见 ADR-050 C2 + §16）
- `apps/acowork-desktop/src/components/chat/chatAdapterStore.ts` — **v1.1 新增**：liveBuffer（仅 `thinkingStream` / `assistantStream` 两字段）+ legacy 投影字段（`isThinking` / `thinkingContent` / `assistantStreamingContent` / `assistantStreamingStartTime` / `isAssistantReplying` / `isPinnedToBottom` / `optimisticEntries`）+ 模块级 `activeStreams` / `lastThinkingFlush` / `lastAssistantFlush` 节流 Map
- `apps/acowork-desktop/src/components/chat/chatListAdapter.ts` — **v1.1 新增**：`isAtTail()` 判定（`limit === 0` 视为 atTail）；`buildSnapshot` 仅取 `thinkingStream` / `assistantStream`
- `apps/acowork-desktop/src/lib/paginationUtils.ts` — **v1.1 新增**：`isAtTail(offset, limit, total)` 共享 helper
- `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` — 指示器渲染逻辑；通过 `useLiveStream()` 订阅 chatAdapterStore
- `apps/acowork-desktop/src/components/chat/SessionPanel.tsx` — Tab 栏状态（`isStreaming` → `isProcessing()`）
- `apps/acowork-desktop/src/components/chat/StreamingSourceBlock.tsx` — 通用流式预览组件（variant=thought|assistant）
- `apps/acowork-desktop/src/components/chat/ThinkBlock.tsx` — 简化为 StreamingSourceBlock 薄包装
- `apps/acowork-desktop/src/components/chat/VirtualMessageList.tsx` — **v1.1 修订**：assistant 流式预览改为 `isLive && type === "assistant"` block 路由到 StreamingSourceBlock；trailing virtual item 已废弃
- `apps/acowork-desktop/src/components/chat/ExploreBlock.tsx` — **v1.1 修订**：新增 `isLive` prop，避免与已折叠的 live thought 重复渲染
- `apps/acowork-desktop/src/components/chat/blockLayout.ts` — `REPLYING_INDICATOR_HEIGHT` 高度估算（trailing slot 废弃后可考虑删除，见 §不做事项）

---

## 修订记录

### v1.1（2025-01-16）— 与 ADR-050 post-C5 对齐

**背景**：ADR-050 C1-C5 + post-C5（commit `dcc182b2`）已全部落地，重构了流式数据归属：
- 流式字段从 `chatStore` 迁移到 `chatAdapterStore`
- `messages[]` 成为已确认消息的唯一容器（HTTP 历史 + MQTT `record_complete` 直接写入）
- `liveBuffer` 从 4 字段缩为 2 字段（`thinkingStream` + `assistantStream`）
- Trailing virtual item 废弃，assistant 流式预览改为 `adapter.blocks` 中 `isLive: true` block 路由

**修订要点**（详见 `docs/_internal/archive/review/zh/29-adr-049-vs-adr-050-post-c5-alignment.md`）：

1. §前置：增 ADR-050 §3.3 + §16 交叉引用
2. §影响范围：增 `chatAdapterStore.ts` / `chatListAdapter.ts` / `paginationUtils.ts`；`VirtualMessageList.tsx` / `ExploreBlock.tsx` 标注 post-C5 路由变更
3. §前端新类型定义：增 `isProcessing()` 替代 `isSessionActive()`
4. §Frontend assistant live preview：删除 trailing slot 描述；数据流图改为 `isLive` block 路由
5. §Tab 栏状态：列出 4 个 `sessionStatus` 引用点（`ChatPanel` / `SessionPanel` / `ContextUsageIcon` / `ResultsPanel`）作为同步替换目标
6. §实施步骤 Commit 5 第 4 条：record_complete 主路径改为写 `messages[]`，清空字段改为副作用
7. §A.2 末尾：删除「`isAssistantReplying` 仍是 DOM 提示灯」表述（post-C5 已无任何 DOM 元素以其为判据）
8. §不做事项：新增 chatAdapterStore legacy 投影字段退役归 ADR-050 后续
9. §迁移风险：新增 chatStore ↔ chatAdapterStore 协调遗漏风险

**未变**：核心 6 变体 SessionStatus 设计、`ProcessingPhase` 派生、`getProcessingPhase()` 穷举映射、Commit 1-2 后端逻辑、Commit 4 Tab 栏 `isProcessing()` 替换意图。

**适用范围**：本 v1.1 适用于 ADR-050 post-C5 架构（commit `dcc182b2` 及之后）。实施前需先在 PR 中确认 chatAdapterStore 投影字段已就位（避免在 chatStore 中反向"复活"已被 ADR-050 删除的字段）。

---

## 决策摘要

**将 `SessionStatus::Streaming` 拆分为 3 个语义明确的子状态**，让前端能直接从后端状态机判断当前阶段，去掉前端"根据数据参数组合状态"的复杂推导逻辑。

### 变更概览

**后端**：4 变体 → 6 变体
```
Idle | Streaming | WaitingApproval | Paused
    ↓
Idle | LlmAwaitingFirstChunk | LlmStreaming | ToolExecuting | WaitingApproval | Paused
```

**前端**：删除 3 个合成布尔变量（`isAssistantReplying`、`isThinking`、`showInterStepProcessing`），`sending` 从 4 项组合推导简化为 `sessionStatus` 直接映射。

---

## 背景与动机

### 1. 当前 `Streaming` 是一个语义黑洞

`SessionStatus::Streaming` 自 ADR-014 以来承担了 LLM 请求生命周期中所有"非静默"阶段：

- **TTFT 阶段**（LLM HTTP 请求已发出，等待首个 token，可能耗时 10-30s）
- **流式输出阶段**（LLM 正在产出可见 token）
- **工具执行阶段**（LLM 返回了工具调用，工具正在执行）

这些阶段对用户而言感知完全不同——"等待回复"、"正在生成回复"、"正在执行工具"——但后端的 `SessionStatus` 无法区分它们。

### 2. 前端被迫用"组合推导"补救

因为后端状态信息不足，`ChatPanel.tsx` 中用 7 个布尔变量从 3 个独立数据源交叉推导当前状态：

| 推导变量 | 来源 | 逻辑 |
|---------|------|------|
| `sending` | `sessionStatus` | `status === "streaming" \|\| "waiting_approval" \|\| "paused"` |
| `isAssistantReplying` | `activeStreams` 行数 | 累积行数 > 3 才亮；< 3 不亮（阈值 `ASSISTANT_REPLYING_LINE_THRESHOLD`） |
| `isThinking` | `stream_delta` 的 `role` | 收到 `role === "thought"` 时边缘触发 |
| `showWorkingItemAfterUser` | `messages[]` 末尾扫描 | 遍历最后一条消息判断类型 |
| `showInterStepProcessing` | 4 项布尔组合 | `sending && !canShowWorkingItemAfterUser && !showReplyingItem && !showCompactingItem` |
| `showReplyingItem` | `isAssistantReplying` | 行数阈值 > 3 |
| `showWorkingItem` | 上面两项 OR | `showWorkingItemAfterUser \|\| showInterStepProcessing` |

这导致：
- **`isAssistantReplying` 延迟 3 行才亮**：因为判断条件是 `activeStreams.lineCount > 3`，用户看到"正在回复"指示器有 3 行的视觉延迟
- **`isThinking` 是边缘触发而非状态驱动**：从 `stream_delta` 事件的数据参数推断，而非状态机
- **`showInterStepProcessing` 是 4 项布尔运算**：脆弱且隐含时间窗口依赖
- **Tab 栏的 `isStreaming` 与 `ChatPanel` 的 `sending` 逻辑不一致**：Tab 栏只检查 `status === "streaming"`，ChatPanel 还包含 `waiting_approval` 和 `paused`，造成不同步

### 3. 传输层已经支持自由扩展

`mqtt_payload.proto:370-382` 的 `SessionState.status` 字段是 `string` 类型，实际传输的是 `SessionStatus` 的 JSON 序列化字符串（`serde_json::to_string`）。扩展现有 enum 变体**不需要修改 proto 定义**。

---

## 决策：6 变体业务状态机

### 新枚举定义

```rust
/// Lifecycle status of a session, managed by Runtime as the source of truth.
///
/// ADR-014: The Runtime owns session status; the frontend is read-only.
/// ADR-049: `Streaming` is split into three semantic sub-states so the
/// frontend can derive processing phase directly from session status
/// without composing from data parameters.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum SessionStatus {
    /// Session is idle — no LLM call in progress.
    #[default]
    Idle,

    /// LLM HTTP request has been sent; waiting for the first content chunk
    /// (TTFT phase — TCP/TLS/HTTP-headers/SSE-first-chunk, can take 10-30s).
    LlmAwaitingFirstChunk,

    /// LLM is actively streaming content. The first chunk has arrived.
    /// `message_id` matches the streaming message, if available.
    LlmStreaming {
        message_id: Option<String>,
    },

    /// Tool calls have been dispatched to the tool registry; waiting for
    /// their results. This covers both parallel tool execution and
    /// special tools (ask_user_question, todo_write).
    ToolExecuting,

    /// A tool requires user approval before execution.
    WaitingApproval {
        request_id: String,
    },

    /// Iteration limit reached, debug pause, or 429 retry wait —
    /// awaiting user decision.
    Paused {
        iteration: Option<u32>,
        max_iterations: Option<u32>,
        /// 429 retry wait info. `None` for non-retry pauses.
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_info: Option<RetryPauseInfo>,
    },
}
```

### 状态机迁移图

```mermaid
stateDiagram-v2
    direction TB
    [*] --> Idle

    state "LLM 请求中" as Awaiting
    state "LLM 流式输出中" as Streaming
    state "工具执行中" as Executing

    Idle --> Awaiting: run_inner 入口 / 用户发消息
    Awaiting --> Streaming: 首个 chunk 到达
    Awaiting --> Idle: 用户 Stop / 网络错误 / 取消
    Streaming --> Idle: 文本回复完成(无工具调用)<br/>_见 ADR-035 D9.2 — `record_complete` 触发前端「预览塌缩到真实气泡」_
    Streaming --> Executing: LLM 返回工具调用
    Streaming --> WaitingApproval: 工具需要授权
    Streaming --> Paused: debug pause / 429 rate limit
    Executing --> WaitingApproval: 工具需要授权
    Executing --> Awaiting: 工具结果合并，下轮 LLM 调用开始
    Executing --> Idle: 用户 Stop / 致命错误
    WaitingApproval --> Awaiting: 授权决策后继续
    Paused --> Awaiting: 用户继续执行
    Paused --> Idle: 用户停止
```

### 状态迁移点全清单

| 文件 | 行(约) | 当前 | 改为 |
|------|--------|------|------|
| `loop_.rs` (run_inner 入口) | 653 | `Idle → Streaming` | `Idle → LlmAwaitingFirstChunk` |
| `loop_llm.rs` (首个 chunk 后) | ~99 | 无 transition | `LlmAwaitingFirstChunk → LlmStreaming` |
| `loop_tools.rs` (dispatch_and_merge 入口) | 977 | 无 transition | `LlmStreaming → ToolExecuting` |
| `loop_approval.rs` (授权后继续) | 432 | `WaitingApproval → Streaming` | `WaitingApproval → LlmAwaitingFirstChunk` |
| `loop_interaction.rs` (交互后继续) | 97 | `→ Streaming` | `→ LlmAwaitingFirstChunk` |
| `loop_.rs` (Paused→继续, 6 处) | 871/892/1019/1031/1140/1290/1294 | `Paused → Streaming` | `Paused → LlmAwaitingFirstChunk` |
| `loop_.rs` (中断→Idle, 5 处) | 882/901/932/1043/1152/1165/1174/1195/1201/1299 | `→ Idle` | `→ Idle` (不变) |
| `reliable.rs` (emit_streaming_resume) | 235 | `Streaming` | `LlmAwaitingFirstChunk` |
| `session_core.rs` (测试断言) | 1062-1095 | `matches!(Streaming)` | `matches!(LlmAwaitingFirstChunk)` |

### 协议 DTO 同步

`core/acowork-core/src/protocol.rs:1402-1414` 的 `SessionStatusDto` 必须与运行时 enum 保持 1:1 映射，因为 Gateway 在 HTTP 拉取 `GET /api/agents/{id}/sessions/{sid}/state` 时使用此结构体反序列化：

```rust
pub enum SessionStatusDto {
    Idle,
    LlmAwaitingFirstChunk,
    LlmStreaming { message_id: Option<String> },
    ToolExecuting,
    WaitingApproval { request_id: String },
    Paused { iteration: Option<u32>, max_iterations: Option<u32>, retry_info: Option<RetryPauseInfo> },
}
```

---

## 前端简化方案

### 核心原则

**前端不再从数据参数组合状态**。所有阶段信息由 `sessionStatus` 单一来源提供，`activeStream` 缓冲仅保留渲染维度的内容数据。

### Phase 映射

```mermaid
graph LR
    subgraph Backend["后端 sessionStatus"]
        Idle
        LlmAwaitingFirstChunk
        LlmStreaming
        ToolExecuting
        WaitingApproval
        Paused
    end

    subgraph Frontend["前端 Phase 枚举"]
        P_Idle["idle"]
        P_Waiting["waiting"]
        P_Streaming["streaming"]
        P_Tooling["tool_executing"]
        P_Approval["waiting_approval"]
        P_Paused["paused"]
    end

    Idle --> P_Idle
    LlmAwaitingFirstChunk --> P_Waiting
    LlmStreaming --> P_Streaming
    ToolExecuting --> P_Tooling
    WaitingApproval --> P_Approval
    Paused --> P_Paused
```

### 前端删除的变量

| 删除变量 | 替代方案 | 说明 |
|---------|---------|------|
| `isAssistantReplying` | **保留为安全阀状态**而非 UI 判据 | ADR-049 初版提议删除，但实际业务里需要它的行数 / messageId 状态来：(1) 防止 `record_complete` 丢失时 `activeStream` 无界增长（C2 安全阀）；(2) 探测 `stream_delta` 中 `messageId` 变化以重置 duration timer；(3) 驱动 `assistantStreamingStartTime` 的边缘触发字段推送 |
| `isThinking` | `phase === "streaming"` + `thinkingContent` 非空 | 状态信息由后端提供，`thinkingContent` 仅作为渲染数据保留 |
| `showInterStepProcessing` | `phase === "waiting" \|\| phase === "tool_executing"` | 直接由后端状态判断，无需组合布尔运算 |
| `showWorkingItemAfterUser` | `phase !== "idle" && lastMsgIsUser` | 简化判断，不再遍历消息列表扫描 |
| `showReplyingItem` | `phase === "streaming" && assistantStreamingContent !== ""` | 不再被 `isAssistantReplying` 行数阈值 3 阻塞；前置条件是『后端已进入 `LlmStreaming` 且首个 chunk 已积累』。这解决了原版的『用户看到"正在回复"指示器有 3 行视觉延迟』问题 |

### 前端保留的变量

| 保留变量 | 来源 | 说明 |
|---------|------|------|
| `thinkingContent` | `stream_delta` (role=thought) | 渲染数据，非状态信息 |
| `thinkingStartTime` | `stream_delta` (role=thought) | 渲染数据，非状态信息 |
| `assistantStreamingContent` | `stream_delta` (role=assistant) | **v1.1 修订**：渲染数据，非状态信息；归属 `chatAdapterStore` 作为 legacy 投影字段。trailing virtual item 已废弃（ADR-050 C5）；assistant live preview 现在通过 `adapter.blocks` 中 `isLive && type === "assistant"` block 路由到 `StreamingSourceBlock variant="assistant"` |
| `assistantStreamingStartTime` | `stream_delta` 首个 chunk for new messageId | 渲染数据；与 `thinkingStartTime` 字段对称；**v1.1 修订**归属 `chatAdapterStore`，仅供兼容消费者读取 |
| `CompactingStarted`/`CompactingEnded` | ChunkEvent | 独立事件，与本状态机正交 |

### 前端新类型定义

```typescript
// apps/acowork-desktop/src/lib/types.ts

/** ADR-049: Backend session lifecycle status — read-only, single source of truth. */
export type SessionStatus =
  | { status: "idle" }
  | { status: "llm_awaiting_first_chunk" }
  | { status: "llm_streaming"; detail?: { message_id: string | null } }
  | { status: "tool_executing" }
  | { status: "waiting_approval"; detail: { request_id: string } }
  | { status: "paused"; detail?: { iteration: number | null; max_iterations: number | null; retry_info?: { wait_ms: number; attempt: number; max_attempts: number; provider: string } } };

/** ADR-049: Frontend processing phase — derived directly from sessionStatus. */
export type ProcessingPhase =
  | "idle"              // 静默，不显示任何指示器
  | "waiting"           // LlmAwaitingFirstChunk → "正在等待 LLM 回复..."
  | "streaming"         // LlmStreaming → "正在生成回复..."
  | "tool_executing"    // ToolExecuting → "正在执行工具..."
  | "waiting_approval"  // WaitingApproval → "等待授权..."
  | "paused"            // Paused → "已暂停，点击继续"

/** Extract processing phase from session status. Single-source-of-truth mapping. */
export function getProcessingPhase(s: SessionStatus | undefined | null): ProcessingPhase {
  if (!s) return "idle";
  switch (s.status) {
    case "idle": return "idle";
    case "llm_awaiting_first_chunk": return "waiting";
    case "llm_streaming": return "streaming";
    case "tool_executing": return "tool_executing";
    case "waiting_approval": return "waiting_approval";
    case "paused": return "paused";
  }
}

/** Check if the session is actively processing (non-idle). Replaces isSessionActive. */
export function isProcessing(s: SessionStatus | undefined | null): boolean {
  return getProcessingPhase(s) !== "idle";
}

/**
 * v1.1: 替代旧 `isSessionActive()` helper。
 *
 * 旧实现（`lib/types.ts:885-888`）：
 * ```typescript
 * export function isSessionActive(s: SessionStatus | undefined | null): boolean {
 *   return s.status === "streaming" || s.status === "waiting_approval" || s.status === "paused";
 * }
 * ```
 *
 * 旧函数仅适配 4 变体 SessionStatus。本 ADR 上线后 6 变体下的"非 idle"
 * 判断必须用 `isProcessing()`：`isSessionActive` 应在所有调用点
 * （agentStore / chatStore / SessionPanel / ContextUsageIcon / ResultsPanel）
 * 替换为 `isProcessing()`，并在 `lib/types.ts` 中标记 `@deprecated`。
 */
export function isSessionActive_REPLACED(): never {
  throw new Error("isSessionActive() is removed in v1.1; use isProcessing() instead.");
}
```

### Frontend assistant live preview（实施期补遗）

> **后置补遗（2026-07-29）**：ADR-049 初版在前端简化方案中把"assistant streaming" 仅描述为"指示器占位 slot 显示一个小圆点 + Replying 文本"。 实际实施中发现该指示器没有任何 live 内容，用户在 LLM 生成整篇回复期间只能看到静态标签，而 `record_complete` 触发 HTTP refresh 后整篇 `ReactMarkdown` 一次性渲染带来显著内存尖峰（详见 memory profile）。
>
> 同时 ADR-035 D9.2 已经定义 `StreamingSourceBlock` 用 `<pre>` 直接 `textContent` 复用模式作为 `role=thought` 的内存友好流式渲染。但 assistant 侧没有同等机制，导致两个流式数据源的处理路径不对称。
>
> **v1.0 修订**（2026-07-29）：
> 1. `chatStore` 在 `stream_delta (role=assistant)` 中镜像 thought 的累积+节流模式，节流 500ms 推 `assistantStreamingContent` 到 Zustand
> 2. `VirtualMessageList` 的 trailing "replying slot" 改为渲染 `<StreamingSourceBlock variant="assistant">`，DOM 复用同 `thought` 分支
> 3. assistant 完成态仍走 `StreamMarkdown → ReactMarkdown` 渲染，**保留** markdown 格式（标题/列表/代码块/Mermaid）
>
> **v1.1 修订（2025-01-16）— 与 ADR-050 post-C5 对齐**：上述第 2 项已过时。Trailing virtual item 已在 ADR-050 C5 中废弃，改为 `adapter.blocks` 中 `isLive: true` 的 message block 路由。详见以下 v1.1 数据流图与 ADR-050 §16。

#### 数据流图（v1.1 — post-C5 修订）

```mermaid
graph LR
    subgraph Backend
        SD[stream_delta<br/>role=assistant]
        RC[record_complete]
    end

    subgraph chatAdapterStore
        AS[activeStreams Map<br/>per sid]
        LF[lastAssistantFlush<br/>500ms 节流]
        LB[liveBuffer<br/>assistantStream]
        LEG[legacy 投影字段<br/>assistantStreamingContent<br/>assistantStreamingStartTime<br/>isAssistantReplying]
    end

    subgraph chatStore
        MSG[messages[]<br/>HTTP 历史 + record_complete<br/>直接写入]
    end

    subgraph Adapter[chatListAdapter.ts]
        BLK[adapter.blocks<br/>isLive: true 标记]
    end

    subgraph UI
        VML[VirtualMessageList<br/>常规渲染循环<br/>isLive && type === "assistant"]
        SSB[StreamingSourceBlock<br/>variant=assistant]
        MB[MessageBubble<br/>完成态 ReactMarkdown]
    end

    SD -->|cumulate lines<br/>slice -5| AS
    AS -.throttle 500ms.-> LF
    LF --> LEG
    SD --> LB
    RC -->|主路径<br/>直接 append| MSG
    RC -.副作用.-> LB
    LB --> BLK
    MSG --> BLK
    BLK -->|isLive && assistant| VML
    VML --> SSB
    MSG -.同 id 气泡挂载.-> MB

    style SSB fill:#fef3c7,stroke:#f59e0b
```

**v1.1 关键变化**：
- 流式数据从 `chatStore` 全部迁出，归属 `chatAdapterStore`（`liveBuffer` + legacy 投影 + 模块级 `activeStreams` / `lastAssistantFlush` / `lastThinkingFlush`）
- `record_complete` 主路径由 `chatStore` 通过 `convertRecordCompleteToChatMessage` **直接写入 `messages[]`**（见 ADR-050 §16）；清空 stream / 投影字段是副作用
- Trailing virtual item 废弃；assistant 流式预览在 VML 渲染循环中识别 `item.isLive && msg.type === "assistant"` 的 block 后路由到 `StreamingSourceBlock variant="assistant"`。`virtualCount` 不再 `+1`

#### 节流策略（v1.1 修订）

与 `thinkingContent` 完全对称——`lastAssistantFlush: Map<sid, number>` + 500ms 节流，**位置在 `chatAdapterStore` 模块级**（非 `chatStore` 字段，也非 zustand state —— 模块级 Map 直接 mutate，配套 zustand 状态推流）。叠加 `isPinnedToBottom` 守卫——`isPinnedToBottom` 同样是 `chatAdapterStore` 的 legacy 投影字段（见 `chatAdapterStore.ts:106-119`），与 `assistantStreamingContent` 同属 C2 兼容保留。

#### DOM 复用机制

`StreamingSourceBlock` 在挂载时创建一个 `<pre>` DOM 节点；mount-to-unmount 期间不销毁。`useEffect([content])` 内直接写 `preRef.current.textContent = 新内容`——React 不参与文本内容管理，没有 AST、没有元素树、没有 reconciliation。

#### 完成态切换（v1.1 修订）

`record_complete` 触发（**主路径**）：
1. **chatStore 写 `messages[]`**：`convertRecordCompleteToChatMessage` 把完整内容追加到 `messages[]`，并通过 `mergeMessageWindow` 的 id 去重避免 HTTP 刷新重复（见 ADR-050 §16）
2. **chatAdapterStore 清空 stream（副作用）**：`ingestRecordComplete(agentId, sid, role, messageId)` 触发 `liveBuffer.thinkingStream` 或 `liveBuffer.assistantStream` 清空；legacy 投影字段（`assistantStreamingContent` / `assistantStreamingStartTime` / `isAssistantReplying`）随之重置
3. `isAssistantReplying = false` 是**纯安全阀标志翻转**，不再驱动任何 DOM 渲染（v1.1 删除「DOM 提示灯」表述，详见 §A.2 末尾）
4. 同一消息号（`messageId`）的消息气泡在 `messages[]` 中挂载，走 `StreamMarkdown → ReactMarkdown` 渲染

**trailing slot 塌缩问题已不存在**：`assistantStream` 流式预览是 `adapter.blocks` 的常规成员，与后续完整消息气泡共用 `foldMessages` 排序；ResizeObserver 不需要校正 178px → 完整气泡的跳变。

#### 与 ADR-035 D9.2 的对称性

| 维度 | thought | assistant |
|------|---------|-----------|
| 缓冲结构 | `activeStreams.lines (cap 5)` | 同上（新增 assistant 分支） |
| 节流 | 500ms / sid | 同上（独立 Map `lastAssistantFlush`） |
| Zustand 字段 | `thinkingContent` | `assistantStreamingContent` |
| 起始时间字段 | `thinkingStartTime` | `assistantStreamingStartTime` |
| 渲染组件 | `StreamingSourceBlock variant="thought"` | `StreamingSourceBlock variant="assistant"` |
| 内容上限 | 5 行 | 5 行 |
| 完成态清理路径 | `record_complete` → 清空 `thinkingContent` | `record_complete` → 清空 `assistantStreamingContent` |
| 完成态渲染 | ThinkBlock（同样 `<pre>`） | MessageBubble → StreamMarkdown → ReactMarkdown（保留 markdown） |

两个流走的代码路径完全对称（只差 label/icon）；区别仅在完成态——thought 完成态仍用 `<pre>`（无可视化格式需求），assistant 完成态用 ReactMarkdown（需要 markdown 渲染）。

### 渲染逻辑简化

**当前 `ChatPanel.tsx` 中的渲染逻辑**（简化后）：

```
phase = getProcessingPhase(sessionStatus)
working = phase !== "idle"
canShowWorkingItemAfterUser = working && lastMessageIsUser
showInterStepProcessing = working && !canShowWorkingItemAfterUser && !showReplyingItem && !showCompactingItem
showReplyingItem = phase === "streaming" && assistantStreamingContent !== ""
   // 不再用 isAssistantReplying 行数阈值 3，解决 "3 行视觉延迟" 问题
showWorkingItem = showWorkingItemAfterUser || showInterStepProcessing

tabIsActive = working
showInitialWaiting = phase === "waiting" && lastMessageIsUser
showInterStepWaiting = phase === "waiting" && !lastMessageIsUser
showToolExecuting = phase === "tool_executing"
showApproval = phase === "waiting_approval"
showPaused = phase === "paused"
```

### Tab 栏状态

当前 `SessionPanel.tsx:131-137` 的 `isStreaming` 判断：

```typescript
const isStreaming = sessionState?.sessionStatus?.status === "streaming"
  || sessionState?.sessionStatus?.status === "waiting_approval"
```

改为：

```typescript
const isActive = isProcessing(sessionState?.sessionStatus);
```

**v1.1 同步替换清单**：以下 4 个文件中 `sessionStatus.status === "..."` 的 4 变体判断必须同步改为 6 变体语义（`isProcessing()` 或 `phase !== "idle"`）：

| # | 文件 | 行 | 当前（4 变体） | 改为（6 变体） |
|---|------|---|----------------|----------------|
| 1 | `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` | 427-429 | `sending = streaming \|\| waiting_approval \|\| paused` | `sending = phase !== "idle"` |
| 2 | `apps/acowork-desktop/src/components/chat/SessionPanel.tsx` | 136-138 | `isStreaming = streaming \|\| waiting_approval \|\| paused` | `isActive = isProcessing(...)` |
| 3 | `apps/acowork-desktop/src/components/chat/ContextUsageIcon.tsx` | 98 | `isIdle = !s \|\| s.status === "idle"` | `isIdle = phase === "idle"` |
| 4 | `apps/acowork-desktop/src/components/results/ResultsPanel.tsx` | 485-488 | 4 态分别染色（streaming / idle / paused / waiting_approval） | 6 态映射：waiting / streaming / tool_executing / waiting_approval / paused 各对应一种颜色 |

Commit 4 默认仅列了 SessionPanel.tsx；其余 3 处需要在 Commit 4 PR review 中通过 `grep -n 'sessionStatus?.status === "'` 全量锁定。

---

## 实施步骤

### Commit 1：后端 enum 定义 + 迁移点

**文件**：`session_state.rs`，`loop_.rs`，`loop_llm.rs`，`loop_tools.rs`，`loop_approval.rs`，`loop_interaction.rs`，`reliable.rs`，`session_core.rs`，`observer_impl.rs`

变更：
1. 替换 `SessionStatus` 的 `Streaming` 变体为 `LlmAwaitingFirstChunk | LlmStreaming | ToolExecuting`
2. 更新所有 8 处 `transition_status(Streaming)` 写入点
3. 新增 `loop_llm.rs` 中首个 chunk 到达后的 `LlmAwaitingFirstChunk → LlmStreaming` 迁移
4. 新增 `loop_tools.rs` 中 `dispatch_and_merge_tools` 入口的 `ToolExecuting` 迁移
5. 更新 `SessionStatus::is_active()` 方法
6. 更新 `session_core.rs` 中的测试断言

### Commit 2：协议 DTO 同步

**文件**：`core/acowork-core/src/protocol.rs`

变更：将 `SessionStatusDto` 与新的 enum 定义同步。

### Commit 3：前端类型 + store 简化

**文件**：`lib/types.ts`，`chatStore.ts`

变更：
1. 更新 `SessionStatus` TypeScript 类型
2. 添加 `ProcessingPhase` 类型和 `getProcessingPhase()` 函数
3. 删除 `isAssistantReplying` 状态推导（在 `stream_delta` handler 中）
4. `isThinking` 从 store 状态字段降级为局部派生变量
5. `session_state_changed` handler 不再需要 `isAssistantReplying = false` 清理

### Commit 4：前端渲染逻辑简化

**文件**：`ChatPanel.tsx`，`SessionPanel.tsx`，`VirtualMessageList.tsx`

变更：
1. `sending` 改为 `getProcessingPhase(sessionStatus) !== "idle"`
2. `showWorkingItemAfterUser` / `showInterStepProcessing` / `showReplyingItem` 逻辑简化
3. Tab 栏 `isStreaming` 改为 `isProcessing()`
4. 废除 `isAssistantReplying` 相关的行数阈值常量

### Commit 5：ADR 文档 + Frontend assistant live preview 实施

**文件**：`docs/adr/zh/ADR-049-session-status-substates.md`，`StreamingSourceBlock.tsx`（新增），`ThinkBlock.tsx`，`chatStore.ts`，`VirtualMessageList.tsx`，`ChatPanel.tsx`，`blockLayout.ts`，**`chatAdapterStore.ts`**（v1.1 新增），**`chatListAdapter.ts`**（v1.1 新增），**`lib/paginationUtils.ts`**（v1.1 新增）

变更（2026-07-29 后置更新）：
1. 抽出通用 `StreamingSourceBlock` 组件（variant="thought"|"assistant"），`<pre>` DOM 复用模式作为默认渲染
2. `ThinkBlock` 简化为 `StreamingSourceBlock variant="thought"` 的薄包装
3. **`chatStore`**（v1.1 修订）：**不再新增** `assistantStreamingContent` / `assistantStreamingStartTime` / `lastAssistantFlush` 节流 Map —— 这些字段在 ADR-050 C2 已迁移到 `chatAdapterStore`。`stream_delta (role=assistant)` 处理也已在 chatAdapterStore 中。
4. **`record_complete`**（v1.1 修订 — 主路径）：由 `chatStore` 通过 `convertRecordCompleteToChatMessage` **直接写入 `messages[]`**（ADR-050 §16 主路径），不再走 `assistantStreamingContent` 清空逻辑。清空 stream / legacy 投影字段由 `chatAdapterStore.ingestRecordComplete` 在 chatStore 主路径之后作为副作用调用。
5. **VirtualMessageList**（v1.1 修订）：trailing replying slot 已在 ADR-050 C5 废弃。改为在常规渲染循环中识别 `item.isLive && msg.type === "assistant"` 的 block 后路由到 `StreamingSourceBlock variant="assistant"`。**不再有 pulse-dot 纯状态指示器，不再有 `virtualCount` extras 派生**。
6. `REPLYING_INDICATOR_HEIGHT`（v1.1 修订）：trailing slot 废弃后该常量失效，**Commit 5 应删除** `blockLayout.ts` 中 `REPLYING_INDICATOR_HEIGHT` 常量及其引用。
7. ADR-049 前置章节补充 ADR-035 D9.2 交叉引用；删除变量表修正（`isAssistantReplying` 保留为安全阀而非 UI 判据，**v1.1 删除「DOM 提示灯」表述**）；新增强调『Frontend assistant live preview』子节

---

## 不做事项

- **不动 compaction 状态**：`CompactingStarted/CompactingEnded` 是独立事件，与 session 状态机正交
- **不动 `Paused` 内部细节**：`iteration`/`max_iterations`/`retry_info` 已足够
- **不动 `WaitingApproval` 内部细节**：`request_id` 已足够
- **不动 MQTT wire schema**：`SessionState.status` 是 `string`，扩展无需修改 proto 定义
- **不动 `activeStream` 中 role=thought 的截断规则**（5 行限制）：thought 的 buffer 行为完全保留
- **新增 assistant 侧的 `lines (cap 5)`**：仅作为 live preview 用，与 thought 的代码路径完全对称（同样的 `slice(-5)`、同样的 500ms 节流、同样的 `useEffect → textContent` 写入），不引入额外的 trim/buffer 策略
- **不动 assistant 完成态渲染**：assistant 完成态仍走 `StreamMarkdown → ReactMarkdown`，保留标题/列表/代码块/Mermaid 等格式
- **不引入新的 i18n 文案**：新状态对应的中文文案在后续前端 PR 中添加
- **不动 `chatAdapterStore` legacy 投影字段的退役时间表**（v1.1 新增）：`chatAdapterStore.ts:106-135` 中的 legacy 投影（`isThinking` / `thinkingContent` / `assistantStreamingContent` / `assistantStreamingStartTime` / `isAssistantReplying` / `isPinnedToBottom` / `optimisticEntries`）是 ADR-050 C2 为兼容 chatStore 老消费者而保留的字段。C5 之后这些字段仅作 C2 兼容，本 ADR 不负责清理；**退役规划归 ADR-050 后续 C6+ 任务**——尤其是 `optimisticEntries` 何时彻底删除（post-C5 仅由 mount guard 引用，理论上可以删除）和 `isAssistantReplying` 安全阀逻辑是否迁回 `chatStore`（如不迁则需要单独测试）。

---

## 迁移风险

- **后端 emit 点遗漏**：通过 `grep -n 'SessionStatus::Streaming'` 锁定全清单，共 8 处写入点 + 2 处测试断言 + 2 处注释引用，无遗漏风险
- **前端 `processingPhase` 映射遗漏**：`getProcessingPhase()` 函数使用 `switch` 穷举（TypeScript 编译器检查），加新变体时编译器会提示未处理的 case
- **`sessionStatus` 4 变体判断的 4 个引用点遗漏**（v1.1 新增）：Commit 4 默认仅列 `SessionPanel.tsx`，其余 3 处（`ChatPanel.tsx:427-429` / `ContextUsageIcon.tsx:98` / `ResultsPanel.tsx:485-488`）需在 PR review 中通过 `grep -n 'sessionStatus?.status === "'` 全量锁定。详见 §"Tab 栏状态" v1.1 同步替换清单。
- **chatStore ↔ chatAdapterStore 协调遗漏**（v1.1 新增）：本 ADR 实施时如果按 v1.0 prose 在 `chatStore` 反向添加回 `assistantStreamingContent` / `lastAssistantFlush` / `stream_delta (role=assistant)` 分支处理，会与 ADR-050 C2 落地冲突。**前置校验**：实施 Commit 3/5 之前，先确认 `chatAdapterStore.ts` 中 legacy 投影字段已就位（`isAssistantReplying` / `thinkingContent` 等）；如未就位，则按 ADR-050 C2 范围 PR 先补，不在本 ADR 范围内。
- **`isSessionActive` 替换遗漏**（v1.1 新增）：旧 helper 在 `agentStore.ts:603` / `chatStore.ts:2888-2943` 等多处被引用。Commit 3 必须把这些引用点同步替换为 `isProcessing()`，并在 `lib/types.ts` 中将 `isSessionActive` 标记 `@deprecated`（不直接删除以保持外部依赖方兼容）。

---

## 附录 A：前端当前状态推导全路径

### A.1 `ChatPanel.tsx` 中的 `sending` 推导

```
sessionStatus (来自 MQTT `session_state_changed` 事件)
  ↓
sending = sessionStatus.status === "streaming"
       || sessionStatus.status === "waiting_approval"
       || sessionStatus.status === "paused"
```

### A.2 `chatAdapterStore.ts` 中的 `isAssistantReplying` 与 `assistantStreamingContent` 推导（v1.1 修订：归属迁移）

> **v1.1 修订**：原 v1.0 §A.2 标题为"`chatStore.ts` 中的 ... 推导"。ADR-050 C2 把这一推导路径从 `chatStore` 完整迁出到 **`chatAdapterStore`** 模块级 zustand + 模块级 `activeStreams` / `lastAssistantFlush` Map。`chatStore` 现在仅承担 `messages[]` 与 server 同步，stream_delta handler 仅作为入口转发给 `chatAdapterStore.ingestStreamDelta`。

```
stream_delta (MQTT `messages/stream_delta`)
  ↓
chatStore 转发 → chatAdapterStore.ingestStreamDelta(role, line)
  ↓
lines = data.lines[]
role = lines[0].role === 'assistant' ? 'assistant' : 'thought'
  ↓
if role === 'assistant':
  ├─ 边缘触发: new messageId → as.startTime = Date.now()
  ├─ 边缘触发: chatAdapterStore.setState({ assistantStreamingStartTime: as.startTime })
  ├─ as.lineCount += lines.length  (驱动 chatAdapterStore 安全阀 ASSISTANT_LINE_SAFETY_CAP, 不再驱动 UI 阈值)
  ├─ as.lines.push(...); if (as.lines.length > 5) slice(-5)  (live preview 上限)
  ├─ 边缘触发: shouldBeReplying = (lineCount > ASSISTANT_REPLYING_LINE_THRESHOLD)
  │              → chatAdapterStore.setState({ isAssistantReplying: true })  (纯安全阀)
  └─ 节流 500ms + isPinnedToBottom 守卫 (chatAdapterStore 模块级 lastAssistantFlush Map):
       content = as.lines.map(l => l.content).join('\n')
       if (content !== cur.assistantStreamingContent):
         chatAdapterStore.setState({ assistantStreamingContent: content })
         lastAssistantFlush.set(sid, now)
```

**注意（v1.1 修订）**：行数阈值 `ASSISTANT_REPLYING_LINE_THRESHOLD` (3) 在本次修订后**仅保留**用于：(a) `isAssistantReplying` 边缘翻转的兼容触发（仍驱动 Tab 栏活跃状态 / 一些列外的逻辑）；(b) **`isAssistantReplying` 仅作为 chatAdapterStore 内部的安全阀状态**——ADR-050 C5 删除 trailing virtual item 后，前端不再有任何 DOM 元素以其为判据；它的作用收敛为：在 `chatAdapterStore.ingestRecordComplete` 内检测 `record_complete 丢失` 边界（`chatAdapterStore.ts:511` 注释），并在 activeStream tracker 超过阈值时触发内部排查。`showReplyingItem` 改为 `phase === "streaming" && assistantStreamingContent !== ""`，阈值的 UX 副作用被完全消除：用户不再经历 3 行才能看到内容预览的视觉延迟。

`assistantStreamingContent`（**v1.1 已迁移到 `chatAdapterStore` 作为 legacy 投影字段**），**不是 `isAssistantReplying` 的派生量**——它直接来自 activeStream 的 `lines (cap 5)`，绕过行数阈值，front-end 可以从首个 chunk 起就显示 live preview。

### A.3 `chatAdapterStore.ts` 中的 `isThinking` 推导（v1.1 修订：归属迁移）

> **v1.1 修订**：原 v1.0 §A.3 标题为"`chatStore.ts` 中的 ... 推导"。与 §A.2 相同，ADR-050 C2 把 thought 流的累积+节流逻辑迁出到 `chatAdapterStore`。`thinkingContent` / `thinkingStartTime` 在 chatAdapterStore 中作为 legacy 投影字段保留。

```
stream_delta (MQTT `messages/stream_delta`)
  ↓
chatStore 转发 → chatAdapterStore.ingestStreamDelta(role='thought', line)
  ↓
role = lines[0].role === 'assistant' ? 'assistant' : 'thought'
  ↓
if role === 'thought' && !current.isThinking:
  chatAdapterStore.setState({
    isThinking: true,
    thinkingStartTime: Date.now(),
  })
```

### A.4 `ChatPanel.tsx` 中的 `showWorkingItemAfterUser` 推导

```
messages[] (来自 HTTP GET /messages)
  ↓
for (i = messages.length-1; i >= 0; i--):
  msg = messages[i]
  if msg.role === 'user':
    return true  // 最后一条是用户消息 → 正在等待回复
  if msg.role in ('assistant', 'thought', 'tool_call', 'tool_result', 'error'):
    return false  // 已有回复 → 不显示初始等待指示器
return false
```

### A.5 `ChatPanel.tsx` 中的 `showInterStepProcessing` 推导

```
sending = true
&& !canShowWorkingItemAfterUser  // 最后一条不是用户消息
&& !showReplyingItem             // 行数 < 3，还没亮"正在回复"
&& !showCompactingItem           // 不在 compaction 中
```

---

## 附录 B：ADR-014 状态机原文引用

ADR-014 中定义的 Session 状态原则：

1. **Runtime 是 session 状态的唯一写者**（Single Writer）
2. **前端是只读消费者**（Read-Only Consumer）
3. **每次状态变更都通过 `SessionStateChanged` 事件推送**（Event-Driven）
4. **前端不做乐观写**（No Optimistic Writes）

ADR-049 延续这四条原则，只改变"状态粒度"，不改变"状态权属"。条原则，只改变"状态粒度"，不改变"状态权属"。