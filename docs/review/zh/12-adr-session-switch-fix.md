# ADR-007: Session 切换根因修复 — switch_conversation 协议

**Date**: 2026-05-06
**Status**: Fully Implemented (Phases 1.1–1.4 + 1.5 + 2 + 3)
**Decision Fix**: P0 Bug — `handle_create_session` drop 新 session 导致消息写入错误 JSONL

## 问题陈述

### 用户报告的 3 个表面 Bug
1. 新建 session 仍显示 "Untitled"，首句不自动命名
2. 切换 agent 再返回后，当前 session 变空白，新 session 内容被拼接到其他 session 末尾
3. fetchSessions 导致 session 列表闪烁

### 根因分析（P0）

`cli.rs:1332` 的 `handle_create_session` 函数：

```rust
// BEFORE (buggy)
Ok(_session) => {   // ← _session 在这里被 drop！
    *current_session_id = new_session_id.clone();
    // AgentLoop.conversation 从未被更新！
}
```

**问题链路**：
1. Gateway 收到 `POST /api/agents/{id}/sessions`
2. 推送 `IntentReceived { action: "create_session" }` 给 Runtime
3. Runtime 创建了新的 `ConversationSession`（JSONL 文件已创建）
4. **但新 session 立刻被 drop（`_session` 绑定）**
5. `AgentLoop.conversation` 仍指向旧的 session
6. 后续所有 `append_message()` 写入旧 JSONL 文件
7. 前端看到：新 session 的消息"跑到了"其他 session 里

### 为什么前端修复不够

之前在 sessionStore/chatStore 做的前端修复（清除 currentSessionId、fetchSessions 不清空数据等）解决了 **UI 层面的闪烁和残留问题**，但 Runtime 侧的消息路由根本没变——消息仍然写入错误的 JSONL 文件。

## 决策

### 架构约束回顾

> 一个 Agent = 一个 Runtime = **一个活跃 ConversationSession**

这意味着切换 session 不是前端概念，而是 Runtime 必须参与的操作。

### 方案：switch_conversation 协议

```
┌──────────┐    POST /sessions              ┌──────────┐   IntentReceived     ┌─────────┐
│  Frontend │ ────────────────────────────> │ Gateway  │ ──────────────────> │ Runtime │
│          │ <──────────────────────────── │          │ <────────────────── │         │
│          │   { session_id: "xxx" }       │          │   { session_id }    │         │
└──────────┘                               └──────────┘                     └─────────┘
                                                                                    │
                                                                              switch_conversation()
                                                                              (close old → open new)
                                                                                    │
┌──────────┐   POST /sessions/{id}/activate   ┌──────────┐   activate_session   ┌─────────┐
│  Frontend │ ───────────────────────────────> │ Gateway  │ ──────────────────> │ Runtime │
│          │ <──────── OK 200 ────────────── │          │ <── { activated } ── │         │
└──────────┘                                  └──────────┘                     └─────────┘
```

**两层操作**：
1. **create_session**（已有）：新建 session + 调用 `switch_conversation()` 激活
2. **activate_session**（新增）：resume 已有 session + 调用 `switch_conversation()` 切换

## 实现细节

### 1. AgentLoop.switch_conversation() （core/rollball-runtime/src/agent/loop_.rs）

```rust
pub fn switch_conversation(&mut self, new_session: ConversationSession)
    -> Option<ConversationSession>
{
    let old_session = self.conversation.take();  // 取出旧 session
    self.conversation = Some(new_session);        // 安装新 session

    // 异步关闭旧 session + 触发蒸馏（best-effort）
    if let Some(old) = old_session {
        tokio::spawn(async move {
            EpisodeDistiller::distill_on_session_end(...).await;
            // writer 在 ConversationSession drop 时 shutdown
        });
    }
    old_session  // 返回旧 session（调用方可选择是否等待）
}
```

**关键设计决策**：
- `switch_conversation` 是 **sync 方法**（非 async），因为 AgentLoop.run() 的主循环是 sync context
- 旧 session 的 close+distill 通过 `tokio::spawn` 异步执行，不阻塞主循环
- `close_session_with_distillation()` 重构为委托给 `close_session_inner()`

### 2. handle_create_session 修复 （cli.rs）

```diff
- async fn handle_create_session(work_dir, agent_id, current_session_id, grpc_client, params)
+ async fn handle_create_session(work_dir, agent_id, agent_loop, current_session_id, grpc_client, params)

      Ok(new_session) => {
+         agent_loop.switch_conversation(new_session);  // P0 FIX: 激活新 session
          *current_session_id = new_session_id.clone();
      }
```

### 3. activate_session 全链路新增

| 层 | 改动 |
|----|------|
| **Gateway HTTP** | `POST /api/agents/{id}/sessions/{session_id}/activate` 路由 |
| **Gateway handler** | `activate_session()` → forward_session_query("activate_session", ...) |
| **Runtime IPC** | `handle_activate_session()` → `ConversationSession::resume()` → `agent_loop.switch_conversation()` |
| **Frontend** | `switchSession(sessionId, agentId)` 从 sync void 改为 async，调用 activate API |

### 4. 前端 switchSession 改造

```typescript
// BEFORE (sync, no backend notification)
switchSession: (sessionId: string) => {
  set({ currentSessionId: sessionId });
}

// AFTER (async, notifies Runtime)
switchSession: async (sessionId: string, agentId?: string) => {
  await fetch(`/api/agents/${agentId}/sessions/${sessionId}/activate`, { method: 'POST' });
  set({ currentSessionId: sessionId });
}
```

**容错设计**：activate API 调用失败时仍更新本地状态（best-effort），避免 UI 卡死。

## 影响范围

### 修改的文件
| 文件 | 改动类型 |
|------|----------|
| `core/rollball-runtime/src/agent/loop_.rs` | 新增 `switch_conversation()` + `close_session_inner()` |
| `core/rollball-runtime/src/cli.rs` | 修改 `handle_create_session()` 签名+实现；新增 `handle_activate_session()` |
| `core/rollball-gateway/src/http/chat.rs` | 新增 `activate_session` 路由+handler |
| `apps/.../stores/sessionStore.ts` | `switchSession` 改为 async + 调用 activate API |
| `apps/.../components/chat/ChatPanel.tsx` | 更新 `switchSession` 调用签名 |
| `apps/.../components/chat/SessionPanel.tsx` | `handleSwitchSession` 改为 async |

### 未改动但需注意
- **WebSocket 消息路径**：WS 事件不含 session_id，依赖 Runtime 当前活跃 session（这正是 switch_conversation 解决的问题）
- **HistoryManager**：独立于 conversation session，不受影响

## 验证清单

- [x] `cargo check -p rollball-runtime` 编译通过
- [x] `cargo check -p rollball-gateway` 编译通过
- [x] `npx tsc --noEmit` 前端零错误
- [ ] 端到端测试：新建 session 后发送消息，验证 JSONL 写入正确的 session 文件
- [ ] 端到端测试：切换已有 session 后发送消息，验证消息不串入其他 session
- [ ] 边界测试：activate 不存在的 session_id 返回错误
- [ ] 并发测试：快速连续 create → activate → create 不 panic

## 与之前前端修复的关系

| Phase | 修什么 | 层 | 状态 |
|-------|--------|-----|------|
| Phase 0（已完成）| title 更新、fetchSessions 闪烁、残留消息清理 | Frontend only | ✅ |
| **Phase 1.1–1.4**| **switch_conversation 协议、P0 根因修复** | **Full-stack** | ✅ |
| **Phase 1.5**| loadSequence 守卫 + AbortController 竞态防护 | Frontend | ✅ |
| **Phase 2**| Per-session message cache (5min TTL) | Frontend | ✅ |
| **Phase 3**| Title 持久化到后端 (PUT /sessions/{id}/title) | Full-stack | ✅ |

所有 Phase 全部完成。

### Phase 1.5 实现细节

- `loadSequence`: 每次请求递增，响应到达时检查是否匹配当前值，不匹配则丢弃
- `AbortController`: 新请求自动 abort 旧请求，`AbortError` 静默处理
- `abortSessionLoad()`: 公开方法，`switchSession` 时调用确保取消旧加载
- 三重守卫：AbortController → loadSequence → sequence check after await

### Phase 2 实现细节

- 缓存结构：`Record<sessionId, { messages, cursor, hasMore, loadedAt }>`
- TTL: 5 分钟，过期后重新请求 API
- 仅缓存初始加载结果（分页加载不缓存）
- `loadSessionMessages` 先查缓存 → 命中且未过期直接返回 → 未命中/过期走 API

### Phase 3 实现细节

- Runtime: 新增 `ConversationSession::update_title_force()` — 强制覆盖 title
- Runtime: 新增 `AgentLoop::update_session_title()` — 公开便捷方法
- Runtime: 新增 `handle_update_session_title()` IPC handler
- Gateway: 新增 `PUT /api/agents/{id}/sessions/{session_id}/title` 路由
- Frontend: `sendMessage` 和 `done` event 时调用 PUT title API（best-effort）

Phase 0 的前端修复仍然有效且必要——它们解决的是 UI 层面的问题。本次修复解决的是 Runtime 消息路由的根本问题。两者互补。
