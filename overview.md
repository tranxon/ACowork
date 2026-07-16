# ADR-035 修复实施 Overview

**日期**：2026-07-16
**修复范围**：Review 报告中识别的 3 Critical + 4 Major + 4 边界问题
**状态**：✅ 全部修复完成（除 B2 完整重连检测/B3 压测/M3 #4#5/M4 原生槽位 评估后列为后续优化）

## 修复清单

### P0 Critical（已修复）

#### C1. 后端实现 record_complete emit ✅
- `ChunkEvent` 加 `RecordComplete { session_id, role, message_id, content }` 变体
- `StreamingLine` 加 `message_id: String` 字段，`ensure_streaming_line` 创建时分配 UUID
- `flush_streaming_line` 改用 `append_message_with_id`（JSONL 行 id = streaming line message_id）+ emit `ChunkEvent::RecordComplete`
- `loop_tools.rs` `prepare_tool_calls`/`persist_and_emit_tool_results` 中 tool_call/tool_result 也 emit RecordComplete
- `mqtt/client.rs` 新增 `publish_record_complete`（**QoS 1**，对 tool_result 裁剪首5行）
- 删除废弃的 `publish_tool_call`/`publish_tool_result`（dead_code，已被 record_complete 替代）

#### C2. activeStream 兜底（不截断 assistant）✅
- record_complete 提升至 **QoS 1**（ADR O2 建议）——权威终态事件必须至少投递一次
- 前端 `session_state_changed→idle` 时若 activeStream 仍在 → 触发 HTTP 全量重对齐拉取（覆盖 QoS 丢帧场景）
- **assistant activeStream.lines 不加上限**（用户明确要求完整显示）

#### C3. HTTP 路径裁剪 toolResult ✅
- `http/server.rs:get_messages` handler 中遍历 paginated.messages，对 `role=='tool_result'` 调用 `truncate_tool_result_for_display`
- `truncate_tool_result_for_display` 改为 `pub(crate)` 供跨模块调用

### P1 Major（已修复）

#### M1. 修订 ADR D2 cursor 描述 ✅
- ADR D2 更新为"双 cursor 设计"：`delivery_cursor`（HTTP 分页，行号语义）+ `stream_push_offset`（MQTT 推送，字符偏移语义）
- 补充 Phase 3 现状备注

#### M2. stream_delta 携带真实 message_id ✅（在 C1 中一起做了）
- `try_send_stream_delta` 读取 `StreamingLine.message_id` 传入 StreamLine
- `ChunkEvent::StreamDelta` 签名改为 `Vec<(String, String, String)>`（role, message_id, content）

#### 清理 EnableNotify/DisableNotify 全链路 + 死参数 ✅
- 删除 `EnableNotify`/`DisableNotify`：`session_task.rs` enum 变体 + Debug impl + no-op；`session_manager.rs` 3 处 send_to_session；`cli.rs` 2 处；`gateway_loop.rs` 映射+路由；`inbound.rs` 枚举+Debug；`control_handler.rs` 枚举+proto映射改 return None
- `loadSessionMessages` 类型签名删除死参数 `incremental?: boolean`
- `useStreamingContent.ts` 保留但更新注释（ADR-027 → ADR-035）

### P2 Minor（已修复/已评估）

#### M3. D8.1 后端内存治理 ✅ 部分
- `accumulated_content: String::with_capacity(4096)` 预分配完成
- #4（减少 clone）/ #5（复用 BytesMut）评估后列为后续优化（当前实现已符合 ADR"不随历史增长"要求）

#### M4. ThinkBlock 5 固定槽位 ✅ ADR 描述更新
- ADR D4 补充"实现修订"说明：用 `useDeferredValue` + `React.memo` 替代"5 固定槽位"

#### SessionDataStore 类型 ✅
- `types.ts` 新增 `StreamLine`/`ActiveStream`/`SessionDataStore` 类型

#### B1. O5 cursor 对齐守护 ✅（通过 M1 解决）
- ADR D2 双 cursor 描述明确了 delivery_cursor 与 stream_push_offset 的职责区分

### 未做（评估后决定）

- **B2 完整 MQTT 断线重连检测**：C2 idle 兜底已覆盖主要场景（activeStream 未冻结时触发 HTTP 重对齐）
- **B3 多 session 并发推送压测**：需要测试基础设施
- **M3 #4/#5**：减少 clone / 复用 BytesMut——当前实现已符合 ADR"不随历史增长"要求
- **M4 原生 5 固定 DOM 槽位**：当前 useDeferredValue + React.memo 方案性能可接受

## 改动文件清单

### 后端（Rust）
- `core/acowork-runtime/src/agent/loop_.rs` — ChunkEvent 加 RecordComplete 变体 + StreamDelta 签名改
- `core/acowork-runtime/src/agent/session_core.rs` — StreamingLine.message_id + flush_streaming_line emit RecordComplete + try_send_stream_delta 传 message_id + accumulated_content 预分配 4KB
- `core/acowork-runtime/src/agent/loop_tools.rs` — prepare_tool_calls/persist_and_emit_tool_results emit RecordComplete for tool_call/tool_result
- `core/acowork-runtime/src/startup/subsystems.rs` — relay_chunk_event_mqtt 加 RecordComplete 分支 + StreamDelta 适配 3 元组
- `core/acowork-runtime/src/mqtt/client.rs` — publish_record_complete（QoS 1）+ publish_with_qos + 删 publish_tool_call/publish_tool_result
- `core/acowork-runtime/src/conversation.rs` — StreamingLine 加 message_id 字段 + 所有构造点更新
- `core/acowork-runtime/src/cli.rs` — truncate_tool_result_for_display 改 pub(crate) + 删 EnableNotify/DisableNotify 调用
- `core/acowork-runtime/src/http/server.rs` — get_messages 裁剪 toolResult
- `core/acowork-runtime/src/agent/session/session_task.rs` — 删 EnableNotify/DisableNotify enum 变体 + Debug impl + no-op 处理
- `core/acowork-runtime/src/agent/session/session_manager.rs` — 删 EnableNotify/DisableNotify send_to_session 调用
- `core/acowork-runtime/src/startup/gateway_loop.rs` — 删 EnableNotify/DisableNotify 映射+路由
- `core/acowork-runtime/src/agent/inbound.rs` — 删 EnableNotify/DisableNotify 枚举+Debug
- `core/acowork-runtime/src/mqtt/control_handler.rs` — 删 EnableNotify/DisableNotify ControlAction 枚举 + proto 映射改 return None

### 前端（TypeScript）
- `apps/acowork-desktop/src/stores/chatStore.ts` — record_complete case 支持 4 种 role + idle 兜底 + 类型迁移 + 删死参数
- `apps/acowork-desktop/src/lib/types.ts` — 新增 StreamLine/ActiveStream/SessionDataStore 类型
- `apps/acowork-desktop/src/components/chat/useStreamingContent.ts` — 注释更新

### ADR 文档
- `docs/adr/zh/ADR-035-mqtt-streaming-push-refactor.md` — D2 双 cursor 描述 + D1 record_complete QoS 1 + D4 实现修订说明 + Phase 3 现状备注

## 验证结果

- `cargo build -p acowork-runtime` ✅
- `cargo clippy -p acowork-runtime --lib` ✅ 无 warning
- `cargo test -p acowork-runtime --lib session_core::tests` ✅ 11 passed
- `cargo test -p acowork-runtime --lib conversation` ✅ 50 passed
- `npx tsc --noEmit` ✅

## 后续待办

1. **运行时验证**：端到端验证 record_complete 链路（emit → MQTT publish → chat_mqtt 翻译 → chatStore case → messages[] finalization）
2. **B2 完整重连检测**：MQTT 断线监听 + 自动 HTTP 重对齐（当前只有 idle 兜底）
3. **B3 压测**：多 session 并发推送性能验证
4. **M3 #4/#5**：后端减少 clone / 复用 BytesMut（若压测发现瓶颈）
5. **M4 原生 5 槽位**：若 DOM 碎片压测发现问题
