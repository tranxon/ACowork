# 通信协议

> 版本：v3.6 | 更新日期：2026-05-06

> **v3.6 变更**：新增 §1.5 Session 管理 IPC 消息 Proto 定义，支持 Session Actor 多会话并发模型。Session 相关 Gateway ↔ Runtime IPC 消息通过 gRPC Bidirectional Streaming 传输，与 GatewayRequest/Response 的自定义二进制帧分离。

**交叉引用**：
- Session Actor 架构：`15-conversation-persistence.md` §1.7
- Agent 运行时主循环：`03-agent-runtime.md` §2
- Session 生命周期与 JSONL 安全：`15-conversation-persistence.md` §1.5、§1.9

---

## 0. 通信架构总览

Rollball 平台有三条独立的通信通道，各司其职：

```
┌────────────────┐         ┌────────────────┐         ┌────────────────┐
│  Desktop App   │         │  Agent Runtime │         │  Agent Runtime │
│  / CLI         │         │  (Agent A)     │         │  (Agent B)     │
└───────┬────────┘         └───────┬────────┘         └───────┬────────┘
        │                          │                          │
        │ HTTP API                 │ Socket API               │ Socket API
        │ (REST + WS)              │ (二进制帧, IPC)            │ (二进制帧, IPC)
        │                          │                          │
        ▼                          ▼                          ▼
┌──────────────────────────────────────────────────────────────────────┐
│                         Gateway (单进程)                              │
│                                                                      │
│  HTTP API ──────┐  ┌──────── Socket API ────────┐                   │
│  (Axum)         │  │                             │                   │
│  管理面操作       │  │  Agent IPC 操作              │                   │
│  (CRUD, 对话)    │  │  (Key, Intent, 预算)         │                   │
│                 │  │                             │                   │
└─────────────────┘  └─────────────────────────────┘                   │
└──────────────────────────────────────────────────────────────────────┘
        ▲                          ▲                          ▲
        │                          │                          │
        │                          │ Intent Router 转发        │
        └──────────────────────────┼──────────────────────────┘
```

| 通道 | 消费者 | 协议 | 用途 |
|------|--------|------|------|
| **Socket API** | Agent Runtime | 二进制帧（自定义） | Key 分发、Intent 通信、预算上报、速率协调 |
| **HTTP API** | Desktop App / CLI | REST + WebSocket | Agent 管理、对话、Vault、配置 |
| **Debug Protocol** | Desktop App (DevMode) | JSON-RPC 2.0 over WebSocket | 步进调试、录制回放、Skill 热加载 |

Socket API（第 1-2 节）：Agent Runtime 与 Gateway 之间的二进制帧 IPC。
HTTP API（[04-gateway.md](./04-gateway.md) §9）：Desktop App / CLI 对 Gateway 的 REST/WebSocket 调用。
Debug Protocol（[10-debug-protocol.md](./10-debug-protocol.md)）：Desktop App DevMode 对 Agent Runtime 的 JSON-RPC 2.0 WebSocket 调试通道。

## 1. Gateway Service API（Socket API）

Agent Runtime 与 Gateway 通过 **Gateway Service API** 通信。API 的消息格式和交互语义是**平台无关的合同**，传输层由各平台自行选择。

### 1.1 合同层 vs 实现层

| 层次 | 内容 | 说明 |
|------|------|------|
| **合同层**（所有平台必须遵守） | 帧格式、消息类型、请求/响应 JSON schema、握手协议 | Agent 开发者只需关心此层 |
| **实现层**（各平台自行决定） | 传输方式、进程模型、沙箱方式 | 不影响 .agent 包兼容性 |

**传输层实现选择：**

| 平台 | 传输方式 | Endpoint 格式 |
|------|---------|--------------|
| Linux | Unix Domain Socket | `unix:///tmp/agent-gateway.sock` |
| macOS | Unix Domain Socket | `unix:///tmp/agent-gateway.sock` |
| Windows | Named Pipe | `pipe://agent-gateway` |
| Android | Abstract Namespace Socket / Local TCP | `abstract://agent-gateway` / `tcp://127.0.0.1:19876` |
| iOS | Local TCP | `tcp://127.0.0.1:19876` |

Agent Runtime 启动时通过参数接收 endpoint 字符串，内部根据 scheme 选择传输实现。

### 1.2 握手协议

连接建立后，Gateway 与 Agent Runtime 依次完成握手、密钥下发、身份注入和能力概览推送：

```json
// ① Agent Runtime → Gateway
{
    "type": "handshake",
    "agent_id": "com.example.weather",
    "runtime_version": "1.0.0",
    "protocol_version": 1
}

// ② Gateway → Agent Runtime（握手确认）
{
    "type": "handshake_ack",
    "capabilities": ["streaming"],
    "key_delivery": "in_band"
}

// ③ Gateway → Agent Runtime（推送 API Key）
{
    "type": "key_delivery",
    "provider": "openai",
    "api_key": "sk-..."
}

// ④ Gateway → Agent Runtime（推送用户身份）
{
    "type": "identity_delivery",
    "fields": {"name": "张三", "city": "Shanghai", "language": "zh-CN", "timezone": "Asia/Shanghai"}
}

// ⑤ Gateway → Agent Runtime（推送邻居能力概览，名字级摘要）
{
    "type": "capability_overview",
    "agents": [
        {
            "agent_id": "com.rollball.system",
            "running": true,
            "capabilities": ["identity:query", "identity:observe"]
        },
        {
            "agent_id": "com.example.calendar",
            "running": false,
            "capabilities": ["create_event", "query_events", "delete_event"]
        }
    ]
}
```

握手之后的所有消息，不管底层传输是什么，格式完全一致。

**握手顺序说明：** Gateway 在握手确认后依次推送 Key → Identity → Capability Overview，Agent Runtime 在收到全部信息后才进入主循环就绪状态。Key 和 Identity 已在 03-agent-runtime.md 中说明，Capability Overview 详见第 2 节。

### 1.3 帧格式

```
[4 bytes: body length (u32 big-endian)]
[1 byte:  message type (0=request, 1=response, 2=stream_chunk, 3=error)]
[N bytes: JSON body]
```

### 1.4 API 定义

Agent Runtime 只在这些操作上和 Gateway 通信（不代理 LLM 调用和工具执行）：

```rust
enum GatewayRequest {
    // --- 密钥 ---
    KeyRelease { provider: String },           // 获取 API Key（启动时一次性）

    // --- Intent ---
    IntentSend {
        target: String,
        action: String,
        params: serde_json::Value,
        async_: bool,
    },

    // --- Capability 查询 ---
    CapabilityQuery {
        target: Option<String>,                // None = 查询所有已安装 Agent
    },

    // --- 预算协调 ---
    BudgetQuery { provider: String },           // 查询剩余预算
    UsageReport(UsageReport),                   // 上报 LLM 用量

    // --- 速率协调 ---
    RateAcquire { provider: String },           // 申请速率令牌

    // --- 运行时权限请求 ---
    PermissionRequest {
        permission: String,
        reason: String,
    },
}

enum GatewayResponse {
    KeyReleaseResult { api_key: String },
    IntentDelivered { message_id: String },
    IntentReceived { from: String, action: String, params: serde_json::Value },
    CapabilityQueryResult {
        agents: Vec<AgentCapabilityInfo>,       // 见 2.4 节
    },
    BudgetInfo { remaining_tokens: u64, remaining_cost_usd: f64 },
    UsageReportAck {},
    RateToken { granted: bool, retry_after_ms: Option<u64> },
    PermissionResult { granted: bool, reason: Option<String> },
}
```

## 2. 跨 Agent 通信（Intent 机制）

Agent 通过 Gateway 的 Intent Router 发送消息请求调用另一个 Agent 的能力。

### 1.5 Session 管理 IPC 消息（gRPC）

> v3.6 新增（2026-05-06）：支持 Session Actor 多会话并发模型。

Session 相关的 Gateway ↔ Runtime IPC 消息通过 gRPC Bidirectional Streaming 传输。消息格式基于 Protocol Buffers，与 GatewayRequest/Response 的自定义二进制帧分离。

#### Proto 定义

```protobuf
// session.proto — Session 管理消息格式

// Runtime → Gateway：Session 列表查询响应
message SessionList {
    repeated SessionInfo sessions = 1;
}

message SessionInfo {
    string session_id = 1;
    string title = 2;
    int64 message_count = 3;
    string status = 4;  // "active" | "idle" | "ended"
    int64 created_at = 5;     // Unix timestamp ms
    int64 last_active_at = 6; // Unix timestamp ms
}

// Runtime → Gateway：Session 消息查询响应
message ConversationMessages {
    repeated ConversationLine lines = 1;
    bool has_more = 2;
    optional string cursor = 3;     // 下一批的起始行号
}

message ConversationLine {
    string id = 1;
    string role = 2;    // "user" | "assistant" | "tool_call" | "tool_result" | "think" | "system"
    string content = 3;
    map<string, string> metadata = 4;  // tool_name, tool_call_id, model 等
    int64 ts = 5;                       // Unix timestamp ms
}


// Runtime → Gateway：创建 Session 响应
message CreateSessionResponse {
    string session_id = 1;
    bool success = 2;
    optional string error = 3;
}

// Runtime → Gateway：激活 Session 响应
message ActivateSessionResponse {
    string session_id = 1;
    bool success = 2;
    optional string error = 3;
}

// Runtime → Gateway：删除 Session 响应
message DeleteSessionResponse {
    bool success = 1;
    optional string error = 2;
}
```

#### 与 GatewayRequest/Response 的关系


| 消息类型 | 通道 | 协议 | 用途 |
|---------|------|------|------|
| `GatewayRequest / Response` | Socket API | 自定义二进制帧 | Key、Intent、Budget、Rate、Permission |
| `SessionList / ConversationMessages` 等 | gRPC Bidirectional Stream | Protocol Buffers | Session CRUD、消息查询 |
| `IntentReceived`（含 action=chat_message） | gRPC Push | Protocol Buffers | **Chat 消息**（包含 session_id） |


**为什么 Session 用 gRPC 而非 Socket API**：
- Session 消息查询需要返回大量结构化数据（历史消息列表），二进制帧不适合批量数据
- gRPC 的 streaming 支持自然对应"Gateway push → Runtime"的实时消息模式
- 与 LLMConfigDelivery 等现有 gRPC 消息共用同一连接

#### 未知 role 类型的降级处理

当前 `ConversationLine.role` 支持：
- `user` / `assistant` / `tool_call` / `tool_result` / `think` / `system`
- 未来可能扩展（如 `system_message` / `error`）

**降级规则**：
```
读到未知 role → 当 "system" 类型处理
前端行为：展示为普通消息（content 正常显示）
向后兼容：新 role 类型出现时不会破坏已有 Session 的读取
```

### 2.1 Intent 消息格式

#### 消息结构

```rust
/// Intent 消息——Agent 间通信的标准信封
struct IntentMessage {
    /// 消息类型标识，固定为 "intent"
    r#type: String,                    // "intent"

    /// 目标 Agent ID（必填，显式指定）
    /// Rollball 不支持隐式 Intent，target 必须是已安装 Agent 的 agent_id
    target: String,

    /// 请求的动作名称（必填）
    /// 必须匹配目标 Agent manifest 中声明的 capability action
    action: String,

    /// 动作参数（必填，至少为 {}）
    /// 结构必须匹配目标 Agent capability 声明的 input schema
    params: serde_json::Value,

    /// 调用模式
    /// true = 异步（发送即忘，结果通过 callback 查询）
    /// false = 同步（阻塞等待结果，超时由 timeout_ms 控制）
    async_: bool,

    /// 消息唯一标识（Gateway 生成）
    /// 用于关联请求与响应、追踪投递状态
    id: String,

    /// 发送方 Agent ID（Gateway 自动填充，Agent 不可伪造）
    from: String,

    /// 同步模式超时（毫秒，可选，默认 30000）
    /// 仅 async_=false 时有效。超时后 Gateway 返回超时错误
    timeout_ms: Option<u64>,

    /// 响应模式（可选，默认 "direct"）
    /// "direct" = 结果直接返回给调用方
    /// "callback" = 结果通过目标 Agent 的 callback_intent 推送（用于 observe 模式）
    response_type: Option<String>,
}

/// Intent 响应——目标 Agent 处理后返回的结果
struct IntentResponse {
    /// 消息类型标识
    r#type: String,                    // "intent_response"

    /// 原始 Intent 的消息 ID
    request_id: String,

    /// 响应状态
    status: IntentStatus,

    /// 结果数据（成功时填充）
    /// 结构应匹配目标 Agent capability 声明的 output schema
    result: Option<serde_json::Value>,

    /// 错误信息（失败时填充）
    error: Option<IntentError>,
}

enum IntentStatus {
    /// 处理成功
    Ok,
    /// 目标 Agent 处理失败
    Error,
    /// 目标 Agent 未安装
    AgentNotFound,
    /// 目标 Agent 启动失败
    AgentStartFailed,
    /// 同步等待超时
    Timeout,
    /// 目标 Agent 的 capability 不匹配（action 不存在）
    CapabilityNotFound,
    /// 参数校验失败
    InvalidParams,
    /// 发送方缺少 intent:send 权限
    PermissionDenied,
}

struct IntentError {
    /// 错误码（机器可读）
    code: String,                      // "AGENT_NOT_FOUND", "TIMEOUT", etc.
    /// 错误描述（人类可读）
    message: String,
}
```

#### JSON 示例

**同步 Intent（请求 + 响应）**：

```json
// 请求
{
    "type": "intent",
    "target": "com.example.calendar",
    "action": "create_event",
    "params": {"title": "Meeting", "time": "2026-01-01T10:00Z"},
    "async": false,
    "id": "msg-456",
    "from": "com.example.assistant",
    "timeout_ms": 10000,
    "response_type": "direct"
}

// 成功响应
{
    "type": "intent_response",
    "request_id": "msg-456",
    "status": "ok",
    "result": {"event_id": "evt-789", "status": "created"},
    "error": null
}

// 失败响应（目标 Agent 未安装）
{
    "type": "intent_response",
    "request_id": "msg-456",
    "status": "agent_not_found",
    "result": null,
    "error": {
        "code": "AGENT_NOT_FOUND",
        "message": "Agent com.example.calendar is not installed"
    }
}
```

**异步 Intent**：

```json
// 请求
{
    "type": "intent",
    "target": "com.rollball.system",
    "action": "identity:observe",
    "params": {"fields": ["city"], "callback_intent": "com.example.weather"},
    "async": true,
    "id": "msg-789",
    "from": "com.example.weather"
}

// 即时响应（仅确认投递成功）
{
    "type": "intent_response",
    "request_id": "msg-789",
    "status": "ok",
    "result": {"subscribed": true},
    "error": null
}

// 后续变更通知（系统 Agent 推送）
{
    "type": "notification",
    "from": "com.rollball.system",
    "action": "identity:changed",
    "params": {"field": "city", "old_value": "Beijing", "new_value": "Shanghai"}
}
```

**observe 模式（ContentProvider）**：

```json
// 订阅请求
{
    "type": "intent",
    "target": "com.rollball.system",
    "action": "identity:observe",
    "params": {"fields": ["city"], "callback_intent": "com.example.weather"},
    "async": true,
    "id": "msg-101",
    "from": "com.example.weather",
    "response_type": "callback"
}
```

#### 字段安全说明

- `from` 字段由 Gateway 在收到 Agent 的 IntentSend 请求后自动填充，Agent 不可自行指定。这防止了身份伪造攻击。
- `id` 由 Gateway 生成（UUID v4），确保全局唯一。Agent 在 IntentSend 请求中无需提供 id。
- `params` 的大小限制为 64 KB（超过时 Gateway 拒绝转发），防止大 payload 攻击。
```

### 2.2 Capability Registry

#### 设计原则

Rollball 不支持隐式 Intent，所有 Intent 调用必须显式指定 `target`（Agent ID）。因此 Capability Registry 只需回答一个问题：**这个 Agent 声明了这个 Action 吗？** 无需 priority 机制，无路由 ambiguity。

#### 数据结构

**单 HashMap，`"{agent_id}:{action}"` 作为 Key**

```rust
pub struct CapabilityRegistry {
    // Key: "com.weather.app:weather:query"
    // Value: CapabilityDef { version, params, description }
    capabilities: HashMap<String, CapabilityDef>,
}
```

#### 用途覆盖

| 用途 | 实现 | 复杂度 |
|------|------|--------|
| **安装依赖检查** | `capabilities.get("{}:{}".format(requires_agent, requires_action))` | O(1) |
| **运行时校验**（可选） | `capabilities.get("{}:{}".format(target, action))` | O(1) |
| **Agent 能力查询** | `capabilities.iter().filter(|(k, _)| k.starts_with("{}:", agent_id))` | O(n) |

Agent 数量有限，O(n) 全量扫描完全可接受。

#### 安装 / 卸载 / 重启

**安装时：**

```
1. 解析 manifest.capabilities
2. 依赖检查：遍历 manifest.requires，逐一查 Registry
   - 存在 → 继续
   - 不存在 → 返回错误 "requires '{agent}:{action}' not found"
3. 写入 Registry：
   for each (action, def) in capabilities:
       capabilities.insert("{}:{}".format(agent_id, action), def)
```

**卸载时：**

```
capabilities.retain(|k, _| !k.starts_with("{}:", agent_id))
```

**Gateway 重启：**

扫描 `~/.local/share/agent-gateway/agents/` 下所有已安装 agent 的 manifest 重建索引。

#### manifest 中 capabilities 声明格式

```json
{
  "capabilities": {
    "weather:query": {
      "input": {"city": "string", "date": "date?"},
      "output": {"temperature": "number", "condition": "string"},
      "description": "查询城市天气"
    },
    "weather:alert": {
      "input": {"city": "string"},
      "output": {"alert_level": "string"},
      "description": "获取天气预警"
    }
  }
}
```

#### 三个用途

1. **安装时依赖检查**：Agent 声明 `requires` 指向其他 Agent 的 capability，Gateway 校验这些 capability 是否已注册。
2. **运行时校验**（可选）：Agent A 向 Agent B 发送 Intent 时，Gateway 可校验 `target:action` 是否在 Registry 中。
3. **运行时查询**：Agent 可通过 `CapabilityQuery` 接口查询其他 Agent 的详细能力（见 2.4 节）。

### 2.3 Intent 路由流程

1. Agent A 通过 Socket 发送 Intent 到 Gateway。
2. Gateway 查找 target Agent B：
   - 若 B 已安装但未运行 → 按 B 的启动策略决定是否拉起（见下方）。
   - 若 B 未安装 → 返回错误 `"Agent not found"`。
3. Gateway 校验 Intent 的 action 和参数是否匹配 B 的 capability 声明。
4. Gateway 将 Intent 转发给 Agent B。
5. Agent B 处理后返回结果。
6. Gateway 将结果返回给 Agent A（同步模式）或缓存等待 Agent A 下次查询（异步模式）。

**目标 Agent 未运行时的启动策略：**

| 场景 | 行为 |
|------|------|
| 同步 Intent + B 未运行 | Gateway 拉起 B，A 阻塞等待（超时由 A 设置） |
| 异步 Intent + B 未运行 | Gateway 拉起 B，A 不阻塞，B 处理完后 Gateway 缓存结果 |
| B 启动失败 | Gateway 返回错误 `"Agent failed to start"` |
| B 的启动策略为"按需" | 正常拉起 |
| B 的 manifest 禁止被 Intent 唤醒 | Gateway 返回错误 `"Agent does not accept intents"` |

### 2.4 Capability 查询机制

Agent 获取其他 Agent 的能力列表有两种途径：

#### 途径 1：启动时注入（Capability Overview）

Agent Runtime 握手时，Gateway 主动推送当前已安装的所有 Agent 及其 **名字级能力摘要**（不含 input/output schema）。这是**最常用的方式**——Agent 在构建 prompt 时就知道系统里有哪些 Agent 能做什么，可以直接向 LLM 描述可用的协作能力。

```json
// Gateway 推送的 capability_overview（名字级摘要）
{
    "type": "capability_overview",
    "agents": [
        {
            "agent_id": "com.rollball.system",
            "running": true,
            "capabilities": ["identity:query", "identity:observe"]
        },
        {
            "agent_id": "com.example.calendar",
            "running": false,
            "capabilities": ["create_event", "query_events", "delete_event"]
        },
        {
            "agent_id": "com.example.todo",
            "running": false,
            "capabilities": ["create_task", "list_tasks"]
        }
    ]
}
```

**为什么只推名字级摘要？**

如果装了 50 个 Agent，每个 5 个 capability 的完整 schema，推送到 prompt 中约需 6000-8000 token——严重挤占上下文空间，稀释 LLM 对核心指令的注意力。名字级摘要同样规模只有 500-800 token，对 LLM 来说已足够做规划决策（"日历 Agent 能 create_event"就够了），精确参数在调用前按需查询即可。

**推送内容说明：**

- 只包含 capability 名称列表，不含 input/output schema。
- `running` 字段表示当前是否在运行，供 Agent 判断调用延迟预期。
- 总量控制在 1000 token 以内。如果已安装 Agent 过多导致超限，Gateway 按以下策略裁剪：
  1. 优先保留 `running: true` 的 Agent
  2. 其次保留本 Agent 在 manifest 中声明了 intent 依赖的 Agent
  3. 其余按 agent_id 字母序截断

**调用时按需获取详细 schema 的流程：**

```
LLM 决定调用日历 Agent 的 create_event
       │
       ▼
Agent Runtime 发送 CapabilityQuery { target: "com.example.calendar" }
       │
       ▼
Gateway 返回完整 capabilities（含 input/output schema）
       │
       ▼
Agent Runtime 将 schema 注入当前迭代的上下文，LLM 据此构造精确参数
       │
       ▼
LLM 输出 tool_call: intent_send(target="com.example.calendar", action="create_event", params={...})
```

#### 途径 2：运行时查询（CapabilityQuery）

当 Agent 需要精确确认某 Agent 当前状态或获取完整 capability schema 时，主动查询 Gateway：

```json
// 查询指定 Agent
{ "type": "capability_query", "target": "com.example.calendar" }

// 查询所有已安装 Agent
{ "type": "capability_query", "target": null }
```

Gateway 返回完整信息：

```json
{
    "type": "capability_query_result",
    "agents": [
        {
            "agent_id": "com.example.calendar",
            "running": true,
            "capabilities": {
                "create_event": {
                    "input": {"title": "string", "time": "datetime", "remind_before": "duration?"},
                    "output": {"event_id": "string", "status": "created|failed"}
                },
                "query_events": {
                    "input": {"from": "date", "to": "date"},
                    "output": {"events": "array<Event>"}
                }
            }
        }
    ]
}
```

**两种途径的适用场景：**

| 场景 | 途径 | 理由 |
|------|------|------|
| 构建系统 prompt 时描述可用协作能力 | 途径 1（启动注入） | 一次性获取，不需要额外通信 |
| 发送 Intent 前确认目标 Agent 是否安装 | 途径 1 | overview 已包含安装信息 |
| 需要完整的 input/output schema | 途径 2（运行时查询） | overview 只含概要信息 |
| 用户安装/卸载 Agent 后刷新能力视图 | 途径 2 | 启动时的 overview 可能已过期 |

#### 途径 1 的更新机制

启动时推送的 overview 是快照，不会随安装/卸载自动更新。Gateway 在检测到以下变更时，主动推送增量更新：

```json
// 新 Agent 安装
{
    "type": "capability_update",
    "action": "installed",
    "agent": {
        "agent_id": "com.example.todo",
        "running": false,
        "capabilities": ["create_task", "list_tasks"]
    }
}

// Agent 卸载
{
    "type": "capability_update",
    "action": "uninstalled",
    "agent_id": "com.example.todo"
}

// Agent 更新（capabilities 变化）
{
    "type": "capability_update",
    "action": "updated",
    "agent": {
        "agent_id": "com.example.calendar",
        "running": false,
        "capabilities": ["create_event", "query_events", "delete_event", "share_calendar"]
    }
}
```

Agent Runtime 收到更新后，刷新本地缓存的 capability 视图，并在下一轮迭代的上下文构建中反映变化。

### 2.5 WorkspaceContextUpdate（工作区上下文更新）

Gateway → Runtime 单播推送。携带已格式化的工作区上下文文本，用于注入 LLM 的 System Prompt。

```json
{
  "type": "workspace_context_update",
  "context_text": "## Workspace Environment\n...",
  "current_workspace_id": "ws-abc123",
  "current_workspace_path": "/home/user/projects/my-project"
}
```

**触发时机**：
- Agent 会话启动时主动推送
- 用户通过 Desktop App 切换当前工作区时推送

## 3. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| Capability 发现方式 | 启动时注入 + 运行时查询 | 启动注入满足常见需求（构建 prompt），运行时查询满足精确需求 |
| Overview 内容粒度 | 名字级摘要（不含 schema） | 50 Agent × 5 capability 完整 schema 约 6000-8000 token，名字级仅 500-800 token |
| 精确参数获取 | 调用前 CapabilityQuery 按需查询 | LLM 规划只需知道"谁会什么"，精确 schema 在执行时才需要 |
| Overview 推送 vs 拉取 | 推送 | Agent 在启动时就需要知道协作环境，拉取需额外通信 |
| 安装/卸载后的更新 | Gateway 主动推送增量 | 避免 Agent 轮询，减少不必要的通信 |
| Overview 超限裁剪策略 | 保留 running + intent 依赖 + 字母序截断 | 确保最相关的 Agent 信息不丢失 |
| Intent 目标未运行 | Gateway 按需拉起 | Agent 开发者无需关心目标 Agent 的运行状态 |
