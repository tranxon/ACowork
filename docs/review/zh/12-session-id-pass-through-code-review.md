# Code Review: Session ID 显式传递修复

**日期:** 2026-05-20
**范围:** Gateway + Desktop Frontend + Tauri Backend
**提交:** 基于 11-session-id-pass-through-analysis.md 方案 A 的实施

---

## 一、Review 结论

| 维度 | 结果 |
|------|------|
| 逻辑正确性 | ✅ 正确 |
| 向后兼容性 | ✅ 兼容（`#[serde(default)]` / 可选参数） |
| 编译通过 | ✅ Gateway + Desktop Rust 均通过 |
| 测试通过 | ✅ 241/242（1 个已有的端口竞争 flaky test） |
| 代码风格 | ⚠️ 1 处缩进问题（已修复） |
| 关键缺失 | ⚠️ Runtime 请求审批时未带 session_id（见§四） |

---

## 二、逐文件 Review

### 2.1 `core/rollball-gateway/src/http/chat.rs`

**改动点:**
- `SendMessageRequest` +`session_id: Option<String>`（`#[serde(default)]`）
- `WsClientMessage` +`session_id: Option<String>`（`#[serde(default)]`）
- `ContinueExecutionRequest` 新结构体 +`session_id`
- 4 处 `IntentReceived.params` 加 `session_id`
- `update_session_title` `_session_id` → `session_id`

**Review:**
- ✅ `#[serde(default)]` 保证旧客户端不传 `session_id` 不报错
- ✅ Runtime 路由逻辑 `params.session_id > current_session_id` 无需改动
- ✅ `conversation_id` 保留（与 `session_id` 语义不同，不冲突）
- ⚠️ 第 197 行 `IntentReceived` 结构体字段缩进错误（`from:`/`action:` 与 `let intent` 对齐，应再缩进一级）→ **已修复**
- ✅ 其他 3 处 `IntentReceived` 缩进正确

### 2.2 `core/rollball-gateway/src/http/approval.rs`

**改动点:**
- `ApprovalRequest` +`session_id: Option<String>`
- `handle_approval` approval_decision params 加 `session_id`
- 3 处测试补 `session_id: None`

**Review:**
- ✅ `#[serde(default)]` 保证向后兼容
- ✅ 测试代码同步更新
- ⚠️ 关键发现：Runtime 请求审批时 params 中**没有** `session_id`（见§四）

### 2.3 `apps/rollball-desktop/src/stores/chatStore.ts`

**改动点:**
- `sendViaWs` 消息体加 `session_id`
- HTTP fallback `invoke` 加 `sessionId` + `command`
- `stopCurrentMessage` / `sendInterrupt` WS 消息加 `session_id`
- `continueExecution` HTTP body 加 `session_id`

**Review:**
- ✅ `...(sessionId ? { session_id: sessionId } : {})` 模式避免传 `null`
- ✅ `JSON.stringify` 对 `undefined` 的 `command` 会自动忽略
- ✅ Tauri `invoke` 参数名（camelCase）与 Rust command（snake_case）自动匹配
- ⚠️ `sendInterrupt` 和 `stopCurrentMessage` 都发 `type: "stop"`，Gateway 统一转为 `interrupt` action，设计合理

### 2.4 `apps/rollball-desktop/src/stores/permissionStore.ts`

**改动点:**
- `import { useSessionStore }`
- `sendApprovalToGateway` +`sessionId?: string | null`
- `showApprovalDialog` / `approve` 调用传入 `currentSessionId`

**Review:**
- ✅ 自动审批路径（`sessionAllowed`）也带 `sessionId`
- ⚠️ 潜在问题：如果审批弹窗显示期间用户切换了 session，`currentSessionId` 会变，审批发到新 session。→ **需要 Runtime 在请求审批时带 session_id，前端存储 event.session_id 而非用 currentSessionId**（见§四）

### 2.5 `apps/rollball-desktop/src-tauri/src/commands/chat.rs`

**改动点:**
- `send_message` command +`session_id: Option<String>` +`command: Option<String>`

**Review:**
- ✅ 参数顺序与前端 `invoke` 调用一致
- ✅ `as_deref()` 正确转换 `Option<String>` → `Option<&str>`

### 2.6 `apps/rollball-desktop/src-tauri/src/gateway_client.rs`

**改动点:**
- `send_message` +`session_id` +`command` 参数
- body JSON 条件加入字段

**Review:**
- ✅ `let mut body` 可变，条件加入字段逻辑正确
- ✅ HTTP POST 路径完整传递 session_id + command

---

## 三、向后兼容性分析

| 场景 | 旧前端 → 新 Gateway | 新前端 → 旧 Gateway |
|------|---------------------|---------------------|
| send_message HTTP | ✅ `session_id` 缺失 → `#[serde(default)]` → `None` → 走 `current_session_id` fallback | ❌ 旧 Gateway 不认识 `session_id` 字段，但 `#[serde(default)]` 或额外字段通常被忽略 |
| send_message WS | ✅ 同上 | ❌ 旧 Gateway `WsClientMessage` 无 `session_id`，但 serde 默认忽略未知字段 |
| continue_execution | ✅ `session_id` 缺失 → `None` | ❌ 旧 Gateway 端点无 body，新前端发 `{}` 或 `{"session_id":...}` 可能导致 422 |
| approval | ✅ `session_id` 缺失 → `None` | ❌ 旧 Gateway 不认识 `session_id` 字段 |

**结论:** 新 Gateway + 旧前端 = 安全。旧 Gateway + 新前端 = `continue_execution` 可能有 422 风险（如果旧 Gateway 严格校验空 body）。建议前后端同步升级。

---

## 四、关键缺失：Runtime 请求审批未带 session_id

**问题描述:**
Runtime 在 `loop_.rs:1727-1735` 发送 `ChunkEvent::ToolApprovalNeeded` 时，以及 `cli.rs:795-803` 构造 `tool_approval_needed` params 时，均未包含 `session_id`。

**影响:**
1. Gateway 推给 Desktop App 的 `BridgeEvent::ToolApprovalNeeded` payload 中无 `session_id`
2. Desktop App 的 `ToolApprovalNeededEvent` 接口无 `session_id` 字段
3. 前端审批弹窗无法绑定到具体 session，只能依赖 `currentSessionId`
4. 用户在审批弹窗显示期间切换 session → 审批发到错误的 session

**复现路径:**
1. Session A 执行高风险 shell 命令，弹出审批弹窗
2. 用户不关闭弹窗，直接切换到 Session B（触发 `/activate`）
3. 用户点击弹窗上的"允许"
4. 审批决策的 `session_id` = `currentSessionId` = Session B → Runtime 路由到 Session B
5. Session A 永远等不到审批，高风险命令不会执行

**修复建议:**
1. Runtime: `ChunkEvent::ToolApprovalNeeded` 加 `session_id` 字段
2. Runtime: `cli.rs` `tool_approval_needed` params 加 `"session_id": current_session_id`
3. 前端: `ToolApprovalNeededEvent` 接口加 `session_id`
4. 前端: `showApprovalDialog` 存储 `event.session_id`，`approve` 使用存储值而非 `currentSessionId`

---

## 五、Action Items

| # | 优先级 | 事项 | 状态 |
|---|--------|------|------|
| 1 | P0 | Runtime 审批请求带 session_id（§四） | 待修复 |
| 2 | P1 | 前端审批弹窗绑定 event.session_id 而非 currentSessionId | 待修复 |
| 3 | P2 | `conversation_id` vs `session_id` 语义统一 | 待定 |
| 4 | P2 | AgentLoop 构造时注入可靠 session_id | 待定 |
