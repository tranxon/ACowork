# 13 — 前端 Session 管理架构 Review 与重构方案

**日期**: 2026-05-20
**审查范围**: `apps/rollball-desktop/src/` 下所有 session/conversation 相关代码
**问题**: Session 切换时数据串流 — 切换 session 期间 WS 事件写入错误的 session

---

## 一、问题复现路径

1. Session A 正在流式输出（chunk/tool_call 事件持续到达）
2. 用户点击切换到 Session B
3. Session B 显示了 Session A 的消息片段 → **数据串流**

---

## 二、根因分析（3 层）

### P0-根因：`switchSession` 中 `currentSessionId` 更新时机错误

**文件**: `sessionStore.ts:86-118`

```typescript
// 当前代码（有问题）
switchSession: async (sessionId, agentId) => {
  if (sessionId === currentSessionId) return;        // ①
  chatStore.abortSessionLoad();                       // ②
  if (agentId) chatStore.clearMessages(agentId);      // ③
  await fetch(activate API);                          // ④ ← 漏洞窗口！
  set({ currentSessionId: sessionId });               // ⑤
}
```

**时序问题**：
- ③ `clearMessages()` 清空了 `agentStates[agentId]` 的所有消息
- ④ `await activate API` 期间，`currentSessionId` 仍是旧值
- 此时 WS 事件到达 `handleMessageEvent`，session_id 过滤器检查 `currentSessionId`
- 旧 session 的事件通过过滤 → 写入刚被清空的 `agentStates[agentId]`
- ⑤ 才设置新 `currentSessionId`，但脏数据已经写入

**漏洞窗口** = activate API 的网络延迟（通常 50-500ms）

### P1-架构问题：`agentStates[agentId]` 无 session 边界

**文件**: `chatStore.ts:37-57`

`AgentChatState` 以 `agentId` 为 key，同一 agent 的所有 session 共享同一个状态槽。切换 session 时：

1. `clearMessages()` 销毁旧 session 的全部运行时状态
2. 新 session 必须等待 `loadSessionMessages` 完成
3. 如果切换回来，旧 session 的流式状态已丢失，必须重新加载

**本质**：缺少 `agentId → sessionId → ChatState` 的二级映射。

### P1-架构问题：全局状态未按 agent/session 隔离

| 全局字段 | 应归属 | 当前问题 |
|----------|--------|----------|
| `currentModel` / `currentProvider` | `agentModels[agentId]` | 切换 agent 时短暂显示错误模型 |
| `sending` | `agentStates[agentId]` | 切换 agent 时可能误置 |
| `sessionAllowed` (permissionStore) | per-session Set | session A 允许的工具在 session B 也自动允许 |
| `fetchSessionId` (module let) | sessionStore 内部 | 模块级可变状态，不受 Zustand 管理 |
| `reconnectState` (module let) | chatStore 内部 | 同上 |
| `persistedTitles` (module let) | chatStore 内部 | 同上，且永不清理 |
| `lastInitAgentId` / `lastLoadedSessionId` | ChatPanel module let | 组件协调逻辑散落在 store 外部 |

---

## 三、已有防护机制评估

| 机制 | 有效性 | 盲区 |
|------|--------|------|
| `loadSequence` 递增 | ✅ 防过期 HTTP 响应 | ❌ 不防 WS 事件 |
| `AbortController` | ✅ 取消进行中 HTTP | ❌ 不防 WS 事件 |
| `session_id` 过滤 | ✅ 设计意图正确 | ❌ 在 switchSession 漏洞窗口内失效 |
| WebSocket 引用比较 | ✅ 防旧 ws 回调 | ❌ 同一 agent 的 ws 不变，无法区分 |
| `streamingMessageId` 检查 | ✅ 跳过流式中加载 | ❌ 不防新事件写入 |

**结论**：已有机制覆盖了 HTTP 竞态，但 WS 竞态是盲区。session_id 过滤器是唯一防线，但它在 `switchSession` 的异步窗口内失效。

---

## 四、重构方案

### 方案选型：渐进式修复 vs 全面重构

| 维度 | 渐进式修复 | 全面重构 |
|------|-----------|----------|
| 改动量 | ~5 文件，~50 行 | ~8 文件，~300 行 |
| 风险 | 低，只改时序和加锁 | 中，状态结构变化 |
| 收益 | 修复 P0，缓解 P1 | 根治所有层级问题 |
| 测试 | 现有测试 + 手动验证 | 需重写部分测试 |

**推荐策略**：先做渐进式修复（Phase 1），再做全面重构（Phase 2）。Phase 1 立即消除数据串流 bug，Phase 2 消除架构债务。

---

### Phase 1：渐进式修复（P0 止血，3 处改动）

#### 改动 1：`switchSession` 先设 state，再发 API

**文件**: `sessionStore.ts:86-118`

```typescript
switchSession: async (sessionId, agentId) => {
  if (sessionId === useSessionStore.getState().currentSessionId) return;

  // ① 立即更新 currentSessionId — 关闭 WS 事件漏洞窗口
  set({ currentSessionId: sessionId });

  // ② 取消进行中的加载
  useChatStore.getState().abortSessionLoad();

  // ③ 清空旧消息（此时 WS 事件已被新 sessionId 过滤）
  if (agentId) {
    useChatStore.getState().clearMessages(agentId);
  }

  // ④ 异步通知后端（best-effort，不阻塞 UI）
  if (agentId) {
    fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/activate`, {
      method: "POST",
    }).catch((e) => {
      console.warn("[SessionStore] activate_session failed:", e);
    });
  }
}
```

**效果**：`currentSessionId` 立即更新 → WS 过滤器立即生效 → 漏洞窗口归零。

#### 改动 2：`handleMessageEvent` 增加双重防护

**文件**: `chatStore.ts:962-979`

在 session_id 过滤之后，增加 `pendingSessionSwitch` 信号量防护：

```typescript
// 在 chatStore 中新增字段
pendingSessionSwitch: string | null;  // 正在切换中的 session ID

// 在 switchSession 中设置
// set({ currentSessionId: sessionId, pendingSessionSwitch: sessionId });

// 在 handleMessageEvent 中检查
if (contentEventTypes.has(eventType)) {
  const eventSessionId = data.session_id as string | undefined;
  if (eventSessionId !== undefined && eventSessionId !== null) {
    const currentSessionId = useSessionStore.getState().currentSessionId;
    if (eventSessionId !== currentSessionId) return;
  }
  // 双重防护：切换期间丢弃所有内容事件
  const pending = useChatStore.getState().pendingSessionSwitch;
  if (pending) return;
}
```

然后在 `loadSessionMessages` 完成后清除 `pendingSessionSwitch`。

**效果**：即使改动 1 有边界情况，此防护也能兜底。

#### 改动 3：`currentModel` / `currentProvider` 移入 `agentStates`

**文件**: `chatStore.ts:109-113, 603-616`

```typescript
// AgentChatState 新增
interface AgentChatState {
  // ... 现有字段
  model: string | null;
  provider: string | null;
}

// setCurrentModel 改为
setCurrentModel: (model, provider, agentId) => {
  set((state) => ({
    currentModel: model,
    currentProvider: provider,
    ...updateAgentState(state, agentId, { model, provider }),
    agentModels: {
      ...state.agentModels,
      [agentId]: { model, provider },
    },
  }));
};

// ChatPanel 选择 model 时从 agentState 读取
const currentModel = agentState?.model ?? useChatStore.getState().currentModel;
```

**效果**：切换 agent 时不再短暂显示错误模型。

---

### Phase 2：全面重构（根治架构债务）

#### 目标状态模型

```
sessionStore (单一职责：session 列表和选择)
├── sessions: SessionInfo[]
├── currentSessionId: string | null
├── agentSessionMap: Record<string, string>
└── (无 module-level 可变状态)

chatStore (单一职责：per-agent per-session 聊天数据)
├── agentStates: Record<agentId, {
│     model: string | null
│     provider: string | null
│     sessionStates: Record<sessionId, SessionChatState>  ← 新增二级映射
│     activeSessionId: string | null                      ← 当前活跃 session
│     ws: WebSocket | null
│   }>
├── sending: boolean → 移入 agentStates
└── (无 module-level 可变状态)

permissionStore (单一职责：per-session 工具审批)
├── sessionAllowed: Record<sessionId, Set<string>>  ← 从全局 Set 改为 per-session
└── (无 module-level 可变状态)
```

#### SessionChatState（从 AgentChatState 拆出）

```typescript
interface AgentState {
  model: string | null;
  provider: string | null;
  ws: WebSocket | null;
  activeSessionId: string | null;
  reconnectAttempts: number;
  sessionStates: Record<string, SessionChatState>;
}

interface SessionChatState {
  messages: ChatMessage[];
  streamingMessageId: string | null;
  streamBuffer: string;
  thinkingMessageId: string | null;
  isInThinkPhase: boolean;
  currentTurnId: string | null;
  tokenUsage: TokenUsage | null;
  contextUsage: ContextUsageInfo | null;
  hasMoreMessages: boolean;
  messageCursor: string | null;
  iterationLimitPaused: ...;
  pendingApproval: ToolApprovalNeededEvent | null;
  isLoadingSession: boolean;
  loadError: string | null;
  isReasoning: boolean;
}
```

**关键设计决策**：
- `sessionStates` 按需加载，切换时不清空旧 session，保留热数据
- 内存保护：限制最多保留 N 个 session 的缓存数据（建议 N=5），LRU 淘汰
- WS 事件直接按 `event.session_id` 路由到对应的 `SessionChatState`，无需全局过滤

#### 消除 module-level 可变状态

| 当前 | 重构后 |
|------|--------|
| `let fetchSessionId` | 移入 sessionStore，作为 `fetchId: number` 字段 |
| `const reconnectState` | 移入 agentStates[agentId].reconnectAttempts + timer ref |
| `const persistedTitles` | 移入 sessionStore，作为 `persistedTitles: Set<string>` |
| `let lastInitAgentId` | 移入 sessionStore 或 agentStore |
| `let lastLoadedSessionId` | 移入 chatStore agentStates[agentId].lastLoadedSessionId |

#### ChatPanel useEffect 简化

当前 300+ 行的 useEffect 协调 3 个 store 的状态，重构后：

1. **Agent 切换**：agentStore.selectAgent → chatStore.initAgent(agentId) → 单一入口
2. **Session 切换**：sessionStore.switchSession → chatStore.activateSession(agentId, sessionId) → 单一入口
3. **消息加载**：chatStore 自动管理（检测 activeSessionId 变化后触发）

ChatPanel 的 useEffect 从"协调器"降级为"渲染器"，只负责：
- 订阅 store 数据
- 触发 UI 动画（滚动到底部等）

#### WS 事件路由重构

```typescript
function handleMessageEvent(data, set, get, agentId) {
  const eventType = data.type as string;

  if (contentEventTypes.has(eventType)) {
    const eventSessionId = data.session_id as string | undefined;
    const agentState = getAgentState(get(), agentId);

    // 直接路由到 eventSessionId 对应的 SessionChatState
    if (eventSessionId && agentState.sessionStates[eventSessionId]) {
      // 更新对应 session 的状态
      set((state) => updateSessionState(state, agentId, eventSessionId, patch));
    }
    // 不再需要 currentSessionId 全局过滤
  }
}
```

**效果**：
- 后台 session 的 WS 事件也正确路由，不会被丢弃
- 切换回来时消息已更新，无需重新加载
- 彻底消除 session 数据串流的可能性

---

## 五、实施优先级与工作量

| 阶段 | 优先级 | 改动量 | 验证方式 |
|------|--------|--------|----------|
| Phase 1 改动 1 | **P0** | ~15 行 | 手动：快速切换 session 不串数据 |
| Phase 1 改动 2 | **P0** | ~20 行 | 手动：同上 + 并发场景 |
| Phase 1 改动 3 | **P1** | ~30 行 | 手动：切换 agent 模型不闪烁 |
| Phase 2 重构 | **P2** | ~300 行 | 单元测试 + 集成测试 |

**Phase 1 建议立即执行**，改动量小、风险低、直接修复数据串流 bug。
**Phase 2 建议在 Phase 1 验证后启动**，可作为独立 PR。

---

## 七、Phase 2 实施记录（2026-05-20）

Phase 2 已完成实施，所有改动通过 TypeScript 编译验证（tsc --noEmit 零错误）。

### 改动清单

| 文件 | 改动类型 | 说明 |
|------|----------|------|
| `chatStore.ts` | **重写** | `AgentChatState` 拆分为 `AgentState` + `SessionChatState`；`agentStates[agentId]` 改为二级映射 `agentStates[agentId].sessionStates[sessionId]`；WS 事件路由改为 `event.session_id` 直接寻址；`currentModel`/`currentProvider`/`sending` 移入 `AgentState`；`reconnectState`/`persistedTitles` 移入 store；新增 `activateSession()`、`clearSessionState()`、`removeSessionState()`、`evictStaleSessions()` |
| `sessionStore.ts` | **修改** | `switchSession` 先设 `currentSessionId` 再发 activate API（fire-and-forget）；调用 `chatStore.activateSession()` 管理状态隔离；`deleteSession` 调用 `removeSessionState()` 清理缓存 |
| `permissionStore.ts` | **重写** | `sessionAllowed: Set<string>` → `sessionAllowed: Record<string, Set<string>>`；新增 `isSessionAllowed(toolName, sessionId)` 方法 |
| `ChatPanel.tsx` | **修改** | 读取方式从 `agentState.messages` 改为 `sessionState.messages`（二级映射）；`sending`/`currentModel`/`currentProvider` 从 `agentState` 读取 |
| `ResultsPanel.tsx` | **修改** | 同上，`tokenUsage`/`contextUsage`/`messages` 从二级映射读取 |

### 架构变化总结

| 维度 | Before | After |
|------|--------|-------|
| 状态映射 | `agentStates[agentId]` (扁平) | `agentStates[agentId].sessionStates[sessionId]` (二级) |
| WS 事件路由 | `currentSessionId` 全局过滤 | `event.session_id` 直接寻址到 session |
| session 切换 | clearMessages → await API → setState | setState → activateSession → fire-and-forget API |
| Model/Provider | 全局 `currentModel`/`currentProvider` | `agentStates[agentId].model`/`provider` |
| Sending | 全局 `sending` | `agentStates[agentId].sending` |
| Permission | `sessionAllowed: Set<string>` | `sessionAllowed: Record<sessionId, Set<string>>` |
| Module-level 可变状态 | 4 个 (`fetchSessionId`, `reconnectState`, `persistedTitles`, `lastInitAgentId`) | 1 个 (`fetchSessionId`) — 其余移入 store |
| Session 缓存 | 无（切换时清空） | LRU 淘汰，最多 5 个 session 缓存 |
| 后台 session | WS 事件被丢弃 | WS 事件正确路由，切回时无需重新加载 |

---

## 六、Phase 2 的取舍（ADR 格式）

### ADR-013: Session State 二级映射

**状态**: Proposed

**背景**: 当前 `agentStates[agentId]` 扁平存储所有 session 数据，切换时必须清空再加载，导致 WS 事件竞态和切换延迟。

**决策**: 引入 `agentId → sessionId → SessionChatState` 二级映射，WS 事件按 `event.session_id` 直接路由。

**收益**:
- 消除 session 切换的数据串流可能性（架构级保证，非时序依赖）
- 后台 session 消息实时更新，切回时无需重新加载
- session 切换延迟降低（无网络请求，纯本地状态切换）

**代价**:
- 内存占用增加（多 session 缓存），需 LRU 限制
- `updateAgentState` 辅助函数需改为 `updateSessionState`，影响面大
- WS 事件不再丢弃而是路由，后台 session 的消息会持续增长

**缓解**:
- LRU 淘汰策略，最多保留 5 个 session 缓存
- 后台 session 设置消息上限（超过 200 条截断）
- 闲置超过 10 分钟的 session 缓存自动回收

---

## 八、Phase 2 后续修复：流式 session 切换消息消失（2026-05-20）

### 问题现象

Phase 2 重构后引入新 bug：正在交互的 session（LLM 正在流式输出），切到另一个 session 再切回来：
1. chat 气泡和 thinking 消息消失，只剩 tool_call/explore 块
2. 多切几次后，发送按钮从 Stop 图标恢复为 Send 图标
3. 过一段时间 LLM 返回的最终结果能显示，但中间过程消息丢失

### 根因分析（三个 bug 串联）

**Bug 1 — `activateSession` 清空旧 session 瞬态状态**

`chatStore.ts` 原第 361-371 行，切走时清空旧 session 的 `streamingMessageId`/`thinkingMessageId`/`streamBuffer`。但 agent 的 WS 仍在往该 session 发 chunk 事件。清空后：
- `streamingMessageId = null` → 后续 chunk 不追加到正在流式输出的消息，而是创建新的 assistant 消息
- `thinkingMessageId = null` → thinking 消息失去连接
- `streamBuffer = ""` → 部分缓冲区内容丢失

**Bug 2 — ChatPanel useEffect 触发 loadSessionMessages 覆盖**

`ChatPanel.tsx` 第 326-352 行，`currentSessionId` 变化时调 `loadSessionMessages`。切回 session A 时 `lastLoadedSessionId`（值为 B）≠ A → 触发加载 → 后端返回已持久化消息 → 覆盖内存中正在流式输出的消息。

**Bug 3 — loadSessionMessages 的流式保护守卫失效**

`loadSessionMessages` 第 958 行有守卫 `if (sessionState.streamingMessageId != null) return`，但 Bug 1 已将 `streamingMessageId` 清空 → 守卫不触发 → 允许覆盖。

### 修复（2 处改动，0 新结构体）

**修复 1 — activateSession：不清空旧 session 瞬态状态**

删除清空旧 session 瞬态状态的代码块，改为注释说明：agent 可能仍在 WS 往该 session 写入，瞬态状态只在 `done`/`error` 事件自然结束时清除，或由 `clearMessages`/`clearSessionState` 显式清除。

**修复 2 — ChatPanel useEffect：跳过正在 streaming 的 session**

新增守卫：如果 session 已有消息且 agent 正在 streaming（`sending=true` 或 `streamingMessageId` 存在），跳过 `loadSessionMessages`，只更新 `lastLoadedSessionId`。
