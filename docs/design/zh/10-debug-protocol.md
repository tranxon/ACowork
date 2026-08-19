# Debug Protocol（调试协议）

> 版本：v3.1 | 更新日期：2026-04-14

---

Agent Runtime 的 DevMode 通过 Debug Protocol 与 Desktop App 通信。Debug Protocol 在 ADR-048 之后与生产 IPC 通道完全对齐：RPC 走 HTTP REST、事件走 MQTT pub/sub（与 ADR-033 的生产通道复用同一组传输），不再使用 JSON-RPC over WebSocket。

Desktop App 的开发者模式完全依赖本协议，UI 层设计见 [14-desktop-app.md](./14-desktop-app.md)。

## 1. 总览

```
Desktop App (Tauri)              Agent Runtime (DevMode)
       │                                │
       │  Debug RPC  (HTTP REST)        │
       │  POST/GET .../api/debug/{path} │
       │  Gateway 反代 → Runtime        │
       ├───────────────────────────────>│
       │                                │
       │  Debug Events (MQTT, QoS 0)    │
       │  acowork/agents/{id}/debug/    │
       │    events/{type}               │
       │<───────────────────────────────│
       │                                │
       │  同时，Agent Runtime 仍通过       │
       │  Gateway Service API 获取       │
       │  Key、收发 Intent 等            │
       │         │                       │
       │         ▼                       │
       │  ┌─────────────────┐           │
       │  │  Gateway        │           │
       │  │  (独立进程)      │           │
       │  └─────────────────┘           │
```

Desktop App 与 Agent Runtime 之间有两条独立通道：
- **Debug Protocol**（HTTP RPC + MQTT events，与生产 IPC 同构）：开发模式专用，控制执行流、编辑状态、热加载。RPC 走 Gateway `/api/agents/{id}/debug/{*rest}` 反代到 Runtime `/api/debug/{path}`；事件走 MQTT topic `acowork/agents/{id}/debug/events/{type}`
- **Gateway Service API**：生产通道，Agent 仍通过 Gateway 获取 Key、收发 Intent 等

## 2. 传输层

| 平台 | RPC | 事件 | 说明 |
|------|-----|------|------|
| Linux | HTTP REST（Gateway 反代 → Runtime `/api/debug/*`） | MQTT（`localhost:19875` 共享 broker） | 跨平台一致 |
| macOS | 同上 | 同上 | 同上 |
| Windows | 同上 | 同上 | 同上 |

HTTP RPC 与 MQTT 共享 Gateway 现有端口（HTTP `:19876`、MQTT `:19875`）——Debug Protocol **不再占用独立端口**。Agent Runtime 以 DevMode 启动时，在 localhost HTTP server 上注册 `/api/debug/*` 路由并 spawn MQTT 调试事件 publisher。Desktop App 通过 Gateway 反代访问时，自动获得 `localhost-only` 网关 + ACL 的多用户隔离（详见 ADR-040 / ADR-048）。

## 3. 协议定义

Debug Protocol 由两类消息构成，分别走两条独立通道（ADR-048）：

### 3.0 通道与载荷

| 方向 | 通道 | 载荷格式 |
|------|------|----------|
| Desktop → Runtime（RPC） | HTTP REST（Gateway 反代 → Runtime `/api/debug/*`） | JSON request/response |
| Runtime → Desktop（事件） | MQTT `acowork/agents/{id}/debug/events/{type}`（QoS 0） | Protobuf（`DebugStepEvent` / `DebugContextBuiltEvent` / `DebugStateChangeEvent`，定义见 `acowork-core/src/protocol.rs`） |

RPC 请求是普通的 HTTP 调用（`method` / `path` / `query` / `body`），由 Gateway 透明转发到 Runtime 的 axum handler；Runtime 解开 Gateway 的 `{ok, data?, error?}` 信封后返回业务结果。事件侧 Runtime 直接 publish 到 MQTT broker，Desktop 通过已建立的 MQTT 订阅收到，由 `chat_mqtt.rs` 解码后 re-emit 到 Tauri `debug-event` 通道。

```text
RPC 请求（Desktop → Gateway → Runtime）：
POST /api/agents/{agent_id}/debug/{path}
Content-Type: application/json

{ "agent_id": "...", "session_id": "...", ... }

→ 200 { "ok": true,  "data": { ... } }
→ 4xx { "ok": false, "error": { "code": "...", "message": "..." } }

事件（Runtime → MQTT → Desktop）：
acowork/agents/{agent_id}/debug/events/onStep            (DebugStepEvent)
acowork/agents/{agent_id}/debug/events/onContextBuilt    (DebugContextBuiltEvent)
acowork/agents/{agent_id}/debug/events/onStateChange    (DebugStateChangeEvent)
```

### 3.0.1 端点映射（方法名 → HTTP / MQTT 主题）

§3.1–§3.9 用业务方法名（`debugger.resume` 等）描述语义。运行时所有 RPC 走 HTTP，所有事件走 MQTT。下表给出方法名到传输端点的一一对应（由 ADR-048 D2 定义、`http/debug.rs` 实现）：

> **实现状态**：✅ = 已迁移（D1-D4）；⏳ = 文档预留、未实现（旧 WebSocket server 同样未实现，见 ADR-048 范围声明，未来走 ADR-053）。

| 业务方法 | 方向 | 端点 / 主题 | 状态 |
|----------|------|-------------|------|
| `debugger.resume` | RPC | `POST /api/debug/resume` | ✅ |
| `debugger.pause` | RPC | `POST /api/debug/pause` | ✅ |
| `debugger.step` | RPC | `POST /api/debug/step` | ✅ |
| `debugger.stop` | RPC | `POST /api/debug/stop` | ✅ |
| `debugger.restart` | RPC | `POST /api/debug/restart` | ⏳（Desktop 走 `restartAgentInDebug` 进程级重启） |
| `debugger.getState` | RPC | `GET /api/debug/state?session_id=…` | ✅ |
| `debugger.setBreakpoint` | RPC | `POST /api/debug/breakpoints` | ⏳ |
| `debugger.removeBreakpoint` | RPC | `DELETE /api/debug/breakpoints/{bp_id}` | ⏳ |
| `debugger.listBreakpoints` | RPC | `GET /api/debug/breakpoints` | ⏳ |
| `debugger.getContextSnapshot` | RPC | `GET /api/debug/context/{iteration}?session_id=…` | ✅ |
| `debugger.getSection` | RPC | `GET /api/debug/context/{iteration}/sections/{section}?session_id=…` | ✅ |
| `debugger.rewind` | RPC | `POST /api/debug/context/rewind` | ✅ |
| `debugger.patchContext` | RPC | `POST /api/debug/context/patch` | ✅ |
| `debugger.reExecute` | RPC | `POST /api/debug/context/re-execute` | ✅ |
| `debugger.editMessage` | RPC | `PATCH /api/debug/messages/{index}` | ⏳ |
| `debugger.rollback` | RPC | `POST /api/debug/messages/rollback` | ⏳ |
| `debugger.reloadSkills` | RPC | `POST /api/debug/skills/reload` | ⏳ |
| `debugger.switchProvider` | RPC | `POST /api/debug/provider/switch` | ⏳ |
| `debugger.startRecording` | RPC | `POST /api/debug/recording/start` | ⏳ |
| `debugger.stopRecording` | RPC | `POST /api/debug/recording/stop` | ⏳ |
| `debugger.loadRecording` | RPC | `POST /api/debug/recording/load` | ⏳ |
| `debugger.stopReplay` | RPC | `POST /api/debug/recording/replay/stop` | ⏳ |
| `debugger.onStep` | 事件 | MQTT `acowork/agents/{id}/debug/events/onStep` | ✅ |
| `debugger.onBreakpoint` | 事件 | MQTT `acowork/agents/{id}/debug/events/onBreakpoint` | ⏳ |
| `debugger.onRecordStep` | 事件 | MQTT `acowork/agents/{id}/debug/events/onRecordStep` | ⏳ |
| `debugger.onStateChange` | 事件 | MQTT `acowork/agents/{id}/debug/events/onStateChange` | ✅ |
| `debugger.onContextBuilt` | 事件 | MQTT `acowork/agents/{id}/debug/events/onContextBuilt` | ✅ |

> **路径前缀**：Desktop 调 Gateway 时挂 `/api/agents/{agent_id}/debug/{rest}`，由 Gateway 透明转发到 Runtime `/api/debug/{rest}`（详见 `acowork-gateway/src/http/proxy.rs` 中的 Debug 通配反代规则，ADR-048 D5）。

### 3.1 执行控制

```rust
/// 恢复自动执行
method: "debugger.resume"
params: {}

/// 暂停主循环，停在下一个迭代步
method: "debugger.pause"
params: {}

/// 执行一步主循环后暂停
method: "debugger.step"
params: {
    /// 断点粒度
    "granularity": "iteration" | "phase"
}

/// 终止当前对话
method: "debugger.stop"
params: {}

/// 重启对话（清空历史，从初始状态重新开始）
method: "debugger.restart"
params: {}
```

### 3.2 状态查询

```rust
/// 获取当前对话完整状态
method: "debugger.getState"
params: {}
result: {
    "iteration": 3,
    "phase": "ToolExecution",
    "messages": [...],
    "snapshot_ids": ["snap-0", "snap-1", "snap-2"],
    "breakpoints": [...],
    "usage": { "prompt_tokens": 1500, "completion_tokens": 300 }
}
```

```rust
enum Phase {
    BudgetCheck,
    BuildContext,
    LlmCall,
    ParseResponse,
    ToolExecution,
    AppendHistory,
    Idle,
}
```

### 3.3 断点

```rust
/// 设置断点
method: "debugger.setBreakpoint"
params: {
    "condition": {
        "type": "on_phase" | "on_tool_call" | "on_iteration" | "on_tool_result",
        // 根据 type 不同：
        // on_phase: { "phase": "ToolExecution" }
        // on_tool_call: { "tool_name_pattern": "http_*" }
        // on_iteration: { "iteration": 3 }
        // on_tool_result: { "is_error": true }
    }
}
result: { "breakpoint_id": "bp-001" }

/// 移除断点
method: "debugger.removeBreakpoint"
params: { "breakpoint_id": "bp-001" }

/// 列出所有断点
method: "debugger.listBreakpoints"
params: {}
result: { "breakpoints": [...] }
```

### 3.4 上下文快照与检查（Context Snapshot & Inspection）

DevMode 下，Agent Runtime 在每轮迭代的 `BuildContext` 阶段完成后，自动捕获**上下文构建结果**。调试面板按轮次将其树状展开，仅展示 5 个控制面 section：

| Section | 内容 | 调试用途 |
|---------|------|---------|
| `system_prompt` | 系统级指令 | 调试 prompt 工程 |
| `tool_definitions` | 可用工具及参数 Schema | 验证工具注册、修复 Schema 错误 |
| `skill_instructions` | 加载的 SKILL.md 内容 | 调试 Skill 行为 |
| `retrieved_memory` | Grafeo 检索的记忆节点 | 验证记忆检索质量 |
| `identity_context` | 用户身份字段 | 检查身份注入 |

> **设计决策**：`conversation_history` **排除**在调试面板外。左侧聊天面板已按时间线完整展示所有消息——调试面板不需要重复展示只读的对话结果，聚焦于"控制面"即可。

```rust
/// 获取指定轮次的上下文构建快照（仅返回元数据摘要，不含完整内容）
method: "debugger.getContextSnapshot"
params: {
    "iteration": 3  // 轮次编号（0-based）
}
result: {
    "iteration": 3,
    "built_at": "2026-05-09T12:00:00Z",
    "sections": {
        "system_prompt":      { "size_bytes": 2048, "token_estimate": 512,  "hash": "a1b2..." },
        "tool_definitions":   { "size_bytes": 4096, "token_estimate": 1024, "hash": "e5f6..." },
        "skill_instructions": { "size_bytes": 1536, "token_estimate": 384,  "hash": "i9j0..." },
        "retrieved_memory":   { "size_bytes": 3072, "token_estimate": 768,  "hash": "m3n4..." },
        "identity_context":   { "size_bytes": 512,  "token_estimate": 128,  "hash": "q7r8..." }
    },
    "total_token_estimate": 2816,
    "phase": "BuildContext"
}

/// 懒加载某个 section 的完整内容（用户在调试面板点击展开时按需拉取）
method: "debugger.getSection"
params: {
    "iteration": 3,
    "section": "tool_definitions"  // 5 个 section 名之一
}
result: {
    "content": "...",               // 完整文本内容
    "hash": "e5f6...",              // 内容完整性校验
    "token_count": 1024
}
```

**性能策略**：`getContextSnapshot` 仅返回元数据（<500 字节/轮）。section 内容通过 `getSection` 按需懒加载。配合前端默认折叠 + 虚拟滚动，100+ 轮对话的调试面板仍保持流畅。

### 3.5 上下文编辑与回退（Context Editing & Rewind）

当调试发现 tools/skills 上下文有问题时，用户可回退到指定轮次、修补上下文后重新执行：

```
调试工作流:
  1. debugger.getContextSnapshot({ iteration: 3 })  → 检查上下文
  2. debugger.rewind({ to_iteration: 3 })            → 回退到第 3 轮起始状态
  3. debugger.patchContext({ patches: {...} })       → 修补上下文 section
  4. debugger.reExecute()                            → 以修补后的上下文重新执行
```

```rust
/// 回退到指定轮次的起始状态
/// 清除该轮次边界之后的所有消息，允许以修改后的上下文重新执行。
/// 同时清除所有已设置的 patchContext 补丁。
method: "debugger.rewind"
params: {
    "to_iteration": 3  // 回退到第 3 轮 BuildContext 之前的状态
}
result: {
    "rewound_to_iteration": 3,
    "messages_trimmed_to": 12  // messages 数组截断后的长度
}

/// 为下一次 reExecute 修补上下文 section
/// 补丁是临时的——仅在下次 reExecute 时生效，执行后或 rewind 后自动清除。
/// 可多次调用以增量构建补丁。
method: "debugger.patchContext"
params: {
    "patches": {
        "system_prompt": "Updated system instructions...",    // 可选
        "tool_definitions": [{ "name": "...", ... }],          // 可选：替换工具列表
        "skill_instructions": "Updated skill content...",      // 可选
        "retrieved_memory": [...],                             // 可选：覆盖检索记忆
        "identity_context": { "field": "value" }               // 可选
    }
    // 每个 key 均为可选——仅传入的 section 会被修补，其余保持不变
}

/// 以修补后的上下文重新执行当前轮次
/// 如果已通过 patchContext 设置了补丁，则在此次执行中生效。
/// 执行完成后补丁自动清除，Runtime 恢复正常流程（或在断点/Step 模式下暂停）。
method: "debugger.reExecute"
params: {}
result: {
    "iteration": 4,  // 新轮次编号（递增）
    "output": { ... }
}
```

**设计约束**：
- `rewind` 和 `patchContext` 是**分离的操作** —— rewind 不会自动触发 reExecute，必须显式编辑后再执行
- 补丁是**临时性**的 —— 在 reExecute 完成后或 rewind 调用时自动清除
- `patchContext` 可在 reExecute 前**多次调用**以增量构建编辑

**消息级操作**（轮次内的细粒度编辑）：

```rust
/// 编辑对话历史中的某条消息
method: "debugger.editMessage"
params: {
    "index": 2,
    "content": { ... }  // 新的 MessageContent
}

/// 回滚到指定消息索引，丢弃后续消息
method: "debugger.rollback"
params: { "target_index": 2 }
```

### 3.6 Skill 热加载

```rust
/// 重新加载 skills 目录
method: "debugger.reloadSkills"
params: {
    /// 可选，只重载指定 skill
    "skill_name": null | "weather-query"
}
```

### 3.7 Provider 切换

```rust
/// 动态切换 LLM Provider
method: "debugger.switchProvider"
params: {
    "provider": "openai" | "ollama" | "anthropic" | ...,
    "model": "gpt-4o" | "qwen3:8b" | ...,
    /// 可选，覆盖 base_url
    "base_url": null | "http://localhost:11434/v1"
}
```

切换流程：
1. Desktop App 发送 `debugger.switchProvider`
2. Agent Runtime 更新 LLM Client 的当前 provider 配置
3. 如果需要新 Key → 通过 Gateway KeyRelease 获取（如果 Vault 中有）
4. 如果是本地 Provider (ollama) → 直连，无需 Key
5. 下一次 LLM 调用使用新 provider

典型工作流：
```
初始开发 → ollama/qwen3:8b（本地免费，快速迭代）
基本可用 → openai/gpt-4o-mini（低成本测试真实 API）
最终验证 → openai/gpt-4o（全功能测试）
```

### 3.8 录制回放

```rust
/// 开始录制当前会话
method: "debugger.startRecording"
params: {}

/// 停止录制并保存
method: "debugger.stopRecording"
params: {
    /// 录制文件保存路径（默认工作区 recordings/ 目录）
    "output_path": null | "/path/to/recording.jsonl"
}

/// 加载录制文件并回放
method: "debugger.loadRecording"
params: {
    "path": "/path/to/recording.jsonl",
    "mode": {
        "type": "auto" | "manual",
        // auto: { "delay_ms": 500 }
        // manual: {}
    }
}

/// 停止回放
method: "debugger.stopReplay"
params: {}
```

### 3.9 事件通知（Runtime → Desktop App，MQTT 通道）

事件走 MQTT 主题 `acowork/agents/{agent_id}/debug/events/{type}`，载荷用 Protobuf（见 `acowork-core/src/protocol.rs` 的 `DebugStepEvent` / `DebugContextBuiltEvent` / `DebugStateChangeEvent`）。下表以 JSON 形态给出 payload 字段含义，便于阅读；线缆格式以 Protobuf 为准。

```text
topic: acowork/agents/{agent_id}/debug/events/onStep
payload (DebugStepEvent):
{
    "session_id": "…",        // 事件来源会话
    "iteration": 3,
    "phase": "ToolExecution",
    "input": { ... },         // 本步输入（如有）
    "output": { ... },        // 本步输出（如有）
    "usage": { ... }          // LLM 用量（如有，prompt/completion/total_tokens）
}

topic: acowork/agents/{agent_id}/debug/events/onBreakpoint  （⏳ 未发射，见 ADR-048 范围声明）
payload (DebugBreakpointEvent):
{
    "session_id": "…",
    "breakpoint_id": "bp-001",
    "iteration": 3,
    "phase": "ToolExecution"
}

topic: acowork/agents/{agent_id}/debug/events/onRecordStep  （⏳ 未发射）
payload (DebugRecordStepEvent):
{
    "session_id": "…",
    "step_index": 5,
    "phase": "LlmCall",
    "step_data": { ... }      // 序列化的步骤数据
}

topic: acowork/agents/{agent_id}/debug/events/onStateChange
payload (DebugStateChangeEvent):
{
    "session_id": "…",
    "new_state": "Paused"     // DebugState（Running/Paused/Stepping/Stopped）
                              // 或 DebugPhase 名（LlmCall/…，Runtime 复用同一 topic）
    "iteration": 4
}

topic: acowork/agents/{agent_id}/debug/events/onContextBuilt
payload (DebugContextBuiltEvent):
{
    "session_id": "…",
    "iteration": 3,
    "sections": {
        "system_prompt":      { "size_bytes": 2048, "token_estimate": 512,  "hash": "a1b2..." },
        "tool_definitions":   { "size_bytes": 4096, "token_estimate": 1024, "hash": "e5f6..." },
        "skill_instructions": { "size_bytes": 1536, "token_estimate": 384,  "hash": "i9j0..." },
        "retrieved_memory":   { "size_bytes": 3072, "token_estimate": 768,  "hash": "m3n4..." },
        "identity_context":   { "size_bytes": 512,  "token_estimate": 128,  "hash": "q7r8..." }
    },
    "total_token_estimate": 2816
}
```

> **实现侧**：`mqtt/debug_events.rs::DebugEventMqttPublisher` 把 `DebugEventBus` 收到的 `TaggedEvent` 编码为对应 Protobuf 后 PUBLISH 到上述主题。Desktop 侧 `commands/chat_mqtt.rs` 三个 proto 解码 arm 解码后 emit Tauri `debug-event` 事件，由 `stores/debugStore.ts` 接收。

## 4. 消息快照机制

Agent Runtime 在 DevMode 下的每一轮迭代结束，自动创建一个轻量快照：

```rust
struct ConversationSnapshot {
    /// 快照 ID（递增）
    id: String,
    /// 对应的迭代轮次
    iteration: u32,
    /// 快照时刻 messages 数组的长度（截断点）
    message_count: usize,
    /// 快照时刻的 LLM 用量
    cumulative_usage: Usage,
    /// 时间戳
    timestamp: SystemTime,
}
```

快照的实现极其轻量——messages 数组是 append-only 的，快照只需要记录长度。回滚时截断到目标长度即可，无需深拷贝。

```
messages: [msg0, msg1, msg2, msg3, msg4, msg5]

快照 @ iteration 2: message_count = 4  →  回滚: [msg0, msg1, msg2, msg3]
快照 @ iteration 3: message_count = 6  →  回滚: [msg0, msg1, msg2, msg3, msg4, msg5]
```

## 5. 录制格式

录制的会话保存为 JSONL 文件，每行一个步骤：

```jsonl
{"type":"recording_header","agent_id":"com.example.weather-dev","timestamp":"2026-04-09T12:00:00Z","provider":"openai","model":"gpt-4o"}
{"type":"user_input","content":"北京今天天气怎么样","iteration":0}
{"type":"llm_request","messages_count":2,"iteration":0}
{"type":"llm_response","content":"tool_call(http_request,...)","usage":{"prompt_tokens":150,"completion_tokens":30},"iteration":0}
{"type":"tool_call","name":"http_request","params":{"method":"GET","url":"https://api.weather.com/v1?city=Beijing"},"iteration":0}
{"type":"tool_result","name":"http_request","result":{"temp":25,"condition":"晴"},"iteration":0}
{"type":"llm_request","messages_count":4,"iteration":1}
{"type":"llm_response","content":"北京今天25度，晴天","usage":{"prompt_tokens":200,"completion_tokens":20},"iteration":1}
```

录制文件保存在 Agent 工作区的 `recordings/` 目录下。

### 5.1 回放模式

| 模式 | 说明 | 适用场景 |
|------|------|---------|
| **自动回放** | 按录制顺序自动推进，每步可设延迟 | 全流程演示、回归测试 |
| **手动步进** | 每步需用户手动 Step 推进 | 逐帧检查、调试特定步骤 |
| **对比回放** | 加载多个录制文件，同屏对比 | A/B 测试不同 Provider/Prompt 的效果 |

### 5.2 回放与编辑结合

回放过程中可以随时：
- 编辑某步的消息内容，然后 Re-execute
- 切换 Provider 后从某步重新执行
- 插入新的用户消息，偏离原录制路径，进入自由调试

录制文件既是"回归测试用例"，也是"调试起点"。

### 5.3 录制 + Provider 切换

回放模式下可以切换 Provider，实现"同样的对话，不同的 LLM"对比：

```
录制: 用 gpt-4o 录制了一段完整对话
回放: 切换到 qwen3:8b，回放同样的用户输入和工具调用
      → 对比两个模型对同样上下文的回复差异
```

## 6. DevMode vs 生产模式

Agent Runtime 的 DevMode 是生产模式的**超集**：

| 维度 | DevMode | 生产模式 |
|------|---------|---------|
| Debug Protocol | HTTP RPC `/api/debug/*`（localhost Runtime HTTP）+ MQTT 调试事件 publisher（共享 `:19875`） | 不注册 routes、不 spawn publisher |
| 主循环 | 受调试器控制（Pause/Step/Resume） | 自动连续执行 |
| 上下文快照 | 每轮自动创建上下文快照（5 section） | 不创建 |
| 上下文编辑 | 支持迭代级回退与修补（`rewind`/`patchContext`/`reExecute`） | 不支持 |
| 消息快照 | 每步自动创建 ConversationSnapshot | 不快照 |
| Provider 切换 | 动态可切换（`debugger.switchProvider`） | 按 manifest 固定配置 |
| 录制 | 可录制/回放（JSONL） | 不录制 |
| Skill 加载 | 热加载（`debugger.reloadSkills`） | 启动时一次性加载 |
| 消息编辑 | 支持（`debugger.editMessage`） | 不支持 |
| 消息回滚 | 支持（`debugger.rollback`） | 不支持 |

生产模式下 Agent Runtime 与 03-agent-runtime.md 设计完全一致。DevMode 的复杂度全部封装在 Agent Runtime 和 Desktop App 内部，Gateway 不需要任何修改。

DevMode 启动方式（Gateway 侧）：

```toml
# Gateway 启动 Agent 时，如果 Agent 标记为 dev: true，则追加 --dev-mode 参数
agent-runtime /path/to/agent --endpoint pipe://agent-gateway --agent-id com.example.weather-dev --dev-mode
```

## 7. Agent 克隆协议

Agent 克隆通过 Gateway HTTP API 执行（Desktop App 调用，Gateway 执行），不通过 Debug Protocol。定义如下：

### 7.1 克隆请求

```http
POST /api/agents/:id/clone
Content-Type: application/json

{
  "mode": "skeleton" | "full",
  "new_id": "com.example.weather-dev"
}
```

### 7.2 克隆流程

```
Desktop App → Gateway POST /api/agents/:id/clone
       │
       ▼
Gateway:
  ├─ 读取源 Agent 工作区
  ├─ 按模式复制文件:
  │   ├─ skeleton: manifest.toml (清除 agent_id, 置为 new_id)
  │   │             prompts/ (完整复制)
  │   │             config/ (完整复制)
  │   │             tools/ (完整复制)
  │   │             resources/ (完整复制)
  │   │
  │   └─ full 额外复制:
  │       skills/ (完整复制)
  │       data/ (完整复制)
  │       conversations/ (当前 session JSONL 快照)
  │       memory/private.grafeo (复制快照)
  │
  ├─ 写入新 Agent 工作区:
  │   ~/.local/share/agent-gateway/agents/<new_id>/
  │
  ├─ 新 Agent 标记为 dev: true
  │
  └─ 返回克隆结果
```

### 7.3 克隆限制

- 系统 Agent（`com.acowork.system`）不可克隆——无 Platform 签名，无法获得系统特权
- 克隆体与源 Agent 独立，后续源 Agent 更新不会同步
- 完整克隆的 Grafeo 快照是克隆时刻的副本，之后双方各自演化

## 8. 发布流程

调试完成的 Agent 从开发态转为发布态，通过 Desktop App 的发布向导执行。

### 8.1 发布步骤

```
Step 1: 检查
  ├─ manifest.toml 完整性校验
  ├─ 必填字段检查（agent_id, version, name, runtime_version）
  ├─ skills/ 目录下每个 SKILL.md 格式校验
  ├─ prompts/ 目录下文件存在性检查
  └─ 权限声明合理性检查

Step 2: 清理
  ├─ 移除 dev 标记（manifest 中 dev: false 或删除该字段）
  ├─ 清空 recordings/ 目录（不打包进发布包）
  ├─ 清空或保留 data/（用户选择）
  ├─ 清空私有 Grafeo（发布包不含个人记忆）
  └─ 重置 config/ 为默认值（可选）

Step 3: 打包
  ├─ 按 .agent 包格式打包为 ZIP
  └─ 输出到 build/<agent_id>-<version>.unsigned.agent

Step 4: 签名
  ├─ 调用 acowork-sign 签名
  ├─ 可选：选择密钥（已有 / 新生成）
  └─ 输出 build/<agent_id>-<version>.agent

Step 5: 分发
  ├─ 本地安装：Gateway Package Manager 安装到生产位置
  ├─ 仓库上传：推送到配置的仓库源
  └─ 导出文件：仅保存 .agent 文件到指定路径
```

### 8.2 发布 API

```http
# Step 1-2: 验证 + 清理
POST /api/agents/:id/publish/prepare
→ { "ready": true, "warnings": [...] }

# Step 3-4: 打包 + 签名
POST /api/agents/:id/publish/build
body: { "sign_key": null | "/path/to/key" }
→ { "output_path": "build/com.example.weather-1.0.0.agent" }

# Step 5a: 本地安装
POST /api/agents/:id/publish/install-locally
body: { "package_path": "build/com.example.weather-1.0.0.agent" }

# Step 5b: 导出文件
POST /api/agents/:id/publish/export
body: { "package_path": "...", "export_to": "/user/choosen/path" }
```

## 9. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 协议格式（ADR-048） | HTTP REST（RPC）+ MQTT pub/sub（事件） | 与生产 IPC 完全同构（ADR-033 / ADR-034），无需引入第三种协议栈；ACL / 多用户隔离天然继承 |
| 调试面板范围 | 仅 5 个控制面 section，排除 conversation_history | conversation_history 已在左侧聊天面板完整展示；调试面板聚焦于可编辑的"控制面"上下文 |
| 上下文快照 | 元数据摘要 + 懒加载 | 每轮 <500 字节元数据（size/token/hash），section 内容按需拉取；配合虚拟滚动保证百轮对话的流畅性 |
| 上下文编辑模型 | rewind + patchContext + reExecute 分离 | rewind 不自动触发执行，编辑后需显式 reExecute；补丁临时生效，执行后自动清除 |
| 快照机制 | 记录 message_count | 极轻量，无需深拷贝；messages 是 append-only，截断即可回滚 |
| 录制格式 | JSONL | 逐行追加写入，无需完整序列化；崩溃不丢失已录制内容；易于调试和人工审阅 |
| DevMode 启动参数 | `--dev-mode` CLI flag | Gateway 通过启动参数控制，Runtime 侧零配置变更 |
| DevMode 是超集 | 不改变生产逻辑 | 生产模式下代码路径完全不变；DevMode 仅在检测到 flag 后初始化调试组件 |
| 端口默认值 | 19877（ADR-048 后不再占用） | 可配置，但默认值应避免与常见服务冲突；ADR-048 后 Debug RPC 走 Runtime localhost HTTP 随机端口 + Gateway 反代，MQTT 事件复用 `:19875`，不再监听独立 Debug 端口 |
| Agent 克隆走 Gateway API | 不走 Debug Protocol | 克隆是 Gateway 侧的文件操作，与 Agent Runtime 无关 |
| 调试中会话的 DevMode 入口 | Agent 克隆 + 克隆体 `--dev-mode` 启动 | 不采用运行时动态切换 DevMode；克隆体隔离数据，原 Agent 不受影响 |
| 克隆体 conversations 目录 | full 模式复制当前 session JSONL | 支持"聊天到一半开启调试"场景，克隆体恢复对话状态 |
