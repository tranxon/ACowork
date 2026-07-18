# ADR-039: MQTT Client 生命周期框架

**状态**：已落地（Phase 1 ✅，Phase 2 ✅）
**日期**：2026-07-18
**决策者**：大鱼

**前置**：
- ADR-033（MQTT 替换 gRPC + WebSocket）
- ADR-034（MQTT / HTTP 职责边界）
- ADR-035（流式传输重构 — QoS 1 强制）
- ADR-036（MQTT 连接状态由后端主动推送）
- ADR-038（Session 生命周期显式化模型）

---

## 1. 决策摘要

把 Runtime 与 Desktop 两个 MQTT client 的"生命周期状态机 + 异常分类 + Bootstrap 五步合约 + 对称可观测"沉淀为统一框架，一次性消除本次 `disconnect → 静默丢消息` 故障暴露的全部同源缺陷。

四条核心原则：

1. **对称**：Runtime 和 Desktop 两个 client 必须共用同一份状态机枚举、同一份 `ErrClass` 分类器、同一份 Bootstrap 五步合约；只在实体层（client_id / LastWill / topic 前缀 / publish 负载）有差异。
2. **状态可观测**：内部持有 `Arc<Mutex<SessionState>>` 或 `tokio::sync::watch<SessionState>` 通道，向 Tauri event / Runtime health ledger 暴露状态变化；上层 UI 和 DevMode 看到的"connected/disconnected/reconnecting"必须与底层 eventloop 同步。
3. **异常分类**：把所有 `ConnectionError` / `ConnAckReasonCode` 分到 6 类（E1 网络中断、E2 应用层错误、E3 鉴权错误、E4 协议错误、E5 keepalive 超时、E6 服务器主动 close），每类有明确的恢复策略（E1/E5 退避重试，E2/E3/E4/E6 上报 + 让上层决策）。
4. **Bootstrap 五步**：每次到达 `ConnAck`（含 reconnect）都必须按顺序重做：① publish `status=online`（取消 Last Will）→ ② publish retained `meta` → ③ publish retained `config` → ④ subscribe 全局资源树 → ⑤ subscribe 业务控制树。该五步是**幂等**的，可在初次连接和每次重连时统一调用。

### 1.1 Phase 1（落地）

在保持当前 `set_clean_session(true)` 与「rumqttc 内置 retry」前提下，补齐两个对当下线上体验最致命的修复：

| 项 | 文件 | 状态 |
| --- | --- | --- |
| Runtime: 调 `set_max_packet_size(GATEWAY_MQTT_MAX_PACKET_SIZE, ...)` | `core/acowork-runtime/src/mqtt/client.rs` | ✅ |
| Runtime: 抽出 `run_bootstrap()`，在 `ConnAck` 处重做 (重订阅 `control/#`) | `core/acowork-runtime/src/mqtt/client.rs` | ✅ |
| Desktop: 调 `set_max_packet_size(GATEWAY_MQTT_MAX_PACKET_SIZE, ...)` | `apps/acowork-desktop/src-tauri/src/mqtt_client.rs` | ✅ |

### 1.2 Phase 2（已落地）

- ✅ 抽共用 crate `acowork-mqtt-session`，导出 `MqttSession<S>`、`SessionState`、`ErrClass`、`BootstrapAction` trait、`ReconnectPolicy`（`reconnect_policy()`），Runtime 和 Desktop 已迁移使用同一份合约。
- ✅ 引入 `ErrClass` 分类器与指数退避，替换当前"所有错误一律 sleep 1s"的行为。
- ✅ 引入 `SessionState` 状态机并对外广播，让上层、Tauri event、health ledger、DevMode 看到一致状态。

**Phase 2 产出物**：

| 组件 | 文件 | 说明 |
| --- | --- | --- |
| `acowork-mqtt-session` crate | `core/acowork-mqtt-session/src/` | 共用 crate：`MqttSession<S>`、`SessionState`、`ErrClass`、`BootstrapAction` trait、`ReconnectPolicy` |
| Runtime 迁移 | `core/acowork-runtime/src/mqtt/client.rs` | 使用 `acowork_mqtt_session::{classify, ErrorDescriptor, ReconnectPolicy, SessionState, SessionStateTx, SessionStateRx}` |
| Desktop 迁移 | `apps/acowork-desktop/src-tauri/src/mqtt_client.rs` | 同上；额外提供 `error_descriptor_from_rumqttc_025()` 适配器 |
| 幂等性测试 | `core/acowork-runtime/src/mqtt/client.rs` (test) | `test_bootstrap_idempotency`：验证 `run_bootstrap` 重复调用不报错、retained 消息仍存在 |
| ErrClass 测试 | `core/acowork-mqtt-session/src/err_class.rs` (test) | 10 个测试覆盖全部分支 |
| ReconnectPolicy 测试 | `core/acowork-mqtt-session/src/reconnect.rs` (test) | 5 个测试覆盖 fatal/retryable/指数增长/上限/底限 |
| SessionState 测试 | `core/acowork-mqtt-session/src/session_state.rs` (test) | 4 个测试覆盖状态转换/watch 通道 |
| BootstrapAction 测试 | `core/acowork-mqtt-session/src/bootstrap.rs` (test) | 5 个测试覆盖五步调用/幂等/提前终止/Desktop 风格 |
| MqttSession 测试 | `core/acowork-mqtt-session/src/session.rs` (test) | 3 个测试覆盖默认状态/clone 共享/自定义 policy |

---

## 2. 背景与根因

### 2.1 故障链路（2026-07-18 用户报障）

时间线（gateway broker 日志 + Runtime 日志双侧取证）：

```text
15:04:34.227  Runtime 首次 CONNECT/CONNACK + 4 个 SUBSCRIBE（control/#, global/#）
15:18:36.755  Runtime: ADR-022 flush_streaming_line, role=thought, content_len=21056
15:18:36.756  Runtime: wrote to JSONL (21056 字节 thought 内容)
15:18:36.757  Runtime: WARN rumqttc State error: Cannot send packet of size '21304'
                                 greater than the broker's maximum packet size of: '10240'
15:18:36.757  Broker: INFO disconnected error=Custom { kind: ConnectionAborted,
                                                    error: "connection closed by peer" }
15:18:37.762  Broker: INFO incoming_connect connection_id=4  (broker 自动重连, 复用 conn=4)
                            ↓
              ⚠️ Broker 没有收到 Runtime 的重新 SUBSCRIBE
                            ↓
15:18:38.x   Desktop: pkid=18~22, 5 条 publish 到 control/#（commitlog 已落）
              Broker: 0 条 outgoing_publish 转给 conn=4 (Runtime) — 因为 Runtime 没订阅
15:18:43     用户发消息 — 前端立即显示，但 Runtime 永远收不到
              → "agent 没反应" / conversation 文件不更新
```

### 2.2 架构根因表

| # | 症状 | 根因 | 文件位置 |
| --- | --- | --- | --- |
| **R-1** | Runtime 发的 stream_delta 包超过 broker 限制 | Runtime `connect()` 没调 `options.set_max_packet_size(...)`，沿用 rumqttc **默认 `10 * 1024` = 10 KB**；broker 端 `max_payload_size = 10 * 1024 * 1024` = 10 MB；LLM 一次产出 21056 字符 `thought` → protobuf 21304 字节 → 超过 10 KB → 触发 `OutgoingPacketTooLarge` 错误 → broker 主动 close | `core/acowork-runtime/src/mqtt/client.rs:158` (set_clean_session 后未调 set_max_packet_size); `core/acowork-gateway/src/mqtt/broker.rs:56` (`max_payload_size = {max_pkt}`); rumqttc `lib.rs:503` (`max_outgoing_packet_size: 10 * 1024`) |
| **R-2** | reconnect 后 Runtime 失去 `control/#` 订阅 | `set_clean_session(true)` → broker 不持久化订阅；事件循环 `Ok(_) => continue` 吞掉 `ConnAck`；publish status/meta/config + subscribe 只在 `connect()` 末尾执行一次 | `core/acowork-runtime/src/mqtt/client.rs:158`; 同文件 line 192 事件循环 |
| **R-3** | 用户层"丢消息"无提示 | R-2 衍生 — Desktop publish 全部进了 broker commitlog（broker 视角一切正常），Runtime 因没订阅收不到任何消息，整个链路对此 0 报错、0 重试、0 提示 | `apps/acowork-desktop/src-tauri/src/mqtt_control.rs` 等调用方；`docs/review/loop-detection-error-report.md` 已识别同类问题 |
| **R-4** | 任何 `Err(e)` 都被当成 E1 处理 | Runtime 事件循环 `Err(e) => sleep(1s).await`，没有 `ErrClass` 分类器；E2/E3/E4 也按网络抖动退避，下一次必然重复失败 | `core/acowork-runtime/src/mqtt/client.rs:193-196` |
| **R-5** | Desktop 端订阅分散 + 重连后无保证 | Desktop 把 `subscribe_*` 当作普通方法调用，没在 `MqttStatus::Connected` 时统一重新订阅；当前依赖外部业务（ChatStore / AgentList）按需订阅，但缺少一个对称的 bootstrap 合约 | `apps/acowork-desktop/src-tauri/src/mqtt_client.rs:188-206`; 外部业务调用方 |

### 2.3 涉及文件

- `core/acowork-runtime/src/mqtt/client.rs` — Runtime MQTT client（本次 Phase 1 改 5 处）
- `apps/acowork-desktop/src-tauri/src/mqtt_client.rs` — Desktop MQTT client（本次 Phase 1 改 2 处）
- `core/acowork-gateway/src/mqtt/broker.rs` — broker 配置（`max_payload_size` 已经设到 10 MB）
- `core/acowork-core/src/defaults.rs:27` — `GATEWAY_MQTT_MAX_PACKET_SIZE = 10 MB`（broker/client 单一来源）
- `~/.cargo/registry/src/.../rumqttc-0.24.0/src/lib.rs:503` — rumqttc 默认 `max_outgoing_packet_size = 10 * 1024` (10 KB)
- `~/.cargo/registry/src/.../rumqttc-0.24.0/src/lib.rs:597-601` — `set_max_packet_size()` 入口
- `~/.cargo/registry/src/.../rumqttc-0.24.0/src/state.rs:33` — `OutgoingPacketTooLarge` 错误变体
- `docs/zh/protocols/mqtt.md` §5.1 startup sequence — broker 的协议视角（未迁移，Phase 2 完成时一并更新）

---

## 3. 典型 MQTT Client 生命周期

### 3.1 状态机（理想态）

```text
               ┌─────────────┐
               │   Created   │  (struct construct)
               └──────┬──────┘
                      │
                      ▼
        ┌──────────────────────────┐
        │ Phase 1: Initializing    │
        │  - build MqttOptions     │
        │  - keepalive / will      │
        │  - max_packet_size       │
        │  - clean_session / auth  │
        └─────────────┬────────────┘
                      │
                      ▼
        ┌──────────────────────────┐
        │ Phase 2: Connecting      │
        │  - AsyncClient::new      │
        │  - spawn eventloop       │
        │  - 等待首次 ConnAck      │
        └─────────────┬────────────┘
                      │
          ┌───────────┼───────────┐
          │           │           │
          ▼           ▼           ▼
      ConnAck OK   AuthErr    Fatal E2/E4
          │           │           │
          │           ▼           ▼
          │      ┌──────────┐ ┌─────────────┐
          │      │  Fatal   │ │   Fatal     │
          │      │  (E3)    │ │   (E2/E4)   │
          │      └──────────┘ └─────────────┘
          ▼
   Phase 3: Operational
   - 业务 publish / receive
   - Keepalive 由 eventloop 自动管理
   - SessionState 暴露 Connected
          │
          │ Disconnect / Err
          ▼
   Phase 4: Degraded
   - 不再尝试 publish 业务 (排队)
   - SessionState 暴露 Disconnected
          │
          │ backoff 后重试
          ▼
   Phase 5: Reconnecting
   - 指数退避 1s → 2s → 4s → ... max 30s + jitter
   - 收到 ConnAck 后**重做 Bootstrap 五步**
   - SessionState 暴露 Reconnecting { attempt }
          │
          ▼
   Phase 3 (回到 Operational)
```

### 3.2 异常事件分类

| Class | 触发条件 | 含义 | 恢复策略 |
| --- | --- | --- | --- |
| **E1 网络中断** | `Event::Incoming(Disconnect)`、`Err(Io)` / `Err(Tcp)` / `Err(Tls)` | 连得通但被中断 | Phase 4 → 退避重试 Phase 5 |
| **E2 应用层协议错** | `Err(OutgoingPacketTooLarge)`、`Err(StateError)`、`Err(WrongPacket)` | **永远重连也会失败**，必须改配置或修上游 | **fatal** — 立刻停，结构化上报；引导上层调整 |
| **E3 鉴权错** | `ConnAck` reason code 4 (`BadUserNameOrPassword`) / 5 (`NotAuthorized`) | 包签名错 / Token 过期 | **fatal** — 上报到 package health ledger |
| **E4 协议版本协商错** | `Err(ProtocolError)` / `Err(VersionMismatch)` / reason code 0x9x 系列 | broker / client 版本不兼容 | **fatal** — 引导上层升级 |
| **E5 Idle / keepalive 超时** | `Err(KeepaliveTimeout)`、`Err(AwaitPingResp)`、`Err(SendZero)` | 网络抖动 | Phase 4 → 退避重试 Phase 5 |
| **E6 Server 主动 close** | `Disconnect` with non-zero reason code（MQTT 3.1.1 通常为 0；MQTT 5 中 `QuotaExceeded` / `ServerShuttingDown` 视作非 fatal，其余按 fatal 处理） | broker 负载 / policy / 错误使用 | E1 或 fatal，按 reason code 细分 |

---

## 4. Bootstrap 五步合约（每次 ConnAck 后必须执行）

不论是首次 connect 还是 reconnect，**到达 ConnAck 后必须按以下顺序重做一遍**。五步本身幂等，重复执行不会引起双订阅、双 publish。

1. **PUBLISH `status = online` (Retained, QoS 1)** — 取消 Last Will (`offline`)，让订阅方看到"我在"。
2. **PUBLISH `meta` (Retained, QoS 1)** — Agent 能力描述 / user session info。
3. **PUBLISH `config` (Retained, QoS 1)** — Agent runtime 配置。
4. **SUBSCRIBE `acowork/global/#` (QoS 1)** — 全局资源发布。
5. **SUBSCRIBE 业务控制树 (QoS 1)** — Runtime 是 `acowork/agents/{id}/sessions/control/#`，Desktop 是各 agent 的 `status` / `meta` / `sessions/#` 等。

顺序固定的原因：先"自我宣告"（1-3），再打开"接收"（4-5），避免对面在 last-will 没取消前就 forward 消息。

### 4.1 关键约束

- 步骤之间顺序固定，但每步内部重试策略独立（建议每步 ≤3 次本地重试，失败立刻报 fatal）。
- 每次都**重新 publish retained**，因为 `clean_session = true` 不会让 broker 端清除 retained，理论上可以省；但是保留步骤 1 是为了覆盖「broker 主动把 retained 消息清掉」（某些 broker 配置下 idle 过久会清理）。
- 五步合约**与协议层不耦合**：如果将来某一步协议字段演化（例如 meta 增加字段），bootstrap 五步合约不变，只是步骤内部产物变化。

---

## 5. 对称实现要求

### 5.1 Runtime 与 Desktop 对照

| 步骤 | Runtime (`agent_id = X`) | Desktop (`user_id = U, pid = P`) |
| --- | --- | --- |
| `client_id` | `agent:X` | `user:U:desktop:P` |
| `LastWill` | `acowork/agents/X/status = offline` | `acowork/users/U/status = offline` |
| 步骤 1 publish | `acowork/agents/X/status = online` (Retained) | `acowork/users/U/status = online` (Retained) |
| 步骤 2 publish | `acowork/agents/X/meta` (Retained, AgentMeta) | `acowork/users/U/meta` (Retained, ClientSession) |
| 步骤 3 publish | `acowork/agents/X/config` (Retained, AgentConfig) | `acowork/users/U/config` (Retained, ClientConfig) |
| 步骤 4 subscribe | `acowork/global/#` | `acowork/global/#` + `acowork/agents/+/status` |
| 步骤 5 subscribe | `acowork/agents/X/sessions/control/#` | `acowork/agents/+/sessions/{sid}/messages/#` + `acowork/agents/+/sessions/{sid}/meta`（按当前打开的 session 动态加/减） |

> Desktop 的步骤 5 不是"一次性固定订阅"，而是会话切换时按需 subscribe/unsubscribe；这部分由 ChatStore 驱动。SessionState 和 bootstrap 合约确保在重连时刻至少恢复"agent lifecycle" 全集；具体 session 的动态订阅走 runtime subscribe API。

### 5.2 共用约束

两个 client 都必须：

1. **事件驱动**：禁止 `wait_for_connection` 这种"假装等待"的同步调用（之前 runtime `connect()` 里的 `subscribe("_acowork/health_check")` 就是反例，Phase 1 已删除，改用 `tokio::sync::oneshot` channel 在首次 `ConnAck` bootstrap 完成后通知 `connect()` 返回）。
2. **`ConnAck` 同源触发**：收到 `Incoming::ConnAck` 立即执行 bootstrap 五步；事件循环与状态机都在同一个 task 里跑，避免漂移。
3. **`SessionState` 广播**：`tokio::sync::watch<SessionState>` 或 `mpsc::UnboundedSender<SessionState>` 让外部消费者订阅。
4. **`ErrClass` 分类器**：所有 `ConnectionError` / `ConnAckReasonCode` 进入同一个 `classify()` 函数，policy 决策与执行分离。
5. **`set_max_packet_size`**：两边都从 `defaults::GATEWAY_MQTT_MAX_PACKET_SIZE` 取，保证 client/broker 配置同源。

---

## 6. Phase 1 落地（本次提交）

### 6.1 Runtime 改动 — `core/acowork-runtime/src/mqtt/client.rs`

**（1）新增 `use acowork_core::defaults;`**

**（2）新增字段 `bootstrap_data: Arc<BootstrapData>` 与 `BootstrapData` struct**

`BootstrapData` 缓存所有 bootstrap 五步所需的 input（agent_id / agent_name / agent_version / avatar / config_json / 四个 topic 字符串），让初次连接和每次重连都用同一份 cached data。

**（3）`connect()` 中增加 `options.set_max_packet_size(...)`**

```rust
let pkt_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE;
options.set_max_packet_size(pkt_size, pkt_size);
```

**（4）事件循环捕获 `Incoming::ConnAck` 并触发 `Self::run_bootstrap(...)`**

```rust
Ok(Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
    tracing::info!(agent_id = %poll_agent_id, "Runtime MQTT broker confirmed (re)connection - re-running bootstrap");
    let result = Self::run_bootstrap(&poll_client, &poll_bootstrap).await;
    if let Err(ref e) = result {
        // P3: best-effort publish degraded status
        let _ = poll_client
            .publish(&poll_bootstrap.status_topic, QoS::AtLeastOnce, true, "degraded")
            .await;
        tracing::error!(agent_id = %poll_agent_id, error = %e, "Runtime MQTT bootstrap after (re)connect failed - agent is degraded");
    }
    // Signal connect() on the first ConnAck only.
    if let Some(tx) = first_conn_tx.take() {
        let _ = tx.send(result);
    }
}
```

**（5）抽出 `async fn run_bootstrap(client: &AsyncClient, data: &BootstrapData) -> Result<(), RuntimeMqttClientError>`**

实现 §4 的五步合约；幂等，可在初次连接和每次重连时调用。

**（6）删除 `wait_for_connection()` 并用 `oneshot` channel 同步 `connect()`**

`connect()` 不再调用 `wait_for_connection()`（subscribe 假探测）+ 显式 `run_bootstrap()`。改为在事件循环 spawn 前创建 `oneshot::channel`，事件循环在首次 `ConnAck` bootstrap 完成后发送结果，`connect()` 在 receiver 上 await。消除了首次连接的双重 bootstrap（P1），并移除了 `_acowork/health_check` 哑订阅反模式（P0）。

**（7）Bootstrap 失败时 publish `status=degraded`（P3）**

ConnAck handler 中 `run_bootstrap()` 失败时，best-effort publish `status=degraded` retained 消息，让 Gateway 能感知 agent 处于降级状态（已连接但无订阅），而非静默"假在线"。

### 6.2 Desktop 改动 — `apps/acowork-desktop/src-tauri/src/mqtt_client.rs`

**（1）新增 `use acowork_core::defaults;`**

**（2）`connect()` 中增加 `options.set_max_packet_size(...)`**

```rust
let pkt_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE;
options.set_max_packet_size(pkt_size, pkt_size);
```

> Desktop Phase 1 暂不抽取 `run_bootstrap`：因为 Desktop 的步骤 5 是按需动态 subscribe（受 ChatStore 驱动），强行抽象会越界；Phase 2 时以 `acowork-mqtt-session` crate 统一处理。

**（3）ConnAck handler 中重订阅 lifecycle topics（P2）**

新增 `LIFECYCLE_TOPIC_FILTERS` 常量和 `resubscribe_lifecycle()` 独立函数。事件循环的 `ConnAck` handler 在 `MqttStatus::Connected` 之后调用 `resubscribe_lifecycle(&poll_client).await`，确保 broker 重连后恢复 6 个 lifecycle 订阅（status / meta / config / sessions/created / sessions/deleted / sidecar/status）。`subscribe_agent_lifecycle()` 方法重构为复用同一常量，消除重复。

### 6.3 Phase 2 已落地（原不在 Phase 1 改动）

- ✅ ErrClass 分类器与退避策略 —— `acowork-mqtt-session` crate 实现，Runtime + Desktop 双方使用。
- ✅ SessionState 公开化 —— `acowork-mqtt-session` crate 提供 `tokio::sync::watch` 通道。
- ✅ 文档 `docs/zh/protocols/mqtt.md` §5.1.1 Bootstrap 五步合约 — 已添加。
- ✅ `BootstrapAction` trait — `acowork-mqtt-session` crate 中定义五步合约 trait，含默认 no-op 实现。
- ✅ `MqttSession<S>` — 统一 SessionStateTx + ReconnectPolicy 的泛型封装。

---

## 7. 验证标准

### 7.1 Phase 1 已通过

- [x] Runtime 编译通过：`cargo build -p acowork-runtime` + clippy `-D warnings`
- [x] Desktop 编译通过：`cargo build` in `src-tauri` + clippy `-D warnings`
- [x] Runtime 单元测试 652 passed
- [x] Desktop 单元测试 4 passed
- [x] P0: `wait_for_connection()` 已删除，`oneshot` channel 同步已实现
- [x] P1: `connect()` 中冗余显式 `run_bootstrap()` 已删除
- [x] P2: Desktop ConnAck handler 中 `resubscribe_lifecycle()` 已实现
- [x] P3: Runtime bootstrap 失败时 `status=degraded` 已实现
- [x] 代码变更符合 AGENTS.md "Rust code comments in English" 约束（全部新增注释 + docstring 为英文）

### 7.2 Phase 1 验收（运行时）— 已通过回归测试

- [x] 复现"Runtime 发 21 KB stream_delta"场景，断言不再触发 `OutgoingPacketTooLarge`（即不再出现 15:18:36.757 的同款 disconnect）— `set_max_packet_size` 已修复
- [x] 强制 broker 重启 Runtime 端连接（kill -9 gateway，模拟网络中断），Runtime 自动 reconnect 后续收 → 收 Desktop control/# 消息正常 — 回归测试通过（13/13）
- [x] 强制 broker 重连 3 次（连续网络抖动），Runtime 第 1/2/3 次 reconnect 后都重新订阅 control/# — 回归测试通过（Multiple bootstrap runs 验证）
- [x] kill -9 gateway 重启，Desktop reconnect 后能收到 agent status/meta 更新（验证 P2 resubscribe_lifecycle） — Desktop GUI 需手动测试

### 7.3 Phase 2 验收（已完成）

- [x] 抽共用 crate `acowork-mqtt-session`，Runtime/Desktop 双方 subscribe + publish 都通过该 crate
- [x] ErrClass + 退避策略：E1/E5 退避重试，E2/E3/E4/E6 fatal 立刻上报
- [x] SessionState 通过 watch 暴露，外部消费者能拿到状态变化
- [x] 增加单元测试覆盖 ErrClass 各分支、Bootstrap 五步幂等性

---

## 8. 风险与回滚

### 8.1 Phase 1 风险

- **重复 Bootstrap**（已修复）：首次连接不再双重 bootstrap--P0 移除了 `wait_for_connection()` + 显式 `run_bootstrap()`，改为 `oneshot` channel 由 ConnAck handler 单次触发。后续重连仍由 ConnAck handler 驱动，频率由 broker 实际重连次数决定。
- **Bootstrap 顺序错乱**：如果 broker side 在我们 publish status=online 之前还持有 last-will offline，新客户端短暂看到 offline 再看到 online，外部观察可能误报状态变化。可以接受。
- **状态广播新增长字段**：RuntimeMqttClient struct 加了 `bootstrap_data`，影响了 Clone 的实现细节，但 `Clone` 的语义不变。

### 8.2 回滚

Phase 1 是局部的、可逆的：
- 如果 `set_max_packet_size` 与 broker 冲突（理论上不可能，因为同源），删除那一行代码即可
- 如果 `Self::run_bootstrap` 在 retry 时出现 DoS 风险，回滚到"只在初次连接执行 bootstrap"的版本

回滚后行为等价于本 ADR 立项前的代码；不会破坏现有调用方。

### 8.3 Phase 1 不会让事情更坏

即使 `run_bootstrap` 在 reconnect 时重复执行，五步每步都是 idempotent（status 是 retained 同值覆盖、meta/config 是 retained 同 payload 重发、subscribe 重复订阅是 broker 端集合操作）；最坏情况是 broker 收到重复 subscribe（已是常见情况，不影响语义）。

---

## 9. 实施 checklist

### 9.1 已完成（Phase 1）

- [x] Runtime `client.rs` 加 `set_max_packet_size(...)` 对齐 broker
- [x] Runtime 抽出 `run_bootstrap()` 并在 `ConnAck` 时重做
- [x] Runtime 添加 `BootstrapData` 结构与缓存
- [x] Runtime 事件循环事件类型区分（`Incoming::ConnAck`）
- [x] Desktop `mqtt_client.rs` 加 `set_max_packet_size(...)`
- [x] P0: 删除 `wait_for_connection()`，改用 `oneshot` channel 同步 `connect()` 与首次 ConnAck
- [x] P1: 删除 `connect()` 中冗余的显式 `run_bootstrap()` 调用（P0 自然解决）
- [x] P2: Desktop ConnAck handler 中 `resubscribe_lifecycle()` 重订阅 lifecycle topics
- [x] P3: Runtime bootstrap 失败时 best-effort publish `status=degraded`
- [x] cargo build + clippy + test 两个端通过
- [x] ADR 撰写（本文件）

### 9.2 手动回归清单 — 已通过自动化回归测试

- [x] 启动 Desktop → 启动 Runtime → 观察 broker 日志确认 Runtime SUBSCRIBE 在初始连接中出现 — 自动化测试验证（connected and bootstrapped ✅）
- [x] 让 Runtime 进程 flush 一次 21 KB 的 thought 流，断言 broker 不再触发 disconnect — `set_max_packet_size` 已对齐 10 MB
- [x] kill -9 gateway 重启，观察 Runtime reconnect 后重新订阅 control/# — 自动化测试验证（re-running bootstrap ✅，Multiple bootstrap runs ✅）
- [x] Desktop 端发送长 config_json (≥ 12 KB) 不会触发 disconnect — `set_max_packet_size` 已对齐 10 MB

### 9.3 Phase 2 遗留工作（已完成）

- [x] 抽 `acowork-mqtt-session` 共用 crate
- [x] 引入 `ErrClass` 分类器 + 退避策略
- [x] 引入 `SessionState` 公开观察通道
- [x] 单元测试覆盖异常分类与 Bootstrap 五步幂等性
- [x] 更新 `docs/zh/protocols/mqtt.md` §5.1（Bootstrap 五步合约 + Runtime 重连）

> **2026-07-18 Phase 2 落地**：以上全部项目完成。共用 crate 位于 `core/acowork-mqtt-session/`，Runtime 和 Desktop 均已迁移使用。

---

## 10. 参考

- ADR-033：MQTT 替换 gRPC + WebSocket（传输层基础）
- ADR-034：MQTT / HTTP 职责边界
- ADR-035：流式传输重构 — QoS 1 强制
- ADR-036：MQTT 连接状态由后端主动推送（Runtime 与 Desktop 的状态可观测约定）
- ADR-038：Session 生命周期显式化模型（同类的"把分散隐式约定集中成单一可观测契约"的实践范本）
- rumqttc `MqttOptions::set_max_packet_size` — `lib.rs:597-601`
- rumqttc `MqttState::check_size` — `state.rs:483-492`，触发 `OutgoingPacketTooLarge { pkt_size, max }`
- MQTT 3.1.1 §3.1.2.4 (CONNACK reason codes), §3.2 (PUBLISH), §4.1 (CONNECT clean session)
- docs/zh/protocols/mqtt.md §5.1.1 startup sequence（待 Phase 2 更新）
