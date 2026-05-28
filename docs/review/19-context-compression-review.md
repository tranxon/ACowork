# Runtime 上下文压缩审查报告

**审查日期**：2026-05-28  
**范围**：`rollball-runtime` + `docs/design`  
**核心文件**：`history.rs`, `loop_.rs`, `context.rs`, `token/counter.rs`, `agent_core.rs`, `loop_llm.rs`, `memory/manager.rs`, `episode_distill.rs`, `session_state.rs`  
**设计对照**：`03-agent-runtime.md`, `05-memory.md`, `15-conversation-persistence.md`, `16-adr-context-threshold-dynamic.md`, `04-p2-s2-design-review.md`

---

## 1. 整体架构总览

设计文档定义了三层渐进压缩体系：
- **流水线 A（对话历史）**：内容折叠 → FIFO 淘汰 → 摘要替代（Phase 3）
- **流水线 B（检索结果）**：按 8 级优先级从低到高裁剪
- **弹性预算分配**：固定区（system + output）+ 可分配区（history 75% / retrieval 25%）
- **Token 计数**：三层（Tier 1 精确 / Tier 2 近似 / Tier 3 启发式）

**当前覆盖状态**：

| 组件 | 设计状态 | 实现状态 | 总结 |
|------|---------|---------|------|
| Tool Result 折叠 | ✅ 完全定义 | ✅ 已实现 | 核心逻辑正确 |
| FIFO 裁剪 | ✅ 完全定义 | ✅ 已实现 | 但缺"保留 tool_call 消息对"保护 |
| 紧急裁剪 | ✅ 完全定义 | ✅ 已实现 | 保留 4 条非 system |
| 动态 Token 比率阈值 | ✅ 已修复 (ADR-16) | ✅ 已实现 | 70%warn/90%hard |
| 上下文溢出恢复 | ✅ 完全定义 | ✅ 已实现 | emergency_trim + retry |
| 裁剪后 Episode 蒸馏 | ✅ 完全定义 | ✅ 已实现 | 最佳模型选择 |
| 消息内容清洗 | ✅ 完全定义 | ✅ 已实现 | sanitize_messages |
| TokenCounter 三层架构 | ✅ 完全定义 | ✅ 已实现 | 结构完整 |
| BudgetAllocation 结构 | ✅ 完全定义 | ✅ 已实现 | 结构定义完整 |
| **内容折叠（Phase 1 文件/内联）** | ✅ 完全定义 | ❌ **未实现** | 最大实现缺口 |
| **检索结果 8 级优先级裁剪** | ✅ 完全定义 | ❌ **未实现** | inject() 逐条扫描未按优先级 |
| **BudgetAllocation 未接入流水线** | ✅ 完全定义 | ❌ **未接入** | 结构存在但从未被调用 |
| **System Prompt Token 测量缓存** | ✅ 完全定义 | ❌ **未实现** | system_prompt_cache 存在但未接入 |
| **增量 Token 计数** | ✅ 完全定义 | ❌ **未集成** | count_incremental 存在但未用到 |
| **每消息 token 缓存（_token_count）** | ✅ 完全定义 | ❌ **未实现** | 完全缺位 |
| **tiktoken-rs 精确计数** | ✅ 完全定义 | ❌ **未集成** | 未在 Cargo.toml |

---

## 2. 流水线 A：对话历史裁剪

### 2.1 Tool Result 折叠 — ✅ 已实现，但有偏差

**实现位置**：`history.rs:fold_tool_results()`

**设计要求**：
- 保留最近 4 **轮**（完整迭代，一个迭代 = 1 条 assistant tool_calls 消息 + N 条 tool result 消息）
- 更早的折叠为 `[tool_name] 返回 {前200字符摘要}`

**实现行为**：
- 保留 `keep_full_results`（默认 4）**条 tool result 消息**，非 4 轮
- 老消息折叠为 `[folded] {前200字符}`
- **关键偏差**：没有轮（iteration）的概念，保留的是**单个 tool result 消息数**而非 tool_calls + results 的完整对数。单次迭代有 3 个并行 tool_call 时，4 条消息仅覆盖约 1.3 轮。

### 2.2 FIFO 裁剪 — ✅ 已实现，但缺保护

**实现位置**：`history.rs:trim_fifo()`

**设计要求**：
- 保留最近 3 轮对话
- 保留包含 tool_call 结果的消息对

**实现行为**：
- 简单地从第一条非 system 消息开始逐一移除
- 无"对话轮次"概念
- **不保护 tool_call 消息**：移除可能打乱 tool_calls ↔ 结果的配对
- 但由于每次裁剪后都调用 `sanitize_messages` 来修复配对，此问题部分被掩盖

### 2.3 预裁剪（Preemptive Trim）— ✅ 已实现

**实现位置**：`history.rs:preemptive_trim()`、`loop_.rs` step ②、step ⑥

| 触发点 | 阈值 | 行为 |
|--------|------|------|
| 步骤②（LLM 调用前） | `trim_history_to_budget()` → 80% usable | 折叠 + FIFO + 蒸馏 |
| 步骤②.6（构建后） | 70% warn / 90% hard → emergency_trim | 日志告警 + 紧急裁剪后重建请求 |
| 步骤⑥（tool result 追加前） | 70% usable | 预裁剪为工具结果腾空间 |
| loop_llm.rs（流式错误） | API 返回 ContextOverflow | emergency_trim + 重建 + 重试 |

### 2.4 紧急裁剪 — ✅ 已实现

**实现位置**：`history.rs:emergency_trim()`  
保留最后 4 条非 system 消息，循环中依次 `remove(i)`（O(n²) 复杂度，但数据量小可忽略）。

---

## 3. 流水线 B：检索结果裁剪 — ❌ 核心未实现

### 3.1 8 级裁剪优先级

**设计要求**（05-memory.md §1）：

| 优先级 | 内容 | 裁剪顺序 |
|--------|------|---------|
| 1 | 经历层检索结果 | ✅ 最先裁剪 |
| 2 | 关联扩散结果 | → |
| 3 | 失败教训 / 低优先级经验 | → |
| 4 | 用户偏好 | → |
| 5 | 成功模式 / 程序记忆规则 | → |
| 6 | 语义记忆核心事实 | → |
| 7 | 自传体记忆摘要 | ❌ 绝不裁剪 |
| 8 | Agent 身份定义 | ❌ 绝不裁剪 |

**实现**：`memory/manager.rs:inject()` 只是按 `retrieval.memories` 的返回顺序逐条加入，到 `max_tokens` 上限停止。没有优先级排序，没有按 label 类型分级处理。

### 3.2 BudgetAllocation 未接入

`token/counter.rs` 中的 `BudgetAllocation` 结构体定义了完整的弹性预算分区逻辑：
- `fixed_zone()` = system_prompt + output_reserve
- `distributable_space()` = context_window - fixed_zone
- `history_budget()` = distributable × 0.75
- `retrieval_budget()` = distributable × 0.25（至少 2048）

但该结构体**未被任何实际裁剪流水线调用**。`trim_history_to_budget()` 直接使用 `context_trim_budget()`（返回整个 usable 空间给 history），`MemoryManager::inject()` 接收一个简单 `max_tokens` 参数。两流水线没有协调分区。

---

## 4. Token 计数 — 结构完整但上下分离

### 4.1 三层次 TokenCounter — ✅ 已实现

`token/counter.rs` 完整实现了三层次计数架构。但：

### 4.2 实现内部的计数分裂 — ⚠️

系统存在**两套 Token 估算**：

| 组件 | 算法 | ASCII 估算 | CJK 估算 |
|------|------|-----------|----------|
| `history.rs:estimate_text_tokens()` | 字符级启发式 | chars/4 | chars × 2 |
| `token/counter.rs:count_tier3()` | 单词级启发式 | words × 1.3 | chars × 0.6 |

对于 1000 个汉字：
- `history.rs` → 2000 tokens ❌ 严重高估
- `counter.rs Tier 3` → 600 tokens

**影响**：HistoryManager 维护的 `current_tokens` 可能**严重偏离**实际 token 数，导致过早或过晚触发裁剪。

### 4.3 增量缓存与精确计数 — ❌ 未接入

- **tiktoken-rs**：未在 Cargo.toml 中添加依赖。设计文档 Tier 1 无法运作
- **system_prompt_cache**：`TokenCounter` 中存在但从未被 `ContextBuilder::build()` 调用
- **count_incremental()**：存在但从未被使用
- **_token_count on ChatMessage**：完全缺位，每个消息每次都被重新估算

---

## 5. 内容折叠（Three-phase Phase 1）— ❌ 最大实现缺口

### 设计要求
> 大段内容（单块 > 200 tokens）折叠为紧凑引用：
> - 文件内容 → `📎 path@L1..L150`
> - 工具长输出 → `📎 tool:name(args) → summary`
> - 用户粘贴大文本 → `📎 inline#hash (type, N tokens)`
> - 最近 3 轮不折叠
> - FoldedRef 元数据保留 content_hash，支持按需召回
> - System Prompt 注入折叠召回指引（约 30 tokens）

### 实际实现
仅有 `fold_tool_results()` 简单截断式折叠（`[folded] {200 chars}`）：
- 没有文件内容的行号引用折叠
- 没有用户文本的 inline hash 折叠
- 没有"最近 3 轮不折叠"保护
- 没有 FoldedRef 元数据
- 没有折叠召回指引注入

**典型代码开发场景设计号称可节省 50-60% history tokens**，当前实现完全错失。

---

## 6. 异常恢复路径

### 6.1 上下文溢出恢复 — ✅ 已实现

`loop_llm.rs` 中：
1. 流式处理收到 `StreamEvent::Error`，检查 `error_type == ContextOverflow`
2. 调用 `emergency_trim()` 保留最后 4 条非 system 消息
3. 重建 ChatRequest + `call_llm_streaming_no_retry()`（防止递归）
4. 如 `removed == 0`（4 条消息还超过 budget），直接返回错误

### 6.2 Episode 蒸馏 — ✅ 已实现

`episode_distill.rs`：
- 裁剪时蒸馏被移除的消息
- 会话结束时蒸馏完整会话
- 选择最便宜的可用模型
- 最佳努力，失败不打断主流程

---

## 7. 跨会话 Token 预算分配

### 设计要求（15-conversation-persistence.md §1.8）：
> per-session 隔离 + 全局上限：
> - AgentCore BudgetGuard: total_tokens / remaining / per_session_quota
> - SessionState: budget_quota / current_usage / Arc<Mutex<BudgetGuard>>

### 实现
- `BudgetGuard` 存在于 `session_state.rs` 中
- 但仅被创建时设置，每轮 LLM 调用前未做 budget 预检
- `BudgetAllocation.per_session_quota` 未与 `BudgetGuard` 联动
- per-session 配额切断逻辑尚未实现

---

## 8. 关键发现汇总

### 🚨 严重程度：高

| # | 问题 | 影响 |
|---|------|------|
| H1 | **内容折叠 Phase 1 未实现** | 代码开发场景错失 50-60% 的 token 节省，大文件场景早期 token 爆炸 |
| H2 | **Token 估算分裂** | history.rs 的启发式与 TokenCounter 不一致，CJK 偏差 3x+，裁剪时机不可靠 |
| H3 | **BudgetAllocation 未接入流水线** | history/retrieval 无协调分区，history 独占预算，retrieval 无保护 |

### ⚠️ 严重程度：中

| # | 问题 | 影响 |
|---|------|------|
| M1 | **fold_tool_results 语义是"消息"非"轮"** | `keep_full_results=4` 实际仅保留 1-2 次完整迭代 |
| M2 | **FIFO 不保护 tool_call 消息对** | 可能打断 tool_call ↔ result 配对 |
| M3 | **检索结果注入无优先级** | inject() 未按 8 级优先级裁剪，低价值经历层可能挤走高价值语义记忆 |
| M4 | **tiktoken-rs 未集成** | Tier 1 精确计数无法工作 |
| M5 | **系统提示词 token 未测量** | `system_prompt_cache` 存在但未接入，`BudgetAllocation.system_prompt_tokens` 始终为 0 |

### 🔧 严重程度：低

| # | 问题 | 影响 |
|---|------|------|
| L1 | **No tiktoken-rs dependency** | 设计文档指定但未添加 |
| L2 | **增量计数未使用** | `count_incremental()` 存在但未调用 |
| L3 | **每消息 token 缓存缺位** | 每次 truncate_to/drain_fifo 都重新计算 |
| L4 | **emergency_trim 使用 remove(i) O(n²)** | 数据量小可忽略，但值得改进 |
| L5 | **缺少压缩集成测试** | history.rs 有单元测试但无端到端上下文压缩行为测试 |

---

## 9. 建议实施优先级

### P0（立即修复）：

1. **集成 tiktoken-rs** 作为 Tier 1 精确计数基础
2. **统一 Token 估算**：让 `history.rs` 使用 `TokenCounter` 而非自实现启发式
3. **接入 system_prompt_cache**：在 `ContextBuilder::build()` 中计算 system prompt tokens

### P1（近期）：

4. **实现内容折叠 Phase 1**：文件内容 → `📎 path@L1..L150` + 折叠召回指引
5. **BudgetAllocation 接入裁剪流水线**：history/retrieval 分区预算
6. **检索注入优先级排序**：按 8 级 label 优先级裁剪
7. **fold_tool_results 改为"轮"语义**：跟踪 assistant_tool_calls 配对

### P2（中期）：

8. **FIFO 保留 tool_call 消息对**：实现"最近 N 轮"保护
9. **增量消息 token 缓存**：`ChatMessage` 上附加 `cached_tokens`
10. **每轮 LLM 前 budget 预检**：使用 BudgetGuard 做 per-session 配额检查

---

## 10. 总结

Runtime 的上下文压缩体系**骨架已经建立**：Tool Result 折叠、FIFO 裁剪、紧急裁剪、动态 Token 比率阈值、上下文溢出恢复、Episode 蒸馏以及完整的 TokenCounter 三层架构都已实现。这些构成了一个基础的运行保障。

**主要薄弱环节是"中级压缩能力缺失"**——内容折叠 Phase 1 的缺失意味着长文件/代码开发场景下 token 过早耗尽；BudgetAllocation 未接入导致 history 与 retrieval 间的资源竞争无机制调节；Token 估算的分裂使裁剪阈值定位不准确。

建议优先补齐内容折叠（P0）和统一 Token 计数（P0），在此基础上再对齐 BudgetAllocation 和检索优先级。
