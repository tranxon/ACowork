# ADR-008: 上下文阈值动态化

**日期**: 2026-05-15  
**状态**: 已实施  
**关联**: review/15-stream-error-retry-implementation-review.md  
**后续**: [ADR-010](../adr/ADR-010-context-compression-simplification.md)（2026-05-28）—— 阈值 70%/80%/95% 保留，但触发的不再是程序化折叠策略，而是 LLM 摘要。本文档中提及的"Preemptive Trim 触发折叠+FIFO"描述已过时。

## 背景

Agent 主循环 `loop_.rs` 使用硬编码字节阈值检查请求大小：

```rust
const REQUEST_SIZE_WARN: usize = 200_000;  // 200KB
const REQUEST_SIZE_HARD: usize = 280_000;   // 280KB
let request_size = serde_json::to_vec(&chat_request).map(|v| v.len()).unwrap_or(0);
```

存在两个问题：

1. **不感知模型**：32K 模型（如 Qwen3-4B）到 200KB 才 warn，此时早已超出上下文窗口；1M 模型（如 MiniMax）也在 200KB 就 warn，远未达到实际容量
2. **`context_trim_budget()` 未扣减 output**：直接返回 `context_window`，与 `compute_context_usage()` 的 `usable = context_window - max_output_tokens` 计算方式不一致

## 决策

**精简方案**：3 处代码修改，0 个新 struct，0 个新配置项。

### 修改 1：`context_trim_budget()` 扣减 output 预留

**文件**: `core/rollball-runtime/src/agent/agent_core.rs`

```rust
// Before: 直接返回 context_window
caps.context_window

// After: 减去 max_output_tokens（cap 20K）
let output_reserve = caps.max_output_tokens.min(20_000);
let usable = caps.context_window.saturating_sub(output_reserve);
```

- 与 `compute_context_usage()` 对齐
- 20K cap 防止大 output 模型（如 384K）浪费上下文

### 修改 2：字节阈值 → token 比率阈值

**文件**: `core/rollball-runtime/src/agent/loop_.rs`

```rust
// Before: 硬编码字节
const REQUEST_SIZE_WARN: usize = 200_000;
const REQUEST_SIZE_HARD: usize = 280_000;
let request_size = serde_json::to_vec(&chat_request)...;

// After: 动态 token 比率
let usable = self.context_trim_budget(&current_model);
let warn_threshold = (usable as f64 * 0.70) as u64;
let hard_threshold = (usable as f64 * 0.90) as u64;
let current_tokens = self.session.history.token_count();
```

- 用已有的 `token_count()` 代替 `serde_json::to_vec` 算字节，更准确也更快
- 70%/90% 比率覆盖从 32K 到 2M 的模型

### 修改 3：`loop_llm.rs` 日志增强

**文件**: `core/rollball-runtime/src/agent/loop_llm.rs`

溢出恢复路径增加 `current_tokens` / `remaining_tokens` 结构化日志，便于调试。

## 阈值效果对比

| 模型 | context_window | max_output | usable | 70% warn | 90% hard |
|------|---------------|------------|--------|----------|----------|
| Qwen3-4B | 32K | 8K | 24K | 16.8K | 21.6K |
| GPT-4o | 128K | 16K | 108K | 75.6K | 97.2K |
| Claude Sonnet | 200K | 16K | 180K | 126K | 162K |
| MiniMax | 1M | 16K | 984K | 689K | 886K |
| Gemini | 2M | 8K | 1.99M | 1.39M | 1.79M |

旧方案对所有模型统一 warn=200KB(≈50K tokens) / hard=280KB(≈70K tokens)。

## 与现有体系的关系

本方案不引入新概念，只修复已有体系的脱节点：

- `context_trim_budget()` → 补上 output 扣减，与 `compute_context_usage()` 对齐
- `loop_.rs` 字节阈值 → 切换到 token 比率，与 `preemptive_trim(90%)` / `trim_history_to_budget(80%)` 同体系
- `loop_llm.rs` reactive 恢复 → 保持不变（API 返回 ContextOverflow 时触发，是 proactive 的安全网）

## 调研参考

| Agent | 核心做法 | 我们取了什么 |
|-------|---------|------------|
| OpenCode | `usable = context_window - maxOutputTokens` | ✅ output_reserve 公式 |
| Claude Code | `effective = ctx - min(maxOutput, 20K)`, trigger at effective-13K | ✅ 20K cap |
| ZeroClaw | 5 层渐进 + probe_tier + 16 个可配参数 | ❌ 过度，emergency_trim 够用 |
| Hermes | 双层 + 可配置 + 自适应 summary budget | ❌ 现阶段不需要 |

## 后续可选（Phase 2+）

如果实际使用中发现 70%/90% 固定比率不够灵活，可考虑：

- 比率可配置化（但需要先有 Desktop App 设置面板）
- 更精细的 compaction（当前只有 FIFO trim + Grafeo 蒸馏，未来可加 summary compaction）
- probe_tier 多级探测（应对 API 返回的精确 token 数与估算偏差大的场景）

这些都不急，等有实际数据再决策。
