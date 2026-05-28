# 上下文压缩简化（ADR-010）代码审查报告

**审查日期**：2026-05-28  
**范围**：15 个文件的 git diff（`core/rollball-runtime` + `docs/`）  
**核心依据**：[ADR-010](../adr/ADR-010-context-compression-simplification.md)、[19-context-compression-review.md](./19-context-compression-review.md)

---

## 总体评价

本次修改是一次**架构层面的净化**，删除了约 460 行旧代码/文档（净减少约 305 行），核心思路清晰且与 ADR-010 决策一致。代码变更质量较高，测试已同步更新，编译不会引入新问题。但存在几处**实现缺口**和**文档残留不一致**需要处理。

---

## ✅ 做得好的部分

### 1. `history.rs` 简化彻底

`fold_tool_results()`、`preemptive_trim()`、`preemptive_trim_drain()`、`drain_fifo()` 全部移除，`keep_full_results` 字段和 `HistoryManager::new(max_tokens, keep_full_results)` 双参数签名已清理。净删除约 140 行，模块注释说明了 ADR-010 的设计理由。

```rust
//! ## Design note (2026-05-28)
//!
//! Programmatic folding strategies (Tool Result folding, content folding) have been
//! removed per [ADR-010](../../../../docs/adr/ADR-010-context-compression-simplification.md).
//! Context compression is a semantic understanding task — only an LLM can reliably
//! decide what to discard. The remaining strategies (trim_fifo, emergency_trim) are
//! safety nets for when the LLM-based compaction itself cannot execute.
```

### 2. `config.rs` 字段清理完整

`keep_full_results` 字段、默认值函数 `default_keep_full_results()` 均已移除，对应的 `SessionManagerConfig`、`SessionState::new()`、`cli.rs` 中的引用全部同步更新，无遗漏。

### 3. `BudgetAllocation` 已标记 deprecated

`#[deprecated]` 属性 + 文档说明 + `#[allow(deprecated)]` 抑制测试编译警告，处理方式规范：

```rust
/// Deprecated per [ADR-010]: programmatic budget partitioning has been
/// replaced by LLM-based summarization as the sole compression mechanism.
/// This struct is retained for reference but no longer connected to the
/// compression pipeline.
#[deprecated(
    since = "0.1.0",
    note = "Programmatic budget partitioning replaced by LLM summarization (ADR-010)"
)]
pub struct BudgetAllocation { ... }
```

### 4. 设计文档同步更新

六份文档全部更新，废弃内容明确标记"已废弃"或指向 ADR-010：

| 文档 | 变更 |
|------|------|
| `03-agent-runtime.md` | 主循环图 §②.5 重写为三阶段 LLM 驱动策略；预算分配描述更新 |
| `05-memory.md` | 瞬态层管理大幅简化，移除内容折叠、8 级优先级、弹性分区 |
| `15-conversation-persistence.md` | Budget 策略从 per-session BudgetGuard 配额简化为 LLM 摘要自然控制 |
| `04-p2-s2-design-review.md` | §6.6 三阶段渐进裁剪标记为"已废弃"，指向 ADR-010 |
| `16-adr-context-threshold-dynamic.md` | 添加 ADR-010 交叉引用，标注过期描述 |
| `19-context-compression-review.md` | §11 新增 ADR-010 结论，说明大多数 P0/P1 不再适用 |

### 5. 测试同步

`context.rs` 和 `history.rs` 中所有 `HistoryManager::new(..., 4)` 调用已更新为单参数 `HistoryManager::new(...)`，测试覆盖仍然完整。

---

## ⚠️ 发现的问题

### 🔴 严重 #1：LLM 摘要核心路径未实现（ADR-010 核心功能缺失）

**影响文件**：`loop_.rs`

ADR-010 第 86 行明确要求：
> `history.rs`：新增 `compact_via_llm()` — 组装完整上下文发送给 Compact Model

当前 `loop_.rs` 的 `trim_history_to_budget()` 仅实现了 safety net 路径（`trim_fifo` + `emergency_trim`），**完全没有 70% 告警 → 80% LLM 摘要的核心功能**：

```rust
// loop_.rs:638-653 — 当前实现（只有 safety net，没有核心 LLM 摘要）
fn trim_history_to_budget(&mut self, model_name: &str) {
    let budget = self.context_trim_budget(model_name);
    let trim_budget = (budget as f64 * 0.8) as u64;

    // Stage 1: FIFO trim — 这不是 LLM 摘要！
    self.session.history.trim_fifo();

    // Stage 2: emergency trim — 安全网
    if self.session.history.token_count() > trim_budget {
        self.session.history.emergency_trim();
    }

    self.session.history.truncate_large_messages(trim_budget / 4);
}
```

**问题影响**：
- 80% 阈值时不会有任何 LLM 摘要触发
- 上下文压缩退化为纯 FIFO + emergency_trim（恰好是 ADR-010 明确要避免的程序化策略）
- **实际上是将原来的 `preemptive_trim`（tool result 折叠 + FIFO）替换为了更粗糙的纯 FIFO，而不是替换为 LLM 摘要**

**建议修复**：

`trim_history_to_budget` 需要实现 token 使用率检测（利用 `compute_context_usage` 中已有的 `usage_percent`）：

```rust
fn trim_history_to_budget(&mut self, model_name: &str) {
    let usage_percent = /* 从最近一次 LLM 调用获取 prompt_tokens / context_window */;

    if usage_percent < 70 {
        // Stage 1: 仅日志记录
        tracing::debug!(usage_percent, "Context usage within safe range");
        return;
    }

    if usage_percent < 80 {
        // Stage 1: 告警日志
        tracing::warn!(usage_percent, "Context usage approaching threshold");
    } else if usage_percent < 95 {
        // Stage 2: LLM 摘要（Compact Model）
        self.compact_via_llm(model_name);
    } else {
        // Stage 3: emergency_trim 安全网
        self.session.history.emergency_trim();
    }

    self.session.history.truncate_large_messages(trim_budget / 4);
}
```

---

### 🔴 严重 #2：trim 时 Episode 蒸馏路径丢失

**影响文件**：`loop_.rs`

ADR-010 第 74 行明确将 "Episode 蒸馏" 列为**保留策略**。但本次修改将 `loop_.rs` 中的 `spawn_trim_distillation()` 整个方法删除，这意味着：

- **旧代码**：`preemptive_trim_drain` → 捕获被移除的消息 → `spawn_trim_distillation` → Grafeo
- **新代码**：`trim_fifo` → 消息直接删除，无蒸馏

现在只有 session 结束时有蒸馏，trim 时被删除的消息**永久丢失**。这与 ADR-010 的保留策略矛盾。

**已删除的代码**（diff 第 355-413 行）：
```rust
-        let trimmed_messages = self.session.history.preemptive_trim_drain(trim_budget);
-        if !trimmed_messages.is_empty() {
-            self.spawn_trim_distillation(trimmed_messages);
-        }

-    fn spawn_trim_distillation(&self, trimmed_messages: Vec<ChatMessage>) {
-        // ... 异步 spawn EpisodeDistiller::distill_on_trim ...
-    }
```

**建议修复**：

在 LLM 摘要路径（`compact_via_llm`）中，对被压缩的消息触发 Episode 蒸馏。ADR-010 主循环图 §②.5 也明确标注了 "触发 Episode 提炼"。

---

### 🟡 中等 #3：`03-agent-runtime.md` 存在残留过期描述

**影响文件**：`03-agent-runtime.md`

共 3 处残留：

**3a. §3.6 循环退出条件表（L478）**：

```
| Context exceeded 恢复失败 | 步骤 ③ | Preemptive trim 和 reactive recovery 均无法满足时终止（见 §7.1） |
```
"Preemptive trim" 已被 ADR-010 废弃，应改为 "emergency_trim"。

**3b. §8 设计决策表（L654）**：

```
| 上下文裁剪 | 双流水线 + preemptive trim + reactive recovery | ... |
```
"双流水线" 已被废弃，"preemptive trim" 也被废弃。应更新为三阶段 LLM 驱动策略。

**3c. §4 默认配置表（L493）**：

```
| pruner.keep_full_results | 4 | Tool Result 折叠：保留最近 N 轮完整 tool result |
```
`keep_full_results` 配置字段已从 `config.rs` 移除，此表项应删除或标记废弃。

---

### 🟡 中等 #4：Reactive Recovery 仍描述 Tool Result 折叠

**影响文件**：`03-agent-runtime.md` §7.1（L564-575）

Context Exceeded Reactive Recovery 流程中 Step 1 描述：

```
Step 1: Tool Result 折叠（保留最近 4 轮 → 折叠更早的为摘要）
```

这与 ADR-010 "明确放弃 Tool result 日常折叠" 矛盾。应改为直接进入 emergency_trim。

---

### 🟡 中等 #5：ADR-010 状态与实现不一致

**影响文件**：`docs/adr/ADR-010-context-compression-simplification.md`

ADR-010 文档头标记：

```
**状态**：提议中
```

但代码变更已经按"已决策"执行。状态应更新为 `**状态**：已接受` 或 `已实施`。

---

### 🟢 低 #6：`config.rs` 留有多余空行

**影响文件**：`config.rs`

`keep_full_results` 字段移除后留下一个多余空行，建议清理以保持代码整洁。

---

### 🟢 低 #7：`trim_fifo` 使用 `self.max_tokens` 而非传入的 budget

**影响文件**：`history.rs`、`loop_.rs`

`trim_fifo()` 使用构造时设置的 `self.max_tokens`（默认 128000）而非模型的实际 context window。在 `trim_history_to_budget` 中，即使外部传入 `trim_budget`（基于模型 context window 计算），`trim_fifo()` 也看不到。这是一个**已有问题**（pre-existing），但因 ADR-010 提高了对 trim 行为的依赖而变得更重要。建议后续让 `trim_fifo()` 接受外部 budget 参数。

---

## 📊 变更统计

| 文件 | 变更类型 | 行数变化 | 状态 |
|------|---------|---------|------|
| `history.rs` | 删除折叠逻辑 + 添加设计注释 | -140 行 | ✅ 良好 |
| `loop_.rs` | 简化 trim + 删除 distillation | -81 行 | ⚠️ 蒸馏路径丢失 |
| `context.rs` | 测试适配 | -2 行 | ✅ 良好 |
| `session_state.rs` | 移除 keep_full_results 参数 | -4 行 | ✅ 良好 |
| `session/session_manager.rs` | 移除 keep_full_results 配置 | -4 行 | ✅ 良好 |
| `cli.rs` | 移除 keep_full_results 传递 | -1 行 | ✅ 良好 |
| `config.rs` | 移除 keep_full_results 字段 | -9 行 | ✅ 良好 |
| `token/counter.rs` | BudgetAllocation deprecated | +12 行 | ✅ 良好 |
| `token/mod.rs` | allow(deprecated) 抑制 | +1 行 | ✅ 良好 |
| `03-agent-runtime.md` | 主循环图 + 预算策略 | ~80 行重构 | ⚠️ 残留过期描述 |
| `05-memory.md` | 瞬态层管理大幅简化 | -94 行 | ⚠️ §7.1 残留 |
| `15-conversation-persistence.md` | Budget 策略简化 | -46 行 | ✅ 良好 |
| `04-p2-s2-design-review.md` | §6.6 标记废弃 | -54 行 | ✅ 良好 |
| `16-adr-context-threshold-dynamic.md` | 添加 ADR-010 交叉引用 | +2 行 | ✅ 良好 |
| `19-context-compression-review.md` | §11 新增 ADR-010 结论 | +20 行 | ✅ 良好 |
| **合计** | | **-460 / +155** | **净 -305 行** |

---

## 🎯 修复优先级

### P0 — 必须修复才能合并

| # | 问题 | 文件 |
|---|------|------|
| P0-1 | **实现 LLM 摘要核心路径** — 当前只有 safety net，缺失 ADR-010 核心功能 | `loop_.rs`、`history.rs` |
| P0-2 | **恢复 trim 时 Episode 蒸馏** — 确保被压缩消息不永久丢失 | `loop_.rs` |

### P1 — 合并前建议修复

| # | 问题 | 文件 |
|---|------|------|
| P1-1 | 修复 `03-agent-runtime.md` 中 3 处残留过期描述 | `03-agent-runtime.md` |
| P1-2 | 更新 Reactive Recovery 中的 "Tool Result 折叠" 描述 | `03-agent-runtime.md` §7.1 |
| P1-3 | 更新 ADR-010 状态为 "已接受" | `ADR-010-context-compression-simplification.md` |

### P2 — 后续可修复

| # | 问题 | 文件 |
|---|------|------|
| P2-1 | 清理 `config.rs` 多余空行 | `config.rs` |
| P2-2 | 考虑让 `trim_fifo()` 接受外部 budget 参数 | `history.rs` |

---

## 结论

本次修改的方向和结构完全正确，与 ADR-010 的设计意图高度一致。代码的清理工作（移除废弃字段/方法/测试）执行得很干净。

**但核心功能缺口（LLM 摘要路径未实现 + 蒸馏路径丢失）是阻塞性问题**，合并前必须解决。当前状态下，上下文压缩实际上退化为纯程序化策略（FIFO + emergency_trim），这与 ADR-010 的核心理念（"程序化策略不可靠，LLM 摘要才是正确路径"）自相矛盾。文档方面有少量残留过期描述需要清理，但都是机械性修改，风险低。
