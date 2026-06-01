# Code Review #13: Memory API gRPC Proxy + Grafeo WAL Recovery + 诊断日志

**Commit**: `89c0f8f` — feat: Memory API gRPC proxy + Grafeo WAL recovery + diagnostic logging
**Author**: nicholasyu
**Date**: 2026-05-10
**Scope**: 33 files, +1874 / -823 lines
**Reviewer**: EngineeringSeniorDeveloper

---

## 1. 变更概览

本次提交实现了三大核心改进，打通了完整的 Desktop → Gateway → Runtime → Grafeo 记忆查询链路：

| 改进领域 | 描述 | 影响范围 |
|----------|------|----------|
| **Memory API gRPC 代理** | Gateway HTTP Memory API 不再直接访问 `MemoryStore`，改为通过 gRPC 转发查询到 Runtime，由 Runtime 直接操作 GrafeoStore | Gateway `http/memory_api.rs`, `grpc/server.rs`, `grpc/dispatch.rs`; Runtime `grpc/client.rs`, `cli.rs`, `agent/loop_.rs`, `agent/agent_core.rs`, `agent/context.rs`, `agent/session_state.rs` |
| **Grafeo WAL 恢复** | 在 `GrafeoStore::open_with_config()` 中调用 `db.wal_checkpoint()` 恢复崩溃后未提交的 WAL 数据 | `rollball-grafeo/src/grafeo.rs` |
| **诊断日志 + Windows CRLF** | 在关键路径添加 `tracing::info!` 诊断日志；为 Windows 终端添加 CrlfWriter | Gateway `cli.rs`, `http/memory_api.rs`; Runtime `cli.rs` |

### 1.1 架构变更：从 Gateway 直连到 gRPC 代理

**Before（旧架构）**：
```
Desktop App → Gateway HTTP API → Gateway.memory_store (直连 Grafeo) → 返回结果
```
- Gateway 进程内持有 `MemoryStore` 实例
- Memory 数据存在 Gateway 进程空间中
- 与 Runtime 的 Grafeo 实例是**不同的数据库连接**，数据可能不一致

**After（新架构）**：
```
Desktop App → Gateway HTTP API → gRPC MemoryNodesQuery → Runtime GrafeoStore → gRPC MemoryNodesResult → Gateway HTTP Response
```
- Gateway 不再持有 `memory_store`
- 所有 Memory 查询通过 gRPC 转发到 Runtime 进程
- Runtime 是 Grafeo 数据的唯一权威来源（Single Source of Truth）

---

## 2. 逐模块详细 Review

### 2.1 Proto 定义 (`core/rollball-core/proto/gateway_ipc.proto`)

**变更内容**：新增 4 组 request/response 消息对

```
ServerMessage (Gateway → Runtime):
  MemoryNodesQuery      (field 27) — 分页查询节点
  MemoryStatsQuery     (field 28) — 统计信息
  MemoryConsolidateQuery (field 29) — 触发整合
  MemoryDeleteQuery    (field 30) — 删除节点

ClientMessage (Runtime → Gateway):
  MemoryNodesResult      (field 20)
  MemoryStatsResult      (field 21)
  MemoryConsolidateResult (field 22)
  MemoryDeleteResult     (field 23)
```

**评价**：

| 项目 | 评价 |
|------|------|
| 字段编号 | ✅ 19→20/21/22/23 (ClientMessage), 25→27/28/29/30 (ServerMessage) 跳跃编号合理，为未来扩展留空间 |
| 消息结构 | ✅ `MemoryNodesQuery` 包含分页、类型过滤、关键词搜索、时间范围 |
| `MemoryNodeEntry` | ✅ 字段完整：node_id, node_type, content, confidence, decay_score, created_at, last_accessed_at, access_count, status |
| `MemoryStatsResult` | ✅ 使用 `map<string, uint64>` 表达 by_type/by_status，兼容动态类型 |

**⚠️ 问题**：

1. **`MemoryNodeEntry.node_id` 使用 `uint64`**，但 Grafeo 的 `NodeId` 是 `pub struct NodeId(pub u64)` 的 newtype。Proto 使用裸 `uint64` 可以工作，但缺少类型安全性。当前可接受，但未来如果有多个 ID 类型共用 proto 可能引起混淆。

2. **`MemoryNodesQuery.time_range` 是 `string` 类型**，没有结构化定义（如 `{start: int64, end: int64}`）。当前 Runtime 端的 `handle_memory_nodes_query` 实际上**没有使用 time_range 字段做过滤**，这个字段被静默忽略了。建议：
   - 短期：在 proto 注释中标注 "reserved, not yet implemented"
   - 长期：改为 `int64 time_range_start = 6; int64 time_range_end = 7;` 结构化时间范围

3. **`MemoryConsolidateQuery.retention_days` 是 `uint32`**，但 0 值语义不清（"不限制" 还是 "保留0天即全部清理"？）。当前 Runtime 实现中 `retention_days == 0` 被当作 "不限制" 处理，但 proto 层面缺少文档说明。

### 2.2 Gateway gRPC Session Manager (`core/rollball-gateway/src/grpc/server.rs`)

**核心新增**：`GrpcSessionManager` 增加了 request-response 协调能力

```rust
pub struct GrpcSessionManager {
    sessions: HashMap<String, GrpcSession>,
    pending_requests: HashMap<u64, oneshot::Sender<proto::ClientMessage>>,  // 新增
    next_request_id: AtomicU64,                                               // 新增
}
```

**关键方法**：

| 方法 | 功能 | 评价 |
|------|------|------|
| `send_memory_request()` | 分配 request_id，注册 pending，通过 try_push_request 发送查询 | ✅ 正确的 request-response 模式 |
| `register_pending_request()` | 创建 oneshot channel 并注册 | ✅ |
| `fulfill_pending()` | 根据 request_id 匹配并完成 pending 请求 | ✅ |
| `cleanup_pending()` | 超时后清理 pending 请求 | ✅ |
| `try_push_request()` | 同步版 push，用于 sync Mutex 上下文 | ✅ 避免 deadlock |

**🔒 Deadlock 防护**：这是本次提交最精妙的设计之一。

```
HTTP handler:
  1. lock(grpc_mgr)  →  send_memory_request()  →  get (request_id, rx)
  2. unlock(grpc_mgr)   ← 关键：在 await 之前释放
  3. timeout(30s).await(rx)

Inbound handler:
  1. lock(grpc_mgr)  →  fulfill_pending(request_id, msg)
  2. unlock(grpc_mgr)
```

如果 HTTP handler 持有 lock 等待 rx，而 inbound handler 需要 lock 来 fulfill，就会死锁。代码中通过 **"lock only for push, then wait without lock"** 的模式正确解决了这个问题。

**⚠️ 问题**：

4. **`send_memory_request()` 中 `find_by_agent_id()` 返回不可变引用，但 `try_push_request()` 需要 `&self`**。当前实现是先 get 不可变引用再调用方法，这是正确的。但如果未来需要修改 session 状态，需要注意借用规则。

5. **`cleanup_pending()` 只在超时路径调用**。如果 Runtime 崩溃导致 gRPC 断开，pending_requests 中的条目会泄漏。虽然 gRPC 断开时 session 会被移除，但 pending_requests 不会被清理。建议在 session 移除时也清理该 session 关联的所有 pending requests。

### 2.3 Gateway HTTP Memory API (`core/rollball-gateway/src/http/memory_api.rs`)

**核心变更**：所有 4 个 HTTP handler（list_memory_nodes, get_memory_stats, delete_memory_node, trigger_consolidate）从直接访问 `GatewayState.memory_store` 改为 gRPC 代理。

**变更统计**：+494 / -823 行 — 净减少 329 行，大幅简化。

**删除的代码**：
- `build_memory_filters()` 辅助函数及对应的单元测试（~40行）
- 直接访问 `MemoryStore` 的所有分支逻辑
- `GatewayState.memory_store` 字段

**新的统一模式**（4个 handler 完全一致）：
```rust
// 1. 验证 agent 存在
// 2. lock(grpc_mgr) → send_memory_request() → (request_id, rx)
// 3. unlock → timeout(30s).await(rx)
// 4. 匹配 response payload → 转换为 HTTP response
// 5. 超时 → cleanup_pending → 返回空/错误
```

**评价**：

| 项目 | 评价 |
|------|------|
| 一致性 | ✅ 4个 handler 完全遵循相同模式，可维护性好 |
| 错误处理 | ✅ 超时 30s、sender dropped、unexpected payload type 都有处理 |
| 诊断日志 | ✅ 入口/出口都有 tracing::info! |
| Lock 安全 | ✅ 每处都正确在 await 前释放 lock |

**⚠️ 问题**：

6. **重复代码**：4 个 handler 的 "lock → send → unlock → timeout → match" 模式重复了 4 次。建议提取为泛型辅助函数：
   ```rust
   async fn grpc_memory_query<R>(
       grpc_mgr: &SharedGrpcSessionMgr,
       agent_id: &str,
       payload: proto::server_message::Payload,
   ) -> Result<R, ApiError>
   ```

7. **`trigger_consolidate` 中 `body.force` 和 `body.retention_days` 都 `unwrap_or` 默认值**。但 `ConsolidateRequest` 的 `force` 和 `retention_days` 都是 `Option`。如果客户端不传，force 默认 false 合理，retention_days 默认 0 需要确认语义。

8. **`delete_memory_node` 在没有 gRPC 连接时返回 503**，而 `list_memory_nodes` 和 `get_memory_stats` 返回空数据。行为不一致。建议统一为：列表/统计类操作返回空（幂等），修改类操作返回 503（正确）。

### 2.4 Runtime gRPC Client (`core/rollball-runtime/src/grpc/client.rs`)

**核心变更**：在 inbound loop 中拦截 Memory API query，转发到 `memory_query_tx` channel。

```rust
// 新增字段
memory_query_rx: Option<mpsc::UnboundedReceiver<(u64, proto::server_message::Payload)>>

// Inbound loop 中新增：
if is_memory_query_payload(&msg) {
    memory_query_tx.send((msg.request_id, payload));
    continue;  // 不进入 push channel
}
```

**评价**：

| 项目 | 评价 |
|------|------|
| 设计模式 | ✅ 使用独立 channel 解耦 memory query 和 push message，避免类型混合 |
| `take_memory_query_rx()` | ✅ 使用 Option + take 避免多个 owner 竞争，解决 `tokio::select!` 的 `&mut self` 冲突 |
| `is_memory_query_payload()` | ✅ 显式列举 4 种 memory query 类型 |

**⚠️ 问题**：

9. **`memory_query_tx` 是 unbounded channel**。如果 Gateway 在短时间内发送大量 memory query（例如 UI 频繁刷新），不会有背压控制。当前场景（Desktop App 人工触发）风险较低，但如果未来有自动轮询需求需要改为 bounded channel。

10. **`is_memory_query_payload()` 使用 `matches!` 宏枚举 4 种类型**。如果 proto 新增 memory query 类型但忘记更新这个函数，消息会被静默丢弃到 `push_rx`。建议添加注释提醒保持同步，或使用 oneof + descriptor 检查。

### 2.5 Runtime Gateway Loop 重构 (`core/rollball-runtime/src/cli.rs`)

**核心变更**：

1. **`run_gateway_loop` 从单一 `match` 重构为 `tokio::select!`** — 同时监听 Gateway 消息和 Memory query channel
2. **提取 `process_gateway_recv()` 函数** — 返回 `LoopAction` 枚举控制循环
3. **新增 `spawn_memory_query_handler()`** — 异步处理 Memory query，不阻塞主循环
4. **新增 4 个 `handle_memory_*_query()` 函数** — 在 Runtime 端直接操作 GrafeoStore

**`spawn_memory_query_handler` 设计评价**：

```rust
tokio::spawn(spawn_memory_query_handler(
    store_opt,      // Arc<GrafeoStore> 克隆
    outbound,       // mpsc::Sender 克隆
    request_id,     // u64 copy
    payload,        // proto payload move
));
```

✅ 正确使用 `tokio::spawn` 避免阻塞 `select!` 循环
✅ 克隆 Arc 和 Sender 的开销极小
✅ 每个 query 在独立 task 中执行，互不阻塞

**`handle_memory_nodes_query` 详细 Review**：

```rust
fn handle_memory_nodes_query(
    memory_store: Option<&Arc<GrafeoStore>>,
    query: proto::MemoryNodesQuery,
) -> proto::client_message::Payload
```

**实现逻辑**：
1. 遍历 4 种 label（Episodic, Knowledge, Procedural, Autobiographical）
2. 通过 `graph.nodes_by_label(label)` 获取每个 label 下的 node ID 列表
3. 对每个 node 调用 `store.db().get_node(id)` 获取完整节点
4. `extract_node_content()` 根据 label 类型提取不同格式的 content
5. 应用 keyword 过滤（大小写不敏感的 `contains`）
6. 手动分页：skip + take

**⚠️ 问题**：

11. **🔴 性能问题：全量扫描 + 内存分页**。`handle_memory_nodes_query` 先收集所有匹配节点到 `Vec<MemoryNodeEntry>`，然后 `skip(start).take(size)` 做分页。当 Grafeo 中有数万个节点时，每次查询都会全量扫描。建议：
    - 短期：添加节点数上限保护（如超过 10000 节点时拒绝无过滤查询）
    - 长期：利用 Grafeo 的 graph index 做分页查询，或维护一个独立的分页索引

12. **`time_range` 过滤未实现**。proto 定义了 `time_range` 字段，`MemoryNodesQuery` 传递了这个字段，但 `handle_memory_nodes_query` 完全忽略了它。建议至少添加一个 `tracing::warn!` 提示未实现。

13. **Keyword 过滤使用 `to_lowercase().contains()`**，这是朴素的子串匹配，不是 BM25 语义搜索。对于 Desktop App 的简单搜索足够，但应该有注释说明这是临时实现。

14. **`extract_node_content()` 对每种 label 有不同的字段组合**。这些字段名（role, content, subject, predicate, object, name, action_pattern, key, value）需要与 `MemoryManager.record()` 写入的字段保持一致。目前没有类型层面的保证，如果一边改了字段名另一边不知道就会产生空内容。

15. **`handle_memory_stats_query` 中 `storage_bytes` 和 `avg_decay_score` 硬编码为 0**。proto 中定义了这些字段但无法获取，应该至少在注释中标注 TODO。

### 2.6 Agent Loop 记忆链路 (`core/rollball-runtime/src/agent/loop_.rs`)

**核心新增**：3 个方法 + 1 个辅助函数

| 方法 | 功能 | 行数 |
|------|------|------|
| `retrieve_and_inject_memories()` | 每轮 LLM 调用前检索长期记忆并注入 ContextBuilder | ~50 |
| `record_turn_to_memory()` | 每轮 LLM 返回文本后记录对话轮次 | ~30 |
| `write_distilled_to_grafeo()` | 提取的公共辅助函数，用于 3 处蒸馏写入 | ~15 |

**`run()` 方法签名变更**：
```rust
// Before
pub async fn run(&mut self, user_message: &str, context_builder: &ContextBuilder) -> Result<String>
// After
pub async fn run(&mut self, user_message: &str, context_builder: &mut ContextBuilder) -> Result<String>
```

**记忆生命周期**：
```
1. run() 入口:
   → retrieve_and_inject_memories(user_message, context_builder)
     → clear_retrieved_memory()        // P0: 防止旧记忆泄漏
     → MemoryManager.retrieve()        // Grafeo BM25 搜索
     → MemoryManager.inject()          // 格式化注入
     → context_builder.set_retrieved_memory()
     → 返回 retrieved_memory_ids       // P2-4: 可追溯性

2. run() 退出（文本响应）:
   → record_turn_to_memory(user_message, response, turn_index, retrieved_memory_ids)
     → MemoryManager.record()          // 写入 Episodic 节点

3. 历史裁剪时:
   → write_distilled_to_grafeo()       // P2-1: 蒸馏写入
   → 同样用于 switch-session 和 session-close
```

**评价**：

| 项目 | 评价 |
|------|------|
| P0 fix: clear_retrieved_memory | ✅ 关键修复 — ContextBuilder 在 SessionTask 循环中复用，不清除会导致旧记忆污染 |
| P1-2 fix: turn_counter | ✅ 使用单调递增计数器而非消息索引，避免 parallel tool call 导致的索引跳跃 |
| P2-4 fix: retrieved_memory_ids | ✅ 追溯哪些记忆影响了当前回复，未来可用于记忆评估和衰减 |
| P2-1 fix: write_distilled_to_grafeo | ✅ 消除 3 处重复代码，统一蒸馏写入路径 |
| `run()` 签名改为 `&mut ContextBuilder` | ✅ 合理 — 需要修改 retrieved_memory |

**⚠️ 问题**：

16. **`retrieve_and_inject_memories` 在每次 `run()` 调用时执行**，但在 multi-iteration loop（工具调用循环）中，第二次迭代不会重新检索记忆。这是因为 `run()` 的外层 `loop` 中，`retrieve_and_inject_memories` 只在第一次迭代前执行。如果工具调用的结果影响了记忆相关性，可能需要重新检索。不过当前设计是合理的（避免每轮重检的性能开销），可以后续优化。

17. **`record_turn_to_memory` 在文本响应时记录，但工具调用结果不记录**。如果一个 agent 做了 3 轮工具调用后返回文本，中间的工具调用过程不会被记录到 Grafeo。这是否符合预期？如果用户问 "上次查天气是什么结果"，Grafeo 中不会有工具调用的中间结果。

18. **`retrieved_memory_ids` 使用 `Vec<String>`**，但 Grafeo 的 NodeId 是 `u64`。转换为 String 有开销且丢失类型信息。建议改为 `Vec<u64>` 并在 proto 层面也使用 `repeated uint64`。

### 2.7 AgentCore 记忆存储 (`core/rollball-runtime/src/agent/agent_core.rs`)

**核心变更**：

```rust
pub struct AgentCore {
    // ... 已有字段
    pub(crate) memory_store: Option<Arc<GrafeoStore>>,  // 新增
}
```

新增方法：
- `init_memory_store(&mut self, work_dir: &Path)` — 初始化 Grafeo
- `memory_store() -> Option<&Arc<GrafeoStore>>` — 访问
- `init_memory_manager() -> MemoryManager` — 创建无状态编排器

**评价**：

| 项目 | 评价 |
|------|------|
| 优雅降级 | ✅ 初始化失败时 memory_store 为 None，所有依赖它的功能静默跳过 |
| Arc 共享 | ✅ `clone_for_session()` 中 Arc clone，多 session 共享同一 GrafeoStore |
| 无状态 MemoryManager | ✅ 每次调用 `init_memory_manager()` 创建新实例，无状态可安全共享 |

**⚠️ 问题**：

19. **`init_memory_store()` 在两个地方被调用**：
    - `async_main()` 中 gRPC 模式：`c.init_memory_store(work_dir_path)` — 在 `AgentCore` 创建后
    - `async_main()` 中 standalone 模式：`agent_loop.init_memory_store(work_dir_path)` — 在 `AgentLoop` 创建后
    
    两次调用的时机不同，但都通过 `AgentCore` 的可变引用执行。没有防重入检查（第二次调用会覆盖第一次的 store）。建议添加 `if self.memory_store.is_some() { return; }` 防护。

### 2.8 ContextBuilder (`core/rollball-runtime/src/agent/context.rs`)

**核心变更**：

```rust
pub struct ContextBuilder {
    // ... 已有字段
    retrieved_memory: Option<String>,  // 新增
}
```

新增方法：
- `set_retrieved_memory(&mut self, memory_text: String)`
- `clear_retrieved_memory(&mut self)` — P0 fix

`build()` 中的注入位置：
```rust
// 2.5 Retrieved memory context from Grafeo (long-term memory)
if let Some(ref memory) = self.retrieved_memory {
    system_content.push_str(&format!("\n\n## Relevant Memories\n{memory}"));
}
```

**评价**：

| 项目 | 评价 |
|------|------|
| 注入位置 | ✅ 在 system prompt 中 workspace_context 之后、platform info 之前 |
| 格式 | ✅ 使用 `## Relevant Memories` markdown 标题，LLM 易于理解 |
| P0 clear | ✅ 每轮 run() 开始时清除，防止跨轮污染 |

**⚠️ 问题**：

20. **记忆注入到 system prompt 而非独立 message**。当前实现将记忆文本拼接到 system_content 末尾。如果记忆很长，会占用大量 system prompt token 预算。建议：
    - 短期：添加 `max_inject_tokens` 限制（`MemoryManager.inject()` 已有此参数）
    - 长期：考虑将记忆作为独立的 system message（role=system），而非拼接到主 system prompt

21. **`build()` 中注释 `// 2.5 Autobiographical context` 被替换为 `// 2.5 Retrieved memory context`**。之前的 "Autobiographical context (Phase 1: skip, Phase 2: from Grafeo)" 注释暗示这是一个分阶段实现。现在直接替换为 Retrieved memory，丢失了 Phase 2 的原始规划注释。影响较小，但建议在新注释中说明 Grafeo 的 Retrieved memory 包含了 Autobiographical 类型。

### 2.9 Grafeo WAL Recovery (`core/rollball-grafeo/src/grafeo.rs`)

**变更**：3 行代码

```rust
// 在 init_schema() 之后
let _ = store.db.wal_checkpoint();
```

**评价**：

| 项目 | 评价 |
|------|------|
| 正确性 | ✅ WAL checkpoint 将未提交的 WAL 数据写入主数据库文件，确保重启后查询可见 |
| 错误处理 | ⚠️ 使用 `let _ =` 忽略 checkpoint 错误 |
| 位置 | ✅ 在 open_with_config() 中执行，确保每次打开数据库都恢复 |

**⚠️ 问题**：

22. **`let _ = store.db.wal_checkpoint()` 忽略了错误**。如果 checkpoint 失败（磁盘满、权限问题等），数据丢失不会被报告。虽然 open 本身可能也会失败，但 checkpoint 是独立的操作。建议至少添加 `tracing::warn!` 记录失败原因。

23. **只在 `open_with_config()` 中 checkpoint，`new_in_memory()` 没有**。对于 in-memory 数据库不需要 WAL recovery，所以这是正确的。但如果未来有人添加了其他 open 路径，可能遗漏。

### 2.10 Gateway 依赖清理 (`core/rollball-gateway/Cargo.toml`)

**变更**：从 `[dev-dependencies]` 中移除了 `rollball-grafeo`。

**评价**：✅ 正确。Gateway 不再直接访问 GrafeoStore，依赖移除是架构正确性的体现。

### 2.11 Windows CRLF (`core/rollball-gateway/src/cli.rs`, `core/rollball-runtime/src/cli.rs`)

**变更**：`CrlfWriter<W>` 包装器 + `CrlfStderr` MakeWriter 实现。

**评价**：

| 项目 | 评价 |
|------|------|
| `cfg!(not(windows))` gating | ✅ Unix 上完全透传，零开销 |
| 逐字节扫描 `\n` | ✅ 正确处理，缓冲区中的 `\n` 前插入 `\r` |
| `write()` 返回 `Ok(buf.len())` | ✅ 返回原始长度，符合 Write trait 语义 |
| compact 格式 | ✅ tracing compact 格式更适合终端输出 |

**⚠️ 问题**：

24. **CrlfWriter 的 `write()` 返回 `Ok(buf.len())`，但实际写入可能多于 `buf.len()` 个字节（因为插入了 `\r`）**。这在大多数 tracing 使用场景下没问题（tracing 不检查写入字节数），但严格来说不符合 `Write::write` 的语义约定。如果其他代码依赖写入字节数做 offset 计算，可能出问题。

25. **Runtime 和 Gateway 都有 CrlfWriter 实现，但代码重复**。两个 `cli.rs` 文件中各自实现了一遍相同的 `CrlfWriter` 和 `CrlfStderr`。建议提取到 `rollball-core` 的 shared utility 中。

### 2.12 Legacy 代码清理 (`core/rollball-gateway/src/http/chat.rs`)

**变更**：删除了 `get_conversations_legacy()` 和 `get_latest_conversation_legacy()` 两个函数（~230 行）。

**评价**：✅ 正确的架构清理。旧的 Grafeo 直连路径已经被 gRPC 代理替代，legacy 代码可以安全移除。

**⚠️ 问题**：

26. **删除 legacy 后，`get_conversations` 和 `get_latest_conversation` 在没有运行中的 agent 时返回空数据**。注释说 "No running agent with IPC session — return empty list"。这意味着用户在 Desktop App 中打开一个已停止的 agent，将无法看到历史对话列表。这可能不是期望的行为 — 用户应该能看到历史对话即使 agent 未运行。但这超出了本次提交的范围，是一个后续改进点。

### 2.13 其他小修复

| 文件 | 变更 | 评价 |
|------|------|------|
| `conversation.rs` | `metadata_end_offset` 修复 off-by-one：移除 `+ 1` | ✅ 正确修复 — `read_line()` 返回的字符串包含 `\n`，其长度就是到下一行开头的偏移量 |
| `platform.rs` | `find_git_bash` 使用 let-chain 简化嵌套 | ✅ Rust 1.72+ let-chain 特性，更简洁 |
| `Cargo.toml` (workspace) | `grafeo-engine` 添加 `default-features = false` + `regex` feature | ✅ 减少不需要的默认 feature 依赖 |
| `Cargo.toml` (runtime) | 新增 `grafeo-core` 依赖 | ✅ 用于 `extract_node_content` 中访问 `grafeo_core::graph::lpg::Node` |

---

## 3. 测试覆盖 Review

### 3.1 新增测试

| 测试 | 文件 | 覆盖范围 | 评价 |
|------|------|----------|------|
| `test_diagnostic_memory_write_read_loop` | `rag_integration.rs` | 3 轮对话 write→read，BM25 搜索，中文搜索，limit 搜索，无关搜索 | ✅ 覆盖全面，包含中文 BM25 诊断 |
| 已有测试适配 | `loop_.rs` (14 个) | `ContextBuilder` 从 `&` 改为 `&mut` | ✅ 全部适配，无遗漏 |

### 3.2 缺失的测试

| 缺失测试 | 优先级 | 说明 |
|----------|--------|------|
| Gateway gRPC proxy 端到端测试 | 🔴 高 | Desktop → Gateway → Runtime 的完整链路没有集成测试 |
| `GrpcSessionManager.pending_requests` 并发安全 | 🔴 高 | 多个 HTTP 请求并发发送 memory query 时的线程安全 |
| `send_memory_request` 的 agent 未连接场景 | 🟡 中 | 验证返回 None 的路径 |
| `fulfill_pending` 的 request_id 不匹配场景 | 🟡 中 | 验证返回 false 的路径 |
| `handle_memory_nodes_query` 的分页边界 | 🟡 中 | page=0, page>total, size=0, size=100+ |
| `handle_memory_delete_query` 的 node 不存在场景 | 🟢 低 | 返回 deleted=false 的路径 |
| `CrlfWriter` 单元测试 | 🟢 低 | 验证 \n → \r\n 转换正确性 |

---

## 4. 安全性 Review

| 项目 | 评价 |
|------|------|
| Agent 隔离 | ✅ Memory query 通过 agent_id 路由到对应 Runtime 的 GrafeoStore，不同 agent 的记忆隔离 |
| Lock 安全 | ✅ 所有 tokio::sync::Mutex 都在 .await 前释放 |
| 超时保护 | ✅ 30s 超时避免无限等待 |
| 资源泄漏 | ⚠️ pending_requests 在 Runtime 崩溃时可能泄漏（见问题 #5） |
| 输入验证 | ⚠️ MemoryNodesQuery 的 page/size 使用 `max(1)` 修正，但没有上限保护（见问题 #11） |

---

## 5. 性能 Review

| 项目 | 评价 | 影响 |
|------|------|------|
| Memory Nodes Query 全量扫描 | 🔴 高 | 每次查询遍历所有 label 下的所有节点 |
| `tokio::spawn` 异步处理 | ✅ 好 | Memory query 不阻塞主 Gateway message loop |
| `try_push_request` 同步发送 | ✅ 好 | 避免在 sync Mutex 中 await |
| oneshot channel | ✅ 好 | 比 mpsc 更轻量，适合 1:1 请求响应 |
| `to_lowercase()` keyword 过滤 | ⚠️ 中 | 每个节点创建一个新的 lowercase String |
| Arc clone 开销 | ✅ 好 | AgentCore clone_for_session 中 Arc clone 是 O(1) |

---

## 6. 总结

### 6.1 整体评价

**评分：8.5/10 — 优秀的架构重构，少量可改进点**

本次提交完成了一个重要的架构迁移：将 Memory API 从 Gateway 直连 Grafeo 改为通过 gRPC 代理到 Runtime。这是一次正确且必要的架构调整，使得 Runtime 成为 Grafeo 数据的唯一权威来源（Single Source of Truth），消除了 Gateway 和 Runtime 之间数据不一致的风险。

**关键优点**：
1. ✅ 架构方向正确 — Runtime 拥有 Grafeo，Gateway 通过 gRPC 代理
2. ✅ Deadlock 防护设计精妙 — "lock only for push, then wait without lock"
3. ✅ 优雅降级 — Memory store 初始化失败不影响 agent 运行
4. ✅ P0/P1-2/P2-1/P2-4 修复完整 — 清除旧记忆、turn counter、蒸馏统一、追溯性
5. ✅ WAL recovery 正确解决崩溃后数据不可见问题
6. ✅ 大幅代码简化 — Gateway 移除 329 行直接访问代码
7. ✅ Legacy 代码清理彻底

**关键改进点**：
1. 🔴 性能：全量扫描 + 内存分页在大规模数据下会成为瓶颈
2. 🟡 可靠性：pending_requests 在 Runtime 崩溃时可能泄漏
3. 🟡 代码质量：4 个 HTTP handler 中的重复代理模式可提取为泛型辅助
4. 🟡 功能完整性：time_range 过滤未实现，keyword 过滤是朴素子串匹配
5. 🟢 测试：缺少 gRPC proxy 端到端集成测试

### 6.2 行动项

| # | 优先级 | 行动项 | 建议时间 |
|---|--------|--------|----------|
| 1 | 🔴 P0 | 为 `handle_memory_nodes_query` 添加节点数上限保护（>10K 拒绝无过滤查询） | 下一个 PR |
| 2 | 🔴 P0 | 在 session 移除时清理 `pending_requests`，防止内存泄漏 | 下一个 PR |
| 3 | 🟡 P1 | 提取 gRPC proxy 重复模式为泛型辅助函数 | 1-2 天 |
| 4 | 🟡 P1 | 添加 Gateway→Runtime Memory API 端到端集成测试 | 2-3 天 |
| 5 | 🟡 P1 | `time_range` 过滤要么实现要么移除/标注 reserved | 1 天 |
| 6 | 🟡 P2 | CrlfWriter 提取到 rollball-core shared utility | 0.5 天 |
| 7 | 🟡 P2 | `init_memory_store()` 添加防重入检查 | 0.5 天 |
| 8 | 🟢 P3 | WAL checkpoint 错误添加 `tracing::warn!` | 0.5 天 |
| 9 | 🟢 P3 | `retrieved_memory_ids` 改为 `Vec<u64>` 保持类型一致性 | 1 天 |
| 10 | 🟢 P3 | `storage_bytes`/`avg_decay_score` 硬编码 0 添加 TODO 注释 | 0.5 天 |

---

*Review completed by EngineeringSeniorDeveloper, 2026-05-10*
