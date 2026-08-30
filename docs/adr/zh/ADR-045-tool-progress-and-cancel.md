# ADR-045：工具执行进度心跳与单工具取消

**状态**：已实施
**日期**：2026-08-03
**决策者**：大鱼

**前置**：
- ADR-014（AgentLoop 主循环模块拆分）
- ADR-021（取消数据面流式推送、改由前端 HTTP 拉取）
- ADR-033（MQTT 替换 gRPC + WebSocket — 控制通道统一为 MQTT）
- ADR-034（MQTT / HTTP 职责边界）
- ADR-044（Stop 信号链路的 CancelHandle 统一化）

**触发动因**：
- 单个 shell 工具执行时间可能 1~10 分钟（`cargo build`、`npm install`、大文件下载、长时间运行的 watch 命令），超过用户耐心阈值
- 现有 UX（`apps/acowork-desktop/src/components/chat/ExploreBlock.tsx:614-616`）：工具执行期间前端只显示一个 `animate-pulse rounded-full` 灰点，**用户看不到已用时间、剩余时间、也无法中途取消**
- 调研事实（[`tools/builtin/shell.rs:276-305`](../../core/acowork-runtime/src/tools/builtin/shell.rs)）：`wait_with_output()` 是 OS 级阻塞，期间不发任何 MQTT/HTTP 事件
- 调研事实（[`loop_tools.rs:133-148`](../../core/acowork-runtime/src/agent/loop_tools.rs)）：单工具超时 = `tool_timeout_ms`（默认 10 min），但是被动等待到点
- 用户明确诉求：「前端很不友好，就一直干等」

---

## 1. 目标与非目标

### 1.1 目标
1. **可观测**：长工具执行期间，前端能看到「已用时间 / 总超时」+ 进度条
2. **可干预**：用户能在工具执行中途中止当前工具，让 LLM 看到「被用户中止」的结果并继续推理；不影响 iteration 整体
3. **协议一致性**：复用 `UserOp` + `mqtt_publish_control` + `CancelHandle` 现成机制，不发明新 IPC 通路
4. **UX 渐进（§3.2 / §4 新增）**：心跳延时生效——5s 内完成的工具保持原 UX（仅小灰点），**只有 tool 跑过 5s 才升级为「计时器 + 进度条 + 取消按钮」**。短命令不被打扰，长命令获得完整控制

### 1.2 非目标
- ❌ 不做 shell stdout/stderr 流式回传（属于 ADR-046/后续议题，本次不在范围内）
- ❌ 不做「暂停 / 恢复」单个工具（保持 cancel-only 语义，把复杂度控制在最小）
- ❌ 不变更 `tool_timeout_ms` 默认值（保持 10 min），但心跳事件携带该值让前端可正确显示
- ❌ 不引入新 IPC 通道（不新增 HTTP 端点，cancel 走 MQTT）
- ❌ 不做"工具刚开始就显示完整面板"的激进模式（会破坏短命令的简洁体感）

---

## 2. 现状分析（事实）

### 2.1 工具执行与超时（已有）

```
┌──────────────────────────────────────────────────────────────────┐
│ loop_tools.rs:85-152                                              │
│   tool_timeout = Duration::from_millis(tool_timeout_ms)         │
│   for tc in tool_calls {                                          │
│       tokio::spawn(execute_single_tool(...))                     │
│   }                                                               │
│   for each future:                                                │
│       match tokio::time::timeout(                                  │
│           tool_timeout, await future                              │
│       ) { ... }                                                   │
└──────────────────────────────────────────────────────────────────┘
```

- [`shell.rs:260-305`](../../core/acowork-runtime/src/tools/builtin/shell.rs)：`tokio::task::spawn_blocking` 里调用 `wait_with_output()` —— **完全阻塞，期间不发任何事件**
- [`timeout_config.rs`](../../core/acowork-core/src/timeout_config.rs)：`tool_timeout_ms = 600_000`（10 min）；`iteration_timeout_ms = 900_000`（15 min）

### 2.2 Stop / Pause / Resume 处理模式（已有，可复用）

```
[1] Desktop ChatPanel.tsx
        invoke("mqtt_publish_control", { command: "stop", payload: {...} })
        ↓
[2] Tauri mqtt_publish_control → Gateway MQTT broker
        publish on acowork/agents/{id}/control/{sid}
        ↓
[3] Runtime gateway_loop.rs:66  mqtt_dispatch_tx.send(...)
        parse_control_payload → ControlAction
        ↓ control_action_to_inbound
[4] InboundMessage::Stop { reason }  ─┐
   InboundMessage::UserOperation(op) ─┤  走 session_task inbox
                                      ↓
[5] AgentLoop self.inbound_rx
        ↓ poll_control()（每次 checkpoint 非阻塞 try_recv）
[6] ControlDecision::Stop  → 内部流程返回
        ↓ ADR-044 CancelHandle  +  urgent_stop Notify  (loop_tools.rs:218/271)
[7] select! 命中 → handle.abort() / kill()
```

**已有 4 个关键设施，本次必须复用**：

1. `core/acowork-runtime/src/agent/inbound.rs:44-64` 的 `UserOp` enum（要新增 `CancelTool` 变体）
2. `core/acowork-runtime/src/startup/gateway_loop.rs:145+` 的 `control_action_to_inbound` 单映射器（要新增 `CancelTool` 路由）
3. `core/acowork-core/src/mqtt/control_handler.rs` 的 `ControlAction` enum（要新增 `CancelTool` 变体）
4. `core/acowork-runtime/src/agent/loop_tools.rs:218/271` 的 `tokio::select!` 中的 urgent_stop Notify 模式（要新增 tool-level cancel 触发点）

### 2.3 当前 UX（`ExploreBlock.tsx:609-617`）

```tsx
{isSuccess ? <Check /> :
 isError   ? <X /> :
 isPendingResult ? <span className="... animate-pulse rounded-full bg-zinc-300" /> :
 null}
```

- 工具从「tool_call 入库」到「tool_result 入库」之间，UI **没有时间显示、没有倒计时、没有取消按钮**
- ADR-021 已删除 streaming 数据事件，前端必须在 HTTP 拉取的 JSONL 上额外叠加心跳

---

## 3. 决策

### 决策 A：进度心跳（每次工具执行 N=5 秒发一次）

#### 3.1 新增事件类型

在 `core/acowork-runtime/src/agent/loop_.rs:69-` 的 `ChunkEvent` enum 新增（注意：ADR-021 把数据事件删掉了，但本事件是**纯控制面信号**，不携带数据载荷给前端写 UI，只是触发「重渲染 / 更新计时器」）：

```rust
/// ADR-045: Tool execution progress heartbeat.
/// Pure control-plane signal — carries NO tool result data.
/// Frontend uses it to refresh a timer/countdown display only.
ToolProgress {
    session_id: String,
    tool_call_id: String,
    elapsed_ms: u64,    // 自工具 spawn 起的总耗时
    timeout_ms: u64,    // = tool_timeout_ms（前端用来算进度百分比）
},
```

> **严格边界**：此事件不可携带任何 stdout/stderr、错误信息、tool_result 字段。前端拿到它后只应该用它刷新已经显示的「Running… Xm Ys」标签，不能拿它替换「已经完成的 tool_result」。

#### 3.2 发心跳的位置

在 `core/acowork-runtime/src/agent/loop_tools.rs` 的 `execute_single_tool` 外层（70~152 行之间）用 `tokio::select!`，**不动 inner tool 实现的任何代码**：

```rust
// Pseudocode (实际代码见后续 PR)
let tool_start = Instant::now();
let progress_handle = tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.tick().await; // 跳过首次立即 tick
    loop {
        interval.tick().await;
        let event = ChunkEvent::ToolProgress {
            session_id: session_id.clone(),
            tool_call_id: tc.id.clone(),
            elapsed_ms: tool_start.elapsed().as_millis() as u64,
            timeout_ms: tool_timeout_ms,
        };
        if chunk_tx.try_send(event).is_err() { break; }
        if tool_start.elapsed() > Duration::from_millis(tool_timeout_ms) { break; }
    }
});

let result = match tokio::time::timeout(tool_timeout, future).await { ... };

progress_handle.abort();  // 完成后立即停止心跳 goroutine
```

- 心跳 Task 内是 try_send 非阻塞 → 不影响工具执行
- 完成后 `abort()` 立刻停
- 不增加任何 IPC 协议字段，只是 ChunkEvent 的一个变体，复用现有 MQTT 通道

**首次心跳延时发送（5s 体验阀）**：

```rust
let mut interval = tokio::time::interval(Duration::from_secs(5));
interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
// 关键：第一个 tick 是即时的，但我们需要等 5s 才发——所以先 tick_once() 丢弃立即触发
interval.tick().await;  // 跳过立即触发（吃掉 Instant::now 时刻的 tick）
loop {
    interval.tick().await;
    // ... try_send ChunkEvent::ToolProgress
}
```

**为什么 5s（设计依据）**：
- **短命令不打扰**：常见工具（`ls`、`grep`、`cat`、单文件 read、web_fetch 等）几乎总在 5s 内结束。如果第 1s 就升级 UI，每条命令前面都会"闪一下"完整面板再消失，反而是体验回归
- **长命令不延误**：5s 是用户开始焦虑的边界。从这个点起显示「已用 5s / 10m 0s + 进度条 0.8%」+ 取消按钮 —— 用户能立刻感知「这个工具卡了」
- **零协议成本**：不需要新增「UI 升级触发事件」，**第 1 条心跳本身就是信号**。前端只要 `progressByToolCallId.has(id) === true` 就升级 UI，**网络延迟可忽略**（心跳 5s 触发，到前端 5.0x s，到 UI 升级也是 5.0x s，精度可接受）
- **取消按钮的可用窗口**：工具超时 = 10 min。5s 触发 UI 升级，意味着用户从第 5s 起就有 9m55s 的窗口可以点取消

> 这是 UX-only 决策，**对协议/事件载荷零影响**。后端只需确保第 1 条心跳不在工具开始就发出。

#### 3.3 心跳事件的下游

- `ChunkEvent::ToolProgress` → `try_send_chunk` → `MqttChunkPublisher` → 走现有 `acowork/agents/{id}/chunks/{sid}` 主题 → Gateway → Desktop 订阅
- 与现有 `RecordComplete` 并行投递，**不阻塞主流程**
- 心跳失败（mqtt broker down）→ try_send 失败 → 静默吞掉，**不让心跳影响工具执行本身**
- **前端以「收到第 1 条心跳」为 UI 升级信号**——短命令（5s 内完成）前端始终收不到心跳，UX 维持原状；长命令收到心跳后从 `pendingToolsCount` 灰点升级为完整面板（详见 §4）

### 决策 B：取消单个工具

#### 3.4 新增 UserOp / InboundMessage / ControlAction 变体

| 文件 | 加什么 |
|------|--------|
| `core/acowork-runtime/src/agent/inbound.rs:44-64` | `UserOp::CancelTool { tool_call_id: String }` |
| `core/acowork-runtime/src/agent/inbound.rs:67-` | `InboundMessage::UserOperation(UserOp)`（已有，未变），承载上述 UserOp |
| `core/acowork-core/src/mqtt/control_handler.rs` | `ControlAction::CancelTool { session_id, tool_call_id }` |
| `core/acowork-runtime/src/startup/gateway_loop.rs:145-` | `control_action_to_inbound` 新增 match 分支：`Some((sid, InboundMessage::UserOperation(UserOp::CancelTool { tool_call_id })))` |
| Desktop Tauri `mqtt_publish_control`（已有） | **不需要改 Rust 后端**，前端只需发 `command: "cancel_tool"` 即可（command name 由 `parse_control_payload` 解析） |

#### 3.5 Runtime 内取消路径（核心）

在 `loop_tools.rs` 的 `execute_single_tool` 外层包一层「per-tool cancel token」：

```rust
// Pseudocode
let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
self.pending_tool_cancels.borrow_mut().insert(tc.id.clone(), cancel_tx);

let result = tokio::select! {
    res = tokio::time::timeout(tool_timeout, execute_inner(...)) => res,
    _ = cancel_rx.wait_for(|v| *v) => {
        // kill inner process（见决策 D）
        Err(ToolError::CancelledByUser)
    }
};

self.pending_tool_cancels.borrow_mut().remove(&tc.id);
```

- 在 `AgentLoop` 上加新字段 `pending_tool_cancels: Rc<RefCell<HashMap<String, watch::Sender<bool>>>>`
- 由 `apply_user_op(&UserOp::CancelTool)` 设置对应 `cancel_tx.send(true)`
- 拿 watcher 不用 mutex，零争用
- cancel 后整个工具流程返回 `Err(ToolError::CancelledByUser)`，**LLM 收到 "Tool X was cancelled by user after Ys" 后继续推理**

#### 3.6 取消后的工具结果

`tool_result` JSON 形如：

```json
{
  "success": false,
  "error": "Cancelled by user after 12s",
  "exit_code": null,
  "stdout": "<已读到的输出>",
  "stderr": ""
}
```

- LLM 看到 error 后能选择：换工具、改参数、问用户
- **绝不静默丢失**：哪怕被取消了，写到 `tool_result` + 历史里，保证 LLM 看到

### 决策 D：取消实现细节（最重要的工程问题）

#### 3.7 shell 工具的进程清理（现状已有）

`shell.rs:296-305` 的 `ProcessGuard` 在 Drop 时调 `child.kill()` + `child.wait()` —— **这个机制已经能正确处理中途取消**，只要 outer future 在 `cancel_rx` 触发时被 `Drop`。`tokio::select!` 的取消语义正是这个：分支命中 → outer future 被 drop → `ProcessGuard` 被 drop → 子进程被 kill。这条路径上**不需要改 shell.rs**。

#### 3.8 WASM 工具（WASI）

WASM 工具由 wasmtime 跑，**目前不支持运行时取消**（wasmtime 实例一旦进入 host call 就无法中断）。本次保留限制：

- 取消事件会先到，但只在 tool_natural return 时才生效
- 对普通 shell 工具（占 90% 时长）已足够
- 在 tool_timeout 上限内兜底

> 后续如果 WASM 取消成为强需求，可以在 ADR-046 单独做

---

## 4. 前端 UX

### 4.1 渐进式升级（核心设计）

UI 在工具执行过程中存在两种状态，**由"是否收到过心跳"触发切换**：

| 阶段 | 触发 | UI 形态 | 适用工具 |
|------|------|---------|---------|
| **Phase A**（原有） | tool_call 入库 → 第 1 条心跳到达前 | 仅呼吸灰点 + 「Running…」label | 5s 内完成的短命令 |
| **Phase B**（新增） | 第 1 条心跳到达后 | 灰点 + **计时器** + **进度条** + **取消按钮** | 5s+ 的长命令 |

切换由 store 的 `progressByToolCallId.has(tool_call_id)` 决定 —— 零网络抖动（心跳 5s 触发，前端升级也是 5s 触发，误差 <100ms）。

**短命令体验对照**（**不会回归**）：
```
Before:  ⏳ Running ls …        (灰点)
After:   ⏳ Running ls …        (灰点，不变——心跳在 5s 前已停止，永远不触发 Phase B)
```

**长命令体验对照**：
```
Before:  ⏳ Running cargo build…  (灰点，0 反馈)
After:   ⏳ Running cargo build…  (灰点，0~5s)
         ⏳ Running cargo build…  (0:05 / 10:00  ▓░░░░░░░░░░  0.8%  [X])   ← 5s 触发
         ⏳ Running cargo build…  (0:10 / 10:00  ▓░░░░░░░░░░  1.7%  [X])   ← 10s 触发
         …
```

### 4.2 ExploreBlock 改动（`apps/acowork-desktop/src/components/chat/ExploreBlock.tsx`）

`ToolCallItem` 改成由 `hasProgress` 决定渲染分支：

```tsx
// Phase A：5s 前
{isPendingResult && !hasProgress && (
  <span className="... animate-pulse rounded-full bg-zinc-300" />
)}

// Phase B：5s 后
{isPendingResult && hasProgress && (
  <>
    {/* 1. 计时器（基于心跳事件中的 elapsed_ms） */}
    <span className="text-zinc-500 font-mono text-[10px] tabular-nums">
      {formatElapsed(elapsed_ms)} / {formatElapsed(timeout_ms)}
    </span>

    {/* 2. 进度条（基于 elapsed_ms / timeout_ms） */}
    <div className="h-1 w-12 rounded bg-zinc-200 dark:bg-zinc-600 overflow-hidden">
      <div
        className="h-full bg-amber-400 dark:bg-amber-500 transition-all"
        style={{ width: `${Math.min(100, (elapsed_ms / timeout_ms) * 100)}%` }}
      />
    </div>

    {/* 3. 取消按钮 */}
    <button
      onClick={handleCancel}
      disabled={cancelling}
      className="text-zinc-400 hover:text-red-500 transition-colors"
      title="Cancel this tool"
    >
      <X className="h-3 w-3" />
    </button>
  </>
)}
```

- **计时器**只在收到心跳时更新（不本地自增），避免本地时间和 server 不同步；通过 `tabular-nums` 防数字跳动
- **进度条**颜色选 amber 而非 accent/绿——amber 是「警告 / 注意」语义，与「工具卡了」语境一致
- **进度条满 100%**不需特殊处理——这时通常 tool_timeout 已被 trigger，UI 自然切到「Timed out」状态
- **取消按钮**自身 disable + 防止重复点；cancel 后等 tool_result 事件，灰点自动收起

### 4.3 ExploreBlock 顶栏的「running」提示同步升级

`ExploreBlock.tsx:343-348` 现有「Exploring... (N steps)」不动（这是 step 级别提示）。但**新增一条 5s 后的辅助文案**：

```tsx
{!expanded && !hasFollowUpReply && hasLongRunningTools && (
  <>{" · "}<span className="text-amber-600">{t("exploreBlock.longRunning")}</span></>
)}
```

只有当 `progressByToolCallId.size > 0`（即至少 1 个工具收到心跳）才显示，提示用户「有工具在跑」。

### 4.2 Store 处理

`apps/acowork-desktop/src/stores/chat-store.ts` 加：

```ts
interface PendingToolProgress {
  tool_call_id: string;
  elapsed_ms: number;
  timeout_ms: number;
  received_at: number;
}

// reducer on ToolProgress event:
state.progressByToolCallId.set(event.tool_call_id, { ... });
```

存到 `Map<tool_call_id, PendingToolProgress>`，tool_result 到后清掉。

### 4.3 取消按钮的发送

复用 `mqtt_publish_control` + `command: "cancel_tool"`：

```ts
await invoke("mqtt_publish_control", {
  agentId,
  command: "cancel_tool",
  payloadJson: {
    session_id: currentSessionId,
    tool_call_id: call.tool_call_id,
  },
});
```

Tauri Rust 后端不需要任何改动（command name 是字符串动态分发的）。

---

## 5. 协议兼容性

### 5.1 向后兼容

- `ChunkEvent::ToolProgress` 是新变体；旧前端收到会进 unmatched 分支，**忽略即可**，不报错
- `UserOp::CancelTool`/`ControlAction::CancelTool` 是新变体；旧 runtime 收到会按 `parse_control_payload` 失败 → log + 忽略
- 不影响任何已有事件

### 5.2 主题与 QoS

- 走 `acowork/agents/{id}/chunks/{sid}` 主题，QoS 0（at-most-once），与现有控制事件一致
- 心跳丢失可接受，下一次 5s 后补发

---

## 6. 取舍

### 6.1 为什么不在 `RecordComplete` 之外再开一个新主题？

- 主题爆炸问题（参见 ADR-035 D2.1）
- 心跳不是独立数据流，是控制信号 → 归入 `chunks/{sid}` 与现有 `RecordComplete` 同流
- 心跳失败代价极低（仅丢一次 5s 进度显示）

### 6.2 为什么用 `watch` channel 而不是 `AtomicBool`？

- `watch::Sender` 可以被 clone + 多次取消（未来如果需要「取消 all tools in batch」）
- `wait_for()` 是 cancel-safe，未来加超时组合也方便
- 当前一个 token ≈ 24B HashMap entry，开销可忽略

### 6.3 为什么不动 tool_timeout_ms 默认值？

- 用户提的是前端体验问题，不是配置问题
- 默认值改了，反而把根因（心跳缺失）掩盖
- ADR-045 不动配置，只补前端信号

### 6.4 为什么不用 HTTP？

- cancel 是一次性、不能丢、不需要历史追溯 → MQTT QoS 0 已足够
- 与 `approval_decision` 走同一通路（`ChatPanel.tsx:1084`），前端心智模型一致
- 走 HTTP 反而引入「为什么 stop/approve 走 MQTT、cancel 走 HTTP」的不一致

---

## 7. 实施步骤（增量交付）

> 严格按项目「增量交付」工程原则，每步都是 reviewable 的小 diff

### 7.1 步骤 0：协议层 ✅
- [x] 在 `control_handler.rs` 加 `ControlAction::CancelTool { session_id, tool_call_id }`
- [x] 在 `inbound.rs` 加 `UserOp::CancelTool`
- [x] 在 `gateway_loop.rs` 加 `control_action_to_inbound` 匹配分支
- [x] 常量 `acowork_core::timeout_config::constants::TOOL_HEARTBEAT = 5s`
- [x] 单测：`test_parse_control_cancel_tool`（cargo check --tests 通过）

### 7.2 步骤 1：runtime 取消路径 ✅
- [x] 在 `AgentLoop` 加 `pending_tool_cancels` 字段
- [x] 在 `loop_inbound.rs` 加 `cancel_tool_by_id` 真实现（替换 stub）
- [x] 在 `loop_tools.rs` spawn 外创建 `watch::channel` + 注册到 `pending_tool_cancels`
- [x] 在 `loop_tools.rs` 加 `tokio::select!` 包 `cancel_rx.wait_for` 分支
- [ ] 单测：shell 进程取消后子进程被 kill（**P1，下一轮补**）

### 7.3 步骤 2：心跳发送 ✅
- [x] 在 `mqtt_payload.proto` 加 `ToolProgressPayload` 消息 + `session_message::Event` oneof field 32
- [x] 在 `MqttChunkPublisher` 加 `publish_tool_progress` 方法
- [x] 在 `ChunkEvent` 加 `ToolProgress` 变体
- [x] 在 `subsystems.rs` 加 `relay_chunk_event_mqtt` 匹配分支
- [x] 在 `loop_tools.rs` spawn 闭包内加 heartbeat task（5s 间隔，跳过首次 tick）
- [x] 在 `chat_mqtt.rs`（Tauri）加 `session_message::Event::ToolProgress` JSON 序列化
- [x] 在 `mqtt_client.rs`（Tauri）加 `control_command::Command::CancelTool` 路由
- [ ] 单测：长工具期间观察到 ≥1 条心跳（**P1，下一轮补**）

### 7.4 步骤 3：前端 store ✅
- [x] `chatStore.ts` 加 `toolProgress: Record<tool_call_id, { elapsedMs; timeoutMs }>` 字段
- [x] 处理 `tool_progress` ChunkEvent 增量更新；tool_result 到达后清空对应 entry
- [ ] 单测：tool_result 到达后清空对应 entry（**P1，下一轮补**）

### 7.5 步骤 4：前端 UI ✅
- [x] `ToolCallItem` 增加计时器/进度条/取消按钮
- [x] 取消按钮接 `chatStore.cancelTool` → `mqtt_publish_control cancel_tool`
- [x] 仅 `toolProgress[id]` 存在时升级为 Phase B；5s 内完成的工具保持原 UX（Phase A 灰点）
- 手动测：发起一条 30s+ sleep 命令，观察 UI 有心跳显示，点击取消能停掉进程

### 7.6 步骤 5：文档与 ADR 归档 ✅
- [x] `docs/protocols/zh/mqtt.md` §3.2 + §9.3 增补 ToolProgress / cancel_tool
- [x] `docs/protocols/zh/README.md` §3 sequence + §5 导航同步
- [x] 更新本 ADR 状态为「已实施」

---

## 8. 验证

| 场景 | 期望 |
|------|------|
| `sleep 120` shell 工具 | 8~9s 后看到第一次心跳，UI 显示「1m 12s / 10m 0s」 |
| 点击取消按钮（10s 后） | ≤500ms 内工具进程消失（**关键**），tool_result 含 "Cancelled by user after 10s"，LLM 继续推理 |
| WASM 工具（无 cancel 支持） | 取消事件到达但 UI 仍显示执行中；tool_timeout 兜底 |
| 心跳丢失（broker 临时挂） | UI 不更新（最多 5s 不刷新），不会崩 |
| 旧前端 + 新 runtime | 收到 ToolProgress 忽略，不影响功能 |
| 旧 runtime + 新前端 | 收到的 cancel_tool 命令被 log+丢弃，工具自然完成 |

---

## 9. 后续议题（不在本次范围）

- shell stdout/stderr 流式回传（独立议题，需要重写 `shell.rs` 为 `tokio::process::Command`）
- WASM 工具运行时取消（需要 wasmtime 的 epoch interruption）
- iteration-level 暂停（区别于工具级别取消）
