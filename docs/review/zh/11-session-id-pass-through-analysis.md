# Session ID 通行分析报告

**日期:** 2026-05-20
**范围:** Desktop App ↔ Gateway ↔ Runtime 双通路 session_id 传递与使用
**目标:** 识别多 session 并行场景下因 session_id 缺失或未有效利用导致的信息流/控制流 bug

---

## 一、双通路架构总览

```
通路 1 (正常): Desktop App ──HTTP/WS──▶ Gateway ──gRPC──▶ Runtime
通路 2 (Debug): Desktop App ──WS(JSON-RPC)──▶ Runtime (直连)
```

| 维度 | 通路 1 (正常) | 通路 2 (Debug) |
|------|-------------|---------------|
| 传输层 | HTTP REST + WebSocket → gRPC bidi stream | WebSocket (JSON-RPC 2.0) |
| 中间层 | Gateway (路由/转发) | 无 (直连 Runtime 进程) |
| 端口 | Gateway HTTP 19876 / gRPC 19877 | Runtime Debug 19878+ |
| 多 session 支持 | ✅ 设计支持 | ❌ 单 session 模型 |
| session_id 载体 | IntentReceived.params JSON | 无 |

---

## 二、通路 1 详细分析: Desktop App → Gateway → Runtime

### 2.1 HTTP API 端点 session_id 传递矩阵

| # | 方法 | 端点 | session_id 位置 | 是否传递到 Runtime | 风险 |
|---|------|------|----------------|-------------------|------|
| 1 | POST | `/api/agents/{id}/message` | ❌ 无 | `conversation_id` 代替，非 `session_id` | **HIGH** |
| 2 | GET | `/api/agents/{id}/stream` | ❌ 无 (WS) | WS 消息体无 session_id | **HIGH** |
| 3 | GET | `/api/agents/{id}/sessions` | N/A | 仅查询 | 无 |
| 4 | POST | `/api/agents/{id}/sessions` | N/A | Runtime 生成 | 无 |
| 5 | POST | `/api/agents/{id}/sessions/{sid}/activate` | ✅ 路径参数 | `params.session_id` 传递 | 无 |
| 6 | PUT | `/api/agents/{id}/sessions/{sid}/title` | ⚠️ 路径参数但被忽略 | `session_id` 未传到 Runtime | **MEDIUM** |
| 7 | GET | `/api/agents/{id}/sessions/{sid}/messages` | ✅ 路径参数 | `params.session_id` 传递 | 无 |
| 8 | DELETE | `/api/agents/{id}/sessions/{sid}` | ✅ 路径参数 | `params.session_id` 传递 | 无 |

### 2.2 WebSocket 流消息 session_id 传递矩阵

| WS 消息类型 | session_id | 风险 |
|------------|-----------|------|
| `{ type: "message", content }` | ❌ 无 | **HIGH** |
| `{ type: "stop" }` → Gateway 转为 `interrupt` | ❌ 无 | **HIGH** |
| `{ type: "model_switch", model, provider }` | N/A (广播) | 低 |

### 2.3 Gateway → Runtime 推送消息 session_id 传递矩阵

| Gateway → Runtime 推送 | action | params 含 session_id? | Runtime 路由目标 |
|------------------------|--------|----------------------|-----------------|
| 发送消息 | `chat_message` | ❌ 无 | fallback `current_session_id` |
| 中断 | `interrupt` | ❌ 无 | fallback `current_session_id` |
| 继续执行 | `continue_execution` | ❌ 无 | fallback `current_session_id` |
| 审批决策 | `approval_decision` | ❌ 无 | fallback `current_session_id` |
| 激活会话 | `activate_session` | ✅ 有 | 指定 session |
| 更新标题 | `update_session_title` | ❌ 无 (Gateway 侧丢弃) | fallback `current_session_id` |
| 模型切换 | `model_switch` | N/A | 广播所有 session |

### 2.4 Runtime 侧 session 路由机制

**核心路由逻辑** (`cli.rs:1279-1283`):
```rust
let target_session_id = params.get("session_id")
    .and_then(|v| v.as_str())
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string())
    .unwrap_or_else(|| current_session_id.clone());
```

**`current_session_id` 追踪**:
- 类型: `String` 局部变量 (`cli.rs:1156`)
- 更新时机: 仅在 `activate_session` 时更新 (`cli.rs:1459`)
- 生命周期: 与 Gateway 消息循环一致

**路由优先级**: 显式 `params.session_id` > `current_session_id` (隐式)

---

## 三、通路 2 详细分析: Desktop App → Runtime (Debug 直连)

### 3.1 架构特征

- Debug 协议使用 **WebSocket JSON-RPC 2.0** 直连 Runtime 进程
- 单客户端限制 (仅允许一个 Desktop App 连接)
- DebugController 管理 **单个调试会话**，无 session_id 概念
- 所有事件以 `iteration` 编号为标识，无 session 维度

### 3.2 Debug 协议 session_id 传递矩阵

| JSON-RPC 方法 | session_id | 说明 |
|--------------|-----------|------|
| `debugger.resume` | ❌ 无 | 恢复当前迭代 |
| `debugger.pause` | ❌ 无 | 暂停 |
| `debugger.step` | ❌ 无 | 单步执行 |
| `debugger.stop` | ❌ 无 | 停止 Agent |
| `debugger.setBreakpoint` | ❌ 无 | 设置断点 |
| `debugger.rewind` | ❌ 无 | 回退到指定迭代 |
| `debugger.getContextSnapshot` | ❌ 无 | 获取上下文快照 |
| 所有 `on*` 事件 | ❌ 无 | 通知事件，仅有 iteration/phase |

### 3.3 Debug 通路评估

**当前设计合理**: Debug 模式天然是 1:1 的单 session 调试场景。一个 Runtime 进程在同一时刻只调试一个 session，不需要 session_id 路由。

**潜在风险**: 如果未来支持 "debug 多 session 中特定一个"，需要扩展协议。但当前无需改动。

---

## 四、问题清单

### P0: 发送消息 (chat_message) 缺少 session_id

**位置**:
- Gateway HTTP: `chat.rs:185-188` (`send_message` 端点)
- Gateway WS: `chat.rs:711-713` (WebSocket 流)
- Desktop App: `chatStore.ts:364` (WS 消息体)
- Desktop App: `gateway_client.rs:218-227` (HTTP fallback)

**问题**: 用户在 Session A 输入消息 → 切换到 Session B → 消息到达 Gateway → Gateway 推 `chat_message` 不带 session_id → Runtime 路由到 `current_session_id`(可能是 B) → **消息发到错误 session**

**复现路径**:
1. 打开 Agent，创建 Session A 和 Session B
2. 在 Session A 输入消息但尚未发送
3. 点击切换到 Session B (触发 `/activate`)
4. 发送之前在 Session A 的消息
5. 消息被路由到 Session B (因为 `current_session_id` 已更新)

**影响**: 多 session 并行场景下，消息串台是 P0 级数据污染

---

### P0: 审批决策 (approval_decision) 缺少 session_id

**位置**:
- Gateway approval.rs:107-111 (推给 Runtime 的 params)
- Desktop App: permissionStore.ts:132 (HTTP 请求体)
- Desktop App: ChatPanel.tsx:439-457 (approval 请求体)

**问题**: Agent A Session 1 请求审批 → 用户切到 Session 2 → 审批推回 Runtime 时无 session_id → 路由到 `current_session_id` (Session 2) → **审批发给错误 session**

**复现路径**:
1. Session A 执行高风险 shell 命令，弹出审批弹窗
2. 用户切到 Session B (触发 `/activate`)
3. 用户点击"允许"
4. 审批决策推回 Runtime → 路由到 Session B → Session A 永远等不到审批

**影响**: 审批串台比消息串台更危险——可能导致高风险命令在错误 session 上下文执行

---

### P1: 中断 (interrupt) 缺少 session_id

**位置**:
- Gateway WS: `chat.rs:646-648` (stop → interrupt 转换)
- Desktop App: chatStore.ts:463 (WS stop 消息)

**问题**: 用户想停 Session A 的生成 → 当前 session 是 B → interrupt 发到 Session B → **Session A 继续运行**

---

### P1: 继续执行 (continue_execution) 缺少 session_id

**位置**:
- Gateway HTTP: `chat.rs:855-858`
- Desktop App: chatStore.ts:621

**问题**: 同 interrupt，continue 发到错误 session

---

### P1: update_session_title 丢弃 session_id

**位置**: Gateway `chat.rs:1152` — `Path((agent_id, _session_id))`

**问题**: URL 路径中有 session_id 但被 `_` 前缀忽略，forward 到 Runtime 时 params 只含 `{ "title": "..." }`

**影响**: 标题更新应用到 current_session 而非指定 session

---

### P2: SendMessageRequest 使用 conversation_id 而非 session_id

**位置**: Gateway `chat.rs:44-58` (SendMessageRequest 定义), `chat.rs:188`

**问题**: HTTP `/message` 端点接受 `conversation_id` 但 Runtime 侧用 `session_id` 路由。两者语义不同：`conversation_id` 是客户端生成的标识符，`session_id` 是 Runtime 管理的会话标识

---

### P2: AgentLoop 无可靠 session_id

**位置**: Runtime `loop_.rs:269-271`

**问题**: `current_session_id()` 从 `Option<ConversationSession>` 派生，无 ConversationSession 时返回 `None`，memory/distillation 记录为 `"unknown"`

---

### P2: Debug 通路无 session 感知

**当前合理**: Debug 是 1:1 单 session 模型
**未来风险**: 如需多 session 调试需扩展协议

---

## 五、根因分析

### 核心矛盾: "激活即路由" 隐式协议

当前设计采用**隐式路由**模型：
1. 前端调用 `/activate` 设置 `current_session_id`
2. 后续所有操作都不带 session_id，依赖 `current_session_id` 路由

这在**单 session 顺序操作**场景下没问题，但在**多 session 并行/快速切换**场景下会出问题：

```
时间线:
t0: 用户在 Session A 输入消息
t1: 用户切到 Session B (触发 /activate → current_session_id = B)
t2: 用户发送消息 → chat_message 不带 session_id → 路由到 B ❌
```

### 问题源头

1. **Gateway HTTP 端点**未将 session_id 作为标准参数传递
2. **Gateway WS 流**消息体无 session_id 字段
3. **Desktop App** 有 `currentSessionId` 状态但未在发送消息时使用
4. **Runtime** 的路由 fallback 到 `current_session_id` 放大了上述缺失

---

## 六、修复方案

### 方案 A: 显式传递 (推荐)

**核心原则**: 每个需要 session 路由的操作都显式传递 session_id

#### 前端改动

| 组件 | 改动 |
|------|------|
| `chatStore.ts` sendMessage (WS) | WS 消息体加 `session_id: currentSessionId` |
| `chatStore.ts` sendMessage (HTTP) | invoke 加 `sessionId` 参数 |
| `chatStore.ts` stop | WS 消息体加 `session_id` |
| `chatStore.ts` continueExecution | HTTP params 加 `session_id` |
| `permissionStore.ts` approve | approval 请求体加 `session_id` |
| `gateway_client.rs` send_message | 请求体加 `session_id` |

#### Gateway 改动

| 端点 | 改动 |
|------|------|
| `send_message` | IntentReceived.params 加 `session_id` |
| `agent_stream_ws` chat_message | IntentReceived.params 加 `session_id` |
| `agent_stream_ws` stop → interrupt | IntentReceived.params 加 `session_id` |
| `continue_execution` | IntentReceived.params 加 `session_id` |
| `handle_approval` → approval_decision | IntentReceived.params 加 `session_id` |
| `update_session_title` | `_session_id` → `session_id`，params 加 `session_id` |

#### Runtime 改动

无需改动——路由逻辑已支持 `params.session_id` 优先于 `current_session_id`

**工作量**: 约 2-3 人天

### 方案 B: Gateway 层注入

**核心原则**: 前端不改动，Gateway 根据最近一次 `/activate` 记录自动注入 session_id

**问题**: 时序竞态——activate 和 chat_message 可能并发到达 Gateway

**不推荐**

---

## 七、修复优先级

| 优先级 | 问题 | 修复方案 | 状态 |
|--------|------|---------|------|
| **P0** | chat_message 无 session_id | 前端+Gateway 全链路加 session_id | ✅ 已修复 |
| **P0** | approval_decision 无 session_id | 前端+Gateway 加 session_id | ✅ 已修复 |
| **P1** | interrupt 无 session_id | Gateway WS 加 session_id | ✅ 已修复 |
| **P1** | continue_execution 无 session_id | Gateway HTTP 加 session_id | ✅ 已修复 |
| **P1** | update_session_title 丢弃 session_id | 去掉 `_` 前缀，params 加 session_id | ✅ 已修复 |
| **P2** | conversation_id vs session_id 语义混乱 | 统一为 session_id | 待定 |
| **P2** | AgentLoop 无可靠 session_id | 构造时注入 session_id | 待定 |

**总工作量**: 约 2.5 人天

---

## 八、Debug 通路结论

Debug 通路 (WebSocket JSON-RPC) 当前**不需要修改**：
- 1:1 单 session 模型，不需要 session_id 路由
- DebugController 以 `iteration` 编号为标识，设计合理
- 如未来支持多 session 调试，需扩展协议增加 `session_id` 字段
