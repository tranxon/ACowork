# Rollball Desktop Session 管理与本地缓存诊断报告

## 一、Store 架构全景

| 文件 | 核心职责 |
|------|---------|
| chatStore.ts | 消息流、WebSocket 连接、流式渲染 |
| sessionStore.ts | Session 列表/切换/创建、agent-session 映射 |
| agentStore.ts | Agent 列表/选择/启停 |
| gatewayStore.ts | Gateway 健康检查 |
| memoryStore.ts | Memory 节点 CRUD |
| permissionStore.ts | 工具审批队列 |
| settingsStore.ts | 主题/字号/日志级别 |
| skillStore.ts | 技能列表/导入 |
| workspaceStore.ts | 工作区管理 |

## 二、chatStore.ts 完整结构分析

文件: d:/projects/rust/agent-study/apps/rollball-desktop/src/stores/chatStore.ts

### 2.1 状态字段

- messages: ChatMessage[] - 当前显示的消息列表（单一平面数组）
- streamingMessageId: string | null - 正在流式输出的消息 ID
- sending: boolean
- wsMap: Record<string, WebSocket> - 每个 agent 一个 WS 连接
- tokenUsage: TokenUsage | null
- currentModel / currentProvider: string | null
- agentModels: Record<string, {model, provider}> - 每个 agent 的模型缓存
- availableModels: {name, provider}[]
- currentAgentId: string | null
- iterationLimitPaused: {...} | null
- contextUsage: ContextUsageInfo | null
- hasMoreMessages: boolean - 分页：是否有更老的消息
- messageCursor: string | null - 分页游标
- isLoadingMore / isLoadingSession: boolean
- currentTurnId: string | null - 当前 LLM 调用周期的 turn ID
- streamBuffer: string - 跨 chunk 累积缓冲区
- thinkingMessageId: string | null
- isInThinkPhase: boolean
- loadSequence: number - 防止快速 session 切换竞态（已定义但未使用）

### 2.2 消息缓存策略 - 关键缺陷

messages 是单一全局数组，没有 per-session 缓存：
- 切换 session 时：调用 clearMessages() 清空，然后 loadSessionMessages() 重新从后端加载
- 切换 agent 时：同样 clearMessages() + 重新加载
- 没有本地消息缓存：每次切换回之前的 session，都要重新从 Gateway API 拉取全部消息
- 没有 per-session message map：只维护一个 messages[]，任何时刻只能看到当前 session 的消息

### 2.3 WebSocket 消息处理（handleMessageEvent）

核心过滤逻辑（第198行）：只检查 currentAgentId，不检查 currentSessionId

| 事件类型 | 是否携带 session_id | 当前处理方式 |
|---------|-------------------|-------------|
| chunk | 否 | 直接追加到当前 messages |
| tool_call | 否 | 直接追加到当前 messages |
| tool_result | 否 | 直接追加到当前 messages |
| done | 否 | 结束流式状态，更新 title |
| error | 否 | 追加错误消息 |
| context_usage | 否 | 更新 contextUsage 状态 |
| model_confirmed | 否 | 更新 agentModels 缓存 |
| iteration_limit_paused | 否 | 设置暂停状态 |
| tool_approval_needed | 否 | 弹出审批对话框 |

结论：WebSocket 的所有事件都不携带 session_id，前端无法知道消息属于哪个 session。

## 三、sessionStore.ts 完整结构分析

文件: d:/projects/rust/agent-study/apps/rollball-desktop/src/stores/sessionStore.ts

### 3.1 状态字段

- sessions: SessionInfo[] - 当前 agent 的 session 列表
- currentSessionId: string | null - 当前活跃的 session ID
- isLoading: boolean
- isSessionPanelOpen: boolean
- sessionTitles: Record<string, string | null> - agent_id 到最新 session 标题
- agentSessionMap: Record<string, string> - agent_id 到最后选择的 session_id

### 3.2 核心方法

| 方法 | 功能 | 问题 |
|------|------|------|
| fetchSessions(agentId) | 从 /api/agents/{id}/sessions 加载列表 | 仅在 SessionPanel 打开时调用 |
| switchSession(sessionId) | 设置 currentSessionId | 只改 ID，不加载消息 |
| saveSessionForAgent(agentId, sessionId) | 记录 agent 最后使用的 session | 存活于组件重挂载 |
| createSession(agentId) | POST 创建新 session + clearMessages | |
| updateSessionTitle(sessionId, title) | 本地更新标题（仅在 title 为空时） | 不持久化到后端 |

## 四、agentStore.ts 完整结构分析

文件: d:/projects/rust/agent-study/apps/rollball-desktop/src/stores/agentStore.ts

- agents: AgentInfo[] + selectedAgentId: string | null
- 自动选择 System Agent (com.rollball.system)
- 通过 Tauri invoke 调用 Rust 命令
- 与 session 无直接交互

## 五、ChatPanel.tsx - Agent/Session 切换 Effect 逻辑

文件: d:/projects/rust/agent-study/apps/rollball-desktop/src/components/chat/ChatPanel.tsx

### 5.1 Agent 切换 Effect（第141-210行）

1. 防重复初始化检查（lastInitAgentRef）
2. 记住离开的 agent 的 session（saveSessionForAgent）
3. clearMessages() + sessionStore.reset()
4. 如果 agent 在运行：connectStream() -> loadAgentModel() -> fetchSessions() -> loadSessionMessages() -> switchSession()

### 5.2 Session 切换 Effect（第213-233行）

- 跳过 isInitialLoadRef 期间的触发（防重入）
- 确认 session 属于当前 agent
- 调用 loadSessionMessages() 替换 messages[]

## 六、WebSocket 和消息流深度分析

### 7.1 WebSocket URL 构造

URL 中只包含 agentId，没有 session_id：
ws://127.0.0.1:19876/api/agents/{agentId}/stream

### 7.2 发送消息格式

WS 方式：socket.send(JSON.stringify({ type: message, content, command }))
HTTP fallback：invoke(send_message, { agentId, content, command })
sendMessage 不携带 session_id。

### 7.3 后端 WebSocket 处理（Gateway chat.rs）

WsClientMessage 结构没有 session_id 字段。
HTTP SendMessageRequest 有 conversation_id 字段，但前端从未传入。

### 7.4 Gateway Bridge 事件转发

只按 agent_id 过滤，没有 session_id 概念。

## 八、后端 Session API 详细分析

Session API 路由：
- GET  /api/agents/{id}/sessions - 列出 sessions
- POST /api/agents/{id}/sessions - 创建 session
- GET  /api/agents/{id}/sessions/{session_id}/messages - 获取分页消息

创建 Session 不需要指定 session_id，由 Runtime 自动生成（格式：YYYYMMDD_HHMMSS_6位UUID）。
Runtime 端每个 session 一个 {session_id}.jsonl 文件。
AgentLoop 只有一个 conversation: Option<ConversationSession>。
Gateway IPC Session 是不同概念，管理 IPC 连接状态。

## 九、核心问题诊断

### P1: WebSocket 与 Session 完全脱钩 [高]
WS URL 只包含 agentId，所有消息事件都不携带 session_id。
handleMessageEvent 只检查 currentAgentId，不检查 currentSessionId。
根因：Gateway Bridge 只按 agent_id 过滤，Runtime 只有单一活跃 ConversationSession。

### P2: sendMessage 不关联 Session [高]
WS 发送消息不携带 session_id。HTTP SendMessageRequest 有 conversation_id 但前端未传入。
消息总是发送到 Runtime 当前活跃的 session，可能写入错误的 session。

### P3: 消息没有本地缓存 [中]
messages[] 是单一平面数组，切换时完全清空并重新加载。
分页状态也没有缓存。

### P4: Session 切换时的竞态风险 [中]
loadSequence 定义但未使用。loadSessionMessages 没有 AbortController。
快速连续切换可能导致旧请求结果覆盖新请求结果。

### P5: Session Title 更新不一致 [低]
前端 title 更新不持久化到后端。sessionTitles 只跟踪最新 session 标题。

### P6: Per-Agent WS 非 Per-Session [设计限制]
一个 agent 只有一个 WS，只能对应一个活跃 session。
查看历史 session 时 WS 消息会混入。

### P7: agentSessionMap 过时引用 [低]
保留的 sessionId 可能已不存在，fallback 到 sessions[0]。

## 十一、问题汇总

| # | 问题 | 严重性 | 影响 |
|---|------|--------|------|
| P1 | WS 消息不携带 session_id | 高 | 切换历史 session 时新消息混入 |
| P2 | sendMessage 不关联 session | 高 | 消息可能写入错误的 session |
| P3 | 消息无本地缓存 | 中 | 切换 session 体验差 |
| P4 | Session 切换竞态 | 中 | 快速切换时消息错乱 |
| P5 | Title 更新不一致 | 低 | 前后端 title 可能不同步 |
| P6 | Per-Agent WS 非 Per-Session | 设计限制 | 单 agent 只能一个活跃对话 |
| P7 | agentSessionMap 过时引用 | 低 | Fallback 行为，影响小 |

## 十二、潜在改进方向

1. P1/P2: 在 WS 协议中增加 session_id，或消息发送时通过 HTTP API 指定 session_id
2. P3: 引入 messageCache: Record<string, ChatMessage[]> 按 session 缓存消息
3. P4: 使用 AbortController + loadSequence 实现请求取消
4. P5: 添加 PUT /api/agents/{id}/sessions/{sid}/title API
5. P6: 禁止查看历史 session 时的 WS 消息混入，或在 WS 事件中增加 session_id
