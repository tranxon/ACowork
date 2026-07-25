# 28 — ADR-033 MQTT 替代 gRPC/WebSocket 重构 Code Review

**Date**: 2026-07-13
**Reviewer**: Senior Engineer
**Status**: 🔴 需修复（3 个 P0 阻碍性 + 5 个 P1 功能缺口 + 5 个 P2 架构清理）
**Scope**: commit `d57301d`（74 个文件变更）

**参考文档**:
- 设计决策: [`docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md`](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md)
- 协议规范: [`docs/zh/protocols/mqtt.md`](../../zh/protocols/mqtt.md)

---

## 整体判断

MQTT 重构在**架构方向**上做出了正确的决策——用 MQTT 替代 gRPC + WebSocket 大幅降低了分布式 Agent 通信的双向连接管理复杂性。**协议独立性（独立 proto 文件 `mqtt_payload.proto`）、Topic 树按数据源分类、三层资源分离（HTTP 全量 → MQTT Retained → 本地文件）、Gateway 不转发业务事件**等设计原则得到了良好的体现。Broker 启动、Runtime 启动序列、LWT 生命周期管理、HTTP 反向代理等核心场景的实现也基本正确。

但在**实现完整性**方面存在多个严重缺口：

- **Retained 消息**（MQTT 的核心优势之一）未生效，退化为 5s 全量轮询
- **Session 流式事件**（最大的高频业务流量）未切换到 MQTT，仍走旧 gRPC 通道
- **MQTT-only 控制指令**（CreateSession/DeleteSession/ModelSwitch）被静默丢弃
- **Desktop ↔ Runtime 的控制命令 payload 格式不一致**（Desktop 发 JSON，Runtime 收 Protobuf）

整体实现完成度约 60%，**3 个 P0 问题在合并到主干前必须解决**。

---

## 一、设计正确之处

### 1.1 协议独立性

`core/acowork-core/proto/mqtt_payload.proto`（441 行）使用完全独立的命名空间 `acowork.mqtt.v1`，与 `gateway_ipc.proto` 零耦合：

- `DataEnvelope` oneof 覆盖全部数据资源类型（global/agent/session/control/memory/sidecar）
- `SessionMessage` oneof 包含 15 种事件类型（chunk/tool_call/done/error/stopped 等）
- `ControlCommand` oneof 包含 7 种命令（create/delete/message/stop/model_switch/reasoning_effort/compact_context）

该独立命名空间为未来格式演进（v2、v3）提供了清晰的版本边界，符合"协议即接口"的设计原则。

### 1.2 模块边界清晰

| Crate | MQTT 子模块 | 职责 |
|-------|------------|------|
| `acowork-gateway/src/mqtt/` | `broker` / `client` / `agent_registry` / `global_resources_publisher` / `acl` / `sidecar` / `router` / `dispatch` / `mod` | 嵌入 broker、连接管理、资源发布、ACL |
| `acowork-runtime/src/mqtt/` | `client` / `control_handler` / `available_cache` / `mod` | Runtime 端连接 + LWT + 缓存 |
| `apps/acowork-desktop/src-tauri/src/` | `mqtt_client` / `commands/chat_mqtt` | Desktop 端 UI ↔ MQTT 桥接 |

每个 crate 内的 MQTT 模块都做到了"一处职责、对外面向协议的接口"，依赖关系单向（`acowork-core` 提供 proto 类型 → 上下游 crate 引用）。

### 1.3 Gateway 不转发业务事件

设计文档 §3.2 明确"Runtime ↔ Desktop 直连 broker"。`gateway/mod.rs#L823-L845` 的 MQTT callback 只处理 `http_port` 和 `status` 两个管理类 topic，业务事件（session 流式、control 命令）由 Desktop 直接订阅 broker 上的 `acowork/agents/{id}/sessions/+/messages/#` 等 topic。这大幅简化了 Gateway 的职责边界。

### 1.4 三层资源分离

| 层级 | 通道 | 内容 |
|------|------|------|
| L1 全量原始列表 | HTTP `GET /api/global/{kind}` | 全部已配置的 provider/mcp/search/embedding model |
| L2 已就绪可用状态 | MQTT Retained `acowork/global/*` | 通过健康检查的 provider/mcp 子集 |
| L3 per-agent 运行时选择 | 本地文件 `~/.acowork/runtime/{id}/agent_config.json` | 用户最终选择的 provider/mcp/model |

清晰的分层避免了"全量列表 vs 已就绪列表"的混淆，Desktop 不必为每个用户跑健康检查。

### 1.5 HTTP 反向代理设计

`core/acowork-gateway/src/http/proxy.rs` 的 `RuntimeHttpRegistry` 通过 agent_id → http_port 映射，将大数据查询代理到 Runtime localhost HTTP server：

```
Desktop ──HTTP──▶ Gateway (:19876) ──HTTP reverse proxy──▶ Runtime localhost (:random)
```

这避免了让 Runtime 暴露在 localhost 之外的接口，同时保留了大数据查询的能力。注册通过 `POST /api/agents/{id}/register` 完成（Phase 2）。

### 1.6 LWT 生命周期管理

`core/acowork-runtime/src/mqtt/client.rs#L112`:

```rust
let will = LastWill::new(&status_topic, "offline", QoS::AtLeastOnce, true);
options.set_last_will(will);
```

+ Drop 时 `clean publish "offline"`，覆盖了崩溃和正常退出两种场景。Status topic Retained 保证 Gateway 重启时仍能感知到 Runtime 状态。

### 1.7 Broker 嵌入与生命周期管理

`core/acowork-gateway/src/mqtt/broker.rs` 的 `start_broker_in_thread()` 用独立 OS 线程启动 rumqttd（避免与 Gateway 主 tokio runtime 冲突），并用 500ms 超时确认 broker 已就绪，不会永久阻塞启动序列。

---

## 二、🔴 P0 阻碍性问题（3 个）

### P0-1: Retained 消息未生效，退化为 5s 轮询

**位置**:
- [`core/acowork-gateway/src/mqtt/global_resources_publisher.rs#L118-L120`](../../../../core/acowork-gateway/src/mqtt/global_resources_publisher.rs#L118-L120)
- [`core/acowork-gateway/src/mqtt/global_resources_publisher.rs#L209-L213`](../../../../core/acowork-gateway/src/mqtt/global_resources_publisher.rs#L209-L213)

**代码现状**:

```rust
// L118-120: 注释承认 rumqttd 0.14 不支持 Retained
// Note: rumqttd 0.14 does not support Retained messages reliably,
// so we use frequent periodic publishes instead (every 5s).
let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

// L213: retain 硬编码为 false
.publish_envelope(topic, envelope, MqttQoS::AtLeastOnce, false)
```

**与设计文档的冲突**:

设计文档 `mqtt.md` §3.5 明确：

> **原则 3:Retained 本身就是快照,推送本身就是增量**

当前实现违背了 MQTT 设计意图——5s 全量轮询意味着：

1. Runtime 新启动时无法立即通过 Retained 消息获取快照，必须等到下一个 5s tick
2. 每次 tick 都全量推送 5 个 topic（providers/mcps/searches/embedding_models/lsps），broker 在多 Runtime 订阅时会有大量重复带宽
3. 如果 Gateway 与 Runtime 短暂断开（< 5s），Runtime 仍可能错过部分状态变更

**建议**:

- **方案 A（推荐）**: 升级 rumqttd 到支持 Retained 的版本（查证 0.15+ 是否修复该 issue）
- **方案 B**: 替换为其他嵌入式 broker（如 `mosquitto-rs`，但需要重新评估生态）
- **方案 C**（短期）：保持轮询但将全量改为增量（用 version+etag 机制），Runtime 端做本地缓存比对

修复后，重写 `publish_envelope_raw`：

```rust
async fn publish_envelope_raw(&self, topic: &str, envelope: &DataEnvelope) {
    self.client
        .publish_envelope(topic, envelope, MqttQoS::AtLeastOnce, true)  // retain=true
        .await
        ...
}
```

### P0-2: Session 流式事件未切换到 MQTT

**位置**:
- [`core/acowork-runtime/src/mqtt/client.rs#L354-L432`](../../../../core/acowork-runtime/src/mqtt/client.rs#L354-L432) (`MqttChunkPublisher`)

**代码现状**:

`MqttChunkPublisher` 的三个方法全部标记 `#[allow(dead_code)]`：

```rust
#[allow(dead_code)]
pub(crate) fn publish_chunk(&self, session_id: &str, message_id: &str, delta: &str) {
    // ...
    let payload = serde_json::json!({       // ← JSON，不是 DataEnvelope！
        "message_id": mid,
        "delta": d,
    });
}
```

更糟糕的是，即使将来启用，它使用的 **JSON** 而非 `DataEnvelope` Protobuf，违反 `mqtt.md` §4 强制规则。

**架构影响**:

Session 流式事件是 MQTT 重构要解决的**最大**业务流量。当前实现意味着：

1. Desktop 看不到任何 session 流式事件（因为 Runtime 从未通过 MQTT 发布）
2. Desktop 可能仍然依赖 HTTP polling（`/api/agents/{id}/sessions/{sid}/messages`）或旧 WebSocket 才能看到流式输出
3. MQTT 重构承诺的"统一通信协议、零推送复杂度"完全未实现

**建议**:

1. 将 `MqttChunkPublisher` 的 payload 从 JSON 改为 `DataEnvelope { payload: SessionMessage }` Protobuf 编码
2. 将 `MqttChunkPublisher` 接线到 session loop（参考 `ChunkRelayTask` 的现有架构）
3. QoS 0 + Retained=false（设计文档 §3.4 规定）
4. 移除 `#[allow(dead_code)]`，加上单元测试

参考实现：

```rust
pub(crate) fn publish_chunk(&self, session_id: &str, message_id: &str, delta: &str) {
    let publisher = self.clone();
    let sid = session_id.to_string();
    let message_id = message_id.to_string();
    let delta_text = delta.to_string();
    tokio::spawn(async move {
        let event = SessionMessage {
            event: Some(session_message::Event::Chunk(ChunkEvent {
                message_id: message_id.clone(),
                delta: delta_text,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        publisher.publish(&sid, "chunk", &bytes).await;
    });
}
```

### P0-3: MQTT-only 模式下 CreateSession/DeleteSession/ModelSwitch 被静默丢弃

**位置**:
- [`core/acowork-runtime/src/startup/gateway_loop.rs#L43-L62`](../../../../core/acowork-runtime/src/startup/gateway_loop.rs#L43-L62)

**代码现状**:

```rust
let _mqtt_handle = ctx.control_rx.take().map(|ctrl_rx| {
    let tx = mqtt_dispatch_tx.clone();
    tokio::spawn(async move {
        let mut rx = ctrl_rx;
        while let Some((topic, payload)) = rx.recv().await {
            let action = crate::mqtt::control_handler::parse_control_payload(&topic, &payload);
            match action {
                Some(ControlAction::SendMessage { ... }) => { ... }
                Some(ControlAction::StopGeneration { ... }) => { ... }
                _ => {}  // ← CreateSession / DeleteSession / ModelSwitch 被静默丢弃
            }
        }
    })
});
```

**矛盾点**:

[`control_handler.rs#L137-L160`](../../../../core/acowork-runtime/src/mqtt/control_handler.rs#L137-L160) 中的 `spawn_control_handler()` 函数**已经正确处理**了所有 5 种控制指令（通过 `InboundMessage::SystemNotification`）。但 `gateway_loop.rs` 没有调用 `spawn_control_handler`，而是内联了一个只处理 2 种指令的简化版，且不支持新加的 `ReasoningEffort` / `CompactContext`。

这导致在 MQTT-only 模式下（`--mqtt-port` 配置启用 gRPC 关闭），Desktop 发送 CreateSession/DeleteSession/ModelSwitch 全部会"看似成功"实际无任何效果。

**建议**:

将 `gateway_loop.rs` 中的内联 dispatch 逻辑替换为调用 `spawn_control_handler`：

```rust
let mqtt_handle = ctx.control_rx.take().map(|ctrl_rx| {
    crate::mqtt::control_handler::spawn_control_handler(
        ctrl_rx,
        ctx.agent_id.clone(),
        mqtt_dispatch_tx,
    )
});
```

删除内联的简化 dispatch 代码。

---

## 三、🟡 P1 功能缺口（5 个）

### P1-1: Desktop 与 Runtime 控制命令格式不匹配

**位置**:
- [`apps/acowork-desktop/src-tauri/src/mqtt_client.rs#L201-L209`](../../../../apps/acowork-desktop/src-tauri/src/mqtt_client.rs#L201-L209)
- [`apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs#L83-L96`](../../../../apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs#L83-L96)

**代码现状**:

Desktop 端：

```rust
pub async fn publish_control_json(&self, agent_id: &str, command: &str, json: &serde_json::Value) -> Result<(), ...> {
    let payload = serde_json::to_vec(json)...;
    self.publish_control(agent_id, command, &payload).await
}
```

Runtime 端（[`control_handler.rs#L71`](../../../../core/acowork-runtime/src/mqtt/control_handler.rs#L71)）：

```rust
let envelope = mqtt_proto::DataEnvelope::decode(payload).ok()?;
```

Desktop 发 JSON 文本，Runtime 解 Protobuf。**两端格式不一致，所有 MQTT 控制命令将解析失败、控制流停滞**。

**建议**:

1. 删除 `publish_control_json` 和 `mqtt_publish_control` 中接受 `serde_json::Value` 的入口
2. 新增 `publish_control_protobuf(agent_id, command, control_command)` 方法，接受强类型 `ControlCommand`
3. 在 Tauri command 层做 JSON → Protobuf 转换（使用 `serde`/`prost` 双格式映射）
4. 这与 [P0-2](#p0-2-session-流式事件未切换到-mqtt) 是同一类问题——所有 MQTT payload 都应统一到 DataEnvelope Protobuf

### P1-2: ReasoningEffort 和 CompactContext 控制指令缺失

**位置**:
- [`core/acowork-runtime/src/mqtt/control_handler.rs#L98-L100`](../../../../core/acowork-runtime/src/mqtt/control_handler.rs#L98-L100)

**代码现状**:

```rust
mqtt_proto::control_command::Command::ModelSwitch(sw) => ControlAction::ModelSwitch { ... },
_ => ControlAction::Unsupported {
    command_type: "unknown".to_string(),
},
```

proto 已定义 `ReasoningEffort` 和 `CompactContext` 两种命令（`mqtt_payload.proto` `ControlCommand` oneof），但解析时落入 unsupported 分支。

设计文档 §5.2 要求支持所有 7 种控制指令；缺失 2 种意味着 Desktop 无法通过 MQTT 切换模型推理强度或触发上下文压缩。

**建议**: 在 `ControlAction` enum 中新增两个变体：

```rust
ReasoningEffort {
    session_id: String,
    effort: ReasoningEffortLevel,
},
CompactContext {
    session_id: String,
},
```

并在 `spawn_control_handler` 中增加 `InboundMessage::SystemNotification` 映射。

### P1-3: Runtime HTTP 端口发布到非标准 Topic 且未 Retained

**位置**:
- [`core/acowork-runtime/src/startup/agent_init.rs#L152-L158`](../../../../core/acowork-runtime/src/startup/agent_init.rs#L152-L158)

**代码现状**:

```rust
// ADR-033: Publish HTTP port so Gateway can proxy session queries.
if let Some(port) = runtime_http_port {
    let topic = format!("acowork/agents/{}/http_port", loaded.manifest.agent_id);
    let _ = client.publish_raw(
        &topic,
        port.to_string().as_bytes(),
        MqttQoS::AtLeastOnce,
    ).await;  // ← retain 默认 false，参数缺失
}
```

**两个问题**:

1. **Topic 偏离设计**: 设计文档 `mqtt.md` Topic 树中没有 `agents/{id}/http_port`，Gateway 端 [`gateway/mod.rs#L823-L827`](../../../../core/acowork-gateway/src/gateway/mod.rs#L823-L827) 是用 `ends_with("/http_port")` 匹配的，能工作但说明是 ad-hoc。
2. **未 Retained**: Gateway 重启时，Runtime HTTP 端口信息丢失，proxy 模块将返回 503。

**建议**:

- 短期：在 `topics` 常量模块中新增 `agents::HTTP_PORT = "acowork/agents/{id}/http_port"` 并文档化
- 同时传 `retain=true`：

```rust
let topic = topics::HTTP_PORT_TEMPLATE.replace("{id}", &loaded.manifest.agent_id);
let _ = client.publish_raw(
    &topic,
    port.to_string().as_bytes(),
    MqttQoS::AtLeastOnce,
    true,  // retain
).await;
```

### P1-4: Desktop 全量订阅 agent 所有 session 消息

**位置**:
- [`apps/acowork-desktop/src-tauri/src/mqtt_client.rs#L170-L171`](../../../../apps/acowork-desktop/src-tauri/src/mqtt_client.rs#L170-L171)

**代码现状**:

```rust
let filter = format!("acowork/agents/{}/sessions/+/messages/#", agent_id);
self.subscribe(&filter, MqttQoS::AtMostOnce).await?;
```

订阅该 agent 的**所有** session 消息事件。

**问题**: 设计文档 §5.1.6 建议按需订阅当前活跃 session。当前实现意味着：

- 即使 Desktop 只显示一个 session，也会收到其他 session 的消息流
- Agent 拥有几十个 session 时，流量浪费严重
- 增加不必要的网络/序列化负担

**建议**:

1. Frontend 进入 chat 时只订阅当前 session：
   ```rust
   let filter = format!("acowork/agents/{}/sessions/{}/messages/#", agent_id, current_session_id);
   ```
2. 切换 session 时先 unsubscribe 旧的 filter，再 subscribe 新的
3. 在 `session_state_changed` 事件触发时动态调整订阅

### P1-5: Runtime HTTP server 文件读取与 Grafeo 集成缺失

**位置**:
- [`core/acowork-runtime/src/http/server.rs`](../../../../core/acowork-runtime/src/http/server.rs)

**两个子问题**:

1. `get_file()` 使用 `read_to_string` 只支持文本文件，二进制文件（图片、PDF）会因 UTF-8 解码失败
2. `get_memory_graph()` 直接读 JSONL 文件，未与 Grafeo 引擎集成

**建议**:

- `get_file()` 改造为根据文件扩展名选择处理：
  ```rust
  // 文本文件：read_to_string + content_type="text/plain"
  // 二进制：read + base64 + content_type="application/octet-stream"
  // 图片：content_type="image/{png,jpeg,...}"
  ```
- `get_memory_graph()` 改为调用 Grafeo 的 query API，JSONL 文件作为 fallback

---

## 四、🟢 P2 架构清理问题（5 个）

### P2-1: gRPC 伪删除——代码路径仍存在但不执行

**位置**:
- [`core/acowork-gateway/src/compat.rs`](../../../../core/acowork-gateway/src/compat.rs) (72 行)
- [`core/acowork-gateway/src/gateway/mod.rs#L899-L923`](../../../../core/acowork-gateway/src/gateway/mod.rs#L899-L923)

**代码现状**:

```rust
// compat.rs L40-46: start_grpc_server 永久 pending
pub async fn start_grpc_server<...>(...) -> Result<...> {
    tracing::info!("gRPC disabled (ADR-033)");
    std::future::pending::<()>().await;  // 永久阻塞
    Ok(())
}
```

Gateway 仍然 spawn gRPC server task，传入 `grpc_session_mgr`、`bridge_ctrl_tx`、`grpc_session_pending` 等参数。虽然 `start_grpc_server` 内部永久 pending 不会造成功能问题，但：

- `grpc_session_mgr` 仍在 `GatewayState` 中创建和传递
- `bridge_ctrl_tx` broadcast channel 仍在 [`gateway/mod.rs#L635`](../../../../core/acowork-gateway/src/gateway/mod.rs#L635) 创建
- `GlobalResourcePusher`、`GrpcSessionStub` 等 stub 类型仍在调用图中存在
- `SharedGrpcSessionMgr = Arc<Mutex<GrpcSessionManager>>` 是空 stub

这些 dead code 增加理解和维护成本，新开发者会困惑"为什么所有调用 gRPC 接口的代码都在但什么都没做"。

**建议**:

短期（不破坏调用方）：保留 compat.rs 但标注 `#[deprecated = "ADR-033: gRPC removed"]`。

中期：在 gateway 主模块中彻底移除 gRPC spawn 路径：

```rust
// 删除 L899-L923 的 grpc_handle spawn
// 删除 bridge_ctrl_tx broadcast channel
// 在 GatewayState 中删除 grpc_session_mgr 字段
// 简化 compat.rs 到一个 #![deprecated] 子模块
```

### P2-2: BridgeEvent 和 WebSocket streaming handler 残留

**位置**:
- [`core/acowork-gateway/src/http/routes.rs#L27-L75`](../../../../core/acowork-gateway/src/http/routes.rs#L27-L75) (`BridgeEventType` enum，15 个变体)
- [`core/acowork-gateway/src/http/chat.rs#L5-L13`](../../../../core/acowork-gateway/src/http/chat.rs#L5-L13)（`/api/agents/{id}/stream` WebSocket handler）

**问题**: 设计文档 ADR-033 明确用 MQTT 替代 WebSocket streaming，这些代码应被移除。但 `BridgeEventType` 仍是 routes.rs 公共 API，且 WebSocket handler 仍是 chat_routes() 注册的路由。

**建议**:

- 短期：标注 `#[deprecated = "ADR-033"]`
- 中期：删除整个 `BridgeEventType` enum 和 WebSocket handler

### P2-3: Router/Dispatch scaffolding 未被使用

**位置**:
- [`core/acowork-gateway/src/mqtt/router.rs`](../../../../core/acowork-gateway/src/mqtt/router.rs) (139 行)
- [`core/acowork-gateway/src/mqtt/dispatch.rs`](../../../../core/acowork-gateway/src/mqtt/dispatch.rs) (181 行)

**问题**:

- `topic_matches()` 纯函数（支持 `+` 和 `#` 通配符）从未被实际消息处理调用
- `route_message()` / `dispatch_message()` 全部返回 `RouteResult::Unimplemented`，所有 payload 类型只记录 debug 日志不执行逻辑
- 实际消息处理通过 `gateway/mod.rs` 中的 inline callback（L823-L845）完成

两个模块文件存在但无功能，构成**虚假完成感**（影响 review 准确度）。

**建议**:

- **方案 A（推荐）**: 完善 router/dispatch 并从 `gateway/mod.rs` 中提取 callback 逻辑（提高内聚）。这要求所有 topic 处理都通过 router/dispatch 单一入口。
- **方案 B**: 删除两个 scaffolding 文件，将 `topic_matches()` 移入 client.rs 作为私有函数。

### P2-4: AgentRegistry 创建但未被 HTTP API 使用

**位置**:
- [`core/acowork-gateway/src/mqtt/agent_registry.rs`](../../../../core/acowork-gateway/src/mqtt/agent_registry.rs)
- [`core/acowork-gateway/src/gateway/mod.rs#L819`](../../../../core/acowork-gateway/src/gateway/mod.rs#L819)

**问题**: `AgentRegistry` 从 MQTT status topic 更新 agent 在线状态（准确的语义层是"在线"+LWT 检测），但 `GET /api/agents?status=active` 仍查询 `GatewayState.running_agents`（进程级 PID 状态）。两者存在语义差异：

- `running_agents`: Gateway 视角，进程是否 alive（通过 ProcessTracker）
- `AgentRegistry`: MQTT 视角，last will 是否触发（通过 broker 通知）

二者可能在 Runtime 崩溃但进程尚未被 Gateway 回收时不一致。

**建议**:

- 短期：在 HTTP API 中以 `running_agents` 为主，`AgentRegistry` 作为 sub-status（"mqtt_last_seen_at"）附加
- 中期：用统一的 AgentStatus 抽象替代两个独立 registry

### P2-5: 代码细节问题

**重复导入**:

[`core/acowork-gateway/src/http/proxy.rs#L15`](../../../../core/acowork-gateway/src/http/proxy.rs#L15) 和 [L25](../../../../core/acowork-gateway/src/http/proxy.rs#L25) 都写了 `use std::collections::HashMap;`。

**连接池浪费**:

[`core/acowork-gateway/src/http/proxy.rs`](../../../../core/acowork-gateway/src/http/proxy.rs) 中 `runtime_http_client()` 每次请求都 `reqwest::Client::new()`。reqwest 官方建议**复用**单个 `Client`（内置连接池）。

```rust
// 当前：每次请求新建
async fn runtime_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

// 建议：lazy_static 或 OnceCell
static RUNTIME_HTTP_CLIENT: OnceCell<reqwest::Client> = OnceCell::new();
fn runtime_http_client() -> &'static reqwest::Client {
    RUNTIME_HTTP_CLIENT.get_or_init(|| reqwest::Client::builder()...)
}
```

---

## 五、架构合理性深度分析

### 5.1 模块边界评估

| 维度 | 评价 |
|------|------|
| **协议层** | ⭐⭐⭐⭐⭐ — 独立 `acowork.mqtt.v1` 命名空间，proto 自包含 |
| **Gateway MQTT 模块** | ⭐⭐⭐⭐ — broker/client/publisher/registry 分工清晰，但 callback inline 在 `gateway/mod.rs` 损害内聚 |
| **Runtime MQTT 模块** | ⭐⭐⭐⭐ — client/control_handler/cache 三模块职责明确，但 `MqttChunkPublisher` 未接线 |
| **HTTP 反向代理** | ⭐⭐⭐⭐⭐ — proxy.rs 独立模块，`RuntimeHttpRegistry` 接口清晰 |
| **Desktop MQTT** | ⭐⭐⭐ — 拆分 mqtt_client.rs 与 chat_mqtt.rs 合理，但 publish_control_json 与 Runtime 协议错配 |

### 5.2 耦合问题

1. **`gateway/mod.rs` 过重（1207 行）**:
   - MQTT callback 逻辑 inline 在 `start()` 方法中（L823-L845），应提取到 `mqtt/handlers.rs` 模块
   - HTTP server 启动参数列表 11 个（L878-L892），包括 `mqtt_gw_client`、`mqtt_publisher_trigger` 等 4 个 MQTT 相关参数
   - 这违反了**高内聚原则**——MQTT 消息处理逻辑应内聚在 MQTT 模块，而非散落在 Gateway 主模块

2. **`agent_init.rs` 职责过载**（读取了前 200 行）:
   - Phase A 初始化函数同时处理：load package（41）→ gRPC connect（49）→ workspace config push（87）→ HTTP server start（126）→ MQTT client connect（140）→ system prompt build（166）→ skill registry load（172）→ provider resolution（184）...
   - MQTT 初始化（L140-L164）应提取为独立函数 `connect_mqtt_phase_a(config, agent_id) -> MqttStartupResult`

3. **`compat.rs` 的存在本身就是耦合**:
   - 让所有调用方代码保持原样（引用 `SharedGrpcSessionMgr`、`GlobalResourcePusher` 等类型）
   - 但这些类型全是 no-op
   - 短期可行，但长期让新开发者困惑"为什么所有 gRPC 类型都在但什么都不做"
   - 见 [P2-1](#p2-1-grpc-伪删除代码路径仍存在但不执行) 修复建议

### 5.3 三层资源分离的执行情况

| 层级 | 设计文档要求 | 实际实现 | 状态 |
|------|------------|---------|------|
| L1 全量列表 | HTTP `/api/global/{kind}` | [`core/acowork-gateway/src/http/global.rs`](../../../../core/acowork-gateway/src/http/global.rs)（已于 gRPC 清理提交中删除；评审当时仍存在且完整 ✅） | ✅ 完整 |
| L2 已就绪状态 | MQTT Retained | 实现但 retain=false | ⚠️ 见 [P0-1](#p0-1-retained-消息未生效退化为-5s-轮询) |
| L3 per-agent 选择 | 本地文件 | `RuntimeResourceCache` 已实现 | ✅ |

---

## 六、准确性验证（与设计文档对照）

| 检查项 | 设计文档要求 | 实际实现 | 状态 |
|--------|------------|--------|------|
| Broker 端口 | 19875 | [`defaults.rs`](../../../../core/acowork-core/src/defaults.rs) `MQTT_PORT = 19875` | ✅ |
| Broker 最大连接数 | 100 | broker.rs config | ✅ |
| Broker 最大包大小 | 10 MB | broker.rs config | ✅ |
| QoS 0 for 流式事件 | §3.4 | MqttChunkPublisher 用 QoS 0 | ✅（但 dead code） |
| QoS 1 for 状态/控制 | §3.4 | status/control 用 QoS 1 | ✅ |
| Client ID Runtime | `agent:{id}` | `client.rs#L100` | ✅ |
| Client ID Gateway | `gateway:publisher` | `client.rs` 构造 | ✅ |
| Client ID Desktop | `user:{uid}:desktop:{pid}` | `mqtt_client.rs` 构造 | ✅ |
| Keep Alive | 30s | `client.rs#L108` | ✅ |
| Clean Session | true | `client.rs#L109` | ✅ |
| LWT topic/payload | `agents/{id}/status` = "offline" Retained | `client.rs#L112` | ✅ |
| Runtime 启动序列 | CONNECT→PUBLISH status→PUBLISH meta→PUBLISH config→SUBSCRIBE global/#→SUBSCRIBE control/# | `client.rs` 中按序 | ✅ |
| Topic 树 | `acowork/agents/{id}/...` | 实现一致 | ✅ |
| Protobuf 编码全局资源 | DataEnvelope | 实现 | ✅ |
| Protobuf 编码控制命令 | DataEnvelope + ControlCommand | Runtime 端 ✅ / Desktop 端 ❌ | ⚠️ |
| Protobuf 编码 Session 事件 | DataEnvelope + SessionMessage | ❌（JSON） | ❌ P0-2 |
| Retained 消息 | global/* 和 agents/{id}/status,meta,config | **全部 retain=false** | ❌ P0-1 |

---

## 七、关键问题清单

| 优先级 | 问题 | 修复方向 | 预估工作量 |
|--------|------|---------|-----------|
| **P0-1** | Retained 消息未生效 | 升级/替换 broker，恢复 `retain=true` | 中（验证 + 切换） |
| **P0-2** | Session 流式事件未接线 | `MqttChunkPublisher` 改 Protobuf + 接线到 session loop | 大（涉及 session loop 重构） |
| **P0-3** | MQTT-only 控制指令被丢弃 | `gateway_loop.rs` 使用 `spawn_control_handler` | 小（< 30 行） |
| **P1-1** | Desktop 控制命令格式不匹配 | `publish_control_json` → Protobuf 编码 | 中 |
| **P1-2** | ReasoningEffort/CompactContext 缺失 | `ControlAction` 新增 2 个变体 + 映射 | 小 |
| **P1-3** | http_port topic 与 Retained | 加入 `topics` 常量 + `retain=true` | 小 |
| **P1-4** | Desktop 全量订阅所有 session | 按需订阅当前 session | 中（前端配合） |
| **P1-5** | Runtime HTTP 文件/Grafeo 集成 | 改造 `get_file`、调用 Grafeo API | 中 |
| **P2-1** | gRPC 伪删除 | 移除 spawn + 相关 state/channel | 中 |
| **P2-2** | WebSocket/BridgeEvent 残留 | 标注 deprecated 或彻底移除 | 中 |
| **P2-3** | Router/Dispatch scaffolding | 完善或删除 | 中 |
| **P2-4** | AgentRegistry 未使用 | 与 running_agents 统一 | 中 |
| **P2-5** | 代码细节（重复导入、连接池） | 小修 | 小 |

---

## 八、修复顺序建议

**优先级链**: P0-3 → P0-1 → P0-2

### 第 1 步：P0-3（30 分钟，最小依赖）

将 `gateway_loop.rs#L43-L62` 替换为 `spawn_control_handler()` 调用。这能在不动其他模块的情况下让 MQTT-only 控制指令基本可用。

### 第 2 步：P0-1（1-2 天，基础设施）

升级/替换 broker，恢复 `retain=true`。这是 Release Blocker——其他 Retained 相关测试（Runtime 启动快照恢复）依赖它。

### 第 3 步：P0-2（3-5 天，最大工作量）

将 `MqttChunkPublisher` 改 Protobuf + 接线到 session loop + 移除旧的 WebSocket handler（结合 P2-2）。这一步是 MQTT 重构对外承诺的核心能力，必须先做完才能发布。

### 第 4 步：P1 系列（与 P0-2 并行）

P1-1（Desktop 控制格式）、P1-2（补全 control 命令）、P1-3（http_port topic）都是 P0-2 的配套工作，建议在同一 PR 中完成。

### 第 5 步：P2 清理（独立 PR）

P2 系列是技术债清理，建议独立 PR：

- P2-5（代码细节）→ 5 分钟
- P2-2（WebSocket 残留）→ 1 小时
- P2-1（gRPC 伪删除）→ 半天
- P2-3（router/dispatch 完善或删除）→ 半天
- P2-4（AgentRegistry 统一）→ 1 天

---

## 九、最终结论

| 评估维度 | 评分 | 说明 |
|---------|------|------|
| **架构方向** | ⭐⭐⭐⭐⭐ | MQTT 替代 gRPC+WebSocket 是正确的架构演进 |
| **协议设计** | ⭐⭐⭐⭐⭐ | 独立 proto 命名空间、清晰 Topic 树 |
| **模块边界** | ⭐⭐⭐⭐ | Gateway 主模块过重，inline callback 应提取 |
| **实现完整性** | ⭐⭐ | **3 个 P0 未解决，实现完成度约 60%** |
| **实现准确性** | ⭐⭐ | Retained 不生效、Protobuf 仅部分使用、Desktop/Runtime 格式不匹配 |
| **测试覆盖** | ⭐⭐⭐ | `client.rs` 模块测试存在，但缺少 end-to-end MQTT 集成测试 |
| **文档同步** | ⭐⭐⭐⭐ | 设计文档详细，但代码偏离未及时更新到 ADR/协议文档 |

**整体判断**:

**🔴 不能合并到主干**——P0-1（Retained 不生效）使 Gateway 资源发布完全退化为轮询，违背了 MQTT 重构的核心动机；P0-2（Session 流式事件未切换）使 Desktop 用户仍依赖旧通道；P0-3（控制指令被丢弃）使 MQTT-only 模式不可用。

**建议**:

1. 立即修复 P0-3（最小工作量，立竿见影）
2. 重新评估 broker 选型（如果 rumqttd 0.14 不可修复，需要 PoC 替换方案）后再解 P0-1
3. P0-2 需要独立 PR 完整走完 session loop 重构，并加上 end-to-end 测试

修复后 MQTT 重构可释放**全部架构优势**——统一的发布订阅模型、Broker 语义级生命周期管理、协议/实现完全解耦。
