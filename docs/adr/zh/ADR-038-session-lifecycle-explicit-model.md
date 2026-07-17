# ADR-038: Session 生命周期显式化模型

**状态**：草案
**日期**：2026-07-17
**决策者**：大鱼

**前置**：
- ADR-033（MQTT 替换 gRPC + WebSocket）
- ADR-034（MQTT / HTTP 职责边界 — 字段号偏移规约、命令面边界）
- ADR-035（流式传输重构 — MQTT 数据直推）
- ADR-036（MQTT 连接状态由后端主动推送）

---

## 1. 决策摘要

Session 生命周期从"协议层 + 实现层 + 前端层三处隐式约定"显式化为**单一可观测契约**：

1. **协议层**：新增 `open_session` MQTT 控制命令（proto 字段号 29）作为 Closed/NotFound → Active 的唯一显式转换器；新增 `SessionOpened` / `SessionNotOpened` 事件（proto 字段号 35/36）作为服务端权威 ack 和拒答回执。
2. **后端**：废弃 HTTP `activate_session` / `deactivate_session` action 与 `SessionManager::ensure_session_in_memory` 散落调用；`SessionManager::open()` 成为单一 lazy-resume 入口；非 Active 状态接收 session-level 命令时，runtime 直接发 `SessionNotOpened` 事件，**不再隐式 lazy-resume**。
3. **前端**：拆解 `activateSession / switchSession / openTab` 三个含混动作为 `setActiveTab` (UI 严格切前台) / `openSession` (UI+后端激活) / `closeTab` (UI+后端关闭) 三类边界清晰的操作；`isSessionReady` 作为输入框解锁的唯一开关。

四条核心原则：

1. **状态唯一**：`Active`（在内存）/ `Closed`（磁盘有 JSONL + meta）/ `NotFound`（都没有）三态可观测，不再有"在 runtime 看来是 Active 但在 frontend 看来是空闲"的歧义。
2. **生命周期显式**：Closed → Active 必须由 frontend 发 `open_session` MQTT 命令触发；后端不再帮 frontend "自动恢复"。
3. **契约违反可见**：session-level 命令（chat_message / model_switch / etc.）命中非 Active session 时，runtime 发 `SessionNotOpened` 事件，frontend 弹 toast + 一键 reopen；不再"静默丢消息"。
4. **废弃项彻底**：HTTP `activate_session` / `deactivate_session` 在 Phase 3 直接删除（desktop 从未调用过这两个端点）。

---

## 2. 背景与根因

### 2.1 bug 复现链路

1. 用户关闭某个 session tab → 前端发 `close_session` MQTT → Runtime 关闭 session 任务，从内存删除（JSONL + meta 保留）
2. 用户重新点开同一 session → 前端 `switchSession` **只更新 UI 状态**，没发后端激活命令
3. 用户发消息 → 前端 `chat_message` MQTT → Runtime `forward_to_session_inbound` → **失败：`session not found: <sid>`**
4. conversion 文件不更新，前端保持 idle，但用户看不见错误（toast 路径不存在）

### 2.2 架构根因表

| 问题层 | 表现 | 修复 |
|---|---|---|
| 协议层 | MQTT `ControlCommand` 缺 `open_session`，create/delete/close 三态转换缺一环 | 新增 `open_session`（字段 29）+ `SessionOpened` / `SessionNotOpened` 事件 |
| 后端实现 | HTTP 路径 7 处散落 `ensure_session_in_memory`，MQTT 路径 0 处，行为不一致 | 全部替换为 `get_session().is_none()` 守卫 + 发 `SessionNotOpened`；删除 `ensure_session_in_memory` alias |
| 前端语义 | `switchSession / activateSession / openTab` 三个函数把"切前台"和"打开"耦合 | 拆解为 `setActiveTab` (UI 严格) / `openSession` (UI + 后端) / `closeTab` (UI + 后端) |
| 设计意图 | "lazy resume" 是开发者需要记着的潜规则，review 不可见，bug 不可见 | 显式 `open_session` 命令 + 拒答事件，契约可静态分析 |

### 2.3 涉及文件

- `core/acowork-core/proto/mqtt_payload.proto`（新增 command + 事件）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（`SessionLifecycleState` / `SessionOpenOutcome` / `open()`）
- `core/acowork-runtime/src/agent/inbound.rs`（`InboundMessage::OpenSession`）
- `core/acowork-runtime/src/startup/gateway_loop.rs`（`handle_open_session` + `forward_to_session_inbound` 拒答路径）
- `core/acowork-runtime/src/mqtt/client.rs`（`publish_session_opened` / `publish_session_not_opened`）
- `core/acowork-runtime/src/cli.rs`（删除 activate_session HTTP action + 7 处 ensure_session_in_memory）
- `apps/acowork-desktop/src/stores/chatStore.ts`（`openSession` / `setActiveTab` / `closeTab` + 事件 handler）
- `apps/acowork-desktop/src/stores/agentStore.ts`（删除 `switchSession`，用 `openSession` 替代）
- `apps/acowork-desktop/src/components/chat/SessionTabBar.tsx`（`handleSelect` 用 `openSession`，tab 切换用 `setActiveTab`）
- `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs`（透传 `SessionOpened` / `SessionNotOpened`，识别 `open_session` 命令）
- `apps/acowork-desktop/src-tauri/src/mqtt_client.rs`（`OpenSession` → `"open_session"` topic 映射）
- `apps/acowork-desktop/src/lib/types.ts`（`SessionOpenedEvent` / `SessionNotOpenedEvent` 类型）

---

## 3. 状态机定义

```
            create_session          open_session / lazy resume from disk
   (none) ───────────────→  Active ←────────────────────────────┐
                              │                                   │
                              │ close_session                     │
                              ↓                                   │
                            Closed ───────────────────────────────┘
                              │
                              │ delete_session
                              ↓
                          NotFound
```

| 当前状态 | create_session | open_session | close_session | delete_session | session-level MQTT 命令 |
|---|---|---|---|---|---|
| NotFound | → Active | → Active（meta 不存在则 error） | error 无文件 | no-op | 发 `SessionNotOpened` (reason=session_not_found) |
| Closed | error 已存在 | → Active（从 JSONL 加载） | no-op | → NotFound | 发 `SessionNotOpened` (reason=session_closed) |
| Active | error 已存在 | no-op（幂等） | → Closed | → NotFound | 正常处理 |

**关键不变量**：
- Active 状态的 session **必须在内存中**（有 `SessionHandle` 在 `SessionManager::sessions` 中）
- Closed 状态的 session **磁盘上有 JSONL + meta**，内存无
- NotFound 状态 **磁盘和内存都没有**

观察入口：`SessionManager::get_lifecycle_state(session_id, work_dir) -> SessionLifecycleState`。

---

## 4. 协议扩展

### 4.1 OpenSession 命令

proto 字段号 29（紧接现有 28，符合 ADR-034 §3.2 字段偏移规约）。

```protobuf
message ControlCommand {
  ...
  OpenSession open_session = 29;  // ADR-038 新增
}

message OpenSession {
  string session_id = 2;
}
```

语义：
- **Active session** → 返回 `SessionOpened` (status = `"already_active"`)
- **Closed session** → 从 JSONL + meta 恢复，返回 `SessionOpened` (status = `"resumed_from_disk"`)
- **NotFound session** → 发 `SessionNotOpened` (reason = `"session_not_found"`)

前置背景：旧的"subscribe push" `activate_session` HTTP action 在 ADR-034 阶段曾被改名为 `enable_notify` 并在 ADR-035 Phase 3 移除。本命令是全新的"显式激活"语义（字段号 29），与 `enable_notify` / `disable_notify`（24/25）零关系。

### 4.2 SessionOpened 事件

proto 字段号 35，topic = `acowork/agents/{id}/sessions/{sid}/opened`（Retained，QoS 1）。

```protobuf
message SessionOpened {
  string session_id = 1;
  string status = 2;             // "already_active" | "resumed_from_disk"
  string model = 3;
  string provider = 4;
  int64  last_active_at = 5;
}
```

Desktop 用法：
1. 收到后立即把 `chatStore.agentStates[aid].sessionStates[sid].isSessionReady = true`
2. 把 `model` / `provider` / `last_active_at` 写入 session header（之前只能等 `session_state_changed` 事件）

### 4.3 SessionNotOpened 事件

proto 字段号 36，topic = `acowork/agents/{id}/sessions/{sid}/not_opened`（QoS 0，fire-and-forget）。

```protobuf
message SessionNotOpened {
  string session_id = 1;
  string attempted_command = 2;  // e.g. "chat_message" | "model_switch"
  string reason = 3;             // "session_not_found" | "session_closed"
}
```

Desktop 用法：
1. 把 `isSessionReady = false`（若该 session 当前正 active）
2. 弹 toast："Session is not open (`{reason}`)"，提供 `Reopen` 按钮，点击后调 `chatStore.openSession(aid, sid)` 自动闭环

### 4.4 字段编号分配记录

| 字段号 | 消息 | 用途 | ADR |
|---|---|---|---|
| 35 | `SessionOpened` | OpenSession 成功 ack | ADR-038 |
| 36 | `SessionNotOpened` | session-level 命令拒答 | ADR-038 |
| 29 | `OpenSession`（在 ControlCommand.oneof 内） | 显式激活 | ADR-038 |

旧 `activate_session` / `deactivate_session` 字段号从未对外暴露（HTTP-only，desktop 未调用），直接删除。

---

## 5. 后端契约

### 5.1 SessionManager 新增接口

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleState { NotFound, Closed, Active }

pub enum SessionOpenOutcome { AlreadyActive, ResumedFromDisk }

impl SessionManager {
    /// Observe the lifecycle state of a session.
    pub fn get_lifecycle_state(&self, session_id: &str, work_dir: &Path)
        -> SessionLifecycleState;

    /// Explicit transition: Closed/NotFound → Active (lazy-load from disk).
    /// Idempotent: Active → Active (returns `AlreadyActive`).
    pub async fn open(&mut self, session_id: &str, work_dir: &Path)
        -> Result<SessionOpenOutcome>;
}
```

**删除**：`ensure_session_in_memory`（Phase 1 的 deprecated alias）整个删除。调用方全部用 `open()` 或 `get_session().is_none()` 守卫替代。

### 5.2 gateway_loop 路由

`InboundMessage::OpenSession` 走系统级分支（不走 session-level `forward_to_session_inbound`），调用 `handle_open_session`：

```rust
async fn handle_open_session(...) -> Result<()> {
    match session_manager.get_lifecycle_state(session_id, work_dir) {
        NotFound => publish_session_not_opened("session_not_found").await,
        Active => publish_session_opened("already_active", ...).await,
        Closed => match session_manager.open(session_id, work_dir).await {
            Ok(ResumedFromDisk) => publish_session_opened("resumed_from_disk", ...).await,
            Err(_) => publish_session_not_opened("session_closed").await,
        },
    }
}
```

### 5.3 session-level 命令的拒答路径（替换 lazy resume）

所有 session-level 命令（chat_message / stop / continue_execution / approval_decision / question_answer / intent / model_switch / reasoning_effort / workspace_switch / compact_context / compress_action）的转发统一通过：

```rust
fn forward_to_session_inbound(
    session_manager: &mut SessionManager,
    lifecycle_publisher: &MqttChunkPublisher,
    session_id: &str,
    attempted_command: &str,
    work_dir: &Path,
    msg: InboundMessage,
) -> Result<()> {
    match session_manager.get_session(session_id) {
        Some(handle) => handle.send_inbound(msg),
        None => {
            // Determine reason: Closed (file exists) vs NotFound
            let reason = match session_manager.get_lifecycle_state(session_id, work_dir) {
                Closed => "session_closed",
                _ => "session_not_found",
            };
            tokio::spawn(async move {
                let _ = lifecycle_publisher.publish_session_not_opened(
                    session_id, attempted_command, reason,
                ).await;
            });
            Err(RuntimeError::Config(format!("session not Active ({}): {}", reason, session_id)))
        }
    }
}
```

**不变量**：runtime 永远不会"自动"把 Closed → Active；frontend 必须显式发 `open_session`。

### 5.4 删除项

| 项 | 文件 | Phase 3 操作 |
|---|---|---|
| HTTP `activate_session` action | `cli.rs:1017-1072` | 删除 |
| HTTP `deactivate_session` 段（早就是 no-op） | `cli.rs:1074-1086` | 删除 |
| 7 处 `ensure_session_in_memory` 散落调用 | `cli.rs:lines 1107,1166,1210,1396,1437,1464,1569` | 全部替换为 `get_session().is_none()` 守卫，错误时返回 / 发 agent_error |
| `SessionManager::ensure_session_in_memory` 函数 | `session_manager.rs:1157-1168` | 删除（Phase 1 deprecated alias） |

**grep 验证**：Phase 3 完成后 `grep ensure_session_in_memory core/` 在 `core/` 目录零命中（除 conversation.rs 中的 doc comment，已删除）。

---

## 6. 前端操作语义拆分

### 6.1 三个边界清晰的入口

| 旧函数 | 新函数 | UI 副作用 | 后端副作用 |
|---|---|---|---|
| `activateSession` | `setActiveTab` | 切 `activeSessionId`（仅当 sid ∈ `openSessionIds`） | 无 |
| `switchSession` + `openTab` | `openSession` | 加入 `openSessionIds`，设 `activeSessionId`，lazy-create `sessionStates[sid]` | 发 MQTT `open_session` + HTTP `loadSessionMessages` |
| `closeSession` (agent) | `closeTab` | 从 `openSessionIds` 移除，选邻居 active；`isSessionReady=false` | 发 MQTT `close_session` |

**测试矩阵**：

| 触发场景 | 调用函数 |
|---|---|
| 用户在已开 tab 之间切换 | `setActiveTab` |
| 用户从历史记录 dropdown 点 session | `openSession` |
| 用户点 "+" 创建新 session 后 `session_created` 事件落地 | `activateNewlyCreatedSession`（封装 `openSession`） |
| 用户在 agent 列表切换 → 自动激活 latest session | `openSession`（等价于首次打开） |
| 用户点关 tab 按钮 | `closeTab`（async，等 MQTT） |
| 收到 `session_not_opened` toast 点"Reopen" | `openSession` |

### 6.2 输入框解锁：`isSessionReady`

`SessionChatState.isSessionReady: boolean`，初值 `false`：
- 收到 `session_opened` → `true`
- 收到 `session_not_opened` → `false`（若是当前 active session，弹 toast）
- 调用 `closeTab` 后 → 对应 session 立即 `false`（即使后端还没 ack）

输入框 disable 判定 = `!isSessionReady || isAssistantReplying`。

### 6.3 删除的函数

| 函数 | 位置 | 删除原因 |
|---|---|---|
| `chatStore.activateSession` | `chatStore.ts` | 与 `setActiveTab` 行为重叠但语义被改写为"切前台"，删除避免歧义 |
| `agentStore.switchSession` | `agentStore.ts:266 旧位置` | 与 `chatStore.setActiveTab` / `openSession` 边界不清，调用方改写为 `chat.openSession` |
| `chatStore.openTab` | `chatStore.ts` | deprecated，仅作 `_openTab` 保留以防外部调用方；新代码必须用 `openSession` |

### 6.4 自定义 toast 桥接

`chatStore` 是 zustand 实例（不在 React 树内），不能直接调用 `useToast()`。新增 `ToastProvider` 暴露的 `showToast()` 通过 `window.CustomEvent("acowork:toast")` 桥接，store 里调用 `showToast({...})` 即可。

```typescript
// ToastProvider.tsx
export const TOAST_EVENT = "acowork:toast";
export function showToast(toast: Omit<Toast, "id">): void {
  window.dispatchEvent(new CustomEvent(TOAST_EVENT, { detail: toast }));
}

// ToastProvider 内部 useEffect 监听 + addToast
useEffect(() => {
  const handler = (e: Event) => {
    const detail = (e as CustomEvent<Omit<Toast, "id">>).detail;
    if (detail) addToast(detail);
  };
  window.addEventListener(TOAST_EVENT, handler);
  return () => window.removeEventListener(TOAST_EVENT, handler);
}, [addToast]);
```

---

## 7. 迁移路径

### Phase 1：协议 + 后端契约（不破坏既有行为）

- 1.1 Proto 新增 `OpenSession` 命令字段 29
- 1.2 Proto 新增 `SessionOpened` / `SessionNotOpened` 事件字段 35/36
- 1.3 `SessionManager::get_lifecycle_state` + `SessionOpenOutcome` + `open()`
- 1.4 `InboundMessage::OpenSession` 枚举值
- 1.5 `gateway_loop` 实现 `handle_open_session` + state guard
- 1.6 `MqttChunkPublisher::publish_session_opened / publish_session_not_opened`
- 1.7 `mqtt_e2e` 测试覆盖三个 transition

兼容性：Phase 1 的 `forward_to_session_inbound` 仍走"session 不在内存就 error"路径（无 lazy resume），但 desktop 旧版本不发 `open_session` 仍会触发 error——这是显式错误而非静默丢消息，已经比修复前的体验好。

### Phase 2：前端对齐

- 2.1 `chatStore.activateSession` → `setActiveTab`（strict UI）
- 2.2 `chatStore.openSession`（UI + MQTT + load）
- 2.3 `chatStore.closeTab` 加 MQTT `close_session` 副作用
- 2.4 `agentStore.deleteSession` 重写 `switchSession` 引用为 `chatStore.openSession`
- 2.5 `session_created` 事件 handler 用 `setActiveTab`
- 2.6 `SessionTabBar.handleSelect` 用 `openSession`
- 2.7 `chatStore` 处理 `SessionOpened` / `SessionNotOpened`（含 toast）
- 2.8 `types.ts` 加 `SessionOpenedEvent` / `SessionNotOpenedEvent`
- 2.9 (implicit) `chat_mqtt.rs` 透传新事件 + `mqtt_client.rs` 加 `OpenSession → "open_session"` topic 映射

### Phase 3：清理

- 3.1 `cli.rs` 删除 HTTP `activate_session` / `deactivate_session` action
- 3.2 `cli.rs` 删除 7 处 `ensure_session_in_memory`，替换为 `get_session().is_none()` 守卫
- 3.3 `gateway_loop` 删除 lazy resume fallback（`forward_to_session_inbound` 已发 `SessionNotOpened` 替代）
- 3.4 `SessionManager` 删除 `ensure_session_in_memory` alias
- 3.5 本 ADR（ADR-038）

总估时 ~4 人日，跨 1-2 个 release 周期。

---

## 8. 验收标准

### 8.1 协议层

- [x] proto 字段号 29 (`OpenSession`) + 35 (`SessionOpened`) + 36 (`SessionNotOpened`) 无冲突
- [x] `mqtt_e2e` 测试：`open_session_on_closed_session_triggers_resume`、`..._on_active_session_is_idempotent`、`..._on_not_found_returns_error`

### 8.2 后端

- [x] `SessionManager::open()` 三态转换符合 §3 矩阵
- [x] `SessionManager::get_lifecycle_state()` 返回正确状态
- [x] `forward_to_session_inbound` 命中非 Active 时发布 `SessionNotOpened`
- [x] 旧 `ensure_session_in_memory` 函数已删除
- [x] HTTP `activate_session` / `deactivate_session` action 已删除
- [x] `grep ensure_session_in_memory core/` 零命中

### 8.3 前端

- [x] `chatStore.setActiveTab` (strict) / `openSession` / `closeTab` 三函数边界清晰，无行为重叠
- [x] `agentStore.switchSession` 已删除，调用方全部走 `chatStore.openSession`
- [x] `SessionTabBar.handleSelect` 走 `openSession`
- [x] `isSessionReady` 作为输入框解锁唯一开关
- [x] 收到 `session_not_opened` 时弹 toast + 一键 reopen
- [x] `SessionOpenedEvent` / `SessionNotOpenedEvent` 类型定义在 `types.ts`

### 8.4 构建 / 静态检查

- [x] `cargo check --workspace` 通过
- [x] `cargo clippy --all-targets -- -D warnings` 0 warnings（runtime / tauri）
- [x] `npx tsc --noEmit -p apps/acowork-desktop` 0 errors
- [x] `cargo test -p acowork-runtime` 含新增 e2e 测试 全通过

---

## 9. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| Phase 1 后老 desktop 不发 `open_session` | runtime 拒绝转发，发 `SessionNotOpened` 事件 | 老 desktop 看不到 toast，但至少日志可见 + 不会静默丢消息 |
| Phase 3 后老 desktop 不发 `open_session` | 同上 | 保持至少 1 个 release 的向后兼容（事件面 → 但 desktop 旧版本忽略） |
| proto 字段号冲突 | 编译错误 | 字段号 29/35/36 全部空闲，无冲突 |
| 事件面 payload schema 变更 | 旧 desktop 解析失败 | `SessionOpened` / `SessionNotOpened` 是新增枚举值，旧 desktop 走 `_ => {}` 默认分支忽略 |
| 多 tab 同时 closeTab 同一个 session | 后端收到多次 `close_session` | 后端 `close_session` 幂等（Closed → Closed no-op） |
| `forward_to_session_inbound` 用 `tokio::spawn` 异步发 `SessionNotOpened` | 若 runtime 进程已停，spawn 任务丢失 | 拒答事件是"best-effort"，即使丢失，下一次 session-level 命令又会触发 |

---

## 10. 实施 checklist

### 已完成（Phase 1 + Phase 2 + Phase 3 实现已完成）

#### Phase 1：Proto + 后端契约
- [x] t1.1 Proto 新增 `OpenSession` 命令 (field 29)
- [x] t1.2 Proto 新增 `SessionOpened` / `SessionNotOpened` 事件
- [x] t1.3 SessionManager 加 `SessionLifecycleState` enum 和 `open()` 方法
- [x] t1.4 `InboundMessage` 新增 `OpenSession` 枚举值
- [x] t1.5 `gateway_loop` 实现 `OpenSession` handler + state guard
- [x] t1.6 `MqttChunkPublisher` 加 `publish_session_opened` / `publish_session_not_opened`
- [x] t1.7 `mqtt_e2e` 测试覆盖
- [x] Phase 1 编译验证 `cargo build`

#### Phase 2：前端对齐
- [x] t2.1 `chatStore.activateSession` → `setActiveTab`
- [x] t2.2 `chatStore` 加 `openSession`
- [x] t2.3 `chatStore.closeTab` 加 MQTT 副作用
- [x] t2.4 `agentStore` 删除 `switchSession`
- [x] t2.5 `session_created` handler 改用 `setActiveTab` / `activateNewlyCreatedSession`
- [x] t2.6 `SessionTabBar.handleSelect` 改用 `openSession`
- [x] t2.7 `chatStore` 处理 `SessionOpened` / `SessionNotOpened` 事件
- [x] t2.8 `types.ts` 加事件类型
- [x] t2.9 解决 TypeScript 编译错误（`sessionPanel.tsx` / `agent-start.ts` / 未使用 `evictStaleSessions`）

#### Phase 3：清理
- [x] t3.1 `cli.rs` 删除 `activate_session` / `deactivate_session` HTTP action
- [x] t3.2 `cli.rs` 删除 7 处 `ensure_session_in_memory` 散落调用
- [x] t3.3 `gateway_loop::forward_to_session_inbound` 改为 strict 守卫 + 异步发 `SessionNotOpened`
- [x] t3.4 `SessionManager` 删除 `ensure_session_in_memory` alias
- [x] t3.5 ADR-038（本文件）

### 手动回归清单（人工 QA）

- [ ] 启动 app → 选 agent → 默认 latest session 自动 open
- [ ] 历史列表点 session → 后端收到 `open_session` → 输入框可用（`isSessionReady=true`）
- [ ] 已开 tab 之间切换 → `setActiveTab` → 后端无感知（不重发 `open_session`）
- [ ] 关 tab → 后端收到 `close_session` → 内存释放；`isSessionReady` 立即 false
- [ ] 关后重开 → 后端 lazy resume → 消息可发（`SessionOpened` status=`resumed_from_disk`）
- [ ] session 在 Closed 状态时 desktop 漏发 `open_session` → 第一次发消息收到 `SessionNotOpened` reason=`session_closed` → toast + 一键 reopen 闭环

---

## 11. 参考

- ADR-033：MQTT 替换 gRPC + WebSocket（传输层基础）
- ADR-034 §3.2：MQTT / HTTP 字段号偏移规约（29 / 35 / 36 是空闲号）
- ADR-035：MQTT 流式传输重构（QoS 1 强制）
- ADR-036：MQTT 连接状态由后端主动推送（事件面与 `agent-event` 通道分离原则）
- ADR-034 §7.1 G1：`UpdateSessionTitle` 不再包成 `SystemNotification`（同类教训：本 ADR 一律用结构化命令事件，不走 legacy `Intent` 通路）
