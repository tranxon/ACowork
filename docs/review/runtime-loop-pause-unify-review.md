# Runtime Loop 重构架构评审

## 变更概述

将 runtime loop 中三种"暂停"场景——iteration limit、debug pause、shell tool approval——从分散处理统一为一致的 pause/resume 模式。核心改动：

1. **删除 `GatewayApprovalGate`**：旧架构中 approval 在 spawned task 内通过 gRPC pending map 直接等 IntentDelivered 回复
2. **引入 `ApprovalHandle` + mpsc/oneshot 双通道**：spawned task 通过 mpsc 发请求到 main loop，main loop 暂停等 `InboundMessage::ApprovalDecision`
3. **Gateway 侧拆分双重路径**：gRPC dispatch 不再阻塞 60s 等 approval，改为 spawn 后立即返回空消息；approval 结果同时走 IntentDelivered（旧路径兼容）+ push approval_decision（新路径解 main loop）

---

## 架构正确的部分 ✅

### 1. 统一 pause 模式的方向正确
三种暂停场景现在共享同一个模式：
- **暂停点** → 通知前端 → **阻塞在 `inbound_rx`** → 等待特定 `InboundMessage` → 恢复

这消除了旧架构中 approval 走独立 gRPC pending map 的旁路，逻辑更内聚。

### 2. ApprovalHandle 的 mpsc + oneshot 设计合理
- spawned task 不再需要知道 Gateway 的存在，只向 main loop 提交请求
- main loop 拥有完整的 inbound_rx 控制权，可以在暂停时正确处理 Interrupt 和缓冲其他消息
- oneshot 天然保证一个请求对应一个回复

### 3. Gateway dispatch 的非阻塞化
旧代码在 gRPC handler 内 `timeout(60s, rx).await` 阻塞，导致同一 Runtime 连接上的其他请求（如 session query）被堵住。新代码 spawn 后立即返回，解决了这个实际问题。

### 4. deferred_inbound 的复用
`await_approval_decision` 复用了 `await_debug_resume` 中已验证的 `deferred_inbound` 缓冲模式，非目标消息不会丢失。

---

## 架构缺陷 ⚠️

### P0：并发 approval 请求的死锁风险

**场景**：LLM 返回 2 个 shell tool call（如 bash + powershell），两个 spawned task 同时调用 `ApprovalHandle::request_approval()`。

**问题链**：
1. Task A 发送 `(reqA, oneshotA)` 到 `approval_tx`
2. Task B 发送 `(reqB, oneshotB)` 到 `approval_tx`
3. Main loop 的 `select!` 先收到 reqA → 调用 `handle_approval_request(reqA, oneshotA)` → 进入 `await_approval_decision()` 阻塞在 `inbound_rx`
4. **此时 `approval_rx` 中的 reqB 无人消费**——main loop 在 `await_approval_decision` 中只 poll `inbound_rx`，不再 poll `approval_rx`
5. 用户在前端批准 reqA → `InboundMessage::ApprovalDecision { request_id: "0" }` 到达 → reqA 解除
6. 但 reqB 的 `ToolApprovalNeeded` chunk 从未发送，前端不知道有第二个 approval 等待

**后果**：
- 如果 `approval_rx` 容量（16）未满：reqB 排队等到 reqA 处理完才被消费——**用户体验差**（前端只看到一个 approval 弹窗）
- 如果多个并发 approval 塞满 channel（极端情况）：`request_approval` 在 spawned task 中阻塞，task 无法完成 → `rx.recv()` 不返回 None → main loop 的 `while collected.len() < total` 永远不退出 → **实际死锁**

**修复方向**：
- **方案 A（推荐）**：在 `await_approval_decision` 中也 poll `approval_rx`，收到新 approval 请求时递归处理或排队（但注意不能在 oneshot 上同时等两个条件）
- **方案 B**：在 `handle_approval_request` 之前，先 drain `approval_rx` 中所有待处理请求，批量发送 `ToolApprovalNeeded`，然后依次等回复
- **方案 C（最简）**：限制每次 iteration 最多一个 approval 请求，多余的直接 reject

### P1：gRPC 双重回路的冗余与潜在竞态

新架构中 approval 结果有**两条路径**回到 Runtime：

1. **旧路径**：gRPC dispatch spawn 的 task → `outbound_tx.send(IntentDelivered { "approved:xxx" })` → Runtime gRPC client 的 pending map → `send_intent` 的 oneshot 回复
2. **新路径**：Gateway HTTP approval endpoint → `session.push_message(IntentReceived { "approval_decision" })` → Runtime gRPC client 的 push_rx → cli.rs `process_gateway_recv` → `InboundMessage::ApprovalDecision`

**问题**：
- 旧路径的 `IntentDelivered` 回复无人消费：因为 `tool_approval_needed` 的 IntentSend 在 cli.rs 中用 `request_id: 0` 发送（fire-and-forget），gRPC client 对 `request_id: 0` 的消息走 push 路径而非 pending map。所以 dispatch spawn 发出的 `IntentDelivered` 到达 client 后，会被当作 push message 处理。但这并不会路由到 `InboundMessage::ApprovalDecision`——它会在 push handler 中被当作一个普通的 `IntentDelivered` 事件处理，可能被忽略或产生意外行为。

- **修复**：应完全移除旧路径（dispatch spawn 中的 `outbound.send(IntentDelivered)`），只保留新路径（push approval_decision）。否则当旧路径的 `IntentDelivered` 到达 cli.rs 的 push handler 时，`action` 字段会是 `intent_delivered` 而非 `approval_decision`，不会触发正确的路由。

### P2：`allow_all_session` 语义丢失

旧 `GatewayApprovalGate` 返回 `ApprovalResponse::AlwaysAllow`，在 `check_shell_approval` 中会跳过后续 approval。新的 `ApprovalDecision { allow_all_session: bool }` 被标记为 `#[allow(dead_code)]`——**收到 `allow_all_session: true` 后没有任何逻辑利用这个信息**。

这意味着用户点 "Allow All Session" 后，后续的 shell 命令仍然会弹 approval 对话框——功能退化。

**修复方向**：在 `check_shell_approval_handle` 中，需要一个机制记住 "当前 session 已 allow all"——可以是 `AgentCore` 上的 `shell_approval_bypassed: AtomicBool`，也可以在 `ApprovalHandle` 层面维护一个 session-level flag。

### P3：`check_shell_approval` 和 `check_shell_approval_handle` 大量代码重复

两个函数约 70% 代码相同（参数解析、风险评估、阈值比较、Blocked 检查），只有最后的 approval 请求方式不同。这是典型的 DRY 违反，后续改一处忘另一处的风险高。

**修复方向**：提取 `assess_shell_risk_for_approval(params_json, threshold) -> Option<(ApprovalRequest, ShellRisk)>` 共享函数，两个 check 函数只负责发送请求和处理回复。

### P4：iteration timeout 与 approval pause 的交互

`execute_tools_parallel` 的 `select!` 中有 iteration deadline。当 main loop 在 `handle_approval_request` 中阻塞等待用户决定时，**iteration deadline 计时器仍在倒计时**。

场景：
1. iteration timeout = 120s
2. Shell tool 需要approval，用户在第 100s 才决定
3. Main loop 恢复后回到 `select!`，但 deadline 已过 → 立即 timeout → 其他并行 tool 被abort

**实际上**，由于 `handle_approval_request` 是在 `select!` 分支内 await 的，在 await 期间 `sleep_until(deadline)` 的 future 会被取消（`select!` 只有一个分支执行），所以 deadline 不会在 approval 等待期间过期。但是 **approval 等待回来后**，如果 deadline 已过，下一轮 `select!` 会立即触发 timeout。这个行为是否合理需要确认——用户刚批准了一个命令，结果因为总时间超时而被 abort。

### P5：`request_id` 的命名空间冲突风险

`approval_next_id: AtomicU64` 在 `AgentLoop` 级别递增，而 gRPC dispatch 侧也有自己的 request_id 空间（来自 `client_msg.request_id`）。两者都用字符串 "0", "1", "2"... 作为 `request_id`，但来源不同。

当 dispatch 返回 `proto::ServerMessage { request_id: 0, payload: None }` 作为即时回复时，如果 Runtime 侧恰好有 `request_id: 0` 的 pending request（虽然 `request_id: 0` 在 gRPC client 中表示 fire-and-forget），不会直接冲突。但长期来看，如果 dispatch spawn 的延迟回复的 `request_id` 与其他请求冲突，可能导致 oneshot 被错误消费。

**当前代码中**：dispatch spawn 中 `request_id` 来自 `client_msg.request_id`（即原始 IntentSend 的 request_id），而 cli.rs 中 `tool_approval_needed` 的 IntentSend 用的是 `request_id: 0`。所以 dispatch spawn 发出的 IntentDelivered 回复的 `request_id` 也是 0——会被 client 识别为 push message 而非 pending response。这进一步证实了 P1 中的问题：这条旧路径回复实际上变成了一个无用的 push message。

### P6：Gateway dispatch 的空回复缺乏文档约定

`proto::ServerMessage { request_id: 0, payload: None }` 作为即时回复返回，但 Runtime gRPC client 对 `request_id: 0` 的消息走 push 路径。这意味着 Runtime 会收到一个 `payload: None` 的 push message，最终被当作未知消息忽略（或打印 warn 日志）。虽然不会 crash，但会产生噪音日志。

**修复**：如果保留旧路径（建议不保留），应使用专用 `request_id` 或在 client 侧添加 ack 处理逻辑。

---

## 修复记录（2026-05-20）

### P0 修复：并发 approval 死锁

**变更**：`await_approval_decision` 从单路 `inbound_rx.recv()` 改为 `tokio::select!` 双路轮询 `inbound_rx` + `approval_rx`。

- 收到 `approval_rx` 上的新请求时，递归调用 `handle_approval_request` 处理（发送 `ToolApprovalNeeded` 到前端），然后继续等待当前 request_id 的 decision
- 收到不同 request_id 的 `ApprovalDecision` 时，缓冲到 `deferred_inbound`
- `handle_approval_request` 改为返回 `Pin<Box<dyn Future>>` 以支持递归 async

### P1 修复：target 不匹配 + 双重回路清理

**根因**：cli.rs 中 `ToolApprovalNeeded` 的 IntentSend 用 `target="http-ws"`，但 Gateway dispatch 的 `handle_tool_approval_needed_grpc` 只匹配 `target=="http-api"`。导致 approval 请求走了通用 `handle_intent_send` 路径——虽然 BridgeEvent 转发到了前端，但**没有创建 `approval_pending` oneshot**。用户点击 Allow 后，HTTP approval endpoint 找不到 pending entry → 返回 404 → 无法推送 `approval_decision` 回 Runtime。

**变更**：
1. cli.rs 中 `ToolApprovalNeeded` 的 IntentSend `target` 从 `"http-ws"` 改为 `"http-api"`，并添加 `agent_id` 参数
2. 移除 `handle_tool_approval_needed_grpc` 中 spawn 的 `outbound.send(IntentDelivered)` 旧路径——新架构用 push `approval_decision` 传递结果
3. 简化函数签名：移除 `request_id: u64` 和 `outbound_tx` 参数，early-return 路径统一返回空消息

---

## 未修复项（后续跟进）

| 优先级 | 问题 | 状态 |
|--------|------|------|
| **P2** | `allow_all_session` 语义丢失 | 待修 |
| **P3** | 风险评估代码重复 | 待修 |
| **P4** | approval + timeout 交互 | 待确认行为 |
| **P5** | request_id 命名空间 | 低优先级 |
| **P6** | 空 reply 噪音日志 | 随 P1 一起已解决 |
