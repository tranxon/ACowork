# ADR-011: 上下文摘要与蒸馏统一策略

**状态**：提议中  
**日期**：2026-05-28  
**决策者**：架构讨论  
**影响范围**：`ADR-010`, `03-agent-runtime.md`, `05-memory.md`, `loop_.rs`, `history.rs`, `episode_distill.rs`, `conversation.rs`

---

## 背景

ADR-010 确立了上下文压缩的三阶段策略（70% 告警 → 80% LLM 摘要 → 95% emergency_trim），其中 Phase 2 "LLM 摘要 + trim 时蒸馏恢复" 待实施。

在细化 Phase 2 方案时，发现 ADR-010 隐含了"摘要和蒸馏是两个独立操作"的假设——摘要负责压缩内存上下文，蒸馏将裁剪掉的消息写入 Grafeo。深入分析后发现这个分离设计存在两个问题：

1. **trim 时蒸馏语义不完整**：蒸馏只看被裁剪的中间段消息，看不到最后 2-3 轮，无法知道对话的最终走向。可能将已废弃的决策、已纠正的错误分析记录为长期记忆。

2. **重复调用浪费**：摘要和蒸馏两次调用 Compact Model，摘要输入完整上下文输出的自然语言叙述已经包含了蒸馏 JSON 想要的所有信息，且自然语言在 Grafeo 中的语义检索效果优于结构化 JSON 字段。

## 核心洞察

**摘要文本本身就是最好的蒸馏结果。** 一次 Compact Model 调用产出的自然语言摘要，既可以替换内存中间段（给主 LLM 续接对话），也可以直接写入 Grafeo（作为长期记忆用于跨 session 检索）。不需要两次调用，也不需要蒸馏 JSON 结构化输出。

同时，session 关闭时如果只依赖"是否发生过 compaction"来判断是否跳过蒸馏，会遗漏 compaction 之后新增的对话段落。

## 决策

### 统一策略：摘要即蒸馏

```
Compact Model 输入:  完整上下文（包括最后 2-3 轮，不做任何排除）
Compact Model 输出:  一段自然语言摘要

                     ┌──────────────┴──────────────┐
                     ▼                              ▼
             替换内存中间段                    写入 Grafeo
       [sys, 摘要, 最后 2-3 轮]            摘要文本 = 蒸馏结果
           ↑ 冗余但无害                     语义完整，检索友好
```

**关键原则：**

1. **完整上下文输入**：Compact Model 看到全部消息（system prompt + 所有 user/assistant/tool 消息），包括最后 2-3 轮。不排除任何消息。

2. **内存替换保留尾巴**：用摘要替换中间段时，保留最后 2-3 轮不替换。摘要覆盖了完整上下文，所以摘要和最后 2-3 轮有信息重叠（冗余），但冗余是良性的——不会导致矛盾，主 LLM 能正确理解。

3. **冗余无害**：摘要覆盖了最后 2-3 轮的信息，但主 LLM 接着看原文不会有冲突——类似于先看预告片再看正片，信息一致。

### Session 生命周期蒸馏

```
Session 运行中:
  ├── 80% 触发 → Compaction: 摘要写入 Grafeo
  ├── 再次 80% → Compaction: 摘要写入 Grafeo
  └── ...

Session 关闭时:
  ├── 最后一条消息 == compaction 摘要（之后零新增）
  │     → 跳过（全部知识已在 Grafeo）
  │
  └── 摘要之后还有新增对话
        ├── 发生过 compaction → 蒸馏尾部（仅最后一次 compaction 之后的部分）
        └── 从未 compaction  → 蒸馏完整 JSONL（短 session，必然 < Compact Model 窗口）
```

**判断逻辑**：用 `last_compaction_line` 跟踪最后一次 compaction 写入 JSONL 的位置。关闭时比较 `last_compaction_line` 与当前 JSONL 总行数。

### 不做尾部蒸馏最小阈值

尾部蒸馏始终走 LLM（不直接追加原始文本到 Grafeo），不设 token 阈值。理由：
- 尾部本身不会很大（未触发 80% 意味着 < context_window × 80%）
- 直接追加原始文本会引入寒暄噪声（"好的，谢谢"等），必须经过 LLM 提炼
- Session 关闭是低频操作，额外成本可忽略

### Memory Recall 保持不变

Memory recall 仍然只查询 Grafeo，不做 JSONL 检索。两层职责不变：
- Grafeo：长期记忆的唯一入口（所有 session 的知识经 compaction/session-close 蒸馏汇入）
- JSONL：前端加载历史会话渲染用，不参与检索

### 经历层写入来源简化：移除每轮对话实时写入

原有设计中，每轮对话都会实时写入一条 episode 到 Grafeo 经历层（`05-memory.md` §2）。此设计与 JSONL 功能重叠：

| 存储 | 内容 | 粒度 |
|------|------|------|
| JSONL | 完整对话原文 | 逐条消息 |
| 经历层（旧） | 每轮对话 episode | 逐轮写入 |
| 经历层（新） | compaction / session-close 摘要 | 分段摘要 |

**JSONL 已经是逐条消息的完整真相来源**，在 Grafeo 中再存一份逐轮副本是冗余的：
- 沉淀层离线巩固读取的是摘要文本（自然语言），不依赖逐轮原始记录
- 即时提取（`memory_store` tool call）完全不经过经历层，直接写入沉淀层
- 用户如需检索原始对话，可以直接查 JSONL（前端已支持按 session 加载历史）

**决策**：经历层的唯一写入来源为 compaction 摘要和 session-close 蒸馏摘要。移除每轮对话实时写入经历层的逻辑。

对离线巩固的影响：
- 离线巩固不再能在 session 中途触发（必须等 compaction 或 session 关闭）
- 这不构成功能损失——离线巩固（Phase 3）本身的设计前提就是批量处理已完成的对话段落
- Grafeo 存储更干净，检索噪声更小，巩固 LLM 输入更精准

## 影响

### 需要修改的代码

- `core/rollball-runtime/src/agent/history.rs`：
  - 新增 `compact_via_llm()` — 构造完整上下文发送给 Compact Model，返回摘要文本
  - 新增 `replace_middle_with_summary()` — 用摘要替换中间段，保留最后 N 轮

- `core/rollball-runtime/src/agent/loop_.rs`：
  - `trim_history_to_budget()` 改为三阶段：70% warn → 80% compact_via_llm → 95% emergency_trim
  - 80% 触发时：调 compact_via_llm → 替换内存历史 → 摘要异步写入 Grafeo
  - 不再单独调用 `distill_on_trim`（被 compaction 摘要替代）

- `core/rollball-runtime/src/agent/session_state.rs`：
  - 新增 `last_compaction_line: Option<u64>` 字段

- `core/rollball-runtime/src/conversation.rs`：
  - 新增 `line_count()` 方法，返回当前 JSONL 行数（用于 session 关闭时判断）

- `core/rollball-runtime/src/episode_distill.rs`：
  - `distill_on_trim()` 标记 deprecated（不再从 trim 路径调用）
  - 新增 `distill_tail(session_path, start_line)` — 蒸馏 JSONL 中从 start_line 到末尾的部分
  - 或简化为：始终用 `distill_on_session_end`，但从指定行开始读取

### 简化的部分

- 不再需要单独的蒸馏 JSON 结构化输出（summary + intent_type + decision + tool_summary + keywords + importance）
- 不再需要 trim 时蒸馏（`distill_on_trim` 废弃）
- 不再需要每轮对话实时写入经历层（与 JSONL 冗余）
- ADR-010 Phase 2 的 "trim 时蒸馏恢复" 被本 ADR 明确替代

### 需要更新的设计文档

| 文档 | 变更 |
|------|------|
| ADR-010 | Phase 2 描述更新：标注蒸馏策略由 ADR-011 细化 |
| `03-agent-runtime.md` | §②.5 三阶段压缩描述更新为"摘要即蒸馏" |
| `05-memory.md` | §2 经历层写入来源简化为仅 compaction/session-close 摘要；移除每轮对话实时写入描述 |

## 后果

### 正面

- **一次调用完成两件事**：Compact Model 调用次数减半
- **蒸馏语义完整**：输入完整上下文，不会丢失最后几轮的关键结论
- **Grafeo 检索质量提升**：自然语言摘要的语义检索效果优于结构化 JSON
- **架构简化**：不再区分"蒸馏"和"摘要"两个概念，统一为"摘要"，记忆层和上下文层共用
- **存储精简**：移除每轮对话实时写入经历层，Grafeo 仅存储分段摘要，消除与 JSONL 的存储冗余

### 负面

- **摘要文本稍冗余**：包含最后 2-3 轮的信息，但已论证冗余无害
- **session 关闭时仍需蒸馏尾部**：相比"不蒸馏"，多了一次 Compact Model 调用

### 与 ADR-010 的关系

本 ADR 是 ADR-010 Phase 2 的细化方案，明确了 LLM 摘要的具体实现策略和与 Grafeo 蒸馏的合并方式。ADR-010 中"trim 时蒸馏恢复"的描述被本 ADR 替代。
