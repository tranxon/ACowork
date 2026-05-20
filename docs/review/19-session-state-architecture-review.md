# ADR-014: Session State Machine — 从"事件驱动猜测"到"状态驱动只读"

## Status
Accepted

## 元信息
- 编号: ADR-014
- 日期: 2026-05-20
- 范围: Runtime + Gateway + 前端三端
- 关联: ADR-009(Session ID 显式传递), ADR-013(前端 Session 管理架构)
- 优先级: P0 — 前端 sending/streaming 状态不可靠，影响所有交互

---

## 1. 问题陈述

### 1.1 一句话总结
**整个系统中不存在 `SessionStatus` 状态机。前端用 11 个本地布尔值/可空值拼凑"猜"session 在干什么，而不是从后端读取。**

### 1.2 根因

Session ID 重构（ADR-009/013）解决了"数据串到错误 session"的问题，但只做了**路由层**的修复——WS 事件现在能正确路由到对应 session。**语义层**的问题完全未触及：前端仍然不知道一个 session "在干什么"。

| 层 | 有 Session 状态吗 | 现状 |
|---|---|---|
| `SessionState` (Runtime) | 没有 | 只有 history/conversation/loop_detector/budget_guard/turn_counter |
| `Session` (Gateway IPC) | 没有 | 只有 agent_id/authenticated/push_tx/connection_role |
| `RunningAgentInfo` (Gateway) | 没有 | 只有 connected/ready（agent 级，非 session 级） |
| `SessionInfoDto` (Protocol) | 没有 | 只有 session_id/created_at/message_count/title/corrupted |
| 前端 chatStore | 11 个分散字段拼凑 | sending, iterationLimitPaused, pendingApproval, isReasoning, streamingMessageId, isInThinkPhase, thinkingMessageId, streamBuffer, currentTurnId, isSessionInitLoading, contextUsage |

### 1.3 前端"猜测"状态的完整清单

| 前端状态 | 设置方式 | 应有的真相源 | 风险级别 |
|---------|---------|-------------|---------|
| `sending: true` | `sendMessage()` 时乐观设置 | 后端 `session_status_changed(Streaming)` | **P0** — 若消息发送失败或 WS 断连，sending 永远卡 true |
| `sending: true` (continue) | `continueExecution()` 返回 OK 后设置 | 后端 `session_status_changed(Streaming)` | **P0** — continue 请求可能失败 |
| `sending: false` (stop) | `stopCurrentMessage()` 时立即设置 | 后端 `session_status_changed(Idle)` | **P1** — stop 可能未被 Runtime 真正执行 |
| `iterationLimitPaused: null` | `continueExecution()` 返回 OK 后清除 | 后端 `session_status_changed(Streaming)` | **P1** — 同上 |
| `pendingApproval: null` | `resolveApproval()` 中立即清除 | 后端 `session_status_changed(Streaming/Idle)` | **P1** — 审批 API 可能 404 或失败 |
| `isReasoning: false` | 从 `chunk` 事件推断（几乎每个事件都重置） | 后端 `reasoning_ended` 事件 | **P2** — 窗口极短，实际几乎无用 |
| `streamingMessageId` | 从第一个 `chunk` delta 推断创建 | 后端 `stream_started { message_id }` | **P1** — 多消息交错时 ID 管理出错 |
| `isInThinkPhase` | 解析 `<thinking>`/`</thinking>` 标签 | 后端结构化事件标记 phase | **P2** — 标签跨 chunk 分裂时解析出错 |
| `SessionInfo.status` | `createSession` 硬编码 `"active"` | 后端 `SessionInfoDto.status` 字段 | **P1** — 永远不会被后端更新 |
| `sending: false` (WS close) | WS 断连时清除所有流式状态 | 后端 `session_status_changed(Idle, reason: disconnected)` | **P1** — WS 重连后状态不可恢复 |
| `sending: false` (HTTP fallback) | HTTP 发送成功/失败后设置 | N/A (HTTP fallback 无流式) | **P3** — 降级场景 |

**共 11 处前端写入，其中 6 处 P0/P1 是"乐观猜测"而非"读取真相"。**

### 1.4 症状映射

| 用户观察到的症状 | 根因 |
|---|---|
| 切换 session 后发送按钮状态不对 | `sending` 是前端本地写，切换后丢失/错位 |
| Session 列表 streaming 图标不准 | 前端 `sending` + `activeSessionId` 组合推断，不反映后端真实状态 |
| Agent 崩溃后前端永远显示"发送中" | 没有 `done`/`error` 事件，sending 永远 true |
| 工具审批后状态不确定 | 前端自己清 `pendingApproval`，不等后端确认 |
| Iteration limit pause 后 continue 可能无效 | 前端立即清 pause 状态，不等后端确认恢复 |

---

## 2. 架构分析：缺失的事件

### 2.1 现有事件 vs 缺失事件

```
现有（隐式暗示状态）:
  reasoning_started ──── "推理开始了" (但无对应 ended)
  chunk ─────────────── "有内容来了" (但无 message_id，无法区分哪条消息)
  tool_call ─────────── "工具调用" (但无 session 状态转换信号)
  tool_result ───────── "工具结果" (同上)
  done ──────────────── "完成了" (但不是状态变更，是内容结束)
  error ─────────────── "出错了" (同上)
  tool_approval_needed ─ "需要审批" (但无审批完成确认)
  iteration_limit_paused "迭代暂停" (但无恢复确认)

缺失（显式状态变更）:
  session_state_changed ─ "session 状态从 X 变为 Y" (核心事件)
  stream_started ──────── "流式消息开始，ID=xxx" (替代前端乐观 sending=true)
  reasoning_ended ─────── "推理阶段结束" (替代前端从 chunk 推断)
  approval_resolved ───── "审批已处理，session 恢复" (替代前端 resolveApproval 立即清除)
  session_resumed ─────── "迭代暂停后恢复" (替代前端 continueExecution 乐观设置)
```

### 2.2 关键设计决策：状态枚举 vs 独立事件

| 方案 | 描述 | 优点 | 缺点 |
|------|------|------|------|
| **A. 单一 `session_state_changed` 事件** | 每次状态转换发一个事件，携带新旧状态 | 前端逻辑极简——一个 switch 搞定；状态机有强保证 | 需要后端维护严格状态机；所有中间层需要传递 |
| **B. 多个独立事件** | stream_started, reasoning_ended, approval_resolved 等独立事件 | 增量改动小；可与现有事件共存 | 前端仍需拼凑状态；可能不一致 |
| **C. A+B 混合** | `session_state_changed` 作为主事件 + 保留现有内容事件 | 前端用 state_changed 驱动 UI，内容事件驱动渲染 | 事件冗余；需保证一致性 |

**推荐方案 A**。理由：
1. 前端当前问题的根因就是"分散推断"——独立事件方案延续了这个问题
2. 状态机是 Session 级的，粒度正确
3. 内容事件（chunk/tool_call 等）仍然保留用于渲染，但不用于状态判断

---

## 3. 提议的 Session 状态机

### 3.1 状态枚举

```rust
/// Session 的运行时状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionStatus {
    /// 空闲，等待用户输入
    Idle,

    /// 正在流式处理（LLM 生成 / 工具执行）
    Streaming {
        /// 当前流式消息 ID（前端可用来关联 streamingMessageId）
        message_id: Option<String>,
    },

    /// 等待用户审批工具执行
    WaitingApproval {
        request_id: String,
        tool_name: String,
    },

    /// 迭代限制暂停，等待用户 continue
    Paused {
        iteration: u32,
        max_iterations: u32,
    },

    /// Session 已结束/销毁
    Ended,
}
```

### 3.2 状态转换图

```
                    ┌──────────┐
                    │   Idle   │ ◄─────────────────────────────┐
                    └────┬─────┘                               │
                         │ chat_message / continue_execution    │
                         ▼                                      │
                 ┌──────────────┐                               │
                 │   Streaming  │───────────────────────────────┤
                 │ {message_id} │ done / error                  │
                 └──┬───────┬───┘                               │
                    │       │                                   │
     tool_approval │       │ iteration_limit                    │
      _needed      │       │                                    │
                    ▼       ▼                                    │
          ┌─────────────┐  ┌─────────┐                          │
          │WaitingApprov│  │ Paused  │                          │
          │ {request_id}│  │{iter,max}│                         │
          └──────┬──────┘  └────┬────┘                          │
                 │              │ continue_execution             │
                 │ approval     │                                │
                 │ resolved     └──────────── ──┐                │
                 │                             │                │
                 ▼                             ▼                │
                 ┌──────────────┐                               │
                 │   Streaming  │───────────────────────────────┘
                 └──────────────┘

    任何状态 ──session_destroyed──► Ended
```

### 3.3 转换规则

| 从 | 到 | 触发 | 事件携带 |
|---|---|---|---|
| Idle | Streaming | 收到 chat_message / continue_execution | `message_id` (首个 LLM call 的 ID) |
| Streaming | Streaming | tool_call → tool_result 循环 | 更新 `message_id` |
| Streaming | WaitingApproval | ChunkEvent::ToolApprovalNeeded | `request_id`, `tool_name` |
| Streaming | Paused | ChunkEvent::IterationLimitPaused | `iteration`, `max_iterations` |
| Streaming | Idle | ChunkEvent::Done / Error | 无 |
| WaitingApproval | Streaming | ApprovalDecision(approve=true) | 新 `message_id` |
| WaitingApproval | Idle | ApprovalDecision(approve=false) | 无 |
| Paused | Streaming | ContinueExecution | 新 `message_id` |
| 任何 | Ended | session_destroyed | 无 |

---

## 4. 实现方案

### 4.1 三端改动总览

```
┌───────────────────────────────────────────────────────────────┐
│ Runtime                                                       │
│  1. SessionState 加 status: SessionStatus 字段               │
│  2. AgentLoop.run() 的每个状态转换点调用                      │
│     session_state.set_status() + emit SessionStateChanged    │
│  3. 新增 ChunkEvent::SessionStateChanged 变体               │
│  4. SessionInfoDto 加 status 字段                            │
├───────────────────────────────────────────────────────────────┤
│ Gateway                                                       │
│  5. BridgeEventType 加 SessionStateChanged 变体              │
│  6. SessionInfoResponse 加 status 字段                       │
│  7. list_sessions API 返回含 status 的 session 列表          │
├───────────────────────────────────────────────────────────────┤
│ Frontend                                                      │
│  8. SessionChatState 用 sessionStatus: SessionStatus 替代    │
│     sending + iterationLimitPaused + pendingApproval         │
│  9. 删除所有 sending=true 本地写入，改为从后端事件读取        │
│ 10. SessionPanel 从 sessionStatus 派生 streaming 图标        │
│ 11. ChatPanel 从 sessionStatus 派生发送按钮状态              │
└───────────────────────────────────────────────────────────────┘
```

### 4.2 Runtime 改动详情

#### 4.2.1 SessionState 加 status 字段

```rust
// rollball-runtime/src/agent/session_state.rs

pub struct SessionState {
    pub(crate) status: SessionStatus,  // 新增
    pub(crate) history: HistoryManager,
    pub(crate) conversation: Option<ConversationSession>,
    pub(crate) loop_detector: LoopDetector,
    pub(crate) budget_guard: BudgetGuard,
    pub(crate) turn_counter: u32,
    pub(crate) deferred_inbound: Vec<InboundMessage>,
}

impl SessionState {
    pub fn get_status(&self) -> &SessionStatus {
        &self.status
    }

    /// 设置状态并返回是否发生了变化
    pub fn set_status(&mut self, new_status: SessionStatus) -> bool {
        if self.status != new_status {
            self.status = new_status;
            true
        } else {
            false
        }
    }
}
```

#### 4.2.2 AgentLoop 状态转换点

| AgentLoop 中的位置 | 当前行为 | 新增行为 |
|---|---|---|
| `run_inner()` 收到 `InboundMessage::ChatMessage` | 直接开始处理 | `session.set_status(Streaming)` + emit `SessionStateChanged` |
| `execute_single_iteration()` 开始 LLM call | 无状态标记 | 已是 Streaming，无需额外操作 |
| `execute_tools_parallel()` 开始 | 无状态标记 | 已是 Streaming，无需额外操作 |
| `await_approval_decision()` 进入等待 | 发 `ChunkEvent::ToolApprovalNeeded` | `session.set_status(WaitingApproval)` + emit `SessionStateChanged` |
| `await_approval_decision()` 返回 | 无状态标记 | `session.set_status(Streaming)` + emit `SessionStateChanged` |
| 迭代限制暂停 | 发 `ChunkEvent::IterationLimitPaused` | `session.set_status(Paused)` + emit `SessionStateChanged` |
| `ContinueExecution` 到达 | 无状态标记 | `session.set_status(Streaming)` + emit `SessionStateChanged` |
| `ChunkEvent::Done` | 发 Done 事件 | `session.set_status(Idle)` + emit `SessionStateChanged` |
| `ChunkEvent::Error` | 发 Error 事件 | `session.set_status(Idle)` + emit `SessionStateChanged` |
| `Interrupted` | 发 Interrupted 事件 | `session.set_status(Idle)` + emit `SessionStateChanged` |
| Debug `await_debug_resume()` 返回 Paused | AgentLoop 轮询 DebugController | `session.set_status(Paused)` + emit `SessionStateChanged`（与 iteration limit paused 共用状态） |
| Debug `debugger.resume` | DebugController Running | 已在 Streaming，无需额外操作 |
| Debug `debugger.stop` | DebugController Stopped | `session.set_status(Idle)` + emit `SessionStateChanged` |

#### 4.2.3 新增 ChunkEvent 变体

```rust
pub enum ChunkEvent {
    // ... 现有变体 ...

    /// Session 状态变更事件
    SessionStateChanged {
        session_id: String,
        old_status: SessionStatus,
        new_status: SessionStatus,
    },
}
```

#### 4.2.4 SessionInfoDto 加 status

```rust
// rollball-core/src/protocol.rs

pub struct SessionInfoDto {
    pub session_id: String,
    pub created_at: String,
    pub message_count: u32,
    pub title: Option<String>,
    pub corrupted: bool,
    pub status: SessionStatus,  // 新增
}
```

### 4.3 Gateway 改动详情

#### 4.3.1 BridgeEventType 加 SessionStateChanged

```rust
pub enum BridgeEventType {
    // ... 现有变体 ...
    SessionStateChanged,  // 新增
}
```

#### 4.3.2 WS 消息格式

前端收到的 `session_state_changed` 事件格式：

```json
{
  "type": "session_state_changed",
  "session_id": "sess_xxx",
  "old_status": { "status": "idle" },
  "new_status": { "status": "streaming", "message_id": "msg_xxx" }
}
```

#### 4.3.3 SessionInfoResponse 加 status

```rust
pub struct SessionInfoResponse {
    pub session_id: String,
    pub created_at: String,
    pub message_count: u32,
    pub title: Option<String>,
    pub status: SessionStatus,  // 新增
}
```

### 4.4 前端改动详情

#### 4.4.1 SessionChatState 重构

```typescript
// Before: 分散的布尔值
interface SessionChatState {
  messages: Message[];
  streamingMessageId: string | null;
  streamBuffer: string;
  thinkingMessageId: string | null;
  isInThinkPhase: boolean;
  isReasoning: boolean;
  iterationLimitPaused: { iteration: number; maxIterations: number; message: string } | null;
  pendingApproval: ToolApprovalNeededEvent | null;
  tokenUsage: TokenUsage | null;
  contextUsage: ContextUsage | null;
  currentTurnId: string | null;
  lastAccessed: number;
}

// After: 状态驱动
type SessionStatus =
  | { status: "idle" }
  | { status: "streaming"; message_id?: string }
  | { status: "waiting_approval"; request_id: string; tool_name: string }
  | { status: "paused"; iteration: number; max_iterations: number }
  | { status: "ended" };

interface SessionChatState {
  messages: Message[];
  sessionStatus: SessionStatus;         // 替代 sending + iterationLimitPaused + pendingApproval
  streamingMessageId: string | null;    // 保留，由 chunk 事件驱动（渲染用，非状态用）
  streamBuffer: string;                 // 保留，流式聚合用
  thinkingMessageId: string | null;     // 保留，渲染用
  isInThinkPhase: boolean;              // 保留，渲染用
  isReasoning: boolean;                 // 保留，渲染用
  tokenUsage: TokenUsage | null;
  contextUsage: ContextUsage | null;
  currentTurnId: string | null;
  lastAccessed: number;
}
```

#### 4.4.2 AgentState 中 sending 替换

```typescript
// Before
interface AgentState {
  sending: boolean;
  // ...
}

// After: sending 由 sessionStatus 派生
// AgentState 不再需要 sending 字段
// 发送按钮状态 = 当前活跃 session 的 sessionStatus.status !== "idle"
```

#### 4.4.3 事件处理重构

```typescript
// Before: sendMessage 乐观设置
sendMessage: () => {
  set(state => updateAgentAndSession(state, agentId, { sending: true }, sessionId, {...}));
  ws.send(JSON.stringify({ type: "message", content, session_id }));
}

// After: 只发消息，不设置状态
sendMessage: () => {
  set(state => updateSessionState(state, agentId, sessionId, {
    messages: [...messages, userMsg],
    currentTurnId: null,
  }));
  ws.send(JSON.stringify({ type: "message", content, session_id }));
  // sending 由后端 session_state_changed 事件驱动
}

// 新增: session_state_changed 事件处理
case "session_state_changed": {
  const { session_id, new_status } = data;
  if (session_id) {
    set(state => updateSessionState(state, agentId, session_id, {
      sessionStatus: new_status,
    }));
  }
  break;
}
```

#### 4.4.4 UI 派生逻辑

```typescript
// 发送按钮: stop vs send
const isStreaming = sessionStatus?.status !== "idle" && sessionStatus?.status !== "ended";

// Session 列表: streaming 图标
const isSessionStreaming = session.sessionStatus?.status === "streaming";

// 审批面板
const isWaitingApproval = sessionStatus?.status === "waiting_approval";

// 迭代暂停
const isPaused = sessionStatus?.status === "paused";
```

---

## 5. 一次性解决的问题清单

| # | 问题 | 现状 | ADR-014 后 |
|---|------|------|-----------|
| 1 | 切换 session 后发送按钮状态不对 | sending 是前端本地写，切换后丢失 | 从 sessionStatus 读取，切换后自动正确 |
| 2 | Session 列表 streaming 图标不准 | sending + activeSessionId 组合推断 | sessionStatus.status === "streaming"，精确 |
| 3 | Agent 崩溃后前端永远显示"发送中" | 无 done/error 事件则 sending 永远 true | Runtime 崩溃 → WS 断开 → Gateway 发 session_state_changed(idle) 或前端重连后 list_sessions 查询 |
| 4 | 工具审批后状态不确定 | 前端自己清 pendingApproval | 后端发 session_state_changed(streaming/idle) |
| 5 | continue 后 iterationLimitPaused 立即清 | 前端乐观清，不等后端确认 | 后端发 session_state_changed(streaming) |
| 6 | SessionInfo.status 永远是 "active" | 前端硬编码 | 后端返回真实 status |
| 7 | stop 可能未生效但前端已清 sending | 前端乐观清 | 后端确认后才变更状态 |
| 8 | 多客户端/多窗口状态不同步 | 无 session 级广播 | session_state_changed 通过 WS 广播给所有连接 |
| 9 | WS 重连后状态不可恢复 | 前端清除所有流式状态 | 重连后 list_sessions 返回各 session 当前 status |
| 10 | pendingApproval 与 permissionStore 双重状态 | chatStore 和 permissionStore 各维护一份 | sessionStatus 统一管理，permissionStore 仅管权限判断 |

---

## 6. Trade-off 分析

### 6.1 增加的复杂度

| 复杂度 | 位置 | 评估 |
|--------|------|------|
| 状态枚举定义 | rollball-core/protocol.rs | 新增 ~30 行，一次性 |
| SessionState.set_status() | session_state.rs | 新增 ~15 行 |
| AgentLoop 状态转换点 | loop_.rs | ~10 处插入点，每处 3-5 行 |
| ChunkEvent 新变体 | loop_.rs | 1 个新变体 |
| Gateway BridgeEventType | routes.rs | 1 个新变体 |
| Gateway SessionInfoResponse | chat.rs | 1 个字段 |
| 前端事件处理 | chatStore.ts | 1 个新 case + 删除 6 处乐观写入 |
| 前端类型 | types.ts | SessionStatus 类型定义 |

**总计**: Runtime ~60 行, Gateway ~20 行, 前端 ~-50 行(删的多于加的)

### 6.2 向后兼容

- `session_state_changed` 是新事件，现有事件不受影响
- 前端可渐进迁移：先监听新事件，旧逻辑作为 fallback
- `SessionInfoDto.status` 是新增字段，不影响现有反序列化

### 6.3 给出什么

- **前端变成只读视图层** — 状态由后端驱动，前端只渲染
- **所有 session 状态问题一次性根除** — 不再"头痛医头"
- **Session 列表可直接显示状态** — 无需前端推断
- **多客户端天然同步** — WS 广播 session_state_changed

### 6.4 失去什么

- **前端 sending 响应延迟** — 从"点击即反馈"变成"等后端确认后反馈"
  - 缓解：前端可在 sendMessage 后立即显示 loading spinner（本地 UI 状态），但 sending（业务状态）等后端
  - 或者：前端保留一个极简的 `pending: boolean`（纯 UI 层，表示"我发了消息但还没收到后端状态确认"），收到第一个 session_state_changed 后清除
- **实现工作量** — 三端联动，需要协调

---

## 7. 实施计划

### Phase 1: Runtime 状态机（后端先行）

1. 定义 `SessionStatus` 枚举（rollball-core/src/protocol.rs）
2. `SessionState` 加 `status` 字段 + `set_status()` 方法
3. `ChunkEvent` 加 `SessionStateChanged` 变体
4. AgentLoop 的 10 个转换点插入 `set_status()` + emit
5. `SessionInfoDto` 加 `status` 字段
6. 单元测试：状态转换覆盖

### Phase 2: Gateway 传递层

7. `BridgeEventType` 加 `SessionStateChanged`
8. `SessionInfoResponse` 加 `status` 字段
9. `list_sessions` API 返回含 status 的列表（Pull 修复的数据基础）
10. action string 映射（`"session_state_changed"` → `BridgeEventType::SessionStateChanged`）

### Phase 3: 前端重构

11. 定义前端 `SessionStatus` 类型
12. `SessionChatState` 加 `sessionStatus`，删除 `sending`（AgentState 层）
13. 新增 `session_state_changed` 事件处理（Push 主路径）
14. 删除 6 处乐观写入
15. UI 派生逻辑替换
16. SessionPanel 从 sessionStatus 派生图标
17. `resolveApproval` / `continueExecution` 不再设置状态

### Phase 4: Pull 修复 + 清理

18. `fetchSessions` 增强：用返回的 status 修正本地 sessionStatus（Pull 修复核心）
19. `switchSession` 增强：切换时触发 fetchSessions
20. `connectStream` 增强：WS 连接/重连时触发 fetchSessions
21. 删除 `sending` 字段及相关代码
22. 删除 `pendingApproval` 双重维护（统一为 sessionStatus）
23. `SessionInfo.status` 从后端读取，不再硬编码
24. E2E 测试

---

## 8. 开放问题决议

| # | 问题 | 决议 | 理由 |
|---|------|------|------|
| 1 | 前端 `sendMessage` 后到收到 `session_state_changed(Streaming)` 之间的延迟如何处理？ | **B. 无特殊处理** | 延迟通常 <100ms，先观察效果再决定是否加 pending UI |
| 2 | `streamingMessageId` 是否也由后端在 `SessionStatus::Streaming { message_id }` 中提供？ | **A. 是，前端不再推断** | 后端在 `SessionStatus::Streaming { message_id }` 中携带 message_id，前端直接用 |
| 3 | 一个 Agent 多 Session 并发处理是否支持？ | **已支持** — 见下方详细分析 | 架构已具备，SessionStatus 天然 per-session |
| 4 | WS 重连后状态恢复策略？ | **B. 批量恢复** | 后端只推送状态变化的 session，避免全量刷新 |

### 问题 3 详细分析：多 Session 并发

**结论：Runtime 已经支持同一 Agent 的多 Session 并发处理。**

**架构证据**：

```
SessionManager
├── HashMap<SessionId, SessionHandle>     ← 多个会话并存
├── current_session_id: String            ← 仅用于默认路由，不限制并发
└── send_to_session(session_id, msg)      ← 按 ID 定向投递

SessionTask (per session, 独立 tokio task) ← 每个 session 独立 task
├── agent_loop: AgentLoop                 ← 独立的 per-session AgentLoop
├── inbound_rx: mpsc::Receiver            ← 独立消息队列
└── run() → agent_loop.run()              ← 串行处理本 session 消息

AgentLoop (per session)
├── session: SessionState                 ← 独立的历史/预算/循环检测
└── core: AgentCore                       ← 共享（Arc）Provider/Tools/Config
```

**执行路径**：Session A 正在 streaming 时，Session B 收到 chat_message → `resolve_target_session("B")` → `send_to_session("B", msg)` → Session B 的独立 tokio task 调 `agent_loop.run()` → A 和 B 并发运行，互不阻塞。

**对 SessionStatus 的影响**：
- `SessionStatus` 是 `SessionState` 的字段 → 天然 per-session
- `SessionStateChanged` 事件携带 `session_id` → 前端按 session_id 路由到正确的 `sessionStates[sessionId]`
- 多个 session 可以同时处于 `Streaming` 状态 → 前端可同时显示多个 streaming 图标
- 无需额外改动，架构天然兼容

**唯一限制**：同一 Session 内部消息串行（`SessionTask::run()` 的 loop 是 await 串行的），这是有意设计，保证消息顺序。

---

## 8.5 Debug 模式双通道分析

### 8.5.1 双通道架构

Debug 模式下前端同时持有**两条独立的 WebSocket 连接**：

```
Channel A (Chat):  Frontend ──WS──→ Gateway ──gRPC──→ Runtime
                   chatStore, ws://{gw}/api/agents/{id}/stream
                   传输: ChunkEvent (chunk, tool_call, done, error, ...)

Channel B (Debug): Frontend ──WS──→ Runtime (直连)
                   debugStore, ws://127.0.0.1:{debug_port}
                   传输: DebugEvent (onStep, onStateChange, onExecutionStateChange)
                   协议: JSON-RPC 2.0
```

**Gateway 不代理 Debug WebSocket**——前端直连 Runtime 的 `127.0.0.1:{debug_port}`。

### 8.5.2 SessionStateChanged 应走哪条通道？

**结论：仅走 Channel A（Gateway 通道）。**

理由：

| 选项 | 描述 | 评估 |
|------|------|------|
| A. 仅 Channel A | `SessionStateChanged` 作为 ChunkEvent 发给 Gateway，再转发前端 | **推荐** — session 生命周期管理是 Gateway 职责域；Chat WS 已经携带所有 session 级事件（done/error/tool_approval_needed 等） |
| B. 仅 Channel B | `SessionStateChanged` 作为 DebugEvent 发给前端 | 错误 — Debug WS 不经过 Gateway，Gateway 无法感知 session 状态，且非 debug 模式下不存在此通道 |
| C. 双通道都发 | 两个通道都发送 | 冗余 — 增加一致性问题；且 Debug WS 是可选的 |

### 8.5.3 Debug 模式的暂停与 SessionStatus 的关系

Debug 模式有 `DebugController` 管理暂停（Paused/Stepping/Running/Stopped），这与 `SessionStatus::Paused` 是不同层次的暂停：

| 暂停类型 | 触发 | 通道 | 恢复方式 |
|---------|------|------|---------|
| `SessionStatus::Paused` | 迭代限制暂停 | Channel A (Chat WS) | `continue_execution` API |
| `DebugState::Paused` | 调试器断点/手动暂停 | Channel B (Debug WS) | `debugger.resume` JSON-RPC |
| `DebugState::Stepping` | 单步执行后自动暂停 | Channel B (Debug WS) | 自动执行一步后暂停 |

**设计选择**：Debug 暂停也触发 `SessionStatus::Paused`。

- 当 `await_debug_resume()` 进入 Paused/Stepping 时 → `session.set_status(Paused)`
- 当 `debugger.resume` 恢复执行时 → `session.set_status(Streaming)`
- 前端无需区分"哪种暂停"，统一显示为 Paused 状态

### 8.5.4 Debug 模式下的 Pull 修复

Debug 模式下前端仍通过 Channel A 的 `fetchSessions` 做 Pull 修复。Debug WS 不影响 Pull 路径——`list_sessions` API 走 Gateway HTTP，与 Debug WS 无关。

唯一额外注意：Debug 的 `rewind` 操作会截断 Runtime 侧会话历史，但 Gateway 持久化的消息不变。当前 `debugStore.rewind()` 通过 `chatStore.trimMessagesTo()` 同步了前端显示。Pull 修复时 `list_sessions` 返回的 `message_count` 可能与前端不一致——但这是已有问题，非 ADR-014 引入。

---

## 9. Push + Pull 双保险：事件丢失容错机制

### 9.1 风险

纯 push 架构（前端完全依赖 `session_state_changed` WS 事件）存在事件丢失风险：

| 场景 | 后果 |
|------|------|
| WS 连接短暂断开期间状态变更 | 前端不知道 session 从 Idle 变成了 Streaming |
| Gateway 转发丢包（网络抖动） | 前端永远停在旧状态 |
| Runtime 发事件后崩溃，WS 重连后无法补发 | 前端卡在 Streaming 但实际已 Idle |
| 前端 JS 事件循环阻塞（长渲染） | WS 消息积压甚至被丢弃 |

如果 `session_state_changed(Idle)` 丢失，前端会**永远卡在 Streaming**——和现在 `sending` 卡 true 的问题一样。

### 9.2 设计原则：Push for speed, Pull for correctness

```
正常路径（push）:
  后端状态变更 → WS session_state_changed → 前端立即更新
  延迟 ~10-50ms，实时性好

修复路径（pull）:
  确定性时机 → 前端 GET /api/agents/:id/sessions → 用后端 status 修正本地
  延迟 ~50-200ms，只在关键时刻触发，保证最终一致
```

### 9.3 Pull 触发时机

| 时机 | 为什么是"确定性"的 | 拉取方式 |
|------|-------------------|---------|
| **Session 切换** | 用户明确要离开当前 session 进入另一个，此时必须确保目标 session 状态准确 | `list_sessions` 刷新所有 session 的 status |
| **Agent 启动/WS 首次连接** | 冷启动，前端完全没有状态 | `list_sessions` 初始化所有 session 的 status |
| **WS 重连** | 断连期间可能丢失多个状态变更事件 | `list_sessions` 批量修复 |
| **用户手动刷新** | 兜底手段 | `list_sessions` |
| **打开 Session 列表下拉菜单** | 用户即将做选择，列表上的状态必须准确 | `list_sessions`（已有 `fetchSessions` 调用） |

**不做的时机**：
- ~~定时轮询~~ — 不需要，push 覆盖了 99.9% 场景
- ~~每次 chunk 事件后校验~~ — 过于频繁，无意义

### 9.4 实现方案

#### 9.4.1 `list_sessions` API 已天然支持

`list_sessions` 返回 `SessionInfoResponse[]`，ADR-014 Phase 2 已计划加 `status` 字段。前端只需在现有 `fetchSessions()` 调用后，用返回的 `status` 修正本地 `sessionStatus`。

```typescript
// sessionStore.ts - fetchSessions 增强
fetchSessions: async (agentId: string) => {
  const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions`);
  const data = await resp.json();
  const sessions = data.sessions ?? [];

  // 用后端返回的 status 修正本地 sessionStatus（pull 修复）
  const chatStore = useChatStore.getState();
  for (const session of sessions) {
    if (session.status) {
      const sessionState = chatStore.getSessionState(agentId, session.session_id);
      if (sessionState && JSON.stringify(sessionState.sessionStatus) !== JSON.stringify(session.status)) {
        // 状态不一致，以后端为准
        chatStore.updateSessionStatus(agentId, session.session_id, session.status);
      }
    }
  }

  set({ sessions });
},
```

#### 9.4.2 切换 Session 时触发 Pull

```typescript
// sessionStore.ts - switchSession 增强
switchSession: async (agentId, sessionId) => {
  // 1. 先设 state（保持 UI 响应）
  set({ currentSessionId: sessionId });

  // 2. Pull 修复目标 session 状态
  await get().fetchSessions(agentId);  // 已含 status 修正逻辑

  // 3. 激活 session（best-effort）
  await activateAPI(agentId, sessionId);
},
```

#### 9.4.3 WS 连接/重连时触发 Pull

```typescript
// chatStore.ts - connectStream 增强
const ws = new WebSocket(url);
ws.onopen = () => {
  // ... 现有逻辑 ...

  // Pull 修复：WS 连接建立后刷新 session 状态
  const { fetchSessions } = useSessionStore.getState();
  fetchSessions(agentId);
};
```

### 9.5 一致性保证模型

```
┌─────────────────────────────────────────────┐
│              最终一致性 (Eventual)           │
│                                             │
│  Push (实时):  session_state_changed  ──→   │
│               延迟 ~10-50ms，覆盖 99.9%     │
│                                             │
│  Pull (修复):  确定性时机 fetchSessions ──→  │
│               延迟 ~50-200ms，修复 0.1%      │
│                                             │
│  保证: 任何 session 状态在前端               │
│  最多延迟到下一个确定性时机才可能不一致       │
└─────────────────────────────────────────────┘
```

| 场景 | Push 是否覆盖 | Pull 修复时机 | 最大不一致窗口 |
|------|-------------|-------------|--------------|
| 正常 streaming | ✅ | N/A | ~50ms |
| WS 丢包 | ❌ | 切换 session / 重连 / 打开列表 | 用户下次操作 |
| WS 断连 | ❌ | 重连时 | 断连时长 |
| Runtime 崩溃 | ❌ | WS 断连 → 重连 → fetchSessions | 断连时长 |

### 9.6 新增 API 端点（可选增强）

如果未来需要更细粒度的状态查询，可增加：

```
GET /api/agents/:id/sessions/:sid/status
→ { status: SessionStatus }
```

但 **Phase 1 不需要**——`list_sessions` 已含 status，前端只需在确定性时机调用即可。只有当 session 数量极大（>100）导致 `list_sessions` 过重时才考虑单查。

---

## Consequences

**变容易的**:
- 前端状态管理大幅简化——一个 `sessionStatus` 替代 3 个布尔值/可空值
- 所有 session 状态相关 bug 一次性根除
- Session 列表可直接显示 streaming/waiting/paused 状态
- 多客户端状态同步天然解决
- 未来扩展（如 session 排队、优先级）有明确的状态机基础

**变困难的**:
- AgentLoop 的每个执行路径都需要正确维护状态机（遗漏转换点会导致前端卡住）
- 调试时需要追踪状态转换序列而非单个布尔值
- 前端发送消息后的即时反馈需要额外处理
