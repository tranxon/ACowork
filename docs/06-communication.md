# 通信协议

> 版本：v3.0 | 更新日期：2026-04-09

---

## 1. Gateway Service API

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

连接建立后第一条消息用于协商：

```json
// Agent Runtime → Gateway
{
    "type": "handshake",
    "agent_id": "com.example.weather",
    "runtime_version": "1.0.0",
    "protocol_version": 1
}

// Gateway → Agent Runtime
{
    "type": "handshake_ack",
    "capabilities": ["streaming"],
    "key_delivery": "in_band"
}
```

握手之后的所有消息，不管底层传输是什么，格式完全一致。

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
    BudgetInfo { remaining_tokens: u64, remaining_cost_usd: f64 },
    UsageReportAck {},
    RateToken { granted: bool, retry_after_ms: Option<u64> },
    PermissionResult { granted: bool, reason: Option<String> },
}
```

## 2. 跨 Agent 通信（Intent 机制）

Agent 通过 Gateway 的 Intent Router 发送消息请求调用另一个 Agent 的能力。

### 2.1 Intent 消息格式

```json
{
  "type": "intent",
  "target": "com.example.calendar",
  "action": "create_event",
  "params": {"title": "Meeting", "time": "2025-01-01T10:00Z"},
  "async": true,
  "id": "msg-456"
}
```

### 2.2 Capability Registry

每个 Agent 的 manifest 中声明 `capabilities`，Gateway 维护一个 Capability Registry：

```json
{
  "capabilities": {
    "create_event": {
      "input": {"title": "string", "time": "datetime", "remind_before": "duration?"},
      "output": {"event_id": "string", "status": "created|failed"}
    }
  }
}
```

- Agent 安装时，Gateway 检查其 Intent 依赖的 capabilities 是否可用。
- 调用时，Gateway 校验参数类型是否匹配。
- 类似 Android 的 IntentFilter + ContentProvider 机制。

### 2.3 Intent 路由流程

1. Agent A 通过 Unix Socket 发送 Intent 到 Gateway。
2. Gateway 查找 target Agent B，若未运行则启动。
3. Gateway 将 Intent 转发给 Agent B。
4. Agent B 处理后返回结果。
5. Gateway 将结果返回给 Agent A（同步模式）或缓存等待 Agent A 下次查询（异步模式）。
