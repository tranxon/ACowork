# 15 — 流式错误恢复方案实现评审

> **日期**: 2026-05-15
> **基准**: review #14 方案建议
> **范围**: 23 files changed, +476/-153 lines
> **类型**: 代码实现评审

---

## 0. 变更概览

| 模块 | 改动要点 | 对应 #14 方案 |
|------|---------|-------------|
| Core: `traits.rs` | 新增 `StreamError` 结构体、`ContextOverflow`/`StreamDecodeError`/`StreamTimeout` 错误类型 | P0 方案一 |
| Core: `error_patterns.rs` | 集中式错误字符串→结构化类型分类 | P0 方案一 |
| Runtime: `anthropic.rs` | 45s per-chunk read timeout + 结构化错误 | P0 方案二 |
| Runtime: `openai.rs` | 同上 | P0 方案二 |
| Runtime: `reliable.rs` | 流式重试架构（provider链 + per-provider 指数退避） | P1/P2 |
| Runtime: `loop_.rs` | 迭代级重试 + 请求体积告警/熔断 | P1 |
| Runtime: `loop_llm.rs` | 结构化错误匹配替换字符串匹配 | 配套 |
| Runtime: `error.rs` | `StreamError` 新 variant | 配套 |
| 测试 | sleep reduction（349s→~29s） | 配套 |

---

## 1. P0 — 必须修复

### 1.1 `error_patterns.rs` 未纳入 commit（编译阻断）

**位置**: `core/rollball-core/src/providers/mod.rs` line 6 + `error_patterns.rs`

**症状**:
```
core/rollball-core/src/providers/mod.rs:6 pub mod error_patterns;
```
该模块在 `mod.rs` 中声明为 `pub mod error_patterns` 并通过 `pub use` 导出三个函数。但 `error_patterns.rs` 文件状态为 **Untracked**（`git status` 显示未 `git add`），不在当前 commit 中。

**后果**: 任何 `git checkout` 此 commit 的用户都会遇到编译错误：
```
error[E0583]: file not found for module `error_patterns`
```

**修复**: `git add core/rollball-core/src/providers/error_patterns.rs && git commit --amend`

---

### 1.2 请求体积硬限制：裁剪后的 `chat_request` 被丢弃

**位置**: `core/rollball-runtime/src/agent/loop_.rs` 行 ~922-937

**代码**:
```rust
if request_size > REQUEST_SIZE_HARD {
    let removed = self.session.history.emergency_trim();
    if removed > 0 {
        // Rebuild request with trimmed history.
        // The rebuilt chat_request shadows the outer one.
        let chat_request = context_builder.build(     // ← 局部 shadow binding
            &self.core.manifest,
            &self.session.history,
            self.get_model_capabilities(None),
            self.core.max_output_tokens_limit,
        );
        tracing::info!(
            request_size = serde_json::to_vec(&chat_request).map(|v| v.len()).unwrap_or(0),
            "Request size after emergency trim"
        );
    }
    // ← chat_request 在此处仍是原始的（未裁剪的）版本
}
// ← 发送给 LLM 的是原始 chat_request，裁剪版仅用于日志
```

**根因**: `let chat_request = ...` 创建了局部 shadow binding，仅在 `if removed > 0` 块内有效。块外的 `chat_request` 仍是 `context_builder.build()` 第一次调用返回的原始（超大）请求。

**后果**: 整个 P1 "请求体积告警/熔断" 方案 **完全失效**。当请求超过 280KB 硬限制时：
- `emergency_trim()` 确实裁剪了历史记录 ✅
- 裁剪后的请求确实被构建了 ✅
- 但裁剪后的请求**只用于日志打印** ❌
- 实际发送给 LLM 的是**原始超大请求** ❌

**修复**（二选一）:
```rust
// 方案 A: 使用可变绑定
let mut chat_request = context_builder.build(...);
// ... size check ...
if request_size > REQUEST_SIZE_HARD {
    let removed = self.session.history.emergency_trim();
    if removed > 0 {
        chat_request = context_builder.build(...);  // reassign, no let
    }
}

// 方案 B: 在 if 块内完成发送
if request_size > REQUEST_SIZE_HARD {
    let removed = self.session.history.emergency_trim();
    if removed > 0 {
        let chat_request = context_builder.build(...);
        // 在此处直接发送，而非继续使用外部 chat_request
    }
}
```

推荐方案 A，改动最小。

---

## 2. P1 — 应该修复

### 2.1 `loop_llm.rs` 上下文溢出恢复使用旧 `chat_request`

**位置**: `core/rollball-runtime/src/agent/loop_llm.rs` 行 ~238-264

**代码**:
```rust
StreamEvent::Error(e) => {
    if retry_on_overflow
        && e.error_type == ProviderErrorType::ContextOverflow
    {
        let removed = self.session.history.emergency_trim();
        if removed > 0 {
            // ... 日志 ...
            return self
                .call_llm_streaming_no_retry(&chat_request)  // ← 使用的是旧的 chat_request！
                .await;
        }
    }
}
```

**根因**: `emergency_trim()` 修改了 `self.session.history`，但 `chat_request` 是在此之前用旧 history 构建的。重试时传入的仍是旧 `chat_request`，裁剪无效。

**注意**: 这是**已有 bug**（原始代码就存在），不是本次引入。但在本次 PR 重构错误处理路径时，应该一并修复。

**严重性**: 比 1.2 稍低，因为 `retry_on_overflow` 默认为 `false`（仅在 `call_llm_streaming_with_retry` wrapper 中设为 `true`），正常路径不触发。但一旦触发，裁剪同样无效。

**修复**:
```rust
if removed > 0 {
    let rebuilt_request = context_builder.build(
        &self.core.manifest,
        &self.session.history,
        self.get_model_capabilities(None),
        self.core.max_output_tokens_limit,
    );
    return self.call_llm_streaming_no_retry(&rebuilt_request).await;
}
```

---

### 2.2 迭代级重试与 `chat_request` 一致性问题

**位置**: `core/rollball-runtime/src/agent/loop_.rs` 行 ~722-747

**分析**: 迭代级重试循环包裹了 `execute_single_iteration(iteration, context_builder, user_message, &retrieved_memory_ids)`。每次重试时：
- `context_builder` 和 `user_message` 是引用，不拥有数据
- 如果上一次尝试中 history 被修改（如 `emergency_trim` 在 1.2/2.1 路径中），重试时 `context_builder` 持有的 history 引用会反映修改
- 但 `chat_request` 在 `execute_single_iteration` 内部构建，不跨重试

**结论**: 目前看没有正确性问题（history 的修改会通过引用透明反映），但修复 1.2 和 2.1 后需要重新验证此路径。

---

### 2.3 `error_patterns.rs` 默认分类可能掩盖真实错误

**位置**: `core/rollball-core/src/providers/error_patterns.rs` 行 138-141

**代码**:
```rust
} else {
    // Default: treat as a transient network-like error, retryable.
    StreamError::stream_decode(msg.to_string())
}
```

**风险**: 所有无法匹配任何已知模式的错误都被归类为 `StreamDecodeError`（retryable=true）。如果出现某种新的严重错误（如服务端永久拒绝），会被不断重试直到达到 max_retries。

**建议**: 默认分类使用 `Unknown` 且 `retryable=false`，或至少对未知错误使用更保守的重试策略（如仅重试 1 次）。

---

### 2.4 ReliableProvider 流式重试的边界条件

**位置**: `core/rollball-runtime/src/providers/reliable.rs` 行 ~172-210

**分析**: 新实现对每个 provider 做 `max_attempts` 次重试（`chat_stream` 调用），失败后切换下一个 provider。这与 #14 建议的"流式重试是重新发起请求"一致。

**潜在问题**: 
- 如果 `primary` provider 的所有重试都用完，切换到 `fallback[0]`——这意味着相同的 request 会发送给不同的 provider/model。如果 primary 是 GPT-4o 而 fallback 是 DeepSeek-V3，可能产生不一致的结果。
- `sleep(wait)` 是同步阻塞（来自 `std::thread::sleep`），在一个 async 上下文中会阻塞当前线程。不过如果这是在 tokio 的 blocking task 中运行则无问题——需要确认调用上下文。

---

## 3. P2 — 改进建议

### 3.1 硬编码阈值应可配置

| 常量 | 值 | 文件 |
|------|-----|------|
| `STREAM_READ_TIMEOUT` | 45s | anthropic.rs, openai.rs |
| `REQUEST_SIZE_WARN` | 200KB | loop_.rs |
| `REQUEST_SIZE_HARD` | 280KB | loop_.rs |
| `MAX_ITERATION_RETRIES` | 2 | loop_.rs |

建议从 manifest 或配置文件读取，允许不同 Agent/model 使用不同阈值。

### 3.2 `is_balance_exhausted` 包含 MiniMax 业务码

**位置**: `core/rollball-runtime/src/providers/reliable.rs` 行 99 + `error_patterns.rs` 行 108

**代码**:
```rust
if lower.contains("1113") || lower.contains("1311") {
    return true;
}
```

业务码 1113（MiniMax 余额不足）和 1311 是 provider-specific 的。考虑到 `error_patterns.rs` 设计为 provider-agnostic 的集中分类模块，这些业务码放在这里不太合适。建议通过 Provider 自身的错误映射来处理。

### 3.3 `episode_distill.rs` 错误类型变更

**位置**: `core/rollball-runtime/src/episode_distill.rs` 行 268

**变更**: `RuntimeError::Provider(format!(...))` → `RuntimeError::Core(e)`

这是一个语义变更：distillation 的 LLM 调用失败原来是 `Provider` 错误，现在是 `Core` 错误。如果上游有按 `RuntimeError` variant 做分支处理的代码，这个变更会影响行为。需要确认是否所有 consumer 都正确处理。

### 3.4 sleep 优化质量

测试 sleep 优化（349s→29s）总体正确，但有几个细节：

| 测试 | 旧值 | 新值 | 评估 |
|------|------|------|------|
| e2e: 3 session concurrent | 2s | 300ms | ✅ MockProvider 是即时的 |
| e2e: long message 3s→300ms | 3s | 300ms | ✅ 同上 |
| e2e: multi-turn 500ms→150ms | 500ms | 150ms | ✅ 同上 |
| conv: 200ms→50ms | 200ms | 50ms | ⚠️ 如果 writer thread 负载高可能 flaky |
| conv: 500ms→100ms | 500ms | 100ms | ⚠️ 同上 |

对于使用 JSONL writer 的 conversation 测试，50ms 可能在某些 CI 环境中不够。建议改为 `std::thread::sleep(Duration::from_millis(50))` → `100ms` 以获得更好的 CI 稳定性。

---

## 4. 架构评价

### 4.1 做得好的

1. **`StreamError` vs `ProviderError` 分离**：这是整个 PR 最核心的正确设计决策。`StreamError` 携带 `error_type` + `retryable` 元数据，让上游（AgentLoop、ReliableProvider）无需字符串匹配即可做重试决策。完全符合 #14 方案一的意图。

2. **Per-chunk read timeout（45s）**：解决 #14 发现的"2 分钟干等"根因。且使用 `tokio::time::timeout` 包装而非修改 `reqwest` 配置，是正确的实现方式。

3. **错误分类集中化**：`error_patterns.rs` 提供了 `classify_stream_error` 统一入口，覆盖 Anthropic/OpenAI/MiniMax/DeepSeek/Ollama/Mistral/llama.cpp 等主流 provider 的溢出/解码/认证错误模式，且带有完整测试。

4. **`ReliableProvider` 流式重试重构**：从简单的 primary→fallback 升级为 provider 链 + per-provider 指数退避，与 ZeroClaw 的三层架构对齐。

### 4.2 需要关注的

1. **双重重试可能过度**：ReliableProvider 在 provider 层重试 `chat_stream`（per-provider max_attempts 次），AgentLoop 在迭代层重试 `execute_single_iteration`（最多 2 次）。如果两层都触发，可能出现 N × 2 次重试。需要确认 `MAX_ITERATION_RETRIES` 和 `ReliableProvider.max_attempts` 的乘积在合理范围内。

2. **`error_patterns.rs` 不在 commit 中**：如前所述，这是编译阻断问题。

---

## 5. 总结

| 优先级 | 数量 | 关键问题 |
|--------|------|---------|
| P0 | 2 | error_patterns.rs 未纳入 commit（编译阻断）；请求体积硬限制裁剪后 chat_request 被丢弃（功能失效） |
| P1 | 4 | 上下文溢出恢复使用旧 chat_request（已有 bug）；迭代重试一致性；默认错误分类过激进；ReliableProvider 边界条件 |
| P2 | 4 | 硬编码阈值；业务码耦合；错误类型变更兼容性；sleep CI 稳定性 |

**建议**: 修复 P0 后即可合入，P1 可在后续 PR 中修复（2.1 是已有 bug，2.3 是设计权衡，2.4 需要更多验证）。
