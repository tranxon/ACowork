# Session 管理架构 Review

> 审查时间: 2026-05-20
> 范围: Runtime 层 session 管理的状态封装、事件传播、耦合度
> 结论: **3 个 P1 问题 + 2 个 P2 问题，建议渐进式重构**

---

## 一、问题总览

| # | 严重度 | 问题 | 影响范围 |
|---|--------|------|----------|
| P1-1 | 高 | `current_session_id` 散落在 cli.rs，未收归 SessionManager | cli.rs:1180 + 6处修改点 |
| P1-2 | 高 | Session 操作逻辑内联+外联双份，`#[allow(dead_code)]` | cli.rs:1440-1592 vs 2380-2670 |
| P1-3 | 高 | ChunkEvent session_id 通过 watch relay 事后注入，存在竞态 | cli.rs:660-900 |
| P2-1 | 中 | `session_id_watch_tx` 穿透 5 层函数参数 | run_gateway_loop → process_gateway_recv → handle_* |
| P2-2 | 中 | Chunk relay 9 个 match arm 复制粘贴 `params["session_id"]` | cli.rs:672-900 |

---

## 二、详细分析

### P1-1: `current_session_id` 归属错位

**现状**: `current_session_id` 是 `async_main()` 中的 `let mut current_session_id` (cli.rs:1180)，以 `&mut String` 传递给 `run_gateway_loop` 和 `process_gateway_recv`。

**问题**:
- SessionManager 内部已经有 `sessions: HashMap<String, SessionHandle>`，但没有 `current_session_id` 字段
- "哪个 session 是当前活跃 session" 这个语义应该由 SessionManager 管理，不该散落在调用方
- cli.rs 的 `process_gateway_recv` 每次需要 `current_session_id` 时，都是从外部 `&mut String` 读取/修改，绕过了 SessionManager

**后果**:
- SessionManager 对"当前 session"无感知，无法提供 `current_session()` / `activate()` 等方法
- 如果未来有第二个调用方（如 gRPC 直连模式），需要重复实现 current 跟踪
- `get_current_session_id` 的 handler 还需要从外部取值再返回（cli.rs:2437-2457）

**建议**: SessionManager 新增字段和方法：
```rust
pub struct SessionManager {
    sessions: HashMap<String, SessionHandle>,
    current_session_id: String,                          // ← 新增
    session_id_watch_tx: watch::Sender<String>,          // ← 新增（从 cli.rs 移入）
    // ...
}

impl SessionManager {
    pub fn current_session_id(&self) -> &str { &self.current_session_id }
    pub async fn activate_session(&mut self, session_id: &str, work_dir: &Path) -> Result<serde_json::Value> {
        // 1. resume ConversationSession
        // 2. 更新 self.current_session_id
        // 3. 通知 self.session_id_watch_tx
        // 4. 返回 response JSON
    }
}
```

---

### P1-2: Session 操作逻辑内联+外联双份

**现状**:
- **内联版**: cli.rs:1440-1592，在 `process_gateway_recv` 的 if-chain 中直接实现 create/activate/delete
- **外联版**: cli.rs:2380-2670，独立的 `handle_create_session` / `handle_activate_session` / `handle_delete_session` 函数
- 外联版全部标记 `#[allow(dead_code)]`，注释 "Reserved for future refactoring"

**问题**:
- 两份代码做**完全相同的事**，但实现略有差异（内联版走多 session 模式用 `session_manager.create_session_with_id_and_conversation`，外联版走单 session 模式用 `agent_loop.switch_conversation`）
- 之前修 session_id 传递 bug 时，两处都需要同步修改，容易遗漏
- 注释说"Reserved for future refactoring"但已经存在很久了，本身就是技术债

**具体差异**:

| 操作 | 内联版 (行号) | 外联版 (行号) | 差异 |
|------|--------------|--------------|------|
| create | 1440-1470 | 2381-2432 | 内联走 SessionManager，外联走 agent_loop.switch_conversation |
| activate | 1476-1497 | 2476-2533 | 内联仅更新 current + watch，外联调用 switch_conversation |
| delete | 1526-1592 | 2600-2670 | 内联走 SessionManager.destroy，外联走 agent_loop |

**建议**: 删除外联版，将内联版逻辑收归 SessionManager（见 P1-1 建议），cli.rs 只做 `session_manager.xxx()` 调用 + `send_session_response()`。

---

### P1-3: ChunkEvent session_id 事后注入存在竞态

**现状**: ChunkEvent 枚举中只有 `ToolApprovalNeeded` 自带 `session_id: Option<String>`（loop_.rs:126），其余变体（Delta, Done, Error 等）**不携带** session_id。session_id 是在 chunk relay task 中通过 `session_id_rx.borrow()` 读取后注入 params：

```rust
// cli.rs:669
let relay_session_id = session_id_rx.borrow().clone();
// 然后在每个 match arm 中:
params["session_id"] = serde_json::json!(relay_session_id);
```

**竞态场景**:
1. Session A 产生 `ChunkEvent::Delta("hello")`
2. 事件进入 mpsc channel，等待 relay 处理
3. 在 relay 处理该事件**之前**，用户切换到 Session B
4. `session_id_watch_tx.send(session_b_id)` 执行
5. relay 处理 Session A 的 Delta 时，`session_id_rx.borrow()` 读到的是 session_b_id
6. **结果**: Session A 的内容被标记为 Session B 发出

**为什么目前没爆**: 事件在 mpsc channel 中排队很快（微秒级），而用户切换 session 间隔通常在秒级。但在高负载或慢 relay 场景下，这个窗口确实存在。

**建议**: 在 `ChunkEvent` 顶层加 `session_id: String` 字段，在 `SessionTask` 产生事件时就注入：

```rust
pub enum ChunkEvent {
    ReasoningStarted { session_id: String },
    Delta { content: String, session_id: String },
    Done { content: String, message_id: String, session_id: String },
    // ...所有变体
}
```

或者更优雅地，用 struct 包装：
```rust
pub struct SessionChunkEvent {
    pub session_id: String,
    pub event: ChunkEvent,  // 内部事件不带 session_id
}
```

relay 只需要从 event 中读取 session_id，不再需要 watch channel。

---

### P2-1: `session_id_watch_tx` 穿透 5 层参数

**现状**: watch::Sender 从 `async_main` 创建后，需要传递给：
- `run_gateway_loop` (cli.rs:1172)
- `process_gateway_recv` (cli.rs:1293)
- `handle_create_session` (cli.rs:2388)
- `handle_activate_session` (cli.rs:2482)
- `handle_delete_session` (cli.rs:2607)

每次都要加 `session_id_watch_tx: &tokio::sync::watch::Sender<String>` 参数。

**建议**: 如果 P1-1 和 P1-3 都解决了（session_id 归 SessionManager + ChunkEvent 自带 session_id），watch channel 可以**完全移除**：
- SessionManager 持有 current_session_id，内部自行维护一致性
- ChunkEvent 携带 session_id，relay 不再需要外部注入
- watch channel 的存在仅因为"cli.rs 需要知道 current session 且 relay 需要注入"，两个需求消失后 channel 即可删除

---

### P2-2: Chunk relay 9 个 match arm 复制粘贴

**现状**: cli.rs:672-900，每个 ChunkEvent 变体都有 ~20 行的 gRPC 消息构造代码，结构几乎相同：
```rust
let mut params = serde_json::json!({...});
params["session_id"] = serde_json::json!(relay_session_id);  // ← 每处重复
let msg = rollball_core::proto::ClientMessage { ... };
if outbound_tx.send(msg).await.is_err() { ... }
```

**建议**: 提取公共方法 `relay_event(action, params, outbound_tx)` 或直接在 ChunkEvent 上实现 `into_stream_chunk()` 方法，把 gRPC 消息构造逻辑内聚。如果 P1-3 解决了，session_id 从 event 直接取，公共方法更自然：

```rust
async fn relay_chunk(event: SessionChunkEvent, tx: &Sender<ClientMessage>) {
    let (action, mut params) = event.event.into_action_params();
    params["session_id"] = serde_json::json!(event.session_id);
    // ... 统一发送
}
```

---

## 三、已有良好封装的部分

| 模块 | 文件 | 评价 |
|------|------|------|
| SessionManager | session_manager.rs | ✅ 高内聚：sessions HashMap + create/destroy/send/broadcast + 缓存重放 |
| SessionHandle | session_handle.rs | ✅ 简洁：3 个方法 send/send_inbound/is_alive |
| SessionTask | session_task.rs | ✅ 独立执行：own AgentLoop + session_id + chunk_tx |
| SessionMessage | session_task.rs | ✅ 消息协议清晰：12 种消息类型 |
| ConversationSession | conversation.rs | ✅ JSONL 持久化：new/resume/session_id |
| Gateway IPC session_id 保留 | ipc/server.rs | ✅ 三处 if let 保留 session_id |

**关键洞察**: SessionManager 的内部封装已经很好（770 行，清晰的 create/destroy/broadcast），但 **cli.rs 作为"胶水层"承担了太多 session 状态管理职责**，导致状态散落和重复。

---

## 四、重构路线（渐进式）

### Phase A: ChunkEvent 自带 session_id (P1-3 → P2-1 → P2-2)

1. ChunkEvent 加 session_id 字段（或用 SessionChunkEvent 包装）
2. SessionTask 在 emit 事件时注入自身 session_id
3. Chunk relay 简化为读 event.session_id，移除 watch channel
4. 提取公共 relay 方法消除 9 个 match arm 复制粘贴
5. 删除 `session_id_watch_tx/rx` 全部代码

**收益**: 消除竞态，删除 ~30 行参数传递，relay 代码从 ~240 行降到 ~40 行

### Phase B: Session 状态收归 SessionManager (P1-1 + P1-2)

1. SessionManager 新增 `current_session_id` + `activate_session()` + `create_session_for_gateway()` + `delete_session_for_gateway()`
2. cli.rs 内联版改为调用 SessionManager 方法
3. 删除外联版 `handle_create_session` / `handle_activate_session` / `handle_delete_session`
4. 删除 `current_session_id` 局部变量和 `&mut String` 参数传递

**收益**: 消除双份代码，cli.rs 减少 ~400 行，SessionManager 成为 session 状态唯一权威

### Phase C: 清理遗留

1. 删除 AgentLoop::switch_conversation（多 session 模式下不需要，仅 CLI 独立模式使用）
2. 或者明确标注两条路径：CLI 单 session vs Gateway 多 session

---

## 五、风险与约束

| 风险 | 缓解 |
|------|------|
| Phase A 修改 ChunkEvent 枚举，所有 match 都要改 | 编译器强制穷举，不会遗漏 |
| Phase B SessionManager 需要 async 方法 | 已经是 async（create_session_with_id_and_conversation 等），无新增难度 |
| Phase A+B 一起做改动量大 | **强烈建议分两批**，每批独立可测试 |
| 删除 watch channel 后 CLI 单 session 模式如何获取 current | SessionManager.current_session_id() 即可 |

---

## 六、结论

Session 管理的核心问题不是"功能缺失"，而是**封装边界不清晰**：

- SessionManager 是"session 仓库"（create/destroy/store）但不是"session 状态权威"（不知道哪个是 current）
- cli.rs 既是路由层又是 session 状态管理者，职责过载
- ChunkEvent 的 session_id 通过外部注入而非自带，违反了"数据随事件走"的原则

三步重构后：
- **SessionManager** = 唯一 session 状态权威
- **ChunkEvent** = 自带 session_id，不需要外部注入
- **cli.rs** = 纯路由层，不含 session 状态
