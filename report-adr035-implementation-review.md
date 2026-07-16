# ADR-035 实现 Review 报告

**Review 日期**：2026-07-16
**Reviewer**：Senior Developer (高级开发工程师)
**ADR**：`docs/adr/zh/ADR-035-mqtt-streaming-push-refactor.md`
**Review 范围**：Phase 1/2/2.5/3 全部需求 + D1-D9 决策 + 边界情况 + 架构 + 性能
**Review 方法**：源码实证（每条断言均有 file:line 引用）

---

## 总评

**实现完成度约 60%**，但存在 **3 个 Critical 缺陷** 导致 ADR 核心目标未达成：
- ADR 的核心动机"解决最后一轮 assistant 不渲染 bug"**未实现**——因为 `record_complete` 事件后端从未 emit。
- 前端 `record_complete` case 是死代码，依赖它的 `isStreaming: false` 状态转换和 `activeStreams.delete` 都不会触发。
- HTTP 路径的 toolResult 裁剪未实现，违反 D9.2 "无例外"约束。

**结论：当前实现不可上生产，必须修复 C1-C3 后才能验证 ADR 目标是否达成。**

---

## 一、按 Phase 的实现完整度

### Phase 1：后端 stream_delta 推送 — ⚠️ 部分实现

| ADR 要求 | 状态 | 实证 |
|---------|------|------|
| proto 定义 `StreamDeltaPayload`/`StreamLine`/`RecordCompletePayload` | ✅ | `mqtt_payload.proto:455,466,474` |
| `stream_push_offset` 字段 | ✅ | `session_core.rs:65` |
| 500ms tick + `try_send_stream_delta` 推整行 | ✅ | `session_core.rs:367-440` |
| `ChunkEvent::StreamDelta` → `publish_stream_delta` (QoS 0) | ✅ | `subsystems.rs:502`, `mqtt/client.rs:869,434` |
| `enable_notify`/`disable_notify` 不再抑制推送 | ✅ | `session_core.rs:361-363` 注释 |
| `ChunkEvent::RecordComplete` + `publish_record_complete` | ❌ **未实现** | grep 全 crate 无 `RecordComplete` 引用（除 proto 和前端翻译层） |
| 删除 `enable_notify`/`disable_notify` enum 变体 | ❌ 仍是 no-op | `session_task.rs:149-150,1623-1626` |
| `publish_tool_call` / `publish_tool_result` 实际调用 | ❌ `#[allow(dead_code)]` | `mqtt/client.rs:491,519` 从未被调用 |

### Phase 2：前端 per-session 存储 — ⚠️ 部分实现

| ADR 要求 | 状态 | 实证 |
|---------|------|------|
| `activeStreams` 全局 Map per-session 单缓冲 | ✅ | `chatStore.ts:24` |
| `handleMessageEvent` 中 `stream_delta`/`record_complete` case | ⚠️ case 写了，但 record_complete 是死代码 | `chatStore.ts:1588,1609` |
| thinking 末5行裁剪 | ✅ | `chatStore.ts:1604` |
| `switchSession` 删除 `enable_notify`/`disable_notify` | ✅ | `agentStore.ts:620-636` |
| `session_state_changed→idle` 不再 HTTP 兜底 | ✅ | `chatStore.ts:1887` 注释 |
| `SessionDataStore` 类型加入 `types.ts` | ❌ 未加入 | `types.ts:712-719` 只改 PaginatedMessages |
| `useStreamingContent.ts` 删除 | ❌ 仍存在 | `useStreamingContent.ts` 整文件未删 |
| D4 ThinkBlock 末5行原地覆盖 5 固定槽位 | ⚠️ 部分实现 | `ThinkBlock.tsx:84-170` 用 ReactMarkdown 重渲染，非"5 固定槽位" |
| D5 MessageBubble assistant 处理中动画 | ✅ | `MessageBubble.tsx:284-297` |
| assistant `activeStream.lines` 上限裁剪 | ❌ **缺失** | `chatStore.ts:1603-1604` 只裁剪 thought |

### Phase 2.5：toolResult 源头裁剪 — ❌ HTTP 路径未裁剪

| ADR 要求 | 状态 | 实证 |
|---------|------|------|
| gRPC `handle_get_session_messages` 裁剪 | ✅ | `cli.rs:2992` |
| MQTT `publish_tool_result` 裁剪 | ✅（但函数 dead_code） | `mqtt/client.rs:526,902` |
| HTTP `http/server.rs:get_messages` 裁剪 | ❌ **未裁剪** | `http/server.rs:361-420` 无 `truncate_tool_result_for_display` 调用 |
| 前端 ExploreBlock 删除 500 字符二次截断 | ✅ | `ExploreBlock.tsx:582-584` 注释 |

**关键**：前端实际走的是 HTTP 路径（fetch → gateway `proxy_get_messages` → runtime `http/server.rs:get_messages`），所以 D9.2 "无例外"约束**实际未生效**。

### Phase 3：清理旧链路 + O1 — ✅ 基本完成

| ADR 要求 | 状态 | 实证 |
|---------|------|------|
| 删除 `polling.ts` | ✅ | 文件不存在 |
| 删除 `streamingContents` Map | ✅ | `chatStore.ts` 无引用 |
| 删除 `new_data_available` case | ✅ | `chatStore.ts:1587` 注释 |
| 删除 HTTP 增量参数（cli.rs） | ✅ | `cli.rs:2958-2960,2981-2983` 注释 |
| 删除 `EnableNotify`/`DisableNotify` enum 变体 | ❌ 仍是 no-op | `session_task.rs:149-150` |
| O1 N=150 `trimMessages` 三路径截断 | ✅ | `chatStore.ts:53-56,1114,1128,1528,1530` |
| `loadSessionMessages` 类型签名清理 `incremental` 参数 | ❌ 死参数 | `chatStore.ts:345-353` vs `1043-1049` |

---

## 二、严重缺陷（Critical — 必须修复）

### C1. `record_complete` 事件后端未实现 → ADR 核心目标未达成

**现象**：
- proto 定义了 `RecordCompletePayload`（`mqtt_payload.proto:474`）
- 前端翻译层写了 `RecordComplete` 分支（`chat_mqtt.rs:693`）
- 前端 store 写了 `record_complete` case（`chatStore.ts:1609-1624`）
- 但 `ChunkEvent` enum **没有** `RecordComplete` 变体（`loop_.rs:70-204`）
- 后端**没有任何** `publish_record_complete` 函数
- grep 整个 `core/` 只有 proto 和前端翻译层提到 `RecordComplete`

**影响**：
- `record_complete` 是 ADR D1 表中的关键事件：assistant 一次性渲染、thought 冻结进 messages[]、清 activeStream 都依赖它
- 后端不发 → 前端 `isStreaming: false` 转换永不触发（`chatStore.ts:1618,1620`）
- assistant 消息永远卡在"thinking..."动画状态（`MessageBubble.tsx:284`）→ **ADR-035 要解决的"最后一轮 assistant 不渲染"bug 仍然存在**
- thought 永远是 isThinking 状态，不会转为 thought

**直接违反**：
- D1 事件模型表（缺 `record_complete` 行的实现）
- D4 thinking 渲染规则（"定稿后冻结进 messages[]，activeStream=null"）
- D5 assistant 渲染规则（"收到 record_complete role=assistant 一次性渲染"）
- D9.1 thinking 定稿冻结（依赖 record_complete）

**修复方向**：
1. 后端在每条记录落盘定稿时（`append_message` / `flush_streaming_line` 等）emit `ChunkEvent::RecordComplete { role, message_id, content }`
2. `relay_chunk_event_mqtt` 中添加 `ChunkEvent::RecordComplete` 分支 → 新增 `publish_record_complete`
3. `publish_record_complete` 中对 `tool_result` 调用 `truncate_tool_result_lines`（D9.2）
4. 同时把 `publish_tool_call` / `publish_tool_result` 的 `#[allow(dead_code)]` 去掉并接入 emit 点（替代 record_complete 的角色特化路径，或两者并存按 ADR D1 表）

---

### C2. assistant `activeStream.lines` 无上限裁剪 → 内存泄漏

**现象**：
```ts
// chatStore.ts:1603-1604
for (const l of lines) as.lines.push({ ... });
if (as.role === 'thought' && as.lines.length > 5) as.lines = as.lines.slice(-5);
```
- 只对 `role === 'thought'` 做 5 行滑动窗口
- assistant lines **没有上限**

**叠加 C1 影响**：
- record_complete 不来 → `activeStreams.delete(sid)` 不触发（`chatStore.ts:1617`）
- assistant activeStream.lines 持续累积，长会话下线性增长
- 每条 line 是不可变 String，永不被释放
- 直接违反 D8.2 #5 "活动缓冲定稿即清空"

**修复方向**：
- 短期：assistant 也加上限（如 1000 行或按字符数 100KB），避免内存爆炸
- 长期：补齐 record_complete 后此问题自然消失

---

### C3. HTTP 路径未裁剪 toolResult → D9.2 "无例外"违反

**现象**：
- ADR D9.2 明确要求"所有前端可见的投递路径一律裁剪 toolResult 为首 5 行，无任何例外"，列出三条路径：
  - ① MQTT record_complete/tool_result 事件 — `mqtt/client.rs:526` ✅（但 dead_code）
  - ② HTTP GET /messages 分页响应 — `http/server.rs:361` ❌ **未裁剪**
  - ③ HTTP 重连重对齐 — 同 ② ❌
- 实际前端走的路径：`fetch /api/agents/{id}/sessions/{sid}/messages` → gateway `proxy_get_messages`（`proxy.rs:213`）→ runtime HTTP `GET /sessions/{sid}/messages` → `http/server.rs:361 get_messages` handler
- 该 handler 调用 `read_messages_paginated` 后直接 `serde_json::to_value(&messages)`，**未应用 `truncate_tool_result_for_display`**

**影响**：
- 首次打开 session、向上翻历史、重连重对齐时，toolResult 完整内容进入前端
- 违反 D9.2 "前端永远不接收完整 toolResult"的硬约束
- 影响 O1 messages[] 上限效果（toolResult 单条可能很大）

**修复方向**：
- `http/server.rs:361 get_messages` 中 `read_messages_paginated` 后，遍历 `paginated.messages`，对 `role == "tool_result"` 调用 `truncate_tool_result_for_display`
- 或把 `truncate_tool_result_for_display` 移到 `read_messages_paginated` 内部统一应用

---

## 三、实现偏差（Major — 建议修复）

### M1. cursor 推进实现与 ADR 描述不一致

**ADR D2**：沿用既有 `delivery_cursor` 基础设施，从"HTTP 拉"改为"定时推"。
**实际实现**：新增 `stream_push_offset`（`session_core.rs:65`，字符偏移），不是 `delivery_cursor`（行号）。
- 两套 cursor 独立维护：`delivery_cursor`（HTTP，行号）+ `stream_push_offset`（push，字符）
- `reset_delivery_cursor`（cli.rs:3001）只重置 HTTP cursor，不影响 push cursor
- ADR 验证清单自己也提到："当前 Phase 1 用 `streaming_lines.accumulated_content` 派生整行（非 `read_messages_since_cursor`，待 Phase 2/3 移除 HTTP 时切换）"——但 Phase 3 已"完成"，切换没做

**影响**：
- 架构与文档脱节，未来维护者按 ADR 读代码会困惑
- O5 不变量守护不完整（见 B1）

### M2. stream_delta 中 message_id 永远为空字符串

**现象**：`subsystems.rs:505-510` 中：
```rust
.map(|(role, content)| StreamLine {
    role,
    message_id: String::new(),  // ← 永远空字符串
    line_no: 0,                  // ← 永远 0
    content,
})
```

**影响**：
- 前端用空字符串作 message_id（`chatStore.ts:1593`）
- 如果 C1 修复后 record_complete 携带真实 message_id，前端 `as.messageId === msgId` 匹配会失败（空 vs 真实）
- 必须在 `try_send_stream_delta` 中从 `StreamingLine` 提取或分配真实 message_id

### M3. D8.1 后端内存治理 3 项违反

| D8.1 要求 | 状态 | 实证 |
|---------|------|------|
| #1 单 String 追加，禁止 Vec-of-pieces + join | ✅ | `conversation.rs:1088` `accumulated_content: String` |
| #2 预分配容量（4-8 KB 或按历史均值预热） | ❌ | `session_core.rs:235` `String::new()` 无预分配 |
| #3 delta 只发新增整行 | ✅ | `session_core.rs:395-432` |
| #4 序列化不 clone content，写进 protobuf 编码缓冲 | ❌ | `session_core.rs:410,416` `Vec<char>::collect` + `String::collect`；`subsystems.rs:511` `lines.to_vec()`；`mqtt/client.rs:880` `lines.to_vec()` |
| #5 复用 `BytesMut` encode buffer | ❌ | `mqtt/client.rs:894` `prost::Message::encode_to_vec` 每次新 Vec |
| #6 JSONL 流式写（BufWriter + to_writer） | ✅ | `conversation.rs:2186,2599,3351` `serde_json::to_writer` |

**影响**：
- 500ms tick 的堆分配未达 ADR 验证清单要求"不随历史增长"
- 每次 `try_send_stream_delta` 创建 `Vec<char>` + 多个 `String`，长 delta 时压力大

### M4. D8.2 前端 DOM 治理不完整

**ADR D4/D8.2 #4**："5 个固定槽位原地覆盖内容、不增删 DOM 节点"
**实际**：`ThinkBlock.tsx:157` `<ReactMarkdown ...>{deferredContent}</ReactMarkdown>` 每次 content 变化重新解析 markdown AST 并 diff
- DOM 节点数随 markdown 结构变化（标题、列表、代码块数量不同）
- React diff 会创建/销毁节点，非"原地覆盖"
- 违反 D4 "只覆盖、不新增 DOM 节点"

**缓解**：用了 `useDeferredValue` + `React.memo`，性能尚可，但未达 ADR 设计意图。

---

## 四、边界情况

### B1. cursor 对齐不变量（O5）守护不完整 — ⚠️ 风险中

**ADR D2/O5**：HTTP 初始加载后必须 `reset_delivery_cursor`，否则 push 会重复投递历史。
**实际**：
- `reset_delivery_cursor` 只在 cli.rs:3001（gRPC 路径）调用
- HTTP server.rs `get_messages` handler **没有** `reset_delivery_cursor` 调用
- 但前端实际走 HTTP，所以 HTTP 初始加载后 push cursor 不重置

**缓解**：
- `stream_push_offset` 在新 streaming line 创建时归零（`session_core.rs:229`），且新 line `accumulated_content` 为空
- 如果 HTTP 加载完成时 streaming line 是新建的，push 不会重复投递

**风险**：
- 如果 HTTP 初始加载期间 streaming line 已存在且 `accumulated_content` 有内容，push 会从 `stream_push_offset` 继续推送剩余未推部分
- 这部分增量如果**已在 HTTP 加载的 JSONL 行中**（已落盘），会重复投递

### B2. QoS 0 丢帧兜底缺失（O2）— ❌ 未实现

**ADR 对策**：断线重连后 HTTP 重对齐。
**实际**：前端没有 MQTT 断线检测 + HTTP 重对齐逻辑（grep 无相关代码）。
**叠加 C1 影响**：record_complete 本来就没实现，谈不到丢帧兜底。

### B3. 多 session 并发推送量（O3）— ⚠️ 未压测

**ADR**：localhost 单用户场景可接受，待压测。
**实际**：所有打开的 session 都保持订阅，无 backpressure。当前测试规模下应可接受，但无压测验证。

### B4. thinking 5 行可用性（O4）— ✅ 符合 ADR

5 行无滚动是用户明确要求的产品决策，实现符合 ADR。

---

## 五、架构合理性

### 优点
1. **per-session `activeStreams` Map 设计简洁**，符合 D3"每 session 单缓冲"原则
2. **接收与渲染解耦清晰**（foreground 标志只控渲染，不影响接收/存储）
3. **删除 PollingManager 后链路大幅简化**，bug 面积缩小
4. **MQTT QoS 0 + record_complete 终态**的设计模式合理（如果实现完整）

### 问题
1. **ADR 设计与实现系统性脱节**：proto/前端翻译层/前端 case 都写了，但后端 emit 点缺失，形成"形似神不似"的实现。建议未来 ADR 实施时增加"端到端 emit→subscribe→handle 链路验证"作为 Phase 完成标准。
2. **双 cursor 设计**（`stream_push_offset` + `delivery_cursor`）增加认知负担。建议要么按 ADR D2 用单一 `delivery_cursor`，要么修订 ADR 描述实际实现。
3. **前端 `record_complete` 与 `stream_delta` 的 message_id 串联不严谨**（M2）。即使 C1 修复，仍需修复 M2 才能正确匹配。
4. **`useStreamingContent.ts` 保留但 ADR 要求删除**，实际只是包装了 `getStreamingContent`/`subscribeStreaming`，可考虑直接 inline 到 MessageBubble/ExploreBlock 简化层次。

---

## 六、性能

### 后端（D8.1）
- ❌ `accumulated_content` 无预分配（违反 D8.1 #2）
- ❌ `try_send_stream_delta` 中 `Vec<char>::collect` + `String::collect` 多次堆分配（违反 D8.1 #4）
- ❌ `prost::Message::encode_to_vec` 每次新 Vec（违反 D8.1 #5）
- ✅ 单 String 追加、JSONL 流式写、delta 只发新增整行

**ADR 验证清单"500ms tick 堆分配不随历史增长"实际未达成**——每次 tick 的 `Vec<char>` 大小与 delta 成正比，长 delta 时压力大。

### 前端（D8.2）
- ❌ ThinkBlock 非"5 固定槽位"（违反 D8.2 #4）
- ❌ **`activeStream` 定稿不清空**（违反 D8.2 #5）——叠加 C1 导致内存泄漏
- ✅ 存数组不存增长字符串（`array.push` 摊还 O(1)）
- ✅ `useDeferredValue` + `React.memo` 减少不必要重渲染

---

## 七、修复优先级

### P0（Critical，必须立即修复）
1. **C1**：后端实现 `ChunkEvent::RecordComplete` + `publish_record_complete` + emit 点（在 `flush_streaming_line` / `append_message` 等定稿点）
2. **C2**：assistant `activeStream.lines` 加上限（短期缓解内存泄漏）
3. **C3**：`http/server.rs:get_messages` 调用 `truncate_tool_result_for_display`

### P1（Major，建议本迭代修复）
4. **M2**：`try_send_stream_delta` 中提取真实 message_id 传入 `StreamLine`
5. **M1**：统一 cursor 设计——要么按 ADR 用 `delivery_cursor`，要么修订 ADR 描述实际实现
6. 删除 `useStreamingContent.ts`（inline 到消费方）
7. 删除 `EnableNotify`/`DisableNotify` enum 变体（session_task.rs:149-150）
8. 清理 `loadSessionMessages` 类型签名 `incremental?: boolean` 死参数

### P2（Minor，可下迭代）
9. **M3**：后端 `accumulated_content` 预分配 + 复用 `BytesMut` + 减少 clone
10. **M4**：ThinkBlock 改为"5 固定 DOM 槽位"原生实现（非 ReactMarkdown 重渲染）
11. `SessionDataStore` 类型加入 `types.ts`（与 ADR 影响范围对齐）
12. **B2**：MQTT 断线重连 + HTTP 重对齐兜底
13. **B3**：多 session 并发推送压测

---

## 八、验证清单复核

| ADR 验证项 | ADR 标记 | Review 结果 |
|---------|---------|------------|
| 后端 emit `stream_delta`：500ms 整行增量 | [x] | ✅ 已实现 |
| 推送游标只在后端 | [x] | ⚠️ 用 `stream_push_offset` 非 `delivery_cursor`，与 ADR 描述不符 |
| thinking 末5行原地覆盖 5 固定槽位 | [x] | ❌ 用 ReactMarkdown 重渲染，非"5 固定槽位" |
| assistant 处理中动画 + 一次性渲染 | [x] | ⚠️ 动画 ✅，一次性渲染 ❌（record_complete 不来） |
| session A→B→A 切换无 HTTP | [ ] 待验证 | ⚠️ 代码逻辑符合，但因 C1 无法实际验证 finalization |
| 首次 HTTP 首页分页 + 向上翻回填 | [ ] 待验证 | ⚠️ 代码逻辑符合 |
| "最后一轮 assistant 不渲染"bug 不复现 | [ ] 待验证 | ❌ **bug 仍存在**（C1 直接导致） |
| runtime 对后台 session 仍推送 | [x] | ✅ 已实现 |
| 向上翻历史分页回填 | [ ] 待验证 | ⚠️ 代码逻辑符合 |
| 后端 500ms tick 堆分配不随历史增长 | [x] | ❌ 未达成（M3） |
| 前端 activeStream 定稿即清空 | [x] | ❌ 未达成（C1+C2） |
| thinking activeStream.lines 滚动上限 5 行 | [x] | ✅ 已实现 |
| toolResult 所有路径裁剪首 5 行无例外 | [x] | ❌ HTTP 路径未裁剪（C3） |

---

## 九、给 ADR 维护者的建议

1. **ADR 验证清单需要重新审视**：当前 [x] 标记过于乐观，多项实际未达成。建议增加"端到端 emit→subscribe→handle 链路验证"作为 Phase 完成标准。
2. **ADR 与实现同步**：M1（双 cursor）应在 ADR 中更新描述，或代码按 ADR 重构。
3. **Phase 划分需调整**：Phase 2 标"已实施"但 record_complete 后端没做，实际只完成一半。建议增加 "Phase 2.5b: 后端 record_complete emit" 作为前置。
4. **关键风险点 O5 应转为不变量**：在 `http/server.rs:get_messages` 中添加 `reset_delivery_cursor` 调用并加测试守护。

---

**报告完。**
