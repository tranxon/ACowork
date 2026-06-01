# 14 — Agent Runtime 流式中断故障分析：Provider 错误恢复策略调研

> **日期**: 2026-05-15
> **触发**: Senior Engineer Agent 长时间推理（~30 轮 tool 调用）后流式响应中断
> **错误**: `Provider error: Stream error: error decoding response body`
> **类型**: 故障分析 + 竞品调研 + 修复方案建议

---

## 1. 故障复盘

### 1.1 现象

- Agent 执行约 30 轮 tool 调用后，聊天界面显示 `Error: Error: Provider error: Stream error: error decoding response body`
- 发送按钮不再显示 stop 状态，说明 runtime 侧 agent loop 已退出
- 用户无法继续对话，必须新建 session

### 1.2 日志时间线

| 时间 | 事件 | 请求大小 | 历史token |
|------|------|---------|----------|
| 09:43:46 | Runtime 启动，连接 Gateway | - | - |
| 09:45:07 | 用户发送第一条消息 | - | 71 |
| 09:45:24 | 第 2 次 LLM 请求 | - | 2,976 |
| ... | 持续 tool 调用循环 | 递增 | 递增 |
| 09:48:53 | 第 43 次迭代 | 251,174 bytes | 54,831 |
| 09:49:11 | 第 50 次迭代 | 266,375 bytes | 58,179 |
| 09:50:17 | 第 61 次迭代 | 282,579 bytes | 61,632 |
| **09:50:26** | **最后一次请求发出** | **283,364 bytes** | **61,732** |
| 09:50:26→09:52:26 | **2 分钟静默期（无任何日志）** | - | - |
| **09:52:26** | **❌ SessionTask agent loop error** | - | - |

### 1.3 根因分析

**直接原因**：MiniMax API 流式响应在传输过程中被截断，h2 解码失败。

**证据链**：

1. **2 分钟静默 = HTTP 超时**：最后请求发出后整整 2 分钟才报错，恰好等于 `anthropic.rs:38` 设置的 `Duration::from_secs(120)`
2. **h2 流中断**：`error decoding response body` 来自 HTTP/2 层，说明 SSE 流在传输中被服务端或中间代理强制关闭
3. **请求体持续膨胀**：从 251KB 增长到 283KB，MiniMax 服务端在处理大请求时可能触发内部超时
4. **无重试无恢复**：AnthropicProvider 的 `chat_stream` 遇到流错误直接返回 `StreamEvent::Error` 然后 `return`，AgentLoop 传播错误，SessionTask 记录错误并终止

**根本原因**：Provider 层缺少流式重试机制 + 请求体积无上限保护 + read timeout 缺失导致 2 分钟干等。

### 1.4 完整故障链

```
请求体积膨胀 (283KB / 61K tokens)
    → MiniMax API 处理耗时过长
        → 服务端/中间代理超时，强制断开 HTTP/2 stream
            → h2 解码失败: "error decoding response body"
                → AnthropicProvider 无重试，直接返回 StreamEvent::Error
                    → AgentLoop 传播错误
                        → SessionTask 记录错误，发送 ChunkEvent::Error
                            → Gateway 转发 agent_error 给前端
                                → 前端显示 "Error: Provider error: Stream error: ..."
                                    → Agent Loop 退出，用户无法继续
```

### 1.5 相关源码位置

| 文件 | 行号 | 问题 |
|------|------|------|
| `rollball-runtime/src/providers/anthropic.rs` | 38-39 | HTTP 超时仅全局 120s，无 per-chunk read timeout |
| `rollball-runtime/src/providers/anthropic.rs` | 546-553 | 流错误直接 return，无重试 |
| `rollball-runtime/src/agent/loop_.rs` | - | AgentLoop 对 Provider error 无重试逻辑 |
| `rollball-runtime/src/agent/session/session_task.rs` | 363-381 | SessionTask 收到 error 直接发 ChunkEvent::Error 终止 |

---

## 2. 主流编程 Agent 竞品调研

### 2.1 ZeroClaw（RollBall 的参考实现）

**三层重试架构** (`ReliableProvider`，`zeroclaw/src/providers/reliable.rs`)：

```
外层循环：模型降级链 (primary → fallback1 → fallback2)
中层循环：Provider 轮换 (同一模型的多个 API key 轮转)
内层循环：同一 (provider, model) 的指数退避重试
```

**指数退避实现**：

```rust
// 初始 1s，2 倍增长，上限 10s；尊重 Retry-After 头，上限 30s
backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
let wait = compute_backoff(backoff_ms, &e);  // Retry-After 覆盖
tokio::time::sleep(Duration::from_millis(wait)).await;
```

**错误分类**（5 类，决定是否重试）：

| 类别 | 判断函数 | 是否可重试 | 典型模式 |
|------|---------|-----------|---------|
| 认证失败 | `is_non_retryable()` | ❌ | `invalid api key`, `unauthorized`, `forbidden` |
| 模型不存在 | `is_non_retryable()` | ❌ | `model not found`, `invalid model` |
| 业务级限流 | `is_non_retryable_rate_limit()` | ❌ | `insufficient balance`, 业务码 1113/1311 |
| 上下文溢出 | `is_context_window_exceeded()` | ✅（裁剪后） | `exceeds the context window`, `too many tokens` |
| 临时错误 | 默认 | ✅ | 5xx, 429, timeout, connection reset |

**上下文溢出自动恢复**：

```rust
if is_context_window_exceeded(&e) && !context_truncated {
    let dropped = truncate_for_context(&mut effective_messages);  // 丢掉一半旧消息
    if dropped > 0 {
        context_truncated = true;
        continue;  // 用裁剪后的消息重试
    }
}
```

**上下文压缩** (`ContextCompressor`，`zeroclaw/src/agent/context_compressor.rs`)：三阶段

1. **Fast-trim**（无 LLM 调用）：裁剪旧 tool result 到 2,000 字符
2. **LLM 摘要**（60s 超时）：保护 head/tail，压缩中间段为结构化摘要
3. **被动压缩**（出错时）：解析错误中的 context limit 值，逐步降级探测

```rust
const PROBE_TIERS: &[usize] = &[2_000_000, 1_000_000, 512_000, 200_000, 128_000, 64_000, 32_000];
```

**Tool Schema 降级**：检测 `invalid_tool_call` → 自动切换为 prompt 引导模式

**流式错误处理**：`ReliableProvider` 不在流内重试，而是传播错误让调用方决定是否重试整个请求。

---

### 2.2 OpenCode

**Effect Schedule 重试** (`opencode/packages/opencode/src/session/retry.ts`)：

```typescript
RETRY_INITIAL_DELAY = 2000      // 2s 初始
RETRY_BACKOFF_FACTOR = 2        // 2 倍增长
RETRY_MAX_DELAY_NO_HEADERS = 30_000  // 无 Retry-After 时上限 30s
```

**15+ 种 Provider 溢出模式检测** (`opencode/packages/opencode/src/provider/error.ts`)：

```typescript
const OVERFLOW_PATTERNS = [
  /prompt is too long/i,                        // Anthropic
  /exceeds the context window/i,                // OpenAI
  /maximum context length is \d+ tokens/i,      // OpenRouter, DeepSeek
  /exceeds the available context size/i,         // llama.cpp
  /context[_ ]length[_ ]exceeded/i,             // Generic
  /request entity too large/i,                   // HTTP 413
  /prompt too long; exceeded (?:max )?context/i, // Ollama
  /too large for model with \d+ maximum/i,       // Mistral
  /model_context_window_exceeded/i,              // z.ai
  // ... 共 20+ 种模式
]
```

**流式错误结构化分类**：

```typescript
parseStreamError(input) →
  | { type: "context_overflow", message }      // 不可重试，需要压缩
  | { type: "api_error", isRetryable: true }   // 服务端错误，可重试
  | { type: "api_error", isRetryable: false }  // 配额/认证错误，不可重试
```

**上下文压缩** (`SessionCompaction`)：

- 保护最近 40K tokens 的 tool 调用
- 旧 tool 输出裁剪到 2,000 字符
- 用结构化模板调用 LLM 生成摘要

---

### 2.3 Aider

**无限重试 + 23 种异常分类表**：

| 配置项 | 值 |
|--------|-----|
| 初始退避 | 0.125s |
| 退避因子 | 2x |
| 最大重试次数 | **无限制** |

Aider 的哲学是"**永远不要因为临时错误停止**"。对于 5xx / 429 / 网络错误会无限重试，只有认证失败和上下文溢出才停止。23 种异常类型每种都明确标记 `retry` / `no_retry` / `drop_content`。

---

### 2.4 Claude Code

**5 级压缩级联**：

```
Level 1: 移除旧 tool result 中的大块输出
Level 2: 压缩 tool result 为摘要
Level 3: 用 LLM 生成对话摘要替代中间历史
Level 4: 激进裁剪（仅保留 system + 最近 N 轮）
Level 5: 新建 session（完全重新开始）
```

**已知不足**：Provider 层面 **没有重试逻辑**，流式中断会直接停止。与 RollBall 当前行为相同。

---

### 2.5 Cline

- 仅重试 429，最多 3 次
- **致命缺陷**：`await iterator.next()` 无 read timeout → TCP 半开连接导致无限挂起

---

### 2.6 核心差异对比

| 能力 | ZeroClaw | OpenCode | Aider | Claude Code | Cline | **RollBall (当前)** |
|------|----------|----------|-------|-------------|-------|---------------------|
| Provider 重试 | ✅ 三层 | ✅ Effect Schedule | ✅ 无限 | ❌ 无 | ⚠️ 仅 429 | ❌ 无 |
| 指数退避 | ✅ 2x cap 10s | ✅ 2x cap 30s | ✅ 2x 从 0.125s | ❌ | ⚠️ 固定 | ❌ 无 |
| Retry-After 支持 | ✅ cap 30s | ✅ | ❌ | ❌ | ❌ | ❌ 无 |
| 错误分类 | ✅ 5 类 | ✅ 15+ 模式 | ✅ 23 种 | ❌ 二元 | ⚠️ 仅 429 | ❌ 无 |
| 模型降级 | ✅ fallback chain | ❌ | ❌ | ❌ | ❌ | ❌ 无 |
| 上下文压缩 | ✅ 三阶段 | ✅ 结构化摘要 | ⚠️ 简单裁剪 | ✅ 五级 | ❌ | ✅ preemptive trim |
| 上下文溢出重试 | ✅ 自动裁剪+重试 | ❌ 直接报错 | ❌ | ⚠️ 手动 | ❌ | ❌ 无 |
| Tool Schema 降级 | ✅ prompt 引导 | ❌ | ❌ | ❌ | ❌ | ❌ 无 |
| Read Timeout | ⚠️ 120s 全局 | ⚠️ race timeout | ❌ | ❌ | ❌ 致命缺陷 | ⚠️ 120s 全局 |
| 流式静默检测 | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |

> 💡 **"流式静默检测"**——即 SSE 流已建立但长时间无数据返回——是**所有 Agent 的共同盲区**。没有任何一家实现了 read-timeout-based 的静默检测。

---

## 3. 修复方案建议

### 3.1 优先级排序

| 优先级 | 方案 | 工作量 | 影响 |
|--------|------|--------|------|
| **P0** | Provider 层流式重试 | 1-2 天 | 直接解决本次故障 |
| **P0** | 增加 per-chunk read timeout | 0.5 天 | 防止 2 分钟干等 |
| **P1** | 请求体积告警/熔断 | 1 天 | 主动预防大请求 |
| **P1** | AgentLoop 级别有限重试 | 1 天 | 兜底恢复 |
| **P2** | ReliableProvider 三层重试架构 | 3-5 天 | 长期架构升级 |
| **P2** | 流式静默检测 | 1 天 | 行业首创 |

### 3.2 P0 方案一：Provider 层流式重试

**改动位置**：`rollball-runtime/src/providers/anthropic.rs`

**设计**：在 `chat_stream` 外包装 `chat_stream_with_retry`，对可重试的流式错误自动重发请求。

```rust
/// Retryable stream error classification
fn is_retryable_stream_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    // Stream decode errors, connection resets, timeouts → retryable
    (lower.contains("error decoding")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("timed out")
        || lower.contains("io error"))
    // Exclude non-retryable patterns
    && !lower.contains("unauthorized")
    && !lower.contains("invalid api key")
    && !lower.contains("authentication")
    && !lower.contains("context length")
    && !lower.contains("token limit")
}
```

```rust
pub async fn chat_stream_with_retry(
    &self,
    request: &ChatRequest,
    max_retries: u32,
) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
    let mut backoff_ms: u64 = 1000;
    for attempt in 0..=max_retries {
        match self.chat_stream(request).await {
            Ok(stream) => return Ok(stream),
            Err(e) if is_retryable_stream_error(&e.to_string()) && attempt < max_retries => {
                tracing::warn!(
                    attempt,
                    backoff_ms,
                    error = %e,
                    "Stream error, retrying..."
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
```

**注意事项**：
- 重试时必须用**相同的 request** 重新发起，因为流式响应一旦中断，无法从断点续传
- 已收到的部分响应需要丢弃（因为是不完整的 SSE 事件序列）
- 重试次数建议 2 次（即最多 3 次尝试），避免无限循环

### 3.3 P0 方案二：增加 Per-Chunk Read Timeout

**改动位置**：`rollball-runtime/src/providers/anthropic.rs` 的 SSE 解析循环

**设计**：在每次 `chunk.next()` 调用外包装 `tokio::time::timeout`，避免在服务端挂起时干等 2 分钟。

```rust
const STREAM_SILENCE_TIMEOUT: Duration = Duration::from_secs(45);

// 在 SSE 解析循环中
loop {
    match tokio::time::timeout(STREAM_SILENCE_TIMEOUT, stream.next()).await {
        Ok(Some(chunk)) => { /* 正常处理 */ },
        Ok(None) => break,  // 流正常结束
        Err(_) => {
            // 45 秒无数据，服务端可能挂起
            tracing::warn!(
                "Stream silence detected ({}s), treating as retryable error",
                STREAM_SILENCE_TIMEOUT.as_secs()
            );
            let _ = tx.send(Some(StreamEvent::Error(
                format!("Stream timeout: no data received for {}s", STREAM_SILENCE_TIMEOUT.as_secs())
            ))).await;
            return;
        }
    }
}
```

**注意**：`reqwest` 不直接支持 `read_timeout`，需要用 `tokio::time::timeout` 包装。这个超时应该比全局 HTTP timeout 短（建议 45-60s vs 300s），这样能在全局超时前先触发，给重试留出时间。

### 3.4 P1 方案：请求体积告警/熔断

**改动位置**：`rollball-runtime/src/agent/loop_.rs`

**设计**：在每次发送 LLM 请求前检查 request_len，超过阈值时触发更激进的上下文压缩。

```rust
const REQUEST_SIZE_WARN_THRESHOLD: usize = 200_000;   // 200KB 警告
const REQUEST_SIZE_HARD_LIMIT: usize = 280_000;        // 280KB 硬限制

fn check_request_budget(request_len: usize, history_tokens: u64) -> LoopControl {
    if request_len > REQUEST_SIZE_HARD_LIMIT {
        tracing::error!(
            request_len,
            history_tokens,
            "Request body exceeds hard limit, forcing emergency trim"
        );
        LoopControl::EmergencyTrim
    } else if request_len > REQUEST_SIZE_WARN_THRESHOLD {
        tracing::warn!(
            request_len,
            history_tokens,
            "Request body approaching size limit, preemptive trim recommended"
        );
        LoopControl::PreemptiveTrim
    } else {
        LoopControl::Continue
    }
}
```

### 3.5 P1 方案：AgentLoop 级别有限重试

**改动位置**：`rollball-runtime/src/agent/loop_.rs`

**设计**：在 AgentLoop 的 `run()` 方法中，对 Provider error 做有限次重试，而非直接终止。

```rust
// 在 agent loop 的迭代中
match llm_response {
    Err(e) if is_retryable_provider_error(&e) && retry_count < MAX_ITERATION_RETRIES => {
        tracing::warn!(
            iteration,
            retry_count,
            error = %e,
            "Provider error in agent loop, retrying iteration"
        );
        retry_count += 1;
        let backoff = Duration::from_millis(1000 * 2_u64.pow(retry_count as u32 - 1));
        tokio::time::sleep(backoff.min(Duration::from_secs(10))).await;
        continue;  // 重试当前迭代
    }
    Err(e) => return Err(e),  // 不可重试或超过次数
    Ok(response) => {
        retry_count = 0;  // 成功后重置
        // ... 正常处理
    }
}
```

### 3.6 P2 方案：ReliableProvider 架构

参考 ZeroClaw 的 `ReliableProvider`，将重试逻辑从单个 Provider 抽象为包装器：

```rust
pub struct ReliableProvider {
    providers: Vec<(String, Box<dyn Provider>)>,  // (name, provider) 列表
    max_retries: u32,
    base_backoff_ms: u64,
    api_keys: Vec<String>,                         // API key 轮换池
    key_index: AtomicUsize,
    model_fallbacks: HashMap<String, Vec<String>>,  // model -> [fallback1, fallback2]
}
```

三层重试：模型降级链 → Provider 轮换 → 指数退避。这是长期架构升级，建议在 P0/P1 修复完成后再实施。

### 3.7 P2 方案：流式静默检测（行业首创）

这是目前所有编程 Agent 都未解决的问题。RollBall 可以率先实现：

在 SSE 解析循环中加入 per-chunk read timeout（即 P0 方案二），当连续 N 秒无新数据到达时，主动判定为流式静默并触发重试。这比等待全局 HTTP timeout 快得多（45s vs 120s），能显著改善用户体验。

---

## 4. 附录：RollBall 当前代码中的相关防御机制

RollBall Runtime 已有部分防御机制，但在此次故障中未能覆盖：

| 机制 | 位置 | 状态 | 说明 |
|------|------|------|------|
| Preemptive trim | `agent/context.rs` | ✅ 生效 | 在历史 token 超过阈值时主动裁剪 |
| Token budget 计算 | `agent/context.rs` | ✅ 生效 | 基于 context_window 计算 max_output_tokens |
| History truncation | `agent/history.rs` | ✅ 生效 | FIFO 式历史裁剪 |
| Episode distillation | `agent/loop_.rs` | ⚠️ 非致命失败 | session 级 distillation 失败（os error 2），但不影响主流程 |
| Provider retry | `providers/anthropic.rs` | ❌ 缺失 | 流式错误直接终止，无重试 |
| Read timeout | `providers/anthropic.rs` | ❌ 缺失 | 仅全局 120s timeout |
| Error classification | - | ❌ 缺失 | 无可重试/不可重试区分 |
| Stream silence detection | - | ❌ 缺失 | 无 per-chunk read timeout |

---

## 5. 建议实施顺序

```
Week 1: P0 修复（Provider 重试 + Read timeout）  → 直接解决本次故障
Week 2: P1 增强（请求体积告警 + AgentLoop 重试） → 预防和兜底
Week 3-4: P2 架构（ReliableProvider + 流式静默检测） → 长期健壮性
```
