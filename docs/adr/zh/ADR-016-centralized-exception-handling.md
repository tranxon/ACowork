# ADR-016: 集中化异常处理架构 — 分类归 Core、编排归 Reliable、展示归前端

**状态**：草案（待实施）
**日期**：2026-06-23
**决策者**：架构讨论
**影响范围**：

- `core/acowork-core/src/providers/traits.rs`（ProviderError 新增 `user_message` 字段）
- `core/acowork-core/src/providers/error_patterns.rs`（统一分类中心，新增 `from_http_response` / `is_balance_exhausted` / `parse_retry_after_header` / `to_user_friendly`）
- `core/acowork-runtime/src/providers/mod.rs`（`parse_retry_after_header` 移入 core）
- `core/acowork-runtime/src/providers/reliable.rs`（删除 `is_retryable` / `is_balance_exhausted`，改为调用 core）
- `core/acowork-runtime/src/providers/sse_stream.rs`（**新增**：通用 SSE 流读取器）
- `core/acowork-runtime/src/providers/openai.rs`（删除 `sse_to_stream`，改为调用通用模块）
- `core/acowork-runtime/src/providers/anthropic.rs`（同上）
- `core/acowork-runtime/src/providers/ollama.rs`（HTTP 错误转换改为调用 core）
- `core/acowork-runtime/src/agent/loop_.rs`（`ChunkEvent::Error` 携带结构化错误信息）
- `core/acowork-runtime/src/agent/loop_llm.rs`（`StreamEvent::Error` 处理接入 retryable 判定）
- `core/acowork-runtime/src/agent/session/session_task.rs`（错误消息格式化）
- `core/acowork-runtime/src/startup/subsystems.rs`（relay 携带结构化错误字段）
- `apps/acowork-desktop/src/stores/chatStore.ts`（错误消息渲染：摘要 + 详情折叠）

---

## 背景

### 问题 1：异常处理代码分散且重复

当前 LLM 异常处理逻辑散落在 7 个文件、3 个 crate 中，存在三块明显重复：

1. **HTTP → ProviderError 转换**：`openai.rs`、`anthropic.rs`、`ollama.rs` 各写一遍相同的 `from_status_code + parse_retry_after_header + set retry_after_ms` 模式
2. **SSE 流读取循环**：`openai.rs` 的 `sse_to_stream` 和 `anthropic.rs` 的内联流循环几乎一模一样（timeout / error / silence 处理完全相同），仅行解析函数不同
3. **`is_retryable` 判断冗余**：`ProviderError.retryable` 字段在创建时已设好，但 `reliable.rs` 的 `is_retryable()` 又重新检查 `error_type`，逻辑重复

此外，`is_balance_exhausted` 和 `parse_retry_after_header` 放在了 runtime 层，但它们本质上是分类逻辑，应该在 core 层。

### 问题 2：前端错误信息不可读

当前错误传播路径：

```text
Provider (openai.rs)
  └─ ProviderError { message: "OpenAI API error: 429 - {\"error\":{\"message\":\"Rate limit exceeded\",...}}" }
      └─ RuntimeError::Provider(err) 或 RuntimeError::StreamError(err)
          └─ session_task.rs: format!("Error: {}", e)
              └─ ChunkEvent::Error { message: "Error: Provider error: OpenAI API error: 429 - {...}" }
                  └─ Gateway relay → 前端 chatStore.ts
                      └─ ChatMessage { type: "error", content: "Error: Provider error: OpenAI API error: 429 - {...}" }
```

前端直接展示 LLM API 返回的原始 JSON 错误体，用户体验差：

- 技术细节过多（HTTP 状态码、JSON 结构、内部错误码）
- 不同 Provider 的错误格式不统一，前端无法做条件渲染
- 无法区分"余额不足"（需充值）与"限流"（稍后重试）等需要用户不同操作的错误

### 问题 3：流中间断流无重试

`classify_stream_error` 已正确标记 `StreamDecodeError` 和 `StreamTimeout` 为 `retryable: true`，但 `loop_llm.rs` 的消费端忽略了这个标志——除了 `ContextOverflow`（裁剪后重试），其他所有流错误都直接失败。传输层瞬断（connection reset、broken pipe）这类常见且可重试的场景完全没有重试保护。

---

## 决策

### 决策 1：三层分离的异常处理架构

```
┌─────────────────────────────────────────────────────┐
│  acowork-core/providers/error_patterns.rs            │
│  ★ 唯一的错误分类中心                                │
│  - from_http_response()    HTTP → ProviderError      │
│  - classify_stream_error() string → StreamError      │
│  - is_balance_exhausted()  余额耗尽判定              │
│  - parse_retry_after_header()  Retry-After 解析      │
│  - to_user_friendly()      结构化错误 → 用户可读文本 │
│  - is_retryable()          统一可重试判定            │
└──────────────────────┬──────────────────────────────┘
                       │ 依赖
┌──────────────────────▼──────────────────────────────┐
│  acowork-runtime/providers/reliable.rs               │
│  ★ 纯重试编排（不持有任何分类逻辑）                   │
│  - RetryConfig / BackoffStrategy                     │
│  - compute_wait()                                    │
│  - retry_sleep() + UX                                │
│  - chat() / chat_stream() 重试循环                   │
└──────────────────────┬──────────────────────────────┘
                       │ 依赖
┌──────────────────────▼──────────────────────────────┐
│  acowork-runtime/providers/{openai,anthropic,...}    │
│  ★ 纯协议适配（不含错误分类/重试逻辑）                │
│  - 请求构建（native request）                        │
│  - SSE 行解析（provider-specific 格式）               │
│  - 调用 from_http_response() 处理 HTTP 错误           │
│  - 调用 sse_stream::sse_to_stream() 处理流            │
└─────────────────────────────────────────────────────┘
```

**原则**：错误从产生到处理有一条清晰的单向路径——Provider 产生 → core 分类 → reliable 决策重试 → loop_llm 消费 → 前端展示。

### 决策 2：统一 HTTP → ProviderError 转换

在 `error_patterns.rs` 新增 `from_http_response()`，收敛三个 provider 中重复的 HTTP 错误转换代码：

```rust
/// Unified HTTP response → ProviderError conversion.
/// Handles status code classification + Retry-After header parsing.
pub async fn from_http_response(
    response: reqwest::Response,
    provider_name: &str,
) -> Result<reqwest::Response, AcoworkError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let retry_after = parse_retry_after_header(response.headers());
    let body = response.text().await.unwrap_or_default();
    let mut err = ProviderError::from_status_code(
        status.as_u16(),
        format!("{provider_name} API error: {status} - {body}"),
    );
    err.retry_after_ms = retry_after;
    Err(AcoworkError::Provider(err))
}
```

各 provider 只需一行调用：

```rust
let response = from_http_response(response, "OpenAI").await?;
```

### 决策 3：通用 SSE 流读取器

新增 `sse_stream.rs` 模块，将 `openai.rs` 和 `anthropic.rs` 中重复的流读取循环合并为一个通用函数。Provider 只需提供行解析回调：

```rust
/// Generic SSE stream reader with timeout + error classification.
///
/// `line_parser` is provider-specific: takes a raw SSE line, returns
/// parsed StreamEvents. Everything else (timeout, error classification,
/// channel management) is shared.
pub fn sse_to_stream<F>(
    response: reqwest::Response,
    stream_read_timeout: Duration,
    line_parser: F,
) -> Box<dyn Stream<Item = StreamEvent> + Send>
where
    F: Fn(&str) -> Vec<StreamEvent> + Send + 'static,
{ ... }
```

### 决策 4：`is_balance_exhausted` 和 `parse_retry_after_header` 移入 core

- `is_balance_exhausted`（含 MiniMax 1113/1311 码检测）从 `reliable.rs` 移到 `error_patterns.rs`
- `parse_retry_after_header` 从 `runtime/providers/mod.rs` 移到 `error_patterns.rs`

`reliable.rs` 只做重试编排，不再持有分类逻辑。

### 决策 5：删除 `is_retryable` 中的冗余判断

```rust
// 改前：reliable.rs
fn is_retryable(error: &AcoworkError) -> bool {
    match error {
        AcoworkError::Provider(pe) => {
            pe.retryable
                || pe.error_type == RateLimited        // 冗余
                || pe.error_type == StreamDecodeError   // 冗余
                || pe.error_type == StreamTimeout       // 冗余
        }
        AcoworkError::RateLimited(_) => true,           // 死分支
        AcoworkError::Io(_) => true,
        _ => false,
    }
}

// 改后：直接读 ProviderError.retryable
fn is_retryable(error: &AcoworkError) -> bool {
    match error {
        AcoworkError::Provider(pe) => pe.retryable,
        AcoworkError::Io(_) => true,
        _ => false,
    }
}
```

分类职责完全交给 `from_status_code`（创建时设好 `retryable`），`reliable.rs` 只读取不重新判断。

### 决策 6：用户可读的错误消息（双部分错误结构）

#### 6.1 ProviderError 新增 `user_message` 字段

```rust
pub struct ProviderError {
    pub message: String,           // 原始错误信息（含 API 返回的 JSON body）
    pub user_message: String,      // 用户可读的摘要（中文/英文）
    pub status_code: Option<u16>,
    pub error_type: ProviderErrorType,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
}
```

#### 6.2 `to_user_friendly()` 分类生成用户可读消息

在 `error_patterns.rs` 中新增 `to_user_friendly()`，根据 `error_type` 生成结构化的用户可读消息：

```rust
pub fn to_user_friendly(err: &ProviderError) -> String {
    match err.error_type {
        ProviderErrorType::RateLimited => {
            if let Some(ms) = err.retry_after_ms {
                format!("请求过于频繁，请等待约 {} 秒后重试", ms / 1000)
            } else {
                "请求过于频繁，请稍后重试".to_string()
            }
        }
        ProviderErrorType::PaymentRequired => {
            "账户余额不足或配额已用完，请充值或更换 Provider".to_string()
        }
        ProviderErrorType::Unauthorized => {
            "API Key 无效或已过期，请在设置中检查".to_string()
        }
        ProviderErrorType::ServerError => {
            "服务商暂时不可用，请稍后重试".to_string()
        }
        ProviderErrorType::NetworkError => {
            "网络连接异常，请检查网络后重试".to_string()
        }
        ProviderErrorType::ContextOverflow => {
            "对话上下文过长，已自动压缩历史记录".to_string()
        }
        ProviderErrorType::StreamDecodeError => {
            "数据流传输异常，正在自动重试".to_string()
        }
        ProviderErrorType::StreamTimeout => {
            "响应超时，服务商可能暂时过载".to_string()
        }
        ProviderErrorType::ClientError => {
            "请求参数有误，请检查模型和工具配置".to_string()
        }
        ProviderErrorType::Unknown => {
            "发生未知错误，请查看详情或重试".to_string()
        }
    }
}
```

`from_http_response()` 在创建 `ProviderError` 时自动调用 `to_user_friendly()` 填充 `user_message`。

#### 6.3 ChunkEvent::Error 携带双部分错误信息

```rust
pub enum ChunkEvent {
    // ...
    Error {
        /// User-friendly error summary (shown by default)
        user_message: String,
        /// Raw error detail (shown when user clicks "Details")
        detail: String,
        /// Structured error type (for frontend conditional rendering)
        error_type: ProviderErrorType,
        message_id: String,
    },
}
```

#### 6.4 前端渲染

```tsx
// Error message with expandable details
function ErrorMessage({ userMessage, detail, errorType }) {
  const [showDetail, setShowDetail] = useState(false);

  return (
    <div className="error-message">
      <div className="flex items-center gap-2">
        <ErrorIcon type={errorType} />
        <span>{userMessage}</span>
        {detail && (
          <button onClick={() => setShowDetail(!showDetail)}>
            {showDetail ? "收起" : "详情"}
          </button>
        )}
      </div>
      {showDetail && (
        <pre className="error-detail">{detail}</pre>
      )}
    </div>
  );
}
```

### 决策 7：流中间断流接入重试

`loop_llm.rs` 中 `StreamEvent::Error` 的处理改为检查 `retryable` 标志：

```rust
StreamEvent::Error(e) => {
    // ContextOverflow: emergency trim + retry (existing logic)
    if retry_on_overflow && e.error_type == ContextOverflow {
        // ... existing emergency trim logic ...
    }
    // Retryable stream errors: delegate to ReliableProvider's retry loop
    else if e.retryable {
        return Err(RuntimeError::StreamError(e));
        // ↑ This propagates to loop_.rs which already has retry logic
        //   for RuntimeError::StreamError(ref err) if err.retryable
    }
    // Non-retryable: fail immediately
    else {
        return Err(RuntimeError::StreamError(e));
    }
}
```

同时统一 `chat()` 与 `chat_stream()` 的 fallback 重试策略（当前 `chat()` 的 fallback 无重试，`chat_stream()` 的有重试，二者不一致）。

---

## 实施计划

### Phase 1：Core 层集中化（无行为变更）

1. `parse_retry_after_header` 从 runtime 移入 `error_patterns.rs`
2. `is_balance_exhausted`（含 MiniMax 码）从 `reliable.rs` 移入 `error_patterns.rs`
3. 新增 `from_http_response()` 统一 HTTP 错误转换
4. `ProviderError` 新增 `user_message` 字段，新增 `to_user_friendly()`
5. `from_status_code` 和 `from_http_response` 自动填充 `user_message`

### Phase 2：Runtime 层去重（无行为变更）

6. 新增 `sse_stream.rs` 通用 SSE 流读取器
7. `openai.rs` 删除 `sse_to_stream`，改为调用通用模块
8. `anthropic.rs` 同上
9. `ollama.rs` HTTP 错误转换改为调用 `from_http_response()`
10. `reliable.rs` 删除 `is_retryable` 冗余判断，改为直接读 `retryable` 字段

### Phase 3：错误消息格式化（前端可见变更）

11. `ChunkEvent::Error` 扩展为 `{ user_message, detail, error_type, message_id }`
12. `session_task.rs` / `loop_llm.rs` / `loop_context.rs` 中所有 `ChunkEvent::Error` 构造点更新
13. `subsystems.rs` relay 传递新字段
14. 前端 `chatStore.ts` 渲染双部分错误消息

### Phase 4：流重试补全（行为变更）

15. `loop_llm.rs` `StreamEvent::Error` 处理接入 `retryable` 判定
16. 统一 `chat()` 与 `chat_stream()` 的 fallback 重试策略

---

## 迁移策略

- Phase 1-2 是纯重构，不改变运行时行为，可独立合并
- Phase 3 的 `ChunkEvent::Error` 字段变更需要 Runtime 和前端同步发布
- Phase 4 是行为变更，需要在 Phase 1-3 稳定后单独合并

---

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| `from_http_response` 是 async 函数，core 层此前无 async 依赖 | core 已依赖 `reqwest`（通过 `parse_retry_after_header` 接收 `HeaderMap`），async 只是扩展用法 |
| `ChunkEvent::Error` 字段变更导致前后端不兼容 | Phase 3 同步发布；过渡期前端兼容旧格式（`message` 字段作为 fallback） |
| 通用 SSE 流读取器的 `line_parser` 回调可能不适用于所有 provider | Anthropic 的多参数状态可通过 `Fn` 闭包捕获 `&mut` 状态来处理 |
| `user_message` 硬编码中文，不支持 i18n | 当前项目无 i18n 需求；如未来需要，可将 `to_user_friendly` 改为接受 locale 参数 |

---

## 验证标准

1. `grep -r "from_status_code.*retry_after_ms" core/acowork-runtime/src/providers/` 结果为空（所有 provider 改用 `from_http_response`）
2. `grep -r "classify_stream_error" core/acowork-runtime/src/providers/` 结果仅在 `sse_stream.rs` 中（不再散落在各 provider）
3. `grep -r "is_balance_exhausted\|is_minimax_balance_code" core/acowork-runtime/` 结果为空（已移入 core）
4. 前端错误消息不再包含原始 JSON body（除非用户点击"详情"）
5. 流中间断流（connection reset）触发自动重试而非直接失败
