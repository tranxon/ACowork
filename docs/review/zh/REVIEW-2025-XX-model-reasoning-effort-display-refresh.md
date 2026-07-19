# REVIEW: 前端 Model / Reasoning Effort 设置后显示刷新行为不一致

- **报告范围**: 从前端输入框点击 → MQTT publish → Runtime 订阅与生效 → MQTT 反向推送 → 前端 UI 重渲染，5 个环节的完整链路
- **审查方法**: 静态代码追踪 + 现有日志埋点分析
- **报告日期**: 2025-XX-XX
- **报告版本**: v1
- **关联**: ADR-033 §5 通信模式 / chatStore `session_meta` / conversation.rs meta_change_tx

---

## TL;DR

链路**理论上**完整，但发现 **3 个真因 + 2 个潜在脆弱点**：

| # | 问题 | 严重程度 | 偶发性 |
|---|------|----------|--------|
| P0 | frontend optimistic 与 Runtime `default_effort` 计算不一致时，meta 回推会**默默覆盖**前端的乐观值，导致"立即刷新"又"被回滚" | 高 | 是 |
| P0 | front-end chatStore `case "session_meta"` 把 `provider_id: ""` 误当作"无更新"——切到新 provider 时 Runtime 算出的空字符串会**丢失回推** | 高 | 是 |
| P1 | Runtime `ModelSwitch` / `ReasoningEffort` handler 末尾**不调用** `emit_session_state()`，仅依赖 `meta_change_tx → relay → publish_session_meta` 单链路；任意一环丢消息都会导致显示与 backend 不一致 | 中 | 是 |
| P1 | `model_confirmed` / `reasoning_effort_confirmed` 事件在 chatStore 有 handler，但**全代码库没有 producer**，是 dead code | 中 | N/A |
| P2 | meta relay 的 `chunk_tx.try_send()` 在 chunk channel（cap=64）满时**静默 drop** 通知，未触发任何 backpressure 告警 | 低 | 极端 case |

**用户报告的"有时候立即刷新，有时候不刷新"的根因 = P0 + P0 两个事件叠加**：

- "立即刷新" = optimistic-update 完成后到 Runtime 回推到达之间的窗口期
- "不刷新" = Runtime 回推的字段值被 chatStore 的 `&& data.field` 非空过滤给吞掉，导致 store 看起来"没动"；或在更糟的场景下，Runtime 回推了一个**与乐观值不同**的值（用户预期的被覆盖）

---

## 1. 链路全景（已确认步骤）

```
[1] User 点击 ModelMenu / ReasoningEffortMenu
    └─ ChatPanel.tsx:1843 / 1852
        setCurrentModel(m, p, agentId) / setReasoningEffort(e, agentId)

[2] frontend optimistic update（同步）
    └─ chatStore.ts:1281 / 1321
        set(updateSessionState(..., {model, provider, reasoningEffort: defaultEffort}))
        ↳ 写入 zustand store → React 重渲染 → UI 显示新值（**这一步总是立即生效**）

[3] 前端 → MQTT publish（异步 fire-and-forget）
    └─ invoke("mqtt_publish_control", ...)
        │
        ├─ Tauri: chat_mqtt.rs:404 build_control_command
        │   将 {model_id, provider_id, session_id} → mqtt_proto::ModelSwitch
        │
        └─ Tauri: mqtt_client.rs:465 publish_control_protobuf
            topic = "acowork/agents/{id}/sessions/control/{model_switch|reasoning_effort}"
            QoS::AtLeastOnce, retain = false

[4] Broker（acowork-gateway/rumqttd）
    ACL allow: Desktop Publish + Runtime Subscribe on this filter ✅
    (acl.rs:73, 90)

[5] Runtime 端：MQTT client eventloop 收包 → 路由 control_rx
    └─ mqtt/client.rs:274
        if topic.starts_with(control_filter_prefix) → send(control_tx, ...)
    │
    ├─ mqtt/control_handler.rs:147 parse_control_payload
    │   DataEnvelope → ControlAction::ModelSwitch / ReasoningEffort
    │
    └─ startup/gateway_loop.rs:140 control_action_to_inbound
        │
        ├─ ModelSwitch → InboundMessage::ModelSwitchAction{model_id, provider_id}
        │  ⑮ dispatch_inbound → session_manager.route_model_switch(...)
        │
        └─ ReasoningEffort → InboundMessage::ReasoningEffortAction{effort}
           ⑯ dispatch_inbound → session_manager.route_reasoning_effort(...)

[6] SessionTask 处理 inbound message
    └─ agent/session/session_task.rs:1151 ModelSwitch handler
        a) set_model / set_provider (in-memory SessionState)
        b) conv.update_model_provider(...)        ← fires MetaChangeKind::Cold
        c) build_provider_for + update_provider
        d) context_builder.set_override_model
        e) 重新计算 default_effort:
           default = caps.default_reasoning_effort ?? (supports_reasoning ? Auto : None)
        f) set_reasoning_effort(default_effort)
        g) conv.update_reasoning_effort(effort_str)  ← fires MetaChangeKind::Cold
        ❌ 末尾没有 emit_session_state() 调用

    └─ session_task.rs:1220 ReasoningEffort handler
        a) set_reasoning_effort(parsed)
        b) conv.update_reasoning_effort(Some(effort))
        ❌ 末尾没有 emit_session_state() 调用

[7] ConversationSession::update_* → meta_change_tx.notify()
    └─ conversation.rs:786 / 814
        write_meta()           // 持久化到 JSONL 文件
        send(Cold("model"))    // (update_model_provider 同时发 Cold("provider"))
        send(Cold("reasoning_effort"))

[8] spawn_meta_change_relay task
    └─ startup/subsystems.rs:395
        let meta = conv.build_session_meta_snapshot();
        SessionChunkEvent { event: SessionMetaChanged { meta, fields_changed } }
        chunk_tx.try_send(event)   ← cap=64，full 时静默 drop ❗

[9] relay_chunk_event_mqtt
    └─ subsystems.rs:364
        ChunkEvent::SessionMetaChanged { meta, .. }
        → publisher.publish_session_meta(sid, &meta).await
        topic = "acowork/agents/{id}/sessions/{sid}/meta"
        QoS::AtLeastOnce, retain = TRUE  ✅

[10] Desktop MQTT broker → 推送 payload
    └─ mqtt_client.rs 已订阅 acowork/agents/+/sessions/+/meta, QoS 1

[11] Tauri: chat_mqtt.rs:141 解码 SessionMeta → emit Tauri event
     let app_handle.emit("agent-event", {
       type: "session_meta",
       agent_id, session_id, model_id, provider_id,
       reasoning_effort, temperature, message_count,
       input_tokens, output_tokens, total_input_tokens, total_output_tokens,
       workspace_id, title, updated_at
     })

[12] frontend: chatStore.ts:2820 case "session_meta"
        if (sid) {
          if (typeof data.model_id === "string" && data.model_id)   patch.model = ...
          if (typeof data.provider_id === "string" && data.provider_id)  patch.provider = ...
          if (typeof data.message_count === "number")  patch.messageTotal = ...
          if (typeof data.reasoning_effort === "string" && data.reasoning_effort) {
            patch.reasoningEffort = ...
          }
          if (typeof data.temperature === "number" && !Number.isNaN(data.temperature)) ...
          set(updateSessionState(..., patch))
        }
    ❗ 4 个字段被 `&& data.field` 过滤空串 → 空串视为"无更新"

[13] zustand store 更新 → React 重渲染 → UI 显示
```

链路完整性：**12 / 12 步都有代码**，确认无缺链。

---

## 2. 发现的真因（按"用户能感受到的 bug"严重度排序）

### 真因 A: Runtime 切模型时 `reasoning_effort` 被重置，与前端乐观值**不一致**（P0）

**位置**: `core/acowork-runtime/src/agent/session/session_task.rs:1199-1217`

```rust
let caps = agent_loop.core.get_model_capabilities(&model);
let provider_default = caps
    .as_ref()
    .and_then(|c| c.default_reasoning_effort.clone());
let default_effort = provider_default
    .as_deref()
    .and_then(ReasoningEffort::from_str_loose)
    .or_else(|| {
        if caps.as_ref().and_then(|c| c.supports_reasoning).unwrap_or(false) {
            Some(ReasoningEffort::Auto)
        } else {
            None      // ← 关键：会到这里
        }
    });
agent_loop.session.set_reasoning_effort(default_effort.clone());
// Persist new effort to ConversationSession so resume is consistent.
if let Some(conv) = agent_loop.session.conversation() {
    let effort_str = default_effort.as_ref().map(|e| e.to_string());
    conv.update_reasoning_effort(effort_str);   // ← None 也会 fire meta
}
```

**问题建模**:

| 场景 | frontend optimistic | Runtime 算出 | meta 推回 | 用户看到的 |
|------|--------------------|------------|----------|----------|
| 新模型有 `default_reasoning_effort = "low"` | `defaultEffort = "low"` | `"low"` | `"low"` | 一致 ✅ |
| 新模型有 `default_reasoning_effort = "low"` 但前端 `availableModels` 缺这字段 | `defaultEffort = null` → 菜单隐藏 | `"low"` → 菜单恢复 | `"low"` | **菜单先消失再出现** ⚠️ |
| 新模型没有 `default_reasoning_effort` 但 `supports_reasoning = true` | `null` → 隐藏 | `"auto"` → 显示 | `"auto"` | **菜单先消失再出现** ⚠️ |
| 新模型既无 default 也不 supports_reasoning | `null` → 隐藏 | `None` → 永久不显示 | `""` | 乐观值保留 ❌ |
| 新模型不在 `global_provider_list`（caps 是 None） | depends | `None` → 永久不显示 | `""` | 乐观值保留 ❌ |

**结论**: 用户切模型时如果 (a) frontend 的 `availableModels` 缓存里没这个 model，或 (b) Runtime 的 `get_model_capabilities` 返回 None/不支持 reasoning，front-end optimistic 的 reasoningEffort 值会**和 Runtime 不一致**。Meta 回推如果是 `""` 还会被 chatStore 过滤掉，**用户的乐观值永远不会被覆盖**——视觉上就是"立即刷新但接着不动"。

**复现路径**: 在 frontend 切到一个前端 availableModels 里没有的新加载的模型，监控日志，预期看到：
- frontend set(reasoningEffort: null)
- Runtime: agent_loop.session.set_reasoning_effort(None)
- meta 的 reasoning_effort 字段为 ""
- frontend handler 把 `""` 过滤掉，patch 不包含 reasoningEffort → store 仍是 null → 菜单正确隐藏

这条路径是 OK 的。但**反方向**就坏了：用户在旧菜单里点了 "high"，切到新模型，Runtime 也算 "high"（虽然没改），这是平的情况。

**真正会出 bug 的是 [Race Condition]**（真因 B，更隐蔽）。

---

### 真因 B: 前端空串过滤在某些场景会把"Runtime 不知道"误判为"前端应该保留乐观值"（P0）

**位置**: `apps/acowork-desktop/src/stores/chatStore.ts:2828-2838`

```typescript
if (typeof data.model_id === "string" && data.model_id)  patch.model = data.model_id;
if (typeof data.provider_id === "string" && data.provider_id)  patch.provider = data.provider_id;
if (typeof data.message_count === "number")  patch.messageTotal = data.message_count;
if (typeof data.reasoning_effort === "string" && data.reasoning_effort) {
  patch.reasoningEffort = data.reasoning_effort;
}
if (typeof data.temperature === "number" && !Number.isNaN(data.temperature)) {
  patch.temperature = data.temperature;
}
```

**配合 Runtime 端**的构造（`conversation.rs:390-409`）：

```rust
pub fn build_session_meta_snapshot(&self) -> acowork_core::mqtt_proto::SessionMeta {
    let full = self.build_meta();           // 读 in-memory
    let tokens = full.tokens.clone();
    acowork_core::mqtt_proto::SessionMeta {
        ...
        provider_id: full.provider.unwrap_or_default(),         // ← 关键
        model_id: full.model.unwrap_or_default(),
        reasoning_effort: full.reasoning_effort.unwrap_or_default(),
        ...
    }
}
```

**问题建模**: 当用户在 `setCurrentModel(B, "", agentId)` 这种**少见的 provider_id 为空字符串**场景下（理论上 provider_id 应该总是非空，但 ModelMenu UI 只显示来自 `availableModels` 的 entry，且 entry.provider 必填），Runtime:
- 收到 `ModelSwitch { model: B, provider: None }`（空字符串经 `chat_mqtt.rs:596-597` 的 `unwrap_or("")` 转换后由 `parse_control_payload:204` 判空 → `provider_id = None`）
- `update_model_provider(B, None)` → 把 `self.provider = None`
- meta snapshot 的 `provider_id = ""`
- frontend 收到 `provider_id: ""` → 过滤掉 → patch.provider 不更新
- 结果: frontend 乐观更新的 provider 是 "P2"，但 backend 真正的 provider 已经变 None，**frontend 显示和 LLM 实际请求的 provider 不一致**

**这是一个潜在的安全/正确性 bug**: 用户看到的是 P2，LLM 请求实际发到 default provider，可能造成 401/404 错误。

**另一个变种**: `reasoning_effort` 在 ModelSwitch 后变成 `Some("auto")` → frontend 接收 `"auto"`，覆盖 `null` ← OK。但如果是 `None` → `""` → 过滤掉 → 保留 `null`（和真因 A 同一根）。

---

### 真因 C: SessionTask `ModelSwitch` 和 `ReasoningEffort` handler 末尾不调 `emit_session_state()`（P1）

**位置**: `core/acowork-runtime/src/agent/session/session_task.rs:1151-1233`

```rust
Some(SessionMessage::ModelSwitch { model, provider }) => {
    // ... set_model, set_provider, update_model_provider (meta), rebuild provider,
    //     set_override_model, set_reasoning_effort, update_reasoning_effort (meta)
    // ❌ 这里没有调用 agent_loop.emit_session_state()
    // ❌ 这里没有 emit ChunkEvent::SessionStateChanged
}

Some(SessionMessage::ReasoningEffort { effort }) => {
    // ... set_reasoning_effort, update_reasoning_effort (meta)
    // ❌ 这里也没有调用 agent_loop.emit_session_state()
}
```

对比 `Some(SessionMessage::UpdateRuntimeConfig(overrides))`：

```rust
Some(SessionMessage::UpdateRuntimeConfig(overrides)) => {
    agent_loop.apply_runtime_config(&overrides);
    agent_loop.emit_session_state();   // ← 有！只有这条路调了
}
```

**后果**: ModelSwitch/ReasoningEffort 完全依赖 meta_change_tx → relay → publish_session_meta 这条**唯一**链路。如果：
- 任意一环 `mpsc::Sender::send` / `try_send` 因 channel closed 失败
- relay 因 race 在 chunk_tx full 时 drop 通知
- broker 因为某些 ACL 拒收（理论上不应该）
- Desktop MQTT 因为网络抖动丢失 QoS 1 重传

→ meta 永远不会到达 frontend，**且完全没有补偿路径**（不会产生 SessionStateChanged）。

**UpdateRuntimeConfig 的路径**反而有双重保障（meta + emit_session_state），暴露了 ModelSwitch/ReasoningEffort 是单链路。

**修复建议**（在 session_task.rs 这两个 handler 末尾）：

```rust
Some(SessionMessage::ModelSwitch { model, provider }) => {
    // ... existing logic
    // Push authoritative runtime state to frontend (idempotent w/ meta path).
    // The meta path (publish_session_meta, retained) covers the GUI display;
    // this covers the live `session_state_changed` consumers that need
    // temperature / context_usage recalc to apply for the LLM call loop.
    agent_loop.emit_session_state();
}
```

```rust
Some(SessionMessage::ReasoningEffort { effort }) => {
    // ... existing logic
    agent_loop.emit_session_state();
}
```

---

### 真因 D (dead code): `model_confirmed` / `reasoning_effort_confirmed` handler 全代码库无 producer（P1）

**Frontend handler**: `chatStore.ts:2380-2414`，都注释说"Runtime will confirm"：

```typescript
case "model_confirmed": {
  const confirmedModel = data.model as string;
  ...
  console.log("[ChatStore] Model switch confirmed:", confirmedModel, confirmedProvider);
  ...
}
case "reasoning_effort_confirmed": {
  const confirmedEffort = data.effort as string;
  ...
  console.log("[ChatStore] Reasoning effort confirmed:", confirmedEffort);
  ...
}
```

**全部 producer 位置**: 0 处

```bash
$ rg -t rust -t ts "ModelConfirmed|ReasoningEffortConfirmed|model_confirmed|reasoning_effort_confirmed" \
    core/ apps/ docs/
# 只命中 ADR-012.md / session-diagnostic.md / chat_mqtt.rs:140 (注释) / chatStore.ts (handler)
# 没有任何地方触发 emit / publish
```

`CONTENT_EVENT_TYPES` set（chatStore.ts:1975）虽然是包含 `model_confirmed` / `reasoning_effort_confirmed`，但因为没人 publish，所以是 dead code。

实际生效的是 `case "session_meta"`（chatStore.ts:2820），这个命名应该保持一致。

**修复建议**: 把 dead handler 删了，或真去实现 `publish_model_confirmed` / `publish_reasoning_effort_confirmed`（推荐前者，因为 session_meta 已经有了）。

---

### 真因 E: meta relay 在 chunk channel 满时静默 drop（P2）

**位置**: `core/acowork-runtime/src/startup/subsystems.rs:436-445`

```rust
if chunk_tx.try_send(event).is_err() {
    // Channel full or closed — drop. The retained MQTT message
    // already holds the previous snapshot, so a missed push is
    // self-healing on the next successful one.
    tracing::debug!(
        session_id = %session_id,
        field = %field,
        "meta relay: chunk channel full/closed, dropping notification"
    );
}
```

**问题**: 
1. chunk 通道 cap = 64（`config.rs:127`）。在 LLM 流式输出高峰期，`SessionMetaChanged` 通知可能和 `Chunk` / `StreamChunk` 同时排队。
2. drop 是 `tracing::debug!()`，默认 debug 日志不输出，**生产环境几乎无声**。
3. "self-healing on next successful one" 假设是错的——`MetaChangeKind::Cold("model")` 是事件型触发，不是周期性轮询。如果 Cold 消息正好 drop，下一次 Cold 触发可能要等用户下一次手动操作。

**修复建议**: 至少把日志提到 warn：

```rust
if chunk_tx.try_send(event).is_err() {
    tracing::warn!(
        session_id = %session_id,
        field = %field,
        capacity = self.config.chunk_capacity,  // 需要传进 relay
        "meta relay: chunk channel FULL, dropping meta push. Frontend may show stale model/effort."
    );
    // 替代方案: 用 blocking_send + 短期 timeout 同步入队
}
```

或者，更可靠的：用 `permit` 或 `reserve`，让 publish 阻塞 100ms 再 retry。

---

## 3. 用户角度的"有时候立即刷新，有时候不刷新"

把上述 ABCDE 综合推演：

**场景 1: 用户点一个简单切换（例如 reasoning_effort: low → medium）**

链路 [2]→[12] 全部走通，前端 optimistic 立刻可见；meta 在 50-150ms 内回推，且 `[Runtime, frontend]` 都算出 `medium`，无冲突 → **立即刷新 ✅**

**场景 2: 用户切到 runtime 不支持 reasoning 的模型**

- 前端: `availableModels.find(...).default_reasoning_effort = null` → 乐观设 `null` → 菜单隐藏 ✅
- 后端: `caps.supports_reasoning = false` → `default_effort = None` → meta `reasoning_effort = ""`
- 前端: 收到 `""` → **过滤掉** → 不更新（patch 不包含 reasoningEffort） → store 仍是 `null`
- 用户看到: 菜单确实隐藏 ✅（巧合 OK）

**场景 3: 用户切到 global_provider_list 还没缓存的模型**

- 前端: 可能 optimistically 设了 `null` 或某个 default（如果 availableModels 含此 entry）
- 后端: `get_model_capabilities` 返回 `None` → `default_effort = None` → meta `""`
- 前端: 过滤 `""` → 不覆盖
- 结果: **乐观值保留**，但 LLM 实际调用用的是上一个未改的 effort（**实际行为和显示不一致**）

**场景 4: 用户切 provider（前端显示 P1→P2）和 model 一起**

- 前端乐观设 `model: B, provider: P2, reasoningEffort: <some_default_from_B>`
- 后端收到 `ModelSwitch { model: B, provider: Some(P2) }`，一切路径 OK
- meta 回推 `model_id: "B", provider_id: "P2", reasoning_effort: <some_default_from_B>`
- 三个字段都被前端接收，patch 正确 ✅

**场景 5: agent 正在忙于 LLM 调用中点切模型（race）**

- agent loop 在跑长 iteration
- 用户点 model_switch
- 前端乐观设值 ✅
- Runtime `route_model_switch` → `send_to_session` 排队
- agent loop 完成当前 iteration 后才消费 inbound
- 期间 meta_change_tx 已经发了 1-3 个通知，relay 已经 publish 了 1-3 次 meta
- frontend 已经接收到 meta，更新了 store
- agent loop 完成 iteration，新 model 生效，下一次 LLM 调用用新 model
- 用户看到: 立即刷新 ✅（因为 meta 早就到了）

但是！**如果 chunk_tx 在 agent loop 高速运行期间被 stream chunks 占满**（chunk channel cap=64，每个 chunk 是 tokio::sync::mpsc → 占用一个 slot），那么 meta 的 `try_send` 失败 → drop → meta 永远不到。

**结论**: 用户说的"有时候立即刷新，有时候不刷新" 最有可能是：
- **场景 3 的概率命中** (model 不在 global cache)
- **真因 E 的概率命中** (chunk channel 偶发拥塞)  
- **真因 C 的概率命中** (ModelSwitch 单链路，没有 emit_session_state 兜底)

---

## 4. 修复建议（按 ROI 排序）

### Fix 1 (P0, 30 min, ⭐⭐⭐⭐⭐ ROI): 在 ModelSwitch / ReasoningEffort handler 末尾追加 `emit_session_state()`

**位置**: `core/acowork-runtime/src/agent/session/session_task.rs:1151-1233`

```rust
Some(SessionMessage::ModelSwitch { model, provider }) => {
    // ... existing logic ...
    // Emit authoritative runtime state over the (non-retained) session_state_changed
    // topic so live consumers (status_panel, temperature gauge, context_usage bar)
    // update without relying on the (retained) meta snapshot path.  The meta path
    // covers the dropdown display; this complements it for the runtime-state fields.
    agent_loop.emit_session_state();
}

Some(SessionMessage::ReasoningEffort { effort }) => {
    // ... existing logic ...
    agent_loop.emit_session_state();
}
```

**理由**: 与 `UpdateRuntimeConfig` 的现有模式一致，零成本加一份 fallback。

### Fix 2 (P0, 1h, ⭐⭐⭐⭐ ROI): 修正 frontend 空串过滤，明确"missing"语义

**位置**: `apps/acowork-desktop/src/stores/chatStore.ts:2828-2838`

需要区分"missing"（Rust 端 `None`）和"empty string"（真正的空值）。当前 proto 把 `Option<String>` 编码成 `""`，无法在运行时区分。

**方案 A (preferred)**: 改 proto，把 `model_id` / `provider_id` / `reasoning_effort` 改成 `optional string`，新增字段 `field_present: bool` 或类似约定。

**方案 B (band-aid)**: 改 frontend 行为：把 `""` 视为 "**保留原值**"，但加一个独立的 `clearModel` / `clearProvider` action 让 backend 显式通知"被清空"。

**方案 C (minimal)**: 不改 proto，但在 Rust 端把 `Option<String>` 编码成 `None → null JSON → frontend JSON.parse 看到 null` 而非空串。具体方案：

```rust
// proto not nullable → encode None as a sentinel
provider_id: full.provider.clone().unwrap_or_default(),  // ""
// 改成附加 boolean:
is_provider_set: full.provider.is_some(),
```

让我倾向**方案 C**：proto 加 `*_is_set: bool` 字段（如 `provider_id_set`），不破坏向后兼容，frontend 据此判断"显式 None 还是缺失"。

### Fix 3 (P1, 15 min, ⭐⭐⭐⭐ ROI): 删 dead code，或真正实现 `_confirmed` events

**位置**: `apps/acowork-desktop/src/stores/chatStore.ts:2380-2414`

```typescript
// 删除以下两个 case，或
// 重命名为 case "session_meta" 内联 model_id 切完后的二次确认：
case "model_confirmed": { ... }      // 删
case "reasoning_effort_confirmed": { ... }  // 删
```

### Fix 4 (P2, 30 min, ⭐⭐ ROI): meta relay 升级 warn 日志 + chunk channel cap 自适应

**位置**: `core/acowork-runtime/src/startup/subsystems.rs:436-445`

```rust
if chunk_tx.try_send(event).is_err() {
    // 至少 warn，标明 push 失败让 ops 看到
    tracing::warn!(
        session_id = %session_id,
        field = %field,
        "meta relay: chunk channel full/closed, frontend meta push dropped"
    );
}
```

更彻底：把 chunk_tx 换成有界 unbounded，或扩容 chunk_capacity 到 1024。

---

## 5. 验证步骤（实施修复后跑一遍）

1. **静态检查**: `cd core && cargo clippy --all-targets -- -D warnings`、`cd apps/acowork-desktop && pnpm tsc --noEmit`
2. **单元测试**: `cd core && cargo test` — 重点关注 `session` 相关 47+ 测试
3. **手工验证场景**:
   - [ ] 进 Agent，运行中，切 model A→B→A，**期望**: UI 每次都正确显示当前 model
   - [ ] 进 Agent，运行中，切 reasoning_effort low→medium→high，**期望**: 同上
   - [ ] 切到一个新加载的 model（backend 还没缓存 capabilities），**期望**: 不闪退，UI 显示 backend 的 default_effort（Runtime 真值）
   - [ ] 关闭 Desktop 再打开，**期望**: meta retained 立即回填，不需要重新切换
4. **Race 验证**: 让 agent 跑长 iteration，同时点击切 model/reasoning，**期望**: meta 必到 + emit_session_state() 兜底

---

## 6. 不在本次 review 范围内

- `case "model_confirmed"` 等 dead handler 的历史/删除原因（需要单独 ADR）
- chunk channel 容量调整的全局影响（`config.rs:127 default_chunk_capacity = 64`）
- proto 改造的影响范围（影响所有 Desktop/Runtime 双方）

---

## 附录 A: 关键代码引用清单

| 引用 | 文件:行 | 用途 |
|------|---------|------|
| Frontend optimistic update (model) | `apps/acowork-desktop/src/stores/chatStore.ts:1281-1312` | setCurrentModel |
| Frontend optimistic update (effort) | `apps/acowork-desktop/src/stores/chatStore.ts:1321-1334` | setReasoningEffort |
| Tauri Command | `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs:404-427` | mqtt_publish_control |
| Tauri topic format | `apps/acowork-desktop/src-tauri/src/mqtt_client.rs:444-458` | publish_control |
| Tauri proto builder | `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs:434-656` | build_control_command |
| Runtime MQTT subscribe | `core/acowork-runtime/src/mqtt/client.rs:199-206, 460-464` | control_filter + bootstrap |
| Runtime parse | `core/acowork-runtime/src/mqtt/control_handler.rs:147-252` | parse_control_payload |
| Runtime dispatch | `core/acowork-runtime/src/startup/gateway_loop.rs:139-280` | control_action_to_inbound |
| Runtime routing | `core/acowork-runtime/src/agent/session/session_manager.rs:1685-1720` | route_model_switch, route_reasoning_effort |
| SessionTask handler (model) | `core/acowork-runtime/src/agent/session/session_task.rs:1151-1219` | **触发 meta_change 但不 emit_session_state** |
| SessionTask handler (effort) | `core/acowork-runtime/src/agent/session/session_task.rs:1220-1233` | **同上** |
| Persist + notify | `core/acowork-runtime/src/conversation.rs:786-824` | update_model_provider, update_reasoning_effort |
| Meta snapshot builder | `core/acowork-runtime/src/conversation.rs:379-410` | build_session_meta_snapshot |
| Meta relay | `core/acowork-runtime/src/startup/subsystems.rs:395-451` | **chunk_tx full 时 drop** |
| MQTT publish meta | `core/acowork-runtime/src/mqtt/client.rs:727-755` | publish_session_meta (QoS 1, retain) |
| Desktop subscribe | `apps/acowork-desktop/src-tauri/src/mqtt_client.rs:145-157` | lifecycle filters |
| Desktop decode | `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs:140-160` | SessionMeta envelope → agent-event |
| Frontend session_meta handler | `apps/acowork-desktop/src/stores/chatStore.ts:2819-2847` | **空串过滤** |
