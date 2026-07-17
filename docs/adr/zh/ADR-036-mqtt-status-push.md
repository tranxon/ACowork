# ADR-036：MQTT 连接状态由后端主动推送，Frontend 仅消费

**状态**：草案
**日期**：2026-07-16
**决策者**：大鱼
**前置**：
- ADR-033（MQTT 替换 gRPC + WebSocket）
- ADR-034（MQTT / HTTP 职责边界 — 事件面 `messages/*` 主题应带数据）
- ADR-035（流式传输重构 — MQTT 数据直推 + 前端 per-session 行缓冲）

**修订**：
- 修正 ADR-033 中"Frontend 无需感知 MQTT 连接状态，因为 gateway 守护进程保证"的不准确论断 → 真实情况是 MQTT 连接由每个 Agent Runtime 进程独立维护，不在 Gateway 守护进程管辖范围内，Frontend 必须可见其状态。

---

## 决策摘要

Desktop 重启后 reconnect agent 时，session 数据能正常显示（HTTP 通路），但输入框一直停留在"正在连接 agent…"。HTTP 通、MQTT 断，UI 不感知。

根因（**架构层面**）是**状态来源错位**：

- `chatStore.ts` 里的 `mqttConnected` 是个**一次性快照**——只在 `initMqttListener` 首次成功时被设为 `true`，之后即不被任何代码改写。
- `mqtt_client.rs::connect` 用 `rumqttc::AsyncClient`，eventloop 只处理 `Incoming::Publish`，**不监听 `Incoming::ConnAck` / `Incoming::Disconnect`**，不向 Frontend 推送任何连接状态事件。
- `chat_mqtt.rs::connect_mqtt` 成功 connect 后只把 `Arc` 塞进 state，**不发射 disconnect 事件给 Frontend**。
- `chatStore.ts::initMqttListener` 只 `listen("agent-event")` 设 `mqttConnected=true`，**没有任何 disconnect 监听**。
- `AppLayout.tsx:367` 注释错误地写 `"ADR-033: MQTT connection is managed by Rust backend — no reconnect"`，把"暂未实现"伪装成"无需实现"，导致 `mqttConnected=false` 路径彻底没人接。

死字段 `reconnectAttempts` / `reconnectTimer`（`chatStore.ts:301,303`）证明历史上有人开过这条路但没接上。

**四条核心原则**：

1. **连接状态的 source-of-truth 是 Rust 端的 `rumqttc` eventloop**。Frontend 只**消费** `mqttConnected`，不**赋值**。
2. **Rust 端在 eventloop 里观测 `Incoming::ConnAck` / `Incoming::Disconnect`**，通过 `on_status` callback 把状态变化推送出去。
3. **Frontend 通过专门的 `mqtt-status` Tauri event 接收状态推送**，订阅一次、终身有效；不要复用 `agent-event` 通道。
4. **Frontend UI 必须对 MQTT 断连可见**——inputDisabled、错误提示、状态栏均需消费 `mqttConnected`，不允许出现"HTTP 通 MQTT 断但 UI 看似正常"的失真态。

---

## 背景与动机

### 1. bug 现象

用户场景：Desktop 重启 → reconnect agent → 选 session → HTTP 把消息列表加载出来正常显示 → 但输入框一直显示"正在连接 agent…"，无法输入。

诊断：HTTP 走 Gateway 反代拉历史消息，与 MQTT 状态无关；MQTT 实际断开（agent runtime 进程被回收或 socket 异常），但 UI 完全不感知。

### 2. 架构错位（详见 §决策摘要）

不是"缺重试"——是"状态来源错位 + 错误注释掩盖问题"。三个独立证据互相印证：

- `chatStore.ts` 有 `reconnectAttempts` / `reconnectTimer` 死字段 → 历史上有人尝试过、失败了、被遗忘。
- `AppLayout.tsx:367` 注释 `"ADR-033: ... no reconnect"` → 把"未实现"伪装成"无需实现"，导致后续读代码者（包括我）误以为这是 ADR 的明确决策。
- `rumqttc` 文档明确支持 `Incoming::ConnAck` / `Incoming::Disconnect` → 这是标准协议事件，不是隐藏 API。

### 3. ADR-033 边界澄清

ADR-033 中"GATEWAY 是 keep alive 进程，MQTT 连接由其守护"是不准确的——MQTT 连接实际由**每个 Agent Runtime 子进程**独立维护，gateway 只做反代。**MQTT 连接生命周期归属 Runtime 进程，不是 Gateway 守护进程**。本 ADR 修正此边界，并明确连接状态须主动推送至 Frontend。

---

## 设计

### 1. Rust 端：eventloop → status callback（纯异步）

```rust
// mqtt_client.rs
pub enum MqttStatus {
    Connected,
    Disconnected { reason: String },
}

pub async fn connect(
    broker: &str,
    port: u16,
    client_id: &str,
    on_publish: impl Fn(MqttMessage) + Send + Sync + 'static,
    on_status:  impl Fn(MqttStatus)  + Send + Sync + 'static,
) -> Result<Self, String> {
    let (client, mut eventloop) = AsyncClient::new(options, 100);

    // connect() 立即返回，状态变化通过 on_status 回调异步推送。
    // 不阻塞、不 wait、不引入“正在连接”中间态。
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    on_status(MqttStatus::Connected);
                }
                Ok(Event::Incoming(Incoming::Disconnect)) => {
                    on_status(MqttStatus::Disconnected { reason: "broker sent DISCONNECT".into() });
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    on_publish(MqttMessage { topic: p.topic.clone(), payload: p.payload.to_vec() });
                }
                Ok(_) => continue,
                Err(e) => {
                    on_status(MqttStatus::Disconnected { reason: format!("eventloop error: {e}") });
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(Self { client, _eventloop_guard: Arc::new(EventLoopGuard { _task: poll_task }) })
}
```

**为什么不用 `wait_for_connack` / 同步等连接**：

MQTT 客户端的连接状态本来就是事件驱动的——`eventloop.poll()` 拿到 `Incoming::ConnAck` 就是“已连接”的回调通知，不需要包一层“同步等待 ConnAck”：

- 同步等待会**把异步事件同步化**，`connect()` 可能阻塞 10s 让 Tauri 命令在前端 pending，broker 不可达时前端长时间无响应。
- 同步等待需要额外设计超时、退避、错误状态等多个错误路径，增加复杂度。
- 同步等待会让“已连接”和“等待连接”出现中间态，“Rust 是 source-of-truth”的状态机从二态退化成三态。

**正确做法**：让 `connect()` 立即返回，所有状态变化通过 poll task → `on_status` callback 异步推送；另增一个同步查询 `get_mqtt_status` 用于 Frontend listener 注册后的初始状态拉取（详 §3）。

### 2. Tauri 层：emit `mqtt-status` event + 维护 last_mqtt_status slot

```rust
// commands/chat_mqtt.rs
let last_mqtt_status = state.last_mqtt_status.clone();

let on_status = move |status: MqttStatus| {
    let payload = match &status {
        MqttStatus::Connected => serde_json::json!({ "connected": true }),
        MqttStatus::Disconnected { reason } => serde_json::json!({
            "connected": false, "reason": reason,
        }),
    };
    if let Err(e) = app.emit("mqtt-status", payload) {
        warn!("failed to emit mqtt-status: {e}");
    }
    // Mirror into shared slot so `get_mqtt_status` returns the
    // latest value synchronously without waiting for an event.
    let slot = last_mqtt_status.clone();
    tokio::spawn(async move { *slot.write().await = Some(status); });
};
```

`AppState.last_mqtt_status: Arc<RwLock<Option<MqttStatus>>>` 是一个**三态插槽**：`None` = 还未观测到任何过渡，`Some(Connected)` / `Some(Disconnected)` = 最近一次过渡。三态区分“未知 / 已连 / 已断”，避免“前端在 listener 未注册前错过初始状态”的竞态。

### 3. Frontend：订阅事件 + 同步查询初始状态

```typescript
// stores/chatStore.ts
export async function initMqttListener(): Promise<void> {
  // (1) 订阅后续变化。
  _mqttStatusUnlisten = await listen<{ connected: boolean; reason?: string }>(
    "mqtt-status",
    (event) => {
      useChatStore.setState({
        mqttConnected: event.payload.connected,
        lastMqttError: event.payload.connected ? null : event.payload.reason ?? null,
      });
    },
  );

  // (2) listener 注册后，立即拉取当前状态——消除
  //     `connect_mqtt` 返回与 `listen` 完成之间事件丢失的窗口。
  try {
    const snapshot = await invoke<{
      known: boolean; connected: boolean; reason?: string | null;
    }>("get_mqtt_status");
    if (snapshot.known) {
      useChatStore.setState({
        mqttConnected: snapshot.connected,
        lastMqttError: snapshot.connected ? null : snapshot.reason ?? null,
      });
    }
    // snapshot.known === false → poll task 尚未观测到任何过渡，不动 store。
  } catch (err) {
    // 旧二进制不含该命令——退化为仅依赖事件流。
    console.warn("get_mqtt_status failed:", err);
  }
}
```

**Rust 与 Frontend 状态机配合的语义**：
- 启动后 · `mqttConnected=false, lastMqttError=null`：未知状态，输入框 placeholder = “正在连接 agent…”，**不**显示黄色警告条。
- 收到 `Connected` event · `mqttConnected=true`：输入框可用。
- 收到 `Disconnected{reason}` event · `mqttConnected=false, lastMqttError="…"`：输入框禁用 + 状态栏黄色警告。
- 重连后再次 `Connected` · `mqttConnected=true`：清除警告。

`AppLayout.tsx`、`SplashScreen.tsx`、`ChatPanel.tsx` 中所有引用 `mqttConnected` 的地方（inputDisabled、错误提示、状态栏）保持不变——只是 source 由“初始化时猜一次”改为“实时接收推送”。

### 4. 删除死字段

`reconnectAttempts` / `reconnectTimer` 从 `chatStore.ts` 删除。连接状态由 Rust 端管理，Frontend 不需要本地重试计时。

---

## 影响范围

| 文件 | 改动 |
|------|------|
| `apps/acowork-desktop/src-tauri/src/mqtt_client.rs` | `connect` 加 `on_status` 参数；eventloop 处理 `ConnAck`/`Disconnect`/`Err`；**不** wait for ConnAck |
| `apps/acowork-desktop/src-tauri/src/state.rs` | 增加 `last_mqtt_status: Arc<RwLock<Option<MqttStatus>>>` 三态插槽 |
| `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs` | `connect_mqtt` 注册 `mqtt-status` emit + mirror 到 `last_mqtt_status`；新增 `get_mqtt_status` 命令 |
| `apps/acowork-desktop/src-tauri/src/lib.rs` | 注册 `get_mqtt_status` 到 `invoke_handler` |
| `apps/acowork-desktop/src/stores/chatStore.ts` | 删除 `reconnectAttempts`/`reconnectTimer`；`initMqttListener` 订阅 `mqtt-status` + 同步查询 `get_mqtt_status` |
| `apps/acowork-desktop/src/components/layout/AppLayout.tsx` | 修正错误注释为 `ADR-036`；`lastMqttError` 守卫区分“未知”与“已断” |

**预计改动行数**：~120 行（Rust 70 + TS 50），无架构破坏。

---

## 不做的事

- **不在 Rust 端 wait for ConnAck**。连接状态是事件驱动的，wait 会把异步事件同步化，增加超时、退避等错误路径。
- **不在 Frontend 做重试**。重连责任完全归属 Rust 端 eventloop（`rumqttc` 内部有自动 reconnect）。Frontend 只反映状态。
- **不引入 backoff 状态机**。`rumqttc` 内置的 reconnect 已足够，重写等于重复造轮子。
- **不在 Tauri 层加心跳**。MQTT 协议本身有 keep alive，断连会被 eventloop 检测到。
- **不改 ADR-033 的传输边界**。本 ADR 只补全"连接状态可见性"这一治理缺口，不重谈 MQTT 替换 gRPC 的决策。

---

## 修订记录

- **2026-07-16 v2**：删除 `wait_for_connack` 与同步 `on_status(Connected)`——同步等待把异步事件同步化，且让状态机退化为三态。改为 `connect()` 立即返回 + 新增 `get_mqtt_status` 同步查询命令 + `last_mqtt_status: Option<MqttStatus>` 三态插槽（区分"未知 / 已连 / 已断"），让 Frontend listener 注册后立即拉取当前状态。

---

## 验证

1. **功能验证**：Desktop 启动 → 重启 agent runtime 进程（kill -9）→ 验证 `mqtt-status` 事件在 ~1s 内推送 `connected: false`；重启 runtime → 验证 `connected: true`。
2. **UI 验证**：断连时输入框禁用 + 状态栏显示"连接已断开"；重连后输入框恢复。
3. **编译验证**：`cargo clippy --all-targets -- -D warnings` + `tsc --noEmit`。

---

## 替代方案（已否决）

**方案 A（早期尝试）**：在 `chatStore.ts` 里加 `setInterval` 轮询 Rust 端查询 MQTT 状态。

否决理由：违背 `rumqttc` 设计（连接生命周期归属 Rust）、增加无效 IPC 流量、把状态来源再次放到 Frontend。

**方案 B**：让 Frontend 监听 `agent-event` 的某条特殊 topic 自报状态。

否决理由：语义污染——`agent-event` 是业务消息通道，连接元数据不该混进去。单独 `mqtt-status` 通道更干净。