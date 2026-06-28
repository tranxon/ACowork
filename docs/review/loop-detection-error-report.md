# Loop Detection 错误分析报告

**错误信息**: `Unexpected error. Loop detected: Detected repeated call to [content_search] with same parameters (3 consecutive hits)`

**发生时间**: 2026-06-28

## 1. 错误来源定位

### 源代码位置

| 文件 | 行号 | 说明 |
|------|------|------|
| `core/acowork-runtime/src/agent/loop_detector.rs` | 205-207 | ExactRepeat 检测逻辑，生成错误消息 |
| `core/acowork-runtime/src/error.rs` | 33-34 | `RuntimeError::LoopDetected` 错误定义 |
| `core/acowork-runtime/src/agent/loop_tools.rs` | 869 | 循环检测后处理，触发错误 |

### 调用链

```
AgentLoop::run()
  └─> pre_check_loop_detection()  [行682-718]
      └─> loop_detector.peek_check()  [行153-157]
          └─> peek_exact_repeat()  [行195-218]
              └─> 达到阈值(3次) → 返回 LoopDetectionResult::LoopDetected
```

## 2. 检测机制详解

### 2.1 ExactRepeat 模式

这是 **四 种循环检测模式** 中的第一种：

| 模式 | 说明 | 阈值 |
|------|------|------|
| `ExactRepeat` | 相同工具 + 相同参数 连续调用 | 3 次 |
| `PingPong` | A→B→A→B 交替模式 | 4 次循环 |
| `NoProgress` | 相同工具 + 相同结果哈希 | 5 次 |
| `SameToolFlood` | 同一工具在窗口内频繁调用 | 8 次/12 窗口 |

### 2.2 阈值配置 (loop_detector.rs 第85-96行)

```rust
impl Default for LoopDetectionConfig {
    fn default() -> Self {
        Self {
            exact_repeat_threshold: 3,      // ← 触发阈值
            ping_pong_threshold: 4,
            no_progress_threshold: 5,
            no_progress_enabled: true,
            same_tool_flood_threshold: 8,
            same_tool_flood_window: 12,
        }
    }
}
```

### 2.3 渐进响应机制

| 连续命中次数 | 响应级别 | 行为 |
|-------------|---------|------|
| 1 | `Warning` | 注入警告信息，继续执行 |
| 2 | `Block` | 阻止工具调用，返回错误 |
| ≥3 | `Break` | 终止迭代，返回 `RuntimeError::LoopDetected` |

## 3. 错误触发场景分析

### 3.1 当前错误含义

- **工具名**: `content_search`
- **参数**: 完全相同（JSON 参数序列化后一致）
- **连续调用次数**: 3 次
- **响应级别**: `Warning`（第1次命中）

这是 **Warning 级别** 的首次警告，系统仍然允许执行，但会在结果中注入警告信息。

### 3.2 可能的原因

1. **Agent 陷入搜索循环**: LLM 反复使用 `content_search` 搜索相同内容，可能是：
   - 搜索条件未找到结果，不断调整搜索词
   - 结果不满足预期，重新搜索相同关键词

2. **参数规范化问题**: 某些参数在 JSON 序列化后完全相同，但语义上可能不同

3. **上游 prompt 约束**: 某些 Agent 的 system prompt 强制要求使用 `content_search`（见 `examples/senior-engineer-agent/prompts/system.md`）

## 4. 相关代码片段

### 4.1 错误消息生成 (loop_detector.rs 205-207)

```rust
let message = format!(
    "Detected repeated call to [{tool_name}] with same parameters ({hit_val} consecutive hits)"
);
```

### 4.2 预检查拦截 (loop_tools.rs 698-713)

```rust
LoopDetectionResult::LoopDetected { level, pattern, .. } => {
    match level {
        ResponseLevel::Warning => {
            // Warning is handled post-execution; allow the call
            calls_to_execute.push(tc.clone());
        }
        ResponseLevel::Block | ResponseLevel::Break => {
            tracing::warn!(
                tool = %tc.function.name,
                level = ?level,
                "Loop detected (pre-execution), blocking tool call"
            );
            blocked_info.push((idx, pattern));
        }
    }
}
```

### 4.3 错误定义 (error.rs 33-34)

```rust
#[error("Loop detected: {0}")]
LoopDetected(String),
```

## 5. 后续建议

### 短期
- 如果是预期行为（Agent 确实需要多次搜索），可以考虑：
  - 调整 `exact_repeat_threshold` 到更高值
  - 在 prompt 中添加"避免重复搜索"的约束

### 长期
- 添加更智能的参数比较（考虑语义相似而非完全相等）
- 记录触发日志以便分析模式

---

*Report generated: 2026-06-28*