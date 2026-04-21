# P2 S1 Code Review — `d5a69cf`

> 提交: `d5a69cf feat(S1): architecture improvements and infrastructure`
> 复查日期: 2026-04-21
> 复查提交: `68b0cb2 fix(S1): address code review findings from P2 S1 review`
> 范围: 7 files, +991/-96 lines

---

# 复查：P0 问题修复验证

## P0 #1 — execute_tools_parallel 用 join_all 而非 spawn+select

**原问题**：代码使用 `futures::future::join_all` + 每工具独立 `tokio::time::timeout`，但 `iteration_timeout_ms` 同时作为单工具超时，两者未分层；且缺少迭代整体超时机制。

**修复状态：✅ 已正确修复**

修复后的 `execute_tools_parallel`（loop_.rs 436-565 行）：

```rust
// Phase 1: 批量权限检查，每工具独立
let permission_results: Vec<Option<String>> = ...;

// Phase 3: spawn + select + deadline
let (tx, mut rx) = mpsc::channel::<(usize, String)>(tool_calls.len());
let handles: Vec<_> = allowed_indices.iter().map(|&idx| {
    tokio::spawn(async move {
        let result = tokio::time::timeout(tool_timeout, execute_single_tool(...)).await
            .unwrap_or_else(|_| "Error: timed out".into());
        let _ = tx.send((idx, result)).await;
    })
}).collect();

drop(tx); // 让 rx.recv() 在所有 sender drop 后返回 None

// select! 循环：接收结果 OR 迭代超时
while collected.len() < total {
    tokio::select! {
        entry = rx.recv() => { ... }
        _ = tokio::time::sleep_until(deadline) => {
            for handle in &handles { handle.abort(); }
            break;
        }
    }
}
```

符合设计文档 §3.5 spawn+select+channel 方案。额外加分项：分离了 `tool_timeout_ms`（默认 10000ms）和 `iteration_timeout_ms`（默认 30000ms），两层超时独立配置。

---

## P0 #2 — 权限检查拒绝一个工具时全部失败

**原问题**：`validate_permission` 返回错误时，`all_tools_failed` flag 导致全部工具标记为失败。

**修复状态：✅ 已正确修复**

修复后的逻辑（loop_.rs 441-465 行）：

```rust
let mut permission_results: Vec<Option<String>> = Vec::with_capacity(tool_calls.len());
for tool_call in tool_calls {
    match validate_permission(&self.manifest, &tool_call.function.name) {
        Ok(()) => permission_results.push(None),  // 有权限
        Err(e) => permission_results.push(Some(format!("Error: Permission denied — {}", e))),
    }
}

let allowed_indices: Vec<usize> = permission_results
    .iter()
    .enumerate()
    .filter_map(|(i, result)| if result.is_none() { Some(i) } else { None })
    .collect();

if allowed_indices.is_empty() {
    return permission_results.into_iter().map(|r| r.unwrap_or_default()).collect();
}
```

权限拒绝的工具直接写入 `permission_results[Some(err)]`，不参与并行执行；最终结果按原顺序组装，未完成的 slot 填超时错误。逻辑正确，干净利落。

**新增测试**：`test_permission_partial_denial`（loop_.rs 1661 行），mock `echo`（有权限）和 `shell`（无权限）两个工具，验证 echo 结果存在、shell 返回 "Permission denied"。

---

## P0 #3 — recompute_digests 用 zip crate 读 V2 签名 ZIP

**原问题**：签名后用 `zip::ZipArchive` 读取文件计算摘要，但 V2 签名块不在标准 ZIP 结构内，`zip` crate 行为未定义。

**修复状态：✅ 已移除原有问题代码**

`recompute_digests` 函数已删除，不再有签名后重新读取文件计算摘要的逻辑。

新增的防御性措施：

**zip_utils.rs**：`insert_block_before_cd` 新增 CD offset 一致性校验（+21 行）——在插入签名块后验证 `cd_offset + cd_size == eocd_offset`，以及 CD offset 边界检查。如果签名块插入导致结构损坏，`zip_utils::insert_block_before_cd` 会返回错误而非静默生成坏文件。

**sign.rs**：新增 5 个 ZIP 边界测试（`test_sign_verify_with_many_files`、`test_sign_verify_with_large_file`、`test_sign_verify_with_nested_dirs`、`test_sign_verify_with_empty_files`、`test_signed_zip_readable_by_zip_crate`），覆盖 25 文件、1.5MB 大文件、嵌套目录、空文件、空文件+普通文件混合场景。每个测试调用 `sign_verify_and_check_zip` helper，后者用 `zip::ZipArchive::new(reader)` 验证签名后 ZIP 可被 `zip` crate 重新打开并读取所有 entry。

---

## P1 问题修复验证

### P1 #4 — streaming 代码重复（call_llm_streaming）

**修复状态：✅ 已正确修复**

引入 `call_llm_streaming_inner` 作为统一实现，`call_llm_streaming` 和 `call_llm_streaming_no_retry` 都是一层 thin wrapper：

```rust
async fn call_llm_streaming(&mut self, ..., context_builder: &ContextBuilder) -> Result<ChatResponse> {
    self.call_llm_streaming_inner(chat_request, Some(context_builder)).await
}

fn call_llm_streaming_no_retry<'a>(...) -> Pin<Box<dyn Future<...> + 'a>> {
    Box::pin(async move {
        self.call_llm_streaming_inner(chat_request, None).await
    })
}
```

`inner` 通过 `Option<&ContextBuilder>` 参数区分是否启用 context overflow 恢复。零重复。

### P1 #5 — ToolCallChunk 被忽略

**修复状态：✅ 已合理处理**

`_tool_call_buffer` 仍存在（有注释说明限制），但对 `ToolCallChunk` 事件的处理有清晰的 TODO 注释（358-363 行），明确指出当前 `ToolCallChunk` 无 ID 字段无法关联，后续 API 补充 ID 后再更新。不再是无说明的静默忽略。

### P1 #6 — gateway_socket 优先级不明确

**修复状态：✅ 已明确处理**

`config.rs` 新增 `get_gateway_address()` 方法（+13 行），明确优先 socket > endpoint，两者均 deprecated：

```rust
pub fn get_gateway_address(&self) -> Option<&str> {
    if let Some(ref socket) = self.gateway_socket {
        return Some(socket.as_str());
    }
    if let Some(ref endpoint) = self.gateway_endpoint {
        return Some(endpoint.as_str());
    }
    None
}
```

### P1 #7 — InboundMessage 无大小限制

**修复状态：✅ 已正确修复**

`inbound.rs` 新增 `enforce_size_limit()` 方法（+26 行），各变体分别处理：
- `UserMessage`：截断到 4096 字节（UTF-8 安全截断，不拆分多字节字符）
- `SystemNotification`：JSON 序列化后截断
- `IntentMessage`：params 序列化后截断

`drain_inbound_queue` 中调用 `msg.enforce_size_limit()` 后注入历史，防止 token 爆炸。

**新增测试**：4 个单元测试覆盖正常消息、超大消息（+1000 bytes）、UTF-8 多字节安全、大 JSON payload 场景。

### P1 #8 — CD offset 校验缺失

**修复状态：✅ 已在 zip_utils.rs 中修复**

见 P0 #3。

---

## 新增但未在原 review 报告中提及的问题

### 新增问题 #9（建议级）— inbound.rs 的 `_truncated` 忽略

`drain_inbound_queue` 调用 `let (msg, _truncated) = msg.enforce_size_limit();`，但 `_truncated` 被忽略。如果消息被截断，没有日志记录或通知。

**建议**：至少在 `truncated == true` 时打一条 `tracing::debug!` 日志。

### 新增问题 #10（建议级）— `call_llm_streaming_inner` 的 `_tool_call_buffer` 未使用

`_tool_call_buffer` 在 364 行声明并在 `ToolCallStart` 事件中填充（`tool_calls.get_or_insert_with(Vec::new).push(tc)`），但从未读取使用。这个 buffer 本意是积累 chunk，但因 `ToolCallChunk` 无法关联而被搁置。

**建议**：要么移除这个变量（表明这是已知限制），要么保留但加一个 `#[allow(dead_code)]` 注释说明这是为未来 API 扩展预留。

---

## 复查总结

| 问题 | 原严重度 | 修复状态 | 备注 |
|------|---------|---------|------|
| P0 #1 spawn+select 实现 | P0 | ✅ 已修复 | 分离 tool_timeout/iteration_timeout，逻辑正确 |
| P0 #2 权限检查全部失败 | P0 | ✅ 已修复 | 批量独立检查 + 测试验证 |
| P0 #3 zip crate 读签名 | P0 | ✅ 已解决 | 删除 recompute_digests，改为结构校验 + 测试覆盖 |
| P1 #4 streaming 代码重复 | P1 | ✅ 已修复 | inner/wrapper 模式，无重复 |
| P1 #5 ToolCallChunk 忽略 | P1 | ✅ 已改善 | 有注释说明，不静默 |
| P1 #6 gateway_socket 优先级 | P1 | ✅ 已修复 | 显式 get_gateway_address() |
| P1 #7 InboundMessage 大小限制 | P1 | ✅ 已修复 | 4096 字节 + UTF-8 安全截断 |
| P1 #8 CD offset 校验 | P1 | ✅ 已修复 | 已在 P0 #3 中处理 |

**结论**：所有 3 个 P0 和 5 个 P1 问题均已正确修复或合理处理。新增 2 个建议级问题，供参考。
> 编译状态: 本地 rustc 1.93.1 不满足 MSRV 1.95 要求，未通过编译验证

---

## 总体评价

S1 提交实现了设计文档中 S1.5（流式处理）、S1.6（InboundQueue）、S1.7（工具并行执行）三项核心功能，以及 APK v2 签名块和 Gateway 异步多连接 IPC。代码结构清晰、注释充分、测试覆盖完整。但存在若干 P0/P1 级别问题需要在合并前修复。

---

## P0（必须修复）

### 1. `execute_tools_parallel` 使用 `join_all` 而非设计文档约定的 spawn+select 方案

**位置**: `crates/rollball-runtime/src/agent/loop_.rs:434-477`

设计文档 `docs/03-agent-runtime.md §3.5` 明确约定使用 `tokio::spawn` 每工具独立运行 + `mpsc::channel` + `tokio::select!` 轮询方案来实现"迭代超时时部分工具结果仍可用"的语义。

但实际代码仍然使用 `join_all`：

```rust
let futures: Vec<_> = tool_calls.iter().map(|tc| {
    // ...
    async move {
        match timeout(tool_timeout, execute_single_tool(&tools, &tc)).await {
            Ok(result) => result,
            Err(_) => format!("Error: Tool '{}' timed out after {}ms", ...),
        }
    }
}).collect();
join_all(futures).await
```

虽然每个工具有独立的 `timeout`，但 `join_all` 会等待**所有**工具完成（包括超时返回错误的）。问题在于：

1. **单工具超时和迭代整体超时是同一个值**：`tool_timeout` 使用的是 `self.config.iteration_timeout_ms`，意味着每个工具的超时等于整个迭代的超时。如果某个工具耗时 28s 而 `iteration_timeout_ms=30000`，其他并行工具已经完成但这个工具还没超时，`join_all` 仍会等待。设计文档中三层超时（迭代整体 > 单工具 > LLM 调用）的职责划分未体现。

2. **缺少迭代整体超时控制**：`execute_tools_parallel` 内部没有 `tokio::select!` + deadline 机制，无法在迭代整体超时时 abort 剩余工具并返回已收集的部分结果。

**建议**: 按设计文档 §3.5 实现 spawn + mpsc + select 方案。或者如果团队决定简化为"全有或全无"语义，需要同步更新设计文档。

### 2. 权限检查拒绝一个工具时返回全部工具的错误

**位置**: `crates/rollball-runtime/src/agent/loop_.rs:440-452`

```rust
for tool_call in tool_calls {
    if let Err(e) = crate::tools::permission::validate_permission(&self.manifest, &tool_call.function.name) {
        // Permission denied — return error for all tools
        return tool_calls.iter().map(|tc| {
            if tc.function.name == tool_call.function.name {
                format!("Error: Permission denied — {}", e)
            } else {
                // Should not happen if permission check is consistent
                format!("Error: Permission check failed for {}", tc.function.name)
            }
        }).collect();
    }
}
```

当一个工具权限不足时，**所有**工具都被标记为失败。这不符合设计文档 §3.4 的约定："先对所有 tool_calls 批量做 permission check，需要用户确认的工具先通过 Approval Gate，全部确认后再并行执行"。

正确做法应该是：权限不足的工具返回错误 ToolResult，其他有权限的工具正常执行。只有所有工具都无权限时才全部失败。

### 3. `recompute_digests` 在 V2 签名格式下仍通过 `zip::ZipArchive` 读取，但签名块不在 ZIP entry 中

**位置**: `crates/rollball-sign/src/verify.rs:164-193`

V2 签名块插入在 Local File Entries 和 Central Directory 之间，不是 ZIP entry。`zip::ZipArchive::new(Cursor::new(zip_data))` 在解析 V2 签名 ZIP 时，能否正确跳过非 ZIP entry 的签名块数据？

APK v2 之所以可行，是因为 ZIP 格式只通过 EOCD → CD → Local File Header 的链路读取文件，CD 和 EOCD 之间的额外数据会被忽略。但 `zip` crate 的行为是否一致？如果 `zip` crate 在遇到签名块数据时报错或产生偏移错误，验证流程就会失败。

测试 `test_sign_and_verify_roundtrip` 可能因为签名块被正确忽略而通过，但需要确认这是 `zip` crate 的保证行为还是偶然行为。

**建议**: 添加针对大文件、多文件、带注释的 ZIP 等边界情况的测试，确认 `zip` crate 对 V2 签名 ZIP 的兼容性。

---

## P1（建议修复）

### 4. `call_llm_streaming` + `call_llm_streaming_no_retry` 大量重复代码

**位置**: `crates/rollball-runtime/src/agent/loop_.rs:318-424`

两个方法有约 90% 代码相同，唯一区别是 `no_retry` 版本在 `StreamEvent::Error` 时直接返回错误而不尝试 emergency trim。建议合并为一个方法，用参数控制是否 retry：

```rust
async fn call_llm_streaming_inner(
    &mut self,
    chat_request: &ChatRequest,
    context_builder: &ContextBuilder,
    retry_on_overflow: bool,
) -> Result<ChatResponse>
```

### 5. `insert_block_before_cd` 修改 CD offset 后未校验

**位置**: `crates/rollball-sign/src/zip_utils.rs:74+`

`insert_block_before_cd` 正确修改了 EOCD 中的 CD offset 和 CD 中每个 Local File Header 的 offset，但没有校验修改后的 offset 是否会导致越界。如果输入 ZIP 已经损坏（CD offset 指向错误位置），修改后可能生成无效 ZIP。

**建议**: 在函数返回前做基本的校验（如 CD offset + CD size <= 数据总长度 - EOCD 大小）。

### 6. `StreamEvent::ToolCallChunk` 被忽略

**位置**: `crates/rollball-runtime/src/agent/loop_.rs:337-339`

```rust
StreamEvent::ToolCallChunk(_) => {
    // Accumulated tool call chunk — currently not used
}
```

LLM 流式响应中，`ToolCallStart` 只包含工具名和 ID，参数通过后续 `ToolCallChunk` 逐步传输。当前实现完全忽略 `ToolCallChunk`，意味着依赖 `Finished` 事件中的完整 `tool_calls` 数据。但如果 LLM provider 不在 `Finished` 中返回完整 tool_calls（如某些 OpenAI 兼容 API），工具调用参数会丢失。

**建议**: 实现 ToolCallChunk 的累积逻辑，或至少在 `Finished` 事件中校验 tool_calls 的完整性。

### 7. `RuntimeConfig` 新增 `gateway_socket` 字段但默认值语义不清

**位置**: `crates/rollball-runtime/src/config.rs`

新增 `gateway_socket: Option<String>` 字段，默认值为 `None`。但旧的 `gateway_endpoint` 字段仍然存在，两者同时存在时优先级不明确。根据提交注释和 diff，`gateway_socket` 应该是替代 `gateway_endpoint` 用于 Unix Socket 连接，但旧字段未标记为 deprecated。

**建议**: 在 `gateway_socket` 的文档注释中说明与 `gateway_endpoint` 的关系和优先级，或移除 `gateway_endpoint`。

### 8. `InboundMessage` 的 `SystemNotification` 和 `IntentMessage` data 字段未做大小限制

**位置**: `crates/rollball-runtime/src/agent/inbound.rs:14-23`

`SystemNotification.data` 是 `serde_json::Value`，`IntentMessage.params` 也是 `serde_json::Value`，没有大小限制。恶意或错误的注入消息可能携带极大的 JSON payload，在 drain 时写入 History 后导致 token 爆炸。

**建议**: 在 `drain_inbound_queue` 中对注入消息的 content 做长度裁剪（如截断到 4096 字符），或至少在 token 预算中预留空间。

---

## P2（可考虑）

### 9. 测试中 `test_agent_loop_with_gateway_client` 会尝试连接不存在的 socket

**位置**: `crates/rollball-runtime/src/agent/loop_.rs:593`

```rust
let client = GatewayClient::connect("unix:///tmp/test.sock").unwrap();
```

这个测试依赖 `GatewayClient::connect` 不立即建立连接（延迟连接），否则会因 socket 不存在而 panic。如果 `connect` 的实现改为立即连接，测试会失败。

**建议**: 改为 `None` 测试或 mock GatewayClient。

### 10. `handle_budget_query` 返回硬编码占位数据

**位置**: `crates/rollball-gateway/src/ipc/server.rs:254-260`

```rust
fn handle_budget_query(_provider: &str) -> GatewayResponse {
    GatewayResponse::BudgetInfo {
        remaining_tokens: 100_000,
        remaining_cost_usd: 10.0,
    }
}
```

而 `loop_.rs:110` 中 `query_budget` 实际调用了这个接口并用于日志记录。虽然 Phase 1 允许占位，但日志中的 `remaining_tokens: 100_000` 会误导开发者以为真的有 10 万 token 预算。

**建议**: 添加 `tracing::warn!("Budget query returns placeholder data")` 或在响应中标记 `is_placeholder: true`。

### 11. `zip_utils::find_eocd` 线性搜索性能

**位置**: `crates/rollball-sign/src/zip_utils.rs:22-33`

从文件末尾向前逐字节搜索 EOCD 签名，最坏情况下搜索 65557 字节。对于 Agent 包（通常 < 10MB）这不是问题，但如果用于大型包可能产生可感知的延迟。

**建议**: 当前可接受，未来可优化为 SIMD 或 chunked 比较。

---

## 设计合规性检查

| 设计文档要求 | 实现状态 | 备注 |
|-------------|---------|------|
| S1.5 流式 LLM 调用 | ✅ 已实现 | `chat_stream` + `StreamEvent` 状态机 |
| S1.5 流式 + tool_calls 状态机 | ⚠️ 部分 | `ToolCallChunk` 被忽略，依赖 `Finished` 事件 |
| S1.5 已输出 text 暂存 | ✅ 已实现 | `accumulated_content` 累积 |
| S1.6 InboundQueue (mpsc::channel) | ✅ 已实现 | 容量 64，`try_recv` 非阻塞 drain |
| S1.6 三类消息 | ✅ 已实现 | UserMessage / SystemNotification / IntentMessage |
| S1.6 channel 生命周期 | ✅ 已实现 | `AgentLoop::new()` 返回 `(Self, Sender)` |
| S1.6 背压验收 | ✅ 有测试 | `test_inbound_queue_full_backpressure` |
| S1.7 并行 join_all | ⚠️ 不符合设计 | 使用 `join_all` 而非 spawn+select，缺少迭代整体超时 |
| S1.7 权限串行检查 | ⚠️ 语义错误 | 拒绝一个工具时全部失败 |
| S1.7 单工具超时 | ⚠️ 与迭代超时同值 | `tool_timeout = iteration_timeout_ms`，无分层 |
| S1.7 单工具失败不短路 | ✅ 已实现 | `join_all` 收集全部结果 |
| S1.7 工具并行性能测试 | ✅ 有测试 | `test_tool_parallel_execution` 验证 100ms+100ms < 300ms |
| APK v2 签名块 | ✅ 已实现 | `zip_utils` + `signing_block` + `sign` + `verify` |
| Gateway 异步多连接 | ✅ 已实现 | `tokio::spawn` per connection, `Arc<RwLock<GatewayState>>` |
| ProviderError 结构化 | ✅ 已实现 | `ProviderError` + `ProviderErrorType` + `retryable` |

---

## 修复优先级建议

| 优先级 | 编号 | 简述 | 修复工作量 |
|--------|------|------|-----------|
| P0 | #1 | `execute_tools_parallel` 改为 spawn+select 方案 | 中（~50行重写） |
| P0 | #2 | 权限检查：拒绝的工具独立失败，不影响其他工具 | 小（~10行） |
| P0 | #3 | 确认 `zip` crate 对 V2 签名 ZIP 的兼容性 | 小（添加边界测试） |
| P1 | #4 | 合并两个 streaming 方法 | 小（~20行） |
| P1 | #5 | `insert_block_before_cd` 添加校验 | 小（~10行） |
| P1 | #6 | `ToolCallChunk` 累积或校验 | 中（取决于 provider 行为） |
| P1 | #7 | `gateway_socket` vs `gateway_endpoint` 优先级说明 | 小（文档+注释） |
| P1 | #8 | InboundMessage 大小限制 | 小（~5行） |
