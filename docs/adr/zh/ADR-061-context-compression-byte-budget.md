# ADR-061：上下文压缩机制改造 - 8 级递减策略替代按轮数保留

**状态**：提案
**日期**：2026-09-14（自 ADR-060 §12 拆分独立，含代码级事实修正）
**决策者**：大鱼
**前置**：
- [ADR-010](./ADR-010-context-compression-simplification.md)（程序化压缩废止的历史决策）
- [ADR-011](./ADR-011-compaction-as-distillation.md)（上下文摘要与蒸馏统一策略）
- [ADR-052](./ADR-052-tool-compression-llm-autonomous.md)（工具压缩 LLM 自主化）
- [ADR-053](./ADR-053-agent-specific-compaction-prompt.md)（Agent 级压缩提示词）
- [ADR-056](./ADR-056-global-default-compact-model.md)（全局默认压缩模型解析）
- [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md)（Prompt Cache 友好的上下文块重排 —— 本 ADR 的 Block A/B/C/D 前置）

---

## 1. 决策摘要

ADR-060 的 Block A/B/C/D 重排解决了"动态块污染稳定前缀"的问题，但**上下文压缩路径仍是 cache 的最终杀手**：当前机制以轮数（`KEEP_LAST_ROUNDS = 3`）保留尾部，且压缩后仍超限时会退化为 FIFO 删头——FIFO 一旦触发，Block B 全部失效，后续每轮都付全量 token 成本。

本 ADR 决定：

1. **8 级递减压缩策略**替代"保留最近 N 轮"：从最宽松的保留级别开始逐级收紧，直到压缩比 ≥ 10% 为止；优化指标从"轮数"改为"压缩比"。
2. **FIFO 路径物理删除**：`trim_fifo` / `emergency_trim` 在 8 级策略下不可达，删除后极端场景改为**显式失败**（`ChunkEvent::Error` 提示用户），绝不静默牺牲 cache。
3. **工具自动压缩关闭**：`context_abandon` 工具不再注册（LLM 自主压缩破坏 cache 连续性），`context_retrieve` 保留作为压缩后的显式取回通道。
4. **summary 保持既有 marker 契约**：压缩产物仍是 `User` 角色 + `name="compaction_summary"` 的消息（ADR-011/restorer 既有约定），level 元数据以纯文本写在 summary 内容最前面，**不改变消息角色**——避免与 ADR-060 Block B 的 System 过滤及 Anthropic system 提升语义冲突。
5. **LLM 不可用时绝不退化为 FIFO**：不修改 history，向前端 emit `ChunkEvent::Error`，由用户决策（新会话 / 换大窗口模型 / 手动压缩）。

**非目标**（本 ADR 不讨论）：
- Block A/B/C/D 重排与 `cache_control` 字段——见 ADR-060。
- summary 提示词的内容质量工程（per-agent `summary.md` 已由 ADR-053 覆盖）——本 ADR 仅定义 prompt 的强制结构。
- 蒸馏入图（triples 落地知识图谱）——见 ADR-057。

---

## 2. 背景与现状盘点（代码级事实）

### 2.1 为什么"保留最后 3 轮"会触发 FIFO

ADR-011 的压缩机制（`compact_via_llm` + `replace_middle_with_summary`）以**轮数**（`KEEP_LAST_ROUNDS = 3`，见 [core/acowork-runtime/src/agent/loop_context.rs:46](core/acowork-runtime/src/agent/loop_context.rs#L46)）而非**字节预算**保留尾部。典型 agent 任务中，3 轮对话可能包含：

- shell 输出 50 KB 的日志（1 次 `run_shell` tool_result）
- file_read 读了 200 行代码（1 次 `file_read` tool_result）
- content_search 返回 100 条匹配（1 次 `content_search` tool_result）
- 加上 user prompt 与 assistant 文本

3 轮很容易达到 **50K~80K tokens**——超出 `effective_input_budget`（典型 128K context window 减 32K output = 96K 可用 input）时，压缩后仍超限，`trim_history_to_budget` 退到 FIFO → **FIFO 删头 → Block B cache 全部失效**。

### 2.2 现状机制盘点

| 机制 | 现状（代码级） |
|---|---|
| 压缩入口 | `AgentLoop::compact_history_if_needed`（[loop_context.rs:565](core/acowork-runtime/src/agent/loop_context.rs#L565)），80% 阈值触发或手动 force；由 ADR-056 的 `resolve_distill_model` 解析压缩模型 |
| 摘要生成 | `HistoryManager::compact_via_llm`（[history.rs:757](core/acowork-runtime/src/agent/history.rs#L757)）+ `episode_distill::compact_with_llm`；prompt 由 ADR-053 的 per-agent `summary.md`（或内置 `COMPACTION_SYSTEM_PROMPT`）提供 |
| 中间替换 | `HistoryManager::replace_middle_with_summary`（[history.rs:798](core/acowork-runtime/src/agent/history.rs#L798)），保留尾部 `keep_last_rounds` 轮 |
| FIFO 兜底 | `trim_history_to_budget`（[loop_context.rs:218-237](core/acowork-runtime/src/agent/loop_context.rs#L218-L237)）：Stage 1 `trim_fifo` + Stage 2 `emergency_trim` |
| 工具压缩 | `context_abandon` / `context_retrieve` 由 `tool_compression_enabled`（默认 `true`，[agent_config.rs:216](core/acowork-runtime/src/agent_config.rs#L216)）门控注册（[builtin/mod.rs:195-201](core/acowork-runtime/src/tools/builtin/mod.rs#L195-L201)）；`context_abandon` → `AbandonQueue` → `drain_abandon_queue`（[loop_.rs:1722](core/acowork-runtime/src/agent/loop_.rs#L1722)）→ `abandon_tool_result`（[history.rs:555](core/acowork-runtime/src/agent/history.rs#L555)）原位替换占位符 |

> **勘误说明**：早期草稿（ADR-060 §12）曾引用 `auto_compress_tool_results` 调用点——**该函数不存在于代码库**。工具压缩的真实机制是上面的"工具注册门控 + 队列 + 原位替换"链路，本 ADR 以实际代码为准。

### 2.3 summary marker 契约（不可破坏）

`replace_middle_with_summary` 产出的 marker 消息有明确的既有契约，**任何压缩改造都必须保持**：

1. **角色是 `User` 而非 `Assistant`**：`restorer` 注释明确——避免重建请求中 `Assistant → Assistant{tool_calls}` 相邻，glm-5.2 on Volcano Ark 会拒绝（400 InvalidParameter），见 [restorer.rs:22-31](core/acowork-runtime/src/agent/session/restorer.rs#L22-L31)。
2. **身份靠 `name == "compaction_summary"` 识别**：`last_compaction_index`、`emergency_trim` 的保护逻辑、`episode_distill` 的会话收尾蒸馏都依赖它（history.rs:500-508）。
3. **JSONL 锚点**：`kind="compaction"` 条目 + `last_compaction_offset` 决定恢复窗口（restorer.rs:93-123），restorer 只尊重**最近一次** compaction。
4. **不能被 ADR-060 Block B 过滤**：`ContextBuilder::build()` 过滤 history 中所有 `MessageRole::System` 消息（context.rs:541）——若把 summary 改成 SystemMessage，它会从请求中**静默消失**。

> **结论**：压缩产物**必须保持 `User` 角色 marker**。任何"summary 作为独立 SystemMessage 插入 Block A 之后"的方案（早期草稿 §12.4.7/12.4.8 的 cache hit 2 设计）与上述 4 条契约全部冲突，**予以废弃**。level 元数据改为写在 summary 文本最前面（见 §9）。

---

## 3. 成本模型

### 3.1 策略对比

| 策略 | 首次 cache 代价 | 后续轮 token 成本 | 命中场景 |
|---|---|---|---|
| **FIFO 删头** | 0（从不重建 cache） | 每轮 ~200K（无 cache） | 永远 0% |
| **压缩（summary）** | 1 次 cache miss（~200K） | 每轮 ~10K（有 cache） | 压缩后 N 轮内命中率 ~100% |

**算式**（以 200K → 10K 的 95% 压缩比为例）：
- FIFO 路径：每轮付 200K token × N 轮
- 压缩路径：付 1 × 200K + N × 10K
- 临界点：`N × 200K = 200K + N × 10K` → **N ≈ 1.05 轮**

只要压缩后能续命超过 1 轮，压缩路径就比 FIFO 便宜。agent 任务中压缩后通常能续命几十到几百轮，因此**压缩是绝对优势策略**——cache miss 是"沉没成本"。

### 3.2 失效范围因 provider 而异

中间位置压缩的 cache 失效范围并不总是"全量"：

- **OpenAI**（128-token hash 链）：中间任意字节变化 → 之后所有块错位，**全量失效**——上面的"1 次全量 miss"模型严格成立。
- **Anthropic**（breakpoint 前缀缓存）：插入点**之前**的稳定前缀（Block A 及压缩点前的早期历史）仍可命中，失效的是插入点之后的 suffix——实际成本低于全量模型。

工程上按"全量失效"预算（保守），实际收益以 Anthropic 场景更好。

### 3.3 "10% 最低压缩比"是工程启发式

按同一 1.25× 写/读成本模型，压缩比 \(r\) 的盈亏平衡轮数 ≈ \(1.25(1-r)/r\)：\(r=10\%\) 时约 **11 轮**，\(r=50\%\) 时约 1.25 轮。严格地说"低于 10% 不划算"只在预期剩余轮数 < 11 时成立。

**决策**：10% 门槛作为**工程启发式**保留（实现简单、行为可预测），不追求与成本模型严格联动；若后续需要，可升级为"门槛 = f(预期剩余轮数)"的动态模型，本 ADR 不展开。

### 3.4 真正决定成败的是 summary 质量

压缩比的数字只是表象。**summary 是否保留了用户意图、关键决策与未完成任务，才是压缩后对话能否继续的生死线**——这是本 ADR 引入 `<user_intent>` 强制结构（§8）与"级 1-7 必须保留所有 user 消息"（§13.2）的原因。

---

## 4. 与 ADR-010 的关系：程序化裁剪 vs LLM 摘要

ADR-010 的核心结论是"**程序能做的是什么时候叫 LLM 来做摘要，不能代替 LLM 决定压缩什么**"，并因此废止了程序化折叠。本 ADR 的 8 级递减策略表面上回到了"按角色/轮数做程序化裁剪"，需要明确边界：

- **ADR-010 反对的是"程序化决定丢弃内容"**：用 proxy 指标（角色、位置、时间）判断"哪条消息可以扔"。
- **本 ADR 的 8 级只是"保留窗口的优先级顺序"**：丢弃的内容**全部进入 LLM summary**（信息不丢失，只是从原文变成摘要）；程序化部分只决定"哪些进 summary、哪些保留原文"，且逐级回退以保证压缩比。信息重建通道永远是 LLM，不是裁剪。
- 保留优先级（user > assistant > tool）的依据不是"角色 = 语义价值"，而是"**信息重建难度**"：user 消息是 LLM 无法推理得到的硬约束来源，assistant/tool 均可由 summary 重建。

**一句话**：8 级策略是"摘要的粒度调度器"，不是"丢弃决策器"。若此边界在实现中被越过（例如任何一级直接 drop 而不进 summary），即违反 ADR-010 与本文。

---

## 5. 设计原则

1. **FIFO 删头必须被消除**——它是 Block B cache 的最终杀手，与 ADR-060 的核心理念冲突。
2. **压缩是"逐级递减 + 最低压缩比门槛"**——从"少牺牲信息"开始试，直到压缩比达标，而非一次压缩到极致。
3. **对话骨架永远最后才压**——保留优先级：user 消息 > assistant 消息 > 工具调用。
4. **压缩必须同时产出"摘要 + 尾部历史上下文"**——LLM 既记得过去，也记得现在。
5. **工具压缩由 Runtime 统一调度**——不再开放给 LLM 自主调用（`context_retrieve` 仍可手动召回），避免 LLM 自主压缩破坏 cache 连续性。
6. **summary 质量是核心 KPI**——summary LLM 的 prompt、token 预算、保留策略都需要投入工程精力。

---

## 6. 8 级递减策略定义

### 6.1 设计思路

**核心洞察**：以"保留 N 轮"为指标是脆弱的——N 轮的 token 数随工具调用体量变化巨大（N=1 在 long-running task 场景下就可能撑满 budget）。**真正应该优化的指标是"压缩比"**——只要压缩比 ≥ 10%，cache 牺牲就值得；否则降一级再压。

**逐级递减的语义**：从最宽松的保留（级 1）开始尝试，如果压缩比不达标（< 10%），进入更激进的级（级 2），以此类推，直到级 8 仍不达标则放弃压缩（`NoCompressionNeeded`）。

**为什么不用单一策略**：long-running task 场景下，用户消息稀疏但每个 assistant 后都有大量工具调用。固定"保留最近 K 轮"要么 K=3 就撑满 budget，要么 K=1 丢光信息。逐级递减自动适配不同场景的"信息密度"。

### 6.2 8 级策略定义

按"user/assistant 保留度"和"工具调用保留度"两个维度递减：

| 级 | user 消息 | assistant 消息 | 工具调用保留 | 说明 |
|---|---|---|---|---|
| **1** | 全部 | 全部 | 最近 5 个 assistant 消息之间的所有 tool_* | 最宽松，先试这个 |
| **2** | 全部 | 全部 | 最近 3 个 assistant 消息之间的所有 tool_* | 收紧工具范围 |
| **3** | 全部 | 全部 | 最近 1 个 assistant 消息之间的所有 tool_* | 只保留最近轮的工具 |
| **4** | 全部 | 最近 5 个 | 最近 1 个 assistant 消息之间的所有 tool_* | 开始丢远期 assistant |
| **5** | 全部 | 最近 5 个 | **全部丢弃**（全部走 LLM 摘要） | 只剩骨架 |
| **6** | 全部 | 最近 3 个 | **全部丢弃** | 进一步收紧 |
| **7** | 全部 | 最近 1 个 | **全部丢弃** | 极简骨架 |
| **8** | (全部走 LLM 摘要) | (全部走 LLM 摘要) | (全部走 LLM 摘要) | 仅保留 system block + summary + 当前 user message |

**关键澄清**：`ask_user` 工具调用**不构成 user 消息**——它是 round 内部的事件，用户在 ask_user 后的"选择/确认"是 `tool_result`，不是新一轮 user 输入。`user 消息` 只指 `MessageRole::User` 类型的消息。

---

## 7. 压缩算法

### 7.1 主流程：`plan_compression` + `CompressionPlan`

```rust
/// 8 级递减压缩策略
/// 从级 1 开始尝试，直到压缩比达到 ≥ MIN_COMPRESSION_RATIO (10%)
/// 返回 CompressionPlan，执行 plan.apply(history) 完成压缩
pub fn plan_compression(history: &HistoryState) -> Result<CompressionPlan> {
    const MIN_COMPRESSION_RATIO: f64 = 0.10;  // 至少压掉 10% 才算"值得"

    let original_tokens = history.current_tokens;
    let target_tokens = history.effective_input_budget;
    let needed_ratio = 1.0 - (target_tokens as f64 / original_tokens as f64);

    tracing::info!(original_tokens, target_tokens, needed_ratio, "Planning compression");

    // 从级 1 到级 8 逐级尝试
    for level in 1..=8 {
        let plan = CompressionPlan::for_level(level, history);
        let projected_tokens = plan.projected_tokens();
        let compression_ratio = 1.0 - (projected_tokens as f64 / original_tokens as f64);

        tracing::debug!(level, projected_tokens, compression_ratio, "Trying compression level");

        if compression_ratio >= MIN_COMPRESSION_RATIO {
            tracing::info!(level, compression_ratio, "Compression plan selected");
            return Ok(plan);
        }
    }

    // 8 级都不达标——历史已接近 budget，无压缩空间（§13.4）
    Ok(CompressionPlan::no_compression())
}
```

`CompressionPlan::for_level` 按 §6.2 表格实现（伪代码）：

```rust
impl CompressionPlan {
    fn for_level(level: u8, history: &HistoryState) -> Self {
        match level {
            1 => Self::user_assistant_all_tools_for_last_assistants(history, 5),
            2 => Self::user_assistant_all_tools_for_last_assistants(history, 3),
            3 => Self::user_assistant_all_tools_for_last_assistants(history, 1),
            4 => Self::keep_users_all_keep_assistants_last_keep_tools_for_last_assistants(history, 5, 1),
            5 => Self::keep_users_all_keep_assistants_last(history, 5),
            6 => Self::keep_users_all_keep_assistants_last(history, 3),
            7 => Self::keep_users_all_keep_assistants_last(history, 1),
            8 => Self::summary_only(history),
            _ => unreachable!(),
        }
    }
}
```

**级 1-3 语义**：保留所有 user/assistant；找到最近 K 个 assistant 消息，其**之间及之后**的 tool_* 消息保留；其余中间部分 → LLM summary。级 4 收紧 assistant 保留范围；级 5-7 丢弃全部工具；级 8 仅骨架 + summary。

### 7.2 `apply` 强制压缩比校验

```rust
impl CompressionPlan {
    pub fn apply(self, history: &mut HistoryState) -> Result<CompressionOutcome> {
        let original_tokens = history.current_tokens;
        let projected = self.projected_tokens();
        let ratio = 1.0 - (projected as f64 / original_tokens as f64);

        if ratio < MIN_COMPRESSION_RATIO {
            return Err(CompressError::InsufficientCompression { projected_ratio: ratio });
        }

        history.apply_plan(self)?;  // drain 中间 → 插入 summary marker + 保留的 user/assistant/tool

        Ok(CompressionOutcome::Compacted {
            level: self.level,
            original_tokens,
            new_tokens: history.current_tokens,
            compression_ratio: ratio,
        })
    }
}
```

### 7.3 压缩后的消息布局（与 ADR-060 Block 结构对齐）

```
[Block A: system block]                                    ← cache hit 1
[Block B: 保留的 user/assistant + 保留的工具]              ← cache hit 2（前部）
[Block B 内: summary marker（User, name=compaction_summary,
  内容 = level 元数据 + <summary> + <user_intent>）]        ← 插入点，其后 suffix 失效
[Block B: 尾部保留的 user/assistant + 工具]                ← cache hit 3（尾部原文）
[Block C: todo 快照] / [Block D: 当前 user message]        ← 由 ADR-060 负责
```

注意：summary marker 位于 Block B **内部**（中间替换语义不变，`replace_middle_with_summary` 的既有行为），不是独立的 SystemMessage——这是与早期草稿的关键差异（见 §2.3）。

---

## 8. summary 与 user_intent

### 8.1 summary prompt 的强制结构

```rust
pub const COMPACTION_SYSTEM_PROMPT: &str = r#"
You are compressing a conversation history. Output MUST be:

<summary>
[已完成的工作、当前进度、关键决策]
</summary>

<user_intent>
[MUST 列出所有用户的原始意图与显式约束,即使已被满足或不再相关]
</user_intent>

<triples>
[结构化知识:实体、关系、关键事实]
</triples>
"#;
```

（per-agent 定制由 ADR-053 的 `prompts/summary.md` 覆盖，本结构为最低强制要求。）

### 8.2 `<user_intent>` 处理（修正早期草稿）

- **早期草稿方案**："user_intent 作为独立 SystemMessage 插入 Block A 之后，带 cache_control"——**废弃**。理由与 §2.3 相同：`build()` 过滤 history 中所有 System 消息；Anthropic 会把所有 System 消息提升到顶层 `system` 字段并互相覆盖（ADR-060 P0-1 审查结论）。
- **本 ADR 方案**：`<user_intent>` 作为 **summary marker 文本的一部分**（位于 `<summary>` 之后）随 marker 一起保留；解析时单独提取用于校验与调试，但**在请求中不单独成消息**。
- **缺失时 fallback**：LLM 未输出 `<user_intent>` 时，用原始 user 消息拼接作为 user_intent（§13.3）。

### 8.3 summary 格式异常 fallback

`<summary>` 标签缺失时，把整个 LLM 输出当作 summary；`<user_intent>` 缺失时 fallback 原始 user 消息。**保证无论 LLM 输出格式如何，总有可用内容，压缩不会因格式异常失败。**

---

## 9. 压缩 level 元数据

**问题**：压缩完成后，从 history 里只能看到"有一条 summary"，看不出用了哪一级、保留了什么。调试"为什么上下文不对"时，必须翻日志才能知道 `level=6` 意味着"user 全留、assistant 只留 3 轮、工具全丢"。

**设计**：**Runtime 在压缩完成后，把 level 元数据写在 summary marker 内容的最前面**。元数据是 Runtime 生成的（非 LLM 输出），格式固定、可机器解析：

```text
[compressed: level=6]
  user_messages: all(12)
  assistant_messages: last 3
  tool_messages: none
  tokens: 234567 -> 34567 (ratio 85.3%)

<summary>
...
</summary>
```

**写入时机**：`CompressionPlan.apply` 构建 summary marker 时，在 LLM 输出的 `<summary>` 内容**之前**拼接元数据块。

**实现要点**：

```rust
fn build_summary_metadata(plan: &CompressionPlan, original_tokens: u64, new_tokens: u64) -> String {
    let compression_ratio = 1.0 - (new_tokens as f64 / original_tokens as f64);
    format!(
        "[compressed: level={}]\n\
         user_messages: {}\n\
         assistant_messages: {}\n\
         tool_messages: {}\n\
         tokens: {} -> {} (ratio {:.1}%)\n\n",
        plan.level,
        plan.summarize_retention(),   // 如 "all(12)" / "last 3" / "none"
        plan.original_tokens,
        new_tokens,
        compression_ratio * 100.0,
    )
}
```

**level 与保留内容的对应关系**（调试查表即可）：

| level | 可判断的保留结果 |
|---|---|
| 1-3 | 所有 user + 所有 assistant + 最近 K(5/3/1) 个 assistant 之间的工具 |
| 4 | 所有 user + 最近 5 个 assistant + 最近 1 个 assistant 之间的工具 |
| 5-7 | 所有 user + 最近 K(5/3/1) 个 assistant + **无工具** |
| 8 | 仅 system + summary + 当前 user 消息 |

**为什么写在 summary 文本里而不是单独消息**：不引入新消息角色（§2.3 契约）；从 history 直接可见，调试不需要查日志；后续压缩时旧元数据随旧 summary 一起被覆盖，只保留最新一次 level。

---

## 10. 工具自动压缩的归宿

### 10.1 决策

**结论**：**关闭 LLM 自主工具压缩**，`context_abandon` 不再注册。

**理由**：
- LLM 自主调用 `context_abandon` → 原位替换占位符 → 中间字节变化 → Block B cache 失效（与 ADR-060 核心理念冲突）。
- 其价值完全可由 8 级策略覆盖（级 1-7 把"工具调用保留"作为可调维度），且 8 级策略由 Runtime 统一调度，不把 cache 决策权交给不理解 cache 的实体。

### 10.2 改造（以实际代码为准）

| 项 | 现状 | 改造 |
|---|---|---|
| 工具注册门控 | `tool_compression_enabled`（默认 true）同时门控 `context_retrieve` + `context_abandon`（[builtin/mod.rs:195-201](core/acowork-runtime/src/tools/builtin/mod.rs#L195-L201)） | 拆开门控：`context_retrieve` **固定注册**（压缩后取回通道）；`context_abandon` **不再注册**（deprecated，代码保留向后兼容） |
| 配置字段 | `agent_config.rs:216` `tool_compression_enabled: Option<bool>` + `RuntimeConfigUpdate` 热重载 + `AgentCore::sync_platform_tools_to_registry` | 移除字段与热重载路径（或改名为 `context_retrieve_enabled` 仅控制 retrieve） |
| 队列机制 | `AbandonQueue` / `RetrieveQueue`（loop_.rs:383-395） | `AbandonQueue` 删除；`RetrieveQueue` 保留 |
| UI | agent setup "Enable tool compression" 选项 | 移除（P1 UI 改动） |

**保留的能力**：`context_retrieve` 工具保留——LLM 仍可显式召回被压缩的工具结果（走 ADR-060 Block B append-only 路径，不破坏 cache）。

---

## 11. FIFO 路径的归宿：彻底删除

### 11.1 为什么删除

1. **8 级策略下不可达 = 死代码**：级 8 必然把 history 压到最低；级 8 仍不达标 → `NoCompressionNeeded`（history 已够小）；LLM 不可用 → 显式失败。三条路都不需要 FIFO。
2. **FIFO 一旦触发 = 灾难性 cache miss**：比"压缩失败"更糟糕（静默、每轮全量付费）。
3. **极端场景应该显式失败**：让用户决策（新会话 / 大窗口模型），而不是"看似正常但 cache 全失效"。

### 11.2 完整调用点盘点（生产代码，修正早期草稿）

| 调用点 | 位置 | 用途 |
|---|---|---|
| `trim_history_to_budget` 本体 | [loop_context.rs:218-237](core/acowork-runtime/src/agent/loop_context.rs#L218-L237) | Stage 1 FIFO + Stage 2 emergency |
| 迭代主循环 | [loop_.rs:1444](core/acowork-runtime/src/agent/loop_.rs#L1444) | 每轮 LLM 调用前 |
| 会话恢复/工具结果路径 | [loop_.rs:927](core/acowork-runtime/src/agent/loop_.rs#L927)、[loop_.rs:946](core/acowork-runtime/src/agent/loop_.rs#L946)、[loop_.rs:1194](core/acowork-runtime/src/agent/loop_.rs#L1194) | 恢复/暂停恢复等场景 |
| `pre_trim_for_tool_results` | [loop_context.rs:1278-1300](core/acowork-runtime/src/agent/loop_context.rs#L1278-L1300)（1298 调 `trim_history_to_budget`） | 追加大 tool_result 前 |
| `compact_history_if_needed` 内 | [loop_context.rs:788/802-804/879](core/acowork-runtime/src/agent/loop_context.rs#L788-L879) | 压缩失败/压缩后仍超限的 fallback |
| `check_context_overflow_and_trim` | [loop_context.rs:1072+](core/acowork-runtime/src/agent/loop_context.rs#L1072)（1085 emergency） | 90%/95% 硬阈值紧急路径 |
| `call_llm_streaming_inner` | [loop_llm.rs:436](core/acowork-runtime/src/agent/loop_llm.rs#L436) | 流式调用 400/超限重试路径 |

**删除的 API**：
- `HistoryManager::trim_fifo()` → 删除
- `HistoryManager::emergency_trim()` → 删除（保留 `fit_to_budget_lossless` 作为恢复期的无损裁剪——它与 cache 无关，是模型切换时的防御）
- `trim_history_to_budget` → 重写为只走 8 级压缩，不再有 FIFO/emergency 分支
- 上述全部调用点改道 `compact_history_if_needed`（已存在）或显式错误返回

### 11.3 极端场景行为

LLM 不可用 / 压缩失败时：

```rust
match compact_via_llm(...).await {
    Ok(artifacts) => { /* 8 级 plan + apply */ }
    Err(e) => {
        tracing::error!(error = %e, "LLM compaction failed — refusing to fall back to FIFO");
        // 1. 不修改 history
        // 2. emit ChunkEvent::Error
        return CompactResult::LlmUnavailable { reason: e };
    }
}
```

前端响应：`ChunkEvent::Error { user_message: "Context compaction failed. Please start a new conversation or compress manually.", error_type: "ContextOverflow" }`。

用户可选动作：新建会话 / 手动选更大 context window 的模型 / 手动触发压缩（已有 "Compress Summary" 按钮）。

---

## 12. 与 ADR-052 的关系

| ADR-052 提供 | 本 ADR 使用 |
|---|---|
| `context_abandon` 工具（LLM 自主触发） | **废弃**：不再注册，避免 LLM 自主压缩破坏 cache 连续性（§10） |
| `context_retrieve` 工具（LLM 取回） | **保留**：压缩后 LLM 仍可显式取回被压缩的历史，走 ADR-060 Block B append-only |
| `tool_compression_enabled: bool` 开关 | **移除**：注册门控拆分，abandon 不再注册（§10.2） |

**关键决策**：ADR-052 的"LLM 自主触发压缩"模式**不再采用**——工具压缩由 8 级策略统一调度（级 1-7 把工具调用保留作为可调维度），`context_retrieve` 作为取回通道保留。

---

## 13. 验收准则与异常边界

### 13.1 压缩比 ≥ 10%

任何一次成功压缩必须满足 `compression_ratio >= 10%`（启发式依据见 §3.3）。不达标 → 降级重试；8 级全不达标 → `NoCompressionNeeded`。

### 13.2 级 1-7 必须保留所有 user 消息

**核心不变量**：级 1-7 保留**所有** `MessageRole::User` 消息，直到级 8 才允许全部进 summary。

**理由**：user 消息是 LLM 唯一无法推理得到的"硬约束来源"；assistant + tool 是 LLM 自己产出的，丢了还能从 summary 重建；user 丢了就真的丢了。

**实现**：`assert_user_messages_preserved(plan, original)` 校验，违反即 `CompressError::BugInPlan`。

### 13.3 summary 必须包含 user_intent

LLM 输出缺少 `<user_intent>` 时，fallback 用原始 user 消息拼接（§8.2）；`<summary>` 标签缺失时整段输出当 summary（§8.3）。

### 13.4 边界总览表

| 边界 | 类型 | 行为 | 用户感知 |
|---|---|---|---|
| **压缩比 < 10%** | 验收不通过 | 降一级重试；8 级都不达标 → NoCompressionNeeded | 无感（自动降级） |
| **级 1-7 丢失 user** | 验收不通过 | 返回 BugInPlan（plan 自身 bug） | 无感（plan 不会出错） |
| **summary 缺 user_intent** | 验收不通过 | fallback 到原始 user 消息拼接 | 无感 |
| **LLM 不可用** | 异常 | 不修改 history，emit `ChunkEvent::Error` | 前端提示"压缩失败" |
| **8 级全不达标** | 异常 | NoCompressionNeeded（history 已够小） | 无感 |
| **空 history** | 守卫 | 不进入压缩（已有守卫，无需改） | 无感 |
| **summary 格式异常** | fallback | 整段当 summary，user_intent fallback | 无感 |
| **budget < 8K** | 启动时拒绝 | session 启动失败 / model_switch 拒绝 | 前端提示"模型不支持" |

**budget 校验**：

```rust
const MIN_BUDGET_FOR_AGENT: u64 = 8_192;  // 8K

fn validate_model_budget(model_caps: &ModelCapabilitiesInfo) -> Result<()> {
    if model_caps.effective_input_budget(32_768) < MIN_BUDGET_FOR_AGENT {
        return Err(RuntimeError::UnsupportedModel(
            "Model context window too small for agent loop (min 8K)".to_string()
        ));
    }
    Ok(())
}
```

校验点：`session_init`（启动）+ `model_switch` handler。理由：小于 8K 时 system block 占 2K、summary 最少 1K，留给 tail + 当前 user 消息不足 1K——任何 tool_result 都会超限。

**核心原则**：所有边界都有明确行为，**绝不静默退化为 FIFO 或破坏 cache 连续性**。

---

## 14. 可观测性

### 14.1 CompressionOutcome

```rust
pub enum CompressionOutcome {
    NoCompressionNeeded,
    Compacted {
        level: u8,                          // 哪一级策略成功
        original_tokens: u64,
        new_tokens: u64,
        compression_ratio: f64,
        user_messages_kept: usize,
        assistant_messages_kept: usize,
        tool_messages_kept: usize,
        summary_tokens: u64,
        user_intent_tokens: u64,
    },
    LlmUnavailable { reason: String },
}
```

### 14.2 Debug 面板 "Compression History" 子面板

展示：每次压缩的 level / compression_ratio / user_messages_kept / summary_tokens；当前 user_intent 内容（可滚动）；8 级策略的尝试日志（诊断"为什么停在级 3"）。

### 14.3 事件维度 vs 状态维度

- `CompressionOutcome::Compacted.level` 记录**事件**维度（emit 给 observer / Debug 面板），压缩发生时消失；
- §9 的 level 元数据记录**状态**维度（持久存在于 history 中），供事后排查"这个会话最后压缩到什么程度"；
- 两者共享同一个 `CompressionPlan.level` 值，保持一致。

---

## 15. 改造清单

| 编号 | 内容 | 涉及文件 | 优先级 |
|------|------|----------|--------|
| 1 | 常量 `MIN_COMPRESSION_RATIO=0.10` / `MIN_BUDGET_FOR_AGENT=8192` / summary token 上限常量 | 新 `compression_constants.rs` | **P0** |
| 2 | `CompressionPlan::for_level` + `plan_compression` 8 级策略 | `core/acowork-runtime/src/agent/history.rs` | **P0** |
| 3 | `CompressionPlan::apply` 强制压缩比 ≥ 10% + summary marker 构建（保持 User role + `name=compaction_summary`） | `core/acowork-runtime/src/agent/history.rs` | **P0** |
| 4 | `assert_user_messages_preserved` 验收（级 1-7 全保留 user） | `core/acowork-runtime/src/agent/history.rs` | **P0** |
| 5 | `parse_and_validate_summary` + `<user_intent>` fallback 原始 user 消息 | `core/acowork-runtime/src/agent/history.rs` + `prompt.rs` | **P0** |
| 6 | `COMPACTION_SYSTEM_PROMPT` 更新为三章节强制结构 | `core/acowork-runtime/src/agent/prompt.rs` | **P0** |
| 7 | LLM 不可用 → 不回退 FIFO，emit `ChunkEvent::Error` | `core/acowork-runtime/src/agent/loop_context.rs` | **P0** |
| 8 | budget < 8K 校验（session 启动 + model_switch） | `core/acowork-runtime/src/startup/session_init.rs` + `model_switch` handler | **P0** |
| 9 | 8 级全不达标 → `NoCompressionNeeded`（不强行压缩） | `core/acowork-runtime/src/agent/history.rs` | **P0** |
| 10 | `trim_fifo` / `emergency_trim` 删除，`trim_history_to_budget` 重写；§11.2 全部调用点改道 | `core/acowork-runtime/src/agent/{history.rs,loop_context.rs,loop_.rs,loop_llm.rs}` | **P0** |
| 11 | `context_abandon` 停止注册（deprecated 保留代码）；`context_retrieve` 固定注册 | `core/acowork-runtime/src/tools/builtin/mod.rs` + `tools/registry.rs` | **P0** |
| 12 | `tool_compression_enabled` 字段与热重载路径移除（或语义改为仅控 retrieve） | `core/acowork-runtime/src/agent_config.rs` + `AgentCore::sync_platform_tools_to_registry` | **P0** |
| 13 | `AbandonQueue` 删除；`RetrieveQueue` 保留 | `core/acowork-runtime/src/agent/loop_.rs` + `context_compression.rs` | **P0** |
| 14 | `build_summary_metadata` 写入 level / 保留统计 / token 变化到 summary 文本最前 | `core/acowork-runtime/src/agent/history.rs` | **P0** |
| 15 | `CompressionOutcome` 扩展（level / 各角色保留数 / summary_tokens） | `core/acowork-runtime/src/agent/loop_.rs` | **P0** |
| 16 | Debug 面板 "Compression History" 子面板 | `core/acowork-runtime/src/agent/loop_.rs` + observer + 前端 | **P1** |
| 17 | agent setup 界面移除 "Enable tool compression" 选项 | `apps/acowork-desktop/src/...` | **P1** |
| 18 | 压缩回归测试：marker 契约（User role + name）、restorer 恢复、episode_distill 保护 | `core/acowork-runtime/tests/...` | **P0** |

**实施顺序**：2/3 → 5/6（summary 管线）→ 7/9（失败路径）→ 10（FIFO 删除，最大改动，最后）→ 11/12/13（工具压缩关闭）→ 14/15/16（可观测性）→ 8（budget 校验）→ 17（UI）。

---

## 16. 影响与回滚

| 维度 | 改动前 | 改动后 |
|---|---|---|
| 压缩策略 | 保留固定 3 轮 + FIFO 删头 | **8 级递减 + 10% 最低压缩比门槛**（§6） |
| FIFO 触发频率 | 偶发（压缩后仍超限时） | **永远不触发**（代码删除） |
| Block B cache 失效原因 | todo / memory / FIFO 删头 / LLM 自主压缩 | **只剩 todo**（ADR-060 解决） |
| summary LLM 调用次数 | 每超限 1 次 | 每超限 1 次（级 1 达标则只调 1 次） |
| 压缩结果可调试性 | 无（不知道保留了什么） | **summary 内嵌 level 元数据**（§9） |
| 工具压缩 | LLM 自主调用（破坏 cache） | **关闭**，8 级策略统一调度（§10） |
| 极端场景（LLM 不可用） | FIFO 救场，代价 cache 全失效 | **显式失败**，前端提示用户（§11.3） |

**回滚**：核心改动在 `history.rs` + `loop_context.rs` 内，可独立 commit + revert；`trim_fifo`/`emergency_trim` 删除前先确认无其他引用（§11.2 盘点为唯一清单）。

---

## 17. 与现有 ADR 的关系

| 现有 ADR | 关系 |
|---|---|
| [ADR-010](./ADR-010-context-compression-simplification.md) | 本 ADR 的 8 级策略是"摘要粒度调度器"而非"丢弃决策器"，边界见 §4 |
| [ADR-011](./ADR-011-compaction-as-distillation.md) | `KEEP_LAST_ROUNDS=3` 改为 8 级字节预算；summary marker 契约（User role + name）**保持不变** |
| [ADR-052](./ADR-052-tool-compression-llm-autonomous.md) | LLM 自主压缩模式废弃；`context_retrieve` 保留（§12） |
| [ADR-053](./ADR-053-agent-specific-compaction-prompt.md) | summary prompt 的 per-agent 定制机制保留，本 ADR 仅强制三章节结构（§8.1） |
| [ADR-056](./ADR-056-global-default-compact-model.md) | 压缩模型解析链保留，`compact_history_if_needed` 入口不变 |
| [ADR-057](./ADR-057-compaction-distillation-into-graph.md) | 蒸馏入图（triples）独立推进，本 ADR 不涉及 |
| [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md) | 本 ADR 的压缩产物按 Block A/B/C/D 布局注入；不改变 ADR-060 的任何消息角色约定 |

---

## 18. 总结

本 ADR 用**单一原则**——"压缩比是优化指标，LLM 是唯一信息重建通道，FIFO 是必须消除的 cache 杀手"——重构上下文压缩机制：

- **8 级递减策略**：从"全部 user/assistant + 最近 5 个 assistant 之间的工具"逐级收紧，到级 8"仅骨架 + summary"；每级只保留到"压缩比 ≥ 10%"就停，不达标降级重试。
- **对话骨架优先**：级 1-7 保留所有 user 消息，assistant 次之，工具调用最后丢（§13.2）。
- **summary marker 契约不变**：保持 `User` 角色 + `name="compaction_summary"`，level 元数据以纯文本内嵌（§9），与 restorer / episode_distill / ADR-060 Block B 过滤全部兼容。
- **FIFO 物理删除**：§11.2 完整盘点 7 类调用点全部改道；LLM 不可用 → 显式失败 + 用户决策。
- **工具自主压缩关闭**：`context_abandon` 不再注册，`context_retrieve` 保留取回通道。
- **可观测性**：level 元数据（状态维度）+ CompressionOutcome（事件维度）双通道。

**关键澄清**：
- 8 级策略是"摘要粒度调度器"，不是"丢弃决策器"——被压缩的内容全部进入 LLM summary（§4）。
- 压缩的 cache miss 是"沉没成本"，后续 token 节约远超代价；真正决定成败的是 summary 是否保留用户意图与关键决策（§3.4）。
- 压缩失败绝不退化为 FIFO——显式失败 + 用户决策，优于静默 cache 全失效（§11.3）。
