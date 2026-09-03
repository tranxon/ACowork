# ADR-066：LLM Provider Cache Tokens 透传与累计统计

**状态**：实施中
**日期**：2026-07-21
**决策者**：大鱼
**前置 ADR**：ADR-027（SessionTokens per-session 累计）、ADR-028（AgentCore 进程级累计）

**影响范围**：

- `core/acowork-core/src/providers/traits.rs`（`UsageInfo` 字段已存在，本 ADR 不改动）
- `core/acowork-runtime/src/conversation.rs`（`SessionTokens` 扩展 4 字段 + 累计方法 + `scan_sessions_async` 聚合）
- `core/acowork-runtime/src/agent/agent_core.rs`（新增 2 个 `AtomicU64` + `accumulate_llm_usage` / `merge_token_totals` / `agent_token_totals` 扩展）
- `core/acowork-runtime/src/usecases/agent_token.rs`（`AgentTokenService` trait 返回类型扩展）
- `core/acowork-runtime/src/usecases/agent_token_impl.rs`（两实现同步扩展）
- `core/acowork-runtime/src/agent/loop_context.rs`（3 个 ContextUsage 推送点）
- `core/acowork-runtime/src/agent/context.rs`（`compute_context_usage` / `build_context_usage_from_persisted`）
- `core/acowork-runtime/src/startup/session_init.rs`（resume 路径 `merge_token_totals` 扩展）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（`merge_token_totals` 扩展）
- `core/acowork-core/src/protocol.rs`（`ContextUsageInfo` 新增 6 字段）
- `core/acowork-runtime/src/usecases/session_metadata.rs`（`SessionsListResponse` 扩展 2 字段 — 见 §"实际实施偏差"）
- `core/acowork-runtime/src/usecases/session_metadata_impl.rs`（`list_sessions` 同步扩展）
- `core/acowork-runtime/tests/context_usage_cache_e2e.rs`（新增 e2e 测试）
- `apps/acowork-desktop/src/lib/types.ts`（`ContextUsageInfo` 类型扩展 + `agentTokenTotals` 类型扩展）
- `apps/acowork-desktop/src/lib/cacheHitRate.ts`（新增 — 命中率计算纯函数 helper）
- `apps/acowork-desktop/src/lib/cacheHitRate.test.ts`（新增 — helper 单元测试）
- `apps/acowork-desktop/src/stores/agentStore.ts`（`agentTokenTotals` 字段扩展 cache 维度）
- `apps/acowork-desktop/src/components/results/ResultsPanel.tsx`（Agent Status / Session Status 面板增补 cache 行 + 缓存命中率）
- `apps/acowork-desktop/src/components/chat/ContextUsageIcon.tsx`（popover 摘要补充 cache 行 + 命中率行 — 见 §"实际实施偏差"）
- `apps/acowork-desktop/src/i18n/locales/*.json`（i18n key）

> **修订说明**（实施期）：原影响范围列出 `core/acowork-gateway/src/http/chat.rs`
> 与 `core/acowork-gateway/src/grpc/dispatch.rs`，但 `chat.rs` 在 ADR-040（UseCase
> 层重构）时已迁到 `core/acowork-runtime/src/usecases/session_metadata.rs`，
> 并进一步裂分为 `session_metadata.rs` + `session_metadata_impl.rs` 两个文件；
> `dispatch.rs` 路径本身已不存在。实际实施改动了上述 runtime 侧
> session_metadata 系列文件，原 gateway 路径已不再代表真实代码位置。

---

## 背景

### Provider API 原生支持

| Provider | Cache Read | Cache Write | 响应字段 |
|----------|:---------:|:---------:|---------|
| **OpenAI Chat Completions** | ✅ | ❌（自动缓存，不区分 write） | `usage.prompt_tokens_details.cached_tokens` |
| **Anthropic Messages** | ✅ | ✅ | `usage.cache_read_input_tokens` + `usage.cache_creation_input_tokens` |

两个 provider 都在 [`UsageInfo`](core/acowork-core/src/providers/traits.rs#L694) 中预留了 `cache_read_tokens` / `cache_write_tokens` 字段：

- [`providers/openai.rs`](core/acowork-runtime/src/providers/openai.rs#L521) `parse_response` / `parse_sse_line` 已从 `prompt_tokens_details.cached_tokens` 提取并写入 `cache_read_tokens`（`cache_write_tokens` 恒为 0，OpenAI 无此概念）
- [`providers/anthropic.rs`](core/acowork-runtime/src/providers/anthropic.rs#L639) `parse_response` / `parse_anthropic_sse_line` 已从 `cache_creation_input_tokens` + `cache_read_input_tokens` 提取并分别写入 `cache_write_tokens` / `cache_read_tokens`

**Provider 解析层 100% 就绪，无需改动。**

### 现状（数据断点）

```
Provider 响应
  │
  ├─ usage.prompt_tokens_details.cached_tokens          (OpenAI)
  ├─ usage.cache_read_input_tokens                      (Anthropic)
  └─ usage.cache_creation_input_tokens                  (Anthropic)
       │
       ▼
  UsageInfo { cache_read_tokens, cache_write_tokens }   ✅ 已填充
       │
       ▼ ❌ 链路中断
  SessionTokens { last_input, last_output, total_input, total_output }
       │
       ▼ ❌
  AgentCore { agent_total_input_tokens, agent_total_output_tokens }
       │
       ▼ ❌
  ContextUsageInfo { ...无 cache 字段... }
       │
       ▼ ❌
  Frontend ResultsPanel（无 cache 行、无命中率）
```

具体断点：

| 层级 | 位置 | 问题 |
|------|------|------|
| Session 累计 | `conversation.rs::SessionTokens` | 仅 4 字段，丢弃 cache_* |
| Session 累计 | `accumulate_llm_usage` / `accumulate_compaction_usage` | 构造 `SessionTokens` 时不读 cache_* |
| Agent 累计 | `agent_core.rs` | 仅 2 个 AtomicU64 |
| Agent 累计 | `accumulate_llm_usage` | 不累加 cache |
| Agent 累计 | `merge_token_totals` / `agent_token_totals` | 不接受/不返回 cache |
| 协议 | `protocol.rs::ContextUsageInfo` | 无 cache_* 字段 |
| 推送 | `loop_context.rs` × 3 处 | 构造 `ContextUsageInfo` 不带 cache |
| 协议 | `agent_token.rs::AgentTokenService` | trait 返回 `(u64, u64)`，不携带 cache |
| 持久化合并 | `scan_sessions_async` | 不聚合 cache |
| Gateway | `chat.rs::SessionsListResponse` | 无 agent_total_cache_* 字段 |
| 前端类型 | `lib/types.ts::ContextUsageInfo` | 无 cache_* 字段 |
| 前端 store | `agentStore.ts::AgentStorage.agentTokenTotals` | 仅 `{input, output}` |
| 前端 UI | `ResultsPanel.tsx` | 无 cache 行、无命中率 |
| 前端状态栏 | `ContextUsageIcon.tsx` | 无 cache 摘要 |

### 用户需求

1. 在 Desktop App 的 Session Status 面板展示当次 LLM 调用与 session 累计的 cache tokens（read / write）
2. 计算并展示缓存命中率（用于评估 Anthropic prompt caching、OpenAI auto-caching 的成本效益）
3. session 重启后命中率基线能恢复（与 ADR-027/028 一致）
4. 进程级 Agent Total 同样支持 cache 累计（与 ADR-028 一致）

---

## 目标

1. **`SessionTokens` 扩展 4 字段**：`last_cache_read` / `last_cache_write` / `total_cache_read` / `total_cache_write`，全部 `#[serde(default)]`（旧 v3 meta 文件向后兼容）
2. **`AgentCore` 扩展 2 个 `AtomicU64`**：`agent_total_cache_read_tokens` / `agent_total_cache_write_tokens`，沿用 ADR-028 的 `accumulate_llm_usage` / `merge_token_totals` / `agent_token_totals` 模式
3. **`AgentTokenService` trait 返回类型扩展**为 `(in, out, cache_read, cache_write)` 4 元组
4. **`ContextUsageInfo` 扩展 6 字段**：
   - Per-turn：`cache_read_tokens` / `cache_write_tokens`
   - Session total：`total_cache_read_tokens` / `total_cache_write_tokens`
   - Agent total：`agent_total_cache_read_tokens` / `agent_total_cache_write_tokens`
   - 全部 `Option<u64>` + `#[serde(default, skip_serializing_if = "Option::is_none")]`
5. **透传路径完整**：Provider → `UsageInfo` → `SessionTokens` → `AgentCore` → `ContextUsageInfo` → Frontend
6. **前端 UI**：ResultsPanel 增补 cache 行 + 命中率徽标；ContextUsageIcon 状态栏摘要

---

## 方案设计

### 1. 数据模型

#### `SessionTokens`（扩展，[`conversation.rs`](core/acowork-runtime/src/conversation.rs#L47)）

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]  // 向后兼容：旧 v3 meta 文件无 cache 字段时全 default
pub struct SessionTokens {
    pub last_input: u64,
    pub last_output: u64,
    pub total_input: u64,
    pub total_output: u64,
    // ── ADR-066: prompt cache tokens (Provider-reported) ───────────────
    /// Last-turn prompt tokens served from cache (OpenAI cached_tokens /
    /// Anthropic cache_read_input_tokens).
    #[serde(default)]
    pub last_cache_read: u64,
    /// Last-turn prompt tokens written to cache (Anthropic
    /// cache_creation_input_tokens; OpenAI has no concept → 0).
    #[serde(default)]
    pub last_cache_write: u64,
    /// Cumulative cache read tokens across all session LLM calls.
    #[serde(default)]
    pub total_cache_read: u64,
    /// Cumulative cache write tokens across all session LLM calls.
    #[serde(default)]
    pub total_cache_write: u64,
}
```

> **不升 `CONVERSATION_FORMAT_VERSION`**：ADR-027 已升到 v3，本 ADR 复用同一版本号，新字段全部 `#[serde(default)]`，老 v3 文件反序列化时 cache_* 自动为 0，符合 "宁可 miss 也不估计" 原则——把"未记录的 cache"当作 0，不会污染命中率分母（因 cache_read=0 → 命中率=0% 的合理 fallback）。

#### `AgentCore`（扩展，[`agent_core.rs`](core/acowork-runtime/src/agent/agent_core.rs#L286)）

```rust
pub(crate) agent_total_input_tokens: AtomicU64,
pub(crate) agent_total_output_tokens: AtomicU64,
// ── ADR-066: agent-level cache counters ──────────────────────
pub(crate) agent_total_cache_read_tokens: AtomicU64,
pub(crate) agent_total_cache_write_tokens: AtomicU64,
```

`accumulate_llm_usage` 扩展：

```rust
pub fn accumulate_llm_usage(&self, usage: &UsageInfo) {
    if usage.prompt_tokens > 0 {
        self.agent_total_input_tokens
            .fetch_update(..., |cur| Some(cur.saturating_add(usage.prompt_tokens)))
            .ok();
        // cache_read 同样遵循 "prompt_tokens > 0 才累加" 的语义（Provider
        // fallback 时 cache 计数也归零）
        self.agent_total_cache_read_tokens
            .fetch_update(..., |cur| Some(cur.saturating_add(usage.cache_read_tokens)))
            .ok();
    }
    self.agent_total_output_tokens
        .fetch_update(..., |cur| Some(cur.saturating_add(usage.completion_tokens)))
        .ok();
    self.agent_total_cache_write_tokens
        .fetch_update(..., |cur| Some(cur.saturating_add(usage.cache_write_tokens)))
        .ok();
}
```

`merge_token_totals` 扩展为 4 元组：

```rust
pub fn merge_token_totals(
    &self,
    scanned: (Option<u64>, Option<u64>, Option<u64>, Option<u64>),
) {
    // (input, output, cache_read, cache_write) 各自 atomic max
    ...
}

pub fn agent_token_totals(&self) -> (u64, u64, u64, u64) {
    (
        self.agent_total_input_tokens.load(...),
        self.agent_total_output_tokens.load(...),
        self.agent_total_cache_read_tokens.load(...),
        self.agent_total_cache_write_tokens.load(...),
    )
}
```

#### `AgentTokenService` trait（[`usecases/agent_token.rs`](core/acowork-runtime/src/usecases/agent_token.rs)）

```rust
pub trait AgentTokenService: Send + Sync {
    fn accumulate_llm_usage(&self, usage: &UsageInfo);
    fn merge_token_totals(
        &self,
        scanned: (Option<u64>, Option<u64>, Option<u64>, Option<u64>),
    );
    fn agent_token_totals(&self) -> (u64, u64, u64, u64);
}
```

两实现（`NoopAgentTokenService` / `InMemoryAgentTokenService`）同步扩展签名。

#### `ContextUsageInfo`（[`protocol.rs`](core/acowork-core/src/protocol.rs#L737)）

```rust
pub struct ContextUsageInfo {
    // ... 现有字段 ...
    // ── ADR-066: per-turn cache tokens (from UsageInfo) ──────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    // ── ADR-066: session-total cache tokens ─────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cache_write_tokens: Option<u64>,
    // ── ADR-066: agent-total cache tokens ───────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_total_cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_total_cache_write_tokens: Option<u64>,
}
```

> **为什么 `Option` 包裹**：旧版 Runtime / 前端（无 ADR-066 字段）互操作时，`#[serde(default, skip_serializing_if = "Option::is_none")]` 保证 JSON 中省略，前端 `undefined` 时正确处理为"未上报"而非"0"。

### 2. 推送路径

3 个 `ContextUsageInfo` 构造点（[`loop_context.rs`](core/acowork-runtime/src/agent/loop_context.rs#L128) 等）统一更新：

```rust
let (agent_in, agent_out, agent_cache_read, agent_cache_write) =
    self.core.agent_token_totals();
let (total_cache_read, total_cache_write) = session_tokens
    .as_ref()
    .map(|t| (t.total_cache_read, t.total_cache_write))
    .unwrap_or((0, 0));
let ctx_info = ContextUsageInfo {
    // ... 现有字段 ...
    cache_read_tokens: Some(usage.cache_read_tokens),
    cache_write_tokens: Some(usage.cache_write_tokens),
    total_cache_read_tokens: Some(total_cache_read),
    total_cache_write_tokens: Some(total_cache_write),
    agent_total_cache_read_tokens: Some(agent_cache_read),
    agent_total_cache_write_tokens: Some(agent_cache_write),
};
```

`compute_context_usage` 与 `build_context_usage_from_persisted` 也按相同模式补充（保留 ADR-027 的 "compute 只看 per-turn、session total 由 caller patch" 哲学）。

### 3. 持久化合并

`scan_sessions_async` 在遍历 meta 文件时同步累加 `tokens.total_cache_read` / `tokens.total_cache_write`，返回 `(in, out, cache_read, cache_write)` 4 元组：

```rust
// conversation.rs::scan_sessions_async
let mut agent_in = 0u64;
let mut agent_out = 0u64;
let mut agent_cache_read = 0u64;
let mut agent_cache_write = 0u64;
for meta in sessions {
    if let Some(t) = meta.tokens {
        agent_in = agent_in.saturating_add(t.total_input);
        agent_out = agent_out.saturating_add(t.total_output);
        agent_cache_read = agent_cache_read.saturating_add(t.total_cache_read);
        agent_cache_write = agent_cache_write.saturating_add(t.total_cache_write);
    }
}
Ok((agent_in, agent_out, agent_cache_read, agent_cache_write))
```

`list_sessions` handler 调用 `merge_token_totals(scanned_4_tuple)` 与 atomic max merge。

### 4. Gateway & Frontend

**Gateway `SessionsListResponse`**（`chat.rs`）扩展 4 字段（与 `agent_total_*` 平级）：

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub agent_total_cache_read_tokens: Option<u64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub agent_total_cache_write_tokens: Option<u64>,
```

**Frontend `ContextUsageInfo`** 类型（`lib/types.ts`）镜像 6 字段。

**Frontend `AgentStorage.agentTokenTotals`** 类型从 `{ input: number; output: number }` 扩展为 `{ input, output, cacheRead, cacheWrite }`。

### 5. UI 呈现

#### ResultsPanel（Session Status 面板）

增补 3 行（per-turn 区域，与 Prompt/Completion 平级）：

| 行 | 值 | 来源 |
|---|----|------|
| **Cache Read** | `contextUsage?.cache_read_tokens?.toLocaleString()` | UsageInfo |
| **Cache Write** | `contextUsage?.cache_write_tokens?.toLocaleString()` | UsageInfo |
| **Cache Hit Rate** | 计算值（见 §6） | 派生 |

增补 2 行（session total 区域，与 Total Input/Output 平级）：

| 行 | 值 |
|---|----|
| **Total Cache Read** | `contextUsage?.total_cache_read_tokens?.toLocaleString()` |
| **Total Cache Write** | `contextUsage?.total_cache_write_tokens?.toLocaleString()` |

增补 2 行（agent total 区域，与 Agent Total Input/Output 平级）：

| 行 | 值 |
|---|----|
| **Agent Total Cache Read** | `contextUsage?.agent_total_cache_read_tokens ?? agentTokenTotals?.cacheRead` |
| **Agent Total Cache Write** | 同上 |

#### ContextUsageIcon（聊天状态栏）

在现有的 `formatTokens(total_tokens) / formatTokens(context_window) context used` 后面追加：

```
[Cache Hit: 64.2% ▮▮▮▮▮▮▯▯▯▯]
```

仅当 `cache_read_tokens > 0` 或 `total_cache_read_tokens > 0` 时显示（避免无 cache 场景下出现 0% 误导）。

### 6. 命中率计算口径

**两个口径并存，前端按 provider 类型切换**：

| Provider | 公式 | 含义 |
|----------|------|------|
| **Anthropic**（推荐） | `cache_read / (input_tokens + cache_read + cache_write)` | write 是命中前提，写入越多后续 read 越多 |
| **OpenAI** | `cache_read / prompt_tokens` | OpenAI 无 write 概念 |

**实现**：后端不替前端做选择，只透传原始值。前端在 ResultsPanel 渲染时按 `sessionProvider` 字段（已存在于 session 元数据）选择公式：

```typescript
const cacheHitRate = (() => {
  const cacheRead = contextUsage?.total_cache_read_tokens ?? 0;
  const cacheWrite = contextUsage?.total_cache_write_tokens ?? 0;
  const inputTokens = contextUsage?.total_input_tokens ?? 0;
  const promptTokens = contextUsage?.input_tokens ?? 0;
  if (sessionProvider?.startsWith("anthropic")) {
    const denom = promptTokens + cacheRead + cacheWrite;
    return denom > 0 ? cacheRead / denom : null;
  }
  return promptTokens > 0 ? cacheRead / promptTokens : null;
})();
```

`null` 时不显示徽标。

### 7. 数据流总图

```
Provider (OpenAI cached_tokens / Anthropic cache_*_input_tokens)
   │
   ▼
UsageInfo { cache_read_tokens, cache_write_tokens }
   │
   ├── ConversationSession::accumulate_llm_usage(usage)
   │     └── SessionTokens.last_*/total_*  (saturating_add)
   │           └── write_meta() → meta.json on disk
   │
   └── AgentCore::accumulate_llm_usage(usage)
         └── AtomicU64  (saturating_add)
   │
   ▼
ContextUsageInfo push (3 推送点)
   │
   ├── cache_read_tokens/write_tokens           ← per-turn (from usage)
   ├── total_cache_read_tokens/write_tokens     ← session (from SessionTokens)
   └── agent_total_cache_read_tokens/write_tokens ← agent (from AtomicU64)
   │
   ▼ WebSocket / MQTT
   Frontend ContextUsageInfo (typed)
   │
   ├── ResultsPanel: 6 行 + 1 命中率行
   └── ContextUsageIcon: 状态栏徽标

启动 / 重启：
GET /api/agents/:id/sessions
   → scan_sessions_async 累加 total_cache_* 字段
   → AgentCore::merge_token_totals((in, out, cache_read, cache_write))
   → 回写 SessionsListResponse.agent_total_cache_*
   → Frontend agentStore.agents[id].agentTokenTotals.cacheRead/Write
```

---

## 边界情况

### 1. 旧版 v3 meta 文件（无 cache 字段）

`SessionTokens` 全字段 `#[serde(default)]`，反序列化时 `total_cache_*` 自动 = 0。命中率分母中 cache 维度 = 0，公式返回 `null`，UI 不显示——不会污染显示。

### 2. 旧版 Runtime / Desktop App 互操作

所有新字段 `#[serde(default, skip_serializing_if = "Option::is_none")]`：

- 旧 Desktop App 读取到缺失字段 → `undefined` → UI fallback 到 "—"
- 旧 Runtime 返回的 JSON 缺新字段 → Gateway 用 `data.get(...).and_then(...)` 防御性提取，缺则 `None`

### 3. 并发 Accumulate vs Merge

沿用 ADR-028 的 atomic max 语义。cache 维度同理：scan 可能短暂落后于一次 accumulate，但下次 push 或 scan 会立即补正。

### 4. Provider 不报告 cache（无 `cached_tokens` 字段）

OpenAI 早期模型 / Ollama / Mock 等：

- `cache_read_tokens = 0` / `cache_write_tokens = 0`（`unwrap_or(0)`）
- 命中率分母含 cache → 0，公式返回 `null`，UI 不显示
- 与 ADR-027 的 "宁可 miss 也不估计" 一致

### 5. OpenAI 的 cache_write_tokens 恒为 0

设计上 OpenAI 自动缓存、不区分 write。`last_cache_write` / `total_cache_write` / `agent_total_cache_write_tokens` 恒为 0。

UI 上仍然显示 "Cache Write: 0"（OpenAI 视角下语义无害、明确表达 provider 不支持），或按 provider 类型隐藏该行（待设计阶段定）。

### 6. Anthropic 5min / 1h 缓存 TTL 差异

不在本 ADR 范围。命中率口径不变（基于 token 数而非时间窗口）。

---

## 测试

### `conversation.rs` 单元测试（补充 ADR-027 已有测试）

1. **`SessionTokens` serde 向后兼容——缺 cache 字段反序列化**：`{"last_input": 100, ...}` 无 cache_* → 默认 0
2. **`SessionTokens` serde round-trip**：带 cache 字段完整序列化后反序列化一致
3. **`accumulate_llm_usage` 累加 cache**：两次调用 verify `total_cache_read/write` saturating_add
4. **`accumulate_llm_usage` zero-input 行为**：`prompt_tokens=0` 时 cache 不累加（与 input 一致）
5. **`accumulate_compaction_usage` 不污染 last_cache_*，累加 total_cache_***
6. **`set_history_anchor` 不污染 cache 字段**（last_cache_* 保留，total_cache_* 保留）
7. **`scan_sessions_async` 聚合 cache**：多个 session 的 `total_cache_*` 求和正确

### `agent_core.rs` 单元测试（补充 ADR-028 已有测试）

1. **`accumulate_llm_usage` 累加 cache**：四次调用 verify 4 个 AtomicU64
2. **`accumulate_llm_usage` saturating overflow**：cache_* 接近 u64::MAX 不 panic
3. **`merge_token_totals` cache 维度**：4 元组 max 行为与 ADR-028 in/out 一致
4. **`agent_token_totals` 返回 4 元组**：顺序为 `(in, out, cache_read, cache_write)`

### `protocol.rs` 序列化测试

1. **`ContextUsageInfo` 全字段 round-trip**
2. **缺 cache 字段的反序列化 → `None`**
3. **`#[serde(skip_serializing_if = "Option::is_none")]` 行为**：`None` 字段不出现在 JSON 中

### `usecases/agent_token.rs` 测试

1. **trait 4 元组签名扩展后两实现仍工作**
2. **`InMemoryAgentTokenService` cache 维度累加**

### `provider_api.rs` (Gateway) 测试

1. **`list_sessions` 响应携带 `agent_total_cache_*` 字段**
2. **缺字段时 Gateway 防御性兜底为 `None`**

### 前端单测（vitest）

1. **`computeCacheHitRate(anthropic, ...)`** 公式正确
2. **`computeCacheHitRate(openai, ...)`** 公式正确
3. **`computeCacheHitRate` 分母 0 → 返回 `null`**
4. **`ResultsPanel` 渲染：cache 行 + 命中率徽标**（snapshot 测试）

---

## 评审意见

（设计评审记录占位）

---

## 实际实施偏差（评审期记录）

以下偏差均为评审阶段识别并定向接受/修正。每条都列出 ADR 原描述、实际做法、决策理由。

### 1. `SessionsListResponse` 字段类型：Option → 必填 `u64`

**ADR 原描述**（§4 Gateway & Frontend）：`agent_total_cache_*_tokens` 使用 `Option<u64>` +
`#[serde(skip_serializing_if = "Option::is_none")]`，用于"旧版 Runtime 互操作防御"。

**实际实施**：
[`core/acowork-runtime/src/usecases/session_metadata.rs`](core/acowork-runtime/src/usecases/session_metadata.rs)
中两个 cache 字段为必填 `u64`，而非 `Option<u64>`。代码注释自圆其说：

> "Cache fields are emitted unconditionally because the runtime
> always initialises the agent counters (Commit 2 sets both
> `agent_total_cache_read_tokens` and `agent_total_cache_write_tokens`
> to `0` on every construction site). Desktop frontends that do not
> yet read these fields stay compatible."

**理由**：

- ✅ 简化前端 `AgentStorage.agentTokenTotals` 类型（无需 `cacheRead?: number`，
  直接 `cacheRead: number`，零值即"未上报"）。
- ✅ 前端 [`apps/acowork-desktop/src/stores/agentStore.ts`](apps/acowork-desktop/src/stores/agentStore.ts)
  在解析 Gateway 响应时仍保留 `data.agent_total_cache_read_tokens ?? 0`
  防御性兜底，所以语义等价于 `Option<u64>` + `skip_serializing_if`。
- ⚠️ 若未来在 `AgentCore` 构造函数变更时漏初始化任一 cache counter，
  会出现"未定义值"而非"未上报"——这是一个由 `AtomicU64::new(0)` 保证的契约。
- 旧 v3 `SessionTokens` meta 文件仍走 `#[serde(default)]` → 0 路径（行为不变）。

**结论**：简化类型 + 前端兜底等价 = 接受偏差。如未来加新的 agent counter
（成本/折扣等），需在本 ADR 文档再次确认是否沿用"必填 + 0 默认"或回退
"Option + skip_serializing_if"。

### 2. ContextUsageIcon 状态栏：徽标 → 行内文字

**ADR 原描述**（§5 UI 呈现 → ContextUsageIcon）："在现有的
`formatTokens(total_tokens) / formatTokens(context_window) context used`
后面追加 `[Cache Hit: 64.2% ▮▮▮▮▮▮▯▯▯▯]`"。

**实际实施**：
[`apps/acowork-desktop/src/components/chat/ContextUsageIcon.tsx`](apps/acowork-desktop/src/components/chat/ContextUsageIcon.tsx)
在 popover 内追加一行文字 `Cache hit rate 50.0% cached`，而非图标 + ASCII 进度条。

**理由**：

- ✅ 圆形图标按钮（16×16 SVG）无空间显示徽标数字，popover 是合理的展示位置。
- ✅ "百分比 + cached 文字"在视觉密度上与已有 `usage_percent %` 行对齐。
- ⚠️ 没有进度条 → 用户无法直观看到"占满 vs 未占满"。但 cache 命中率是
  比值而非绝对量，进度条的视觉隐喻较弱，文字已经足够。

**结论**：UX 简化 = 接受偏差。如后续反馈"需要进度条"，可在 ResultsPanel 的
Agent Status 面板补一个 mini-bar（那里空间更大）。

### 3. 命中率公式：双公式并存 → 前端 helper 按 provider 分流

**ADR 原描述**（§6 命中率计算口径）："Anthropic 用
`cache_read / (input + cache_read + cache_write)`；OpenAI 用
`cache_read / prompt_tokens`；两种口径并存，前端按 provider 类型切换"

**实际实施**：
[`apps/acowork-desktop/src/lib/cacheHitRate.ts`](apps/acowork-desktop/src/lib/cacheHitRate.ts)
按协议家族分流：
- `getCacheProtocol(providerId)` 显式白名单：`openai` / `azure` /
  `azure-openai` → `"openai"`；`anthropic` / `bedrock` → `"anthropic"`；
  其它（含 `ollama` / `deepseek` / `zhipuai` / `minimax*` /
  `volcengine-agent-plan` / 用户自定义 OpenAI-compatible 端点）→ `null`（不显示）。
- `computeCacheHitRate(providerId, usage)` 优先用 cumulative
  `total_cache_read_tokens`，fallback 到 per-turn `cache_read_tokens`，
  分母匹配同一时间维度。
- `formatCacheHitRate(ratio)` 输出 `12.3%` 形式，clamp 到 `[0%, 100%]`。

**理由**：

- ✅ Provider id 字符串前缀匹配（`startsWith("openai")`）不能 catch 所有
  OpenAI-compatible 自定义端点；显式白名单拒绝误分类（错分类比不显示差）。
- ✅ 单元测试 [`cacheHitRate.test.ts`](apps/acowork-desktop/src/lib/cacheHitRate.test.ts)
  覆盖 23 个 case：协议分类、OpenAI 公式、Anthropic 公式、cumulative 优先、
  Bedrock、null 兜底、NaN/Infinity、clamp。
- ⚠️ 新增 cache-aware provider 时需要修改白名单 + 加测试——这是显式代价。

**结论**：双公式实现完全符合 ADR §6 设计；"按 provider 类型切换" 通过
`getCacheProtocol` 实现。

### 4. `compute_context_usage` 设计哲学：caller patch 散落 → `patch_session_totals` helper

**ADR 原描述**（§2 推送路径）："3 个 `ContextUsageInfo` 构造点统一更新...`ctx_info`
构造时 `cache_read_tokens` / `cache_write_tokens` 设 `Some(usage.*)`，cumulative
session 字段由 caller patch。"

**实际实施**：评审阶段发现 3 个 push 路径（主 push / context_window 推送 /
compaction 推送）独立 inline patch 4 个 cumulative session 字段，存在重复和
漂移风险。重构为：

[`core/acowork-runtime/src/agent/context.rs::patch_session_totals`](core/acowork-runtime/src/agent/context.rs)
单一 helper，统一处理 `total_input_tokens` / `total_output_tokens` /
`total_cache_read_tokens` / `total_cache_write_tokens`。3 个 push 路径都调用此
helper。

**理由**：

- ✅ 集中 patch = 单一真相来源，避免 P0 bug 1（漏 patch `total_cache_*`）复发。
- ✅ 不破坏现有语义，cumulative 仍由 caller 提供，per-turn 仍由
  `compute_context_usage` / `build_context_usage_from_persisted` 填。
- ✅ `build_context_usage_from_persisted`（resume 路径）也走同一 helper。

**结论**：架构合理化 = 接受重构。

### 5. 端到端测试补足

**ADR 原描述**（§测试）：未明确列出 e2e 集成测试，但 §7 数据流总图描绘了
"Provider → meta → push → UI 全链路"。

**实际实施**（评审期补足）：
[`core/acowork-runtime/tests/context_usage_cache_e2e.rs`](core/acowork-runtime/tests/context_usage_cache_e2e.rs)
覆盖：
1. Provider 响应 → `ConversationSession::accumulate_llm_usage` →
   `SessionTokens` 持久化往返。
2. `AgentCore::accumulate_llm_usage` 4 元组累加 + atomic max merge。
3. `patch_session_totals` 主 push 路径 → 验证 `total_cache_read_tokens`
   等 4 字段非 None。
4. `ContextUsageInfo` JSON wire format 含 6 cache 字段；旧 v3 meta 反序列化兼容。
5. `build_context_usage_from_persisted` resume 路径填充 per-turn + cumulative cache。

**结论**：补足 e2e = 闭环验证 P0 bug 1 不再复发。