# ADR-028：AgentCore 进程级累计 Token 消耗缓存

**状态**：实施中
**日期**：2026-07-16
**决策者**：大鱼
**影响范围**：

- `core/acowork-runtime/src/agent/agent_core.rs`（新增 `AtomicU64` counters + `accumulate_llm_usage` / `merge_token_totals` / `agent_token_totals` 方法）
- `core/acowork-runtime/src/conversation.rs`（`scan_sessions_async` 返回 agent 级聚合）
- `core/acowork-runtime/src/cli.rs`（`handle_list_sessions` 合并 + 转发）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（新增 `core()` 访问器）
- `core/acowork-runtime/src/agent/loop_context.rs`（4 个 LLM 调用点 + 3 个 ContextUsage 推送点）
- `core/acowork-runtime/src/agent/loop_.rs`（title 生成调用点）
- `core/acowork-runtime/src/agent/loop_session.rs`（session-end distillation 调用点）
- `core/acowork-core/src/protocol.rs`（`ContextUsageInfo` 新增两个字段）
- `core/acowork-core/proto/gateway_ipc.proto`（protobuf 消息新增字段）
- `core/acowork-core/src/proto_bridge.rs`（双向桥接 + 测试）
- `core/acowork-gateway/src/http/chat.rs`（`SessionsListResponse` 扩展 + handler）
- `core/acowork-gateway/src/grpc/dispatch.rs`（防御性默认值）
- `apps/acowork-desktop/src/lib/types.ts`（`ContextUsageInfo` 类型扩展）
- `apps/acowork-desktop/src/stores/agentStore.ts`（`AgentStorage` 新增 `agentTokenTotals`）
- `apps/acowork-desktop/src/components/results/ResultsPanel.tsx`（Agent Status 面板展示）
- `apps/acowork-desktop/src/i18n/locales/*.json`（5 个语言的 i18n key）

---

## 背景

### 现状

ADR-027 为每个 session 的 `SessionMeta` 添加了 `tokens.total_input` / `tokens.total_output` 字段，记录了该 session 生命周期内所有 LLM 调用的累计 token 消耗。但是：

1. **缺少 agent 级视图**：用户需要知道一个 agent 在所有 session 中共消耗了多少 token，而 Frontend 没有计算能力（无法遍历所有 session 的 meta 文件）。
2. **启动期"空窗"**：当 Runtime 进程刚启动时，第一次 `ContextUsageInfo` push（通过 WebSocket）需要等到第一个 LLM 调用之后才会触发。在此之前，Frontend 的 Results Panel 没有任何 token 数据可展示。

### 用户需求

1. 在 Desktop App 的 Agent Status 面板展示该 agent **进程级累计**的 input / output token 总数
2. 即使没有活跃 session 或 LLM 调用（Runtime 刚启动），Agent Status 面板也能显示历史累计值
3. 进程启动后的 LLM 调用能实时更新累计值
4. 不持久化到磁盘（避免与 `SessionMeta` 字段双重写入产生的数据一致性问题）

---

## 目标

1. 在 `AgentCore` 中增加两个 `AtomicU64` 计数器（`agent_total_input_tokens` / `agent_total_output_tokens`），通过原子操作累加 LLM 调用消耗
2. `scan_sessions_async` 在遍历 meta 文件时累加 `tokens.total_input` / `tokens.total_output`，通过 `atomic max` 合并到 `AgentCore` 计数器
3. 在 `ContextUsageInfo` 中新增两个 `Option<u64>` 字段（`agent_total_input_tokens` / `agent_total_output_tokens`），每次 push 时填入计数器的快照
4. 在 `GET /api/agents/:id/sessions` 响应中增加两个字段，作为 WebSocket 推送的 fallback 数据源
5. Frontend 优先使用 WebSocket 实时数据，降级使用 session-list fallback

---

## 方案设计

### 1. 数据模型

`AgentCore` 新增字段：

```rust
// 见 core/acowork-runtime/src/agent/agent_core.rs
pub(crate) agent_total_input_tokens: AtomicU64,
pub(crate) agent_total_output_tokens: AtomicU64,
```

`ContextUsageInfo` 新增字段（`protocol.rs`）：

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub agent_total_input_tokens: Option<u64>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub agent_total_output_tokens: Option<u64>,
```

`SessionsListResponse` 新增字段（Gateway `chat.rs`）：

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub agent_total_input_tokens: Option<u64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub agent_total_output_tokens: Option<u64>,
```

Frontend `AgentStorage` 新增字段：

```typescript
agentTokenTotals: { input: number; output: number } | null;
```

### 2. 两条数据源

| 数据源 | 触发时机 | 用途 |
|--------|---------|------|
| **Primary (live)** | 每次 `ContextUsageInfo` WebSocket 推送事件携带 `agent_total_input_tokens` / `agent_total_output_tokens` | 当前 Runtime 进程内的实时累计，来自 `AgentCore` 的原子计数器 |
| **Fallback (session list)** | `GET /api/agents/:id/sessions` 响应携带 `agent_total_input_tokens` / `agent_total_output_tokens` | Runtime 刚启动、还没有任何 LLM 调用时，从磁盘扫描所有 session meta 文件的 `tokens` 字段聚合而来 |

#### Frontend 优先级

```typescript
agentTotalInput = contextUsage?.agent_total_input_tokens
                 ?? agentStore.agents[id]?.agentTokenTotals?.input
                 ?? undefined
agentTotalOutput = contextUsage?.agent_total_output_tokens
                  ?? agentStore.agents[id]?.agentTokenTotals?.output
                  ?? undefined
```

- 一旦 live 有值（第一次 LLM 后），始终优先用 live
- live 缺失时降级到 session list fallback
- 两者都没有则不显示该行（或显示 `—`）

### 3. 计数器更新流程

```
LLM Call (4 个调用点)
    │
    ├── ConversationSession::accumulate_llm_usage(usage)   // 写入 SessionMeta
    └── AgentCore::accumulate_llm_usage(usage)              // 原子累加
              │
              ▼
ContextUsageInfo Push (3 个推送点)
    │
    ├── ctx_usage.agent_total_input_tokens = Some(agent_in)
    └── ctx_usage.agent_total_output_tokens = Some(agent_out)
              │
              ▼
          WebSocket → Frontend Results Panel (Agent Status)
                         │
    GET /api/agents/:id/sessions
    │
    ├── scan_sessions_async → (agent_total_input, agent_total_output)
    ├── AgentCore::merge_token_totals((in, out))            // atomic max
    └── 回写 response → Frontend agentStore stash
                           │
                           ▼
                     Results Panel (Agent Status) fallback
```

### 4. Atomic Max Merge 设计

`merge_token_totals` 使用 `fetch_update` 实现 `counter = max(counter, scanned)`：

```rust
pub fn merge_token_totals(&self, scanned: (Option<u64>, Option<u64>)) {
    if let Some(inp) = scanned.0 {
        self.agent_total_input_tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.max(inp))
            })
            .ok();
    }
    // ...
}
```

为什么用 max 而不是直接赋值？

| 场景 | merge(${scanned}) | accumulate_llm_usage(+10) | 最终 | 分析 |
|------|-------------------|--------------------------|------|------|
| 先 merge, 后 accumulate | scanned=1000 | +10 | 1010 | ✅ counter 追上最新 |
| 先 accumulate, 后 merge | +10, counter=10 | scanned=1000 | 1000 | ✅ max 保留扫描的历史值 |
| 同时发生 | scanned=1000 | +10 | max(1000, 10)+10 = 1010 | ⚠️ 一个 LLM 调用被 merge 覆盖，但下一个 accumulate 会补上 |
| 幂等调用 | scanned=1000 | - | 1000(不变) | ✅ merge 幂等 |

**结论**：max 语义在任何并发顺序下都不会让计数器丢失非零的 LLM 调用。唯一可能丢的是一小段时间窗口内的最后一个 LLM 调用，但下次 push 或下次 scan 会立即补上。

### 5. 为什么不做进程启动期的 seed

一种备选方案是 Runtime 启动时读取所有 session 的 meta 文件，将累加值 seed 到 `AgentCore` 计数器。本方案放弃这一做法，理由：

1. **重复扫描**：`handle_list_sessions` 是 session 列表的必经路径，每次返回都会做一次全量扫描 + atomic max merge。在进程启动后通常是第一次用户操作（客户端自动调用），seed 的价值被覆盖。
2. **零额外 I/O 延迟**：启动期 seed 会增加冷启动时间（磁盘 I/O），而按需扫描的延迟对用户不可感知（已有 session 列表的加载时间）。
3. **单点真相**：`AgentCore` 计数器不从磁盘 seed，避免 `SessionMeta` 与 `AgentCore` 之间的数据一致性校验。

---

## 边界情况

### 1. 进程重启

进程重启后 `AgentCore` 计数器归零。下一次 `GET /api/agents/:id/sessions` 请求触发 `scan_sessions_async` → `merge_token_totals`，从磁盘全量扫描恢复基线。恢复之前 Frontend 不显示 agent-total 行（`—`）。

### 2. 并发 Merge vs Accumulate

Atomic `fetch_update` 保证操作的原子性。max 语义在任何并发顺序下都不会丢失正数。详见 §4。

### 3. 大数量 session

`scan_sessions_from_meta` 遍历所有 `.json` 文件，累加 `tokens.total_input` / `tokens.total_output`。时间复杂度 O(n)，空间复杂度 O(1)（仅累加器，不构造 session 对象）。n 在可预见范围内（单 agent < 10^4 session）性能可接受。

### 4. Runtime 版本兼容

Gateway 的 `SessionsListResponse.agent_total_input_tokens` 和 `agent_total_output_tokens` 用 `#[serde(skip_serializing_if = "Option::is_none")]` 标记。旧版 Runtime 首次 scan 返回的 JSON 中不包含这两个字段，Gateway `list_sessions` handler 用 `data.get("agent_total_input_tokens").and_then(|v| v.as_u64())` 提取，缺失时设 `None`，序列化时自动省略。旧版 Desktop App 读取到不认识的字段也静默忽略。

---

## 测试

### `agent_core.rs` 单元测试

1. **`accumulate_llm_usage` 基本累加**：两次调用 verify 最终值正确
2. **`accumulate_llm_usage` zero-input skip**：`prompt_tokens=0` 不累加 input
3. **`accumulate_llm_usage` saturating overflow**：接近 `u64::MAX` 时不 panic
4. **`merge_token_totals` 行为**：初始 → merge → counter == scanned；counter > scanned → 保持；counter < scanned → 取 scanned
5. **混合场景**：accumulate 和 merge 并发调用，验证最终值正确

### `conversation.rs::scan_sessions_async` 测试

- 验证返回的 `agent_totals` 等于所有 session meta 中 `tokens.total_input` / `tokens.total_output` 之和
- 验证空目录返回 `(0, 0)`

### `proto_bridge.rs` 测试

- 新增字段 round-trip + backward-compat（None 字段在 JSON 中省略）

---

## 评审意见

（设计评审记录占位）
