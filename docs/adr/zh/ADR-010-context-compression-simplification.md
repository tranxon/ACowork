# ADR-010: 上下文压缩策略大幅简化

**状态**：已接受（Phase 1/2 完成：移除程序化折叠 + LLM Compaction/蒸馏统一，细化见 ADR-011）  
**细化**：Phase 2 蒸馏策略由 [ADR-011](./ADR-011-compaction-as-distillation.md) 细化（摘要即蒸馏，不再单独区分）  
**日期**：2026-05-28  
**决策者**：架构讨论  
**影响范围**：`03-agent-runtime.md`, `05-memory.md`, `15-conversation-persistence.md`, `16-adr-context-threshold-dynamic.md`, `04-p2-s2-design-review.md`, `history.rs`, `token/counter.rs`, `context.rs`, `loop_.rs`

---

## 背景

经过对业界主流编程 Agent（Claude Code、OpenCode、OpenDev、Aider、Cursor）的上下文压缩策略调研，并结合原有设计的审查（见 `19-context-compression-review.md`），我们发现原有设计存在根本性缺陷：

**原有设计包含大量程序化压缩策略**（tool result 折叠、内容折叠 Phase 1、FIFO 裁剪、检索结果 8 级优先级裁剪等），这些策略试图用代码逻辑判断"哪些内容可以丢弃"，但上下文压缩本质上是一个**语义理解任务**——只有 LLM 才能真正判断信息的相关性和可丢弃性。

任何程序化判断都是在用 proxy 指标（消息角色、字符位置、时间顺序）替代语义理解，而这些 proxy 指标一定会失效。

## 核心洞察

**程序化压缩的三个不可解矛盾：**

1. **截断位置不可控**：无论截断到 200 字符还是 2000 字符，关键信息可能在任意位置。截断保量不保质。
2. **时序 ≠ 重要性**：FIFO 假设老消息不重要，但老消息可能包含尚未解决的依赖（如 "先分析这个配置文件，后面会用到"）。
3. **消息角色 ≠ 语义状态**：检查 "assistant 消息是否存在" 无法判断那个回复是否真正"分析"了 tool result——编程场景中大量回复是过渡语（"让我继续追查..."），不包含任何可被摘要利用的分析。

**根本结论：程序能做的是"什么时候叫 LLM 来做摘要"，不能代替 LLM 决定"压缩什么"。**

## 决策

### 简化后的压缩架构

```
正常路径:  上下文增长 → 70% 告警 → 80% 触发 LLM 摘要 → 继续对话
异常路径:  上下文溢出/API 报错 → emergency_trim → 重试
```

**中间没有任何程序化折叠步骤。**

### 三阶段策略

| 阶段 | 触发条件 | 行为 | 说明 |
|------|---------|------|------|
| Stage 1: 监控 | 70% context 使用率 | 日志记录，不干预 | 纯观测，给后续决策留缓冲 |
| Stage 2: LLM 摘要 | 80% context 使用率 | 用 Compact Model 做 LLM 摘要 | 完整上下文输入，不做任何折叠/截断 |
| Stage 3: 紧急裁剪 | 95% / API ContextOverflow | emergency_trim（保留最后 N 条非 system） | 安全网，仅当 LLM 摘要本身无法执行时使用 |

### LLM 摘要设计要点

1. **使用独立的 Compact Model**：比主对话模型更便宜/更快的模型（如 GPT-4o-mini、Claude Haiku），成本约 $0.02/次
2. **完整上下文输入**：不对 tool result 做任何折叠、截断或预处理，让摘要 LLM 看到全部信息
3. **保护首尾**：system prompt + 最近 2-3 轮完整保留，压缩中间段
4. **完整历史归档**：压缩前的完整对话写入归档文件，agent 可随时取回
5. **摘要质量优先于成本**：单次摘要 2-3 美分，相比折叠导致错误再修正的成本，净收益为正

### 明确放弃的策略

以下原有设计中的策略被**明确放弃**：

| 策略 | 放弃原因 |
|------|---------|
| Tool result 日常折叠（`fold_tool_results`） | 截断位置不可控，关键信息可能丢失 |
| 内容折叠 Phase 1（文件/inline 折叠 + FoldedRef + 召回指引） | 语义判断不可靠，实际收益为负 |
| 检索结果 8 级优先级裁剪 | 程序化优先级无法替代语义相关性判断 |
| BudgetAllocation 弹性分区 | History/Retrieval 资源协调应在 LLM 摘要层面做，而非程序化分区 |
| 摘要前检查 assistant 消息作为"已分析"标记 | 代码无法判断回复是否真正包含分析 |

### 保留的策略

| 策略 | 保留原因 |
|------|---------|
| LLM 摘要 | 核心压缩手段，唯一能理解语义的方式 |
| emergency_trim | 安全网，API 溢出时保命 |
| Episode 蒸馏 | 跨 session 知识传递，与摘要互补 |
| Token 监控 + 阈值触发 | 监控是决策的基础 |
| sanitize_messages | 消息清洗（修复 orphan、空消息），这是格式修复而非压缩 |
| 多模型路由（Compact Model 独立配置） | 用便宜模型做摘要，控制成本 |

## 影响

### 需要修改的代码文件

- `core/rollball-runtime/src/agent/history.rs`：
  - 移除 `fold_tool_results()` 的日常触发
  - `trim_fifo()` 降级为仅在 emergency_trim 时使用
  - 保留 `emergency_trim()`、`sanitize_messages()`、`estimate_text_tokens()`
  - 新增 `compact_via_llm()` — 组装完整上下文发送给 Compact Model

- `core/rollball-runtime/src/agent/loop_.rs`：
  - `trim_history_to_budget()` 逻辑简化：70% warn → 80% compact → 95% emergency
  - 移除 tool result 追加前的 70% 预裁剪

- `core/rollball-runtime/src/agent/context.rs`：
  - `ContextBuilder::build()` 中接入 `system_prompt_cache`（上轮审查标记的缺口）
  - 移除任何在 build 阶段做的折叠/截断

- `core/rollball-runtime/src/token/counter.rs`：
  - `BudgetAllocation` 结构体保留但标记为 deprecated，不再接入裁剪流水线

- `core/rollball-runtime/src/memory/manager.rs`：
  - `inject()` 中检索结果注入简化为语义相似度排序，移除 8 级优先级

### 需要更新的设计文档

| 文档 | 需要更新的节 |
|------|------------|
| `docs/design/03-agent-runtime.md` | §3.1 上下文优先级、§Token 预算分配——移除内容折叠 Phase 1，简化为三阶段 |
| `docs/design/05-memory.md` | §1 瞬态层 Token 管理——移除三阶段渐进裁剪和 8 级检索优先级 |
| `docs/design/15-conversation-persistence.md` | §1.8 Token 预算——简化 per-session 配额描述 |
| `docs/review/16-adr-context-threshold-dynamic.md` | 阈值 70%/90% 保留，移除程序化折叠步骤描述 |
| `docs/review/04-p2-s2-design-review.md` | 三阶段渐进裁剪决策标记为"已废弃" |

## 后果

### 正面

- **可靠性提升**：语义理解任务回归 LLM，避免程序化判断的不可靠性
- **实现复杂度大幅降低**：`history.rs` 从 ~800 行减少到 ~300 行（移除折叠相关逻辑）
- **摘要质量提升**：完整上下文上的摘要远优于折叠后上下文上的摘要
- **维护成本降低**：消除大量启发式判断和边界条件

### 负面

- **单次摘要成本上升**：输入 token 更多（160K vs 80K），但绝对成本仍很低（$0.02-0.03/次）
- **摘要触发更频繁**：不折叠意味着 token 增长更快，每 8-10 轮触发一次（折叠方案是 12-15 轮）
- **emergency_trim 依赖增加**：正常路径没有日常折叠，极端情况下更依赖安全网

### 风险缓解

- Compact Model 选择便宜模型（GPT-4o-mini / Claude Haiku），单次成本可控
- 摘要 prompt 质量是核心——需要精心设计，明确告诉摘要 LLM 保留哪些信息
- emergency_trim 保留 4 条非 system，确保 API 溢出时不会丢失所有上下文
- 完整历史归档确保摘要丢失的信息可以按需取回
