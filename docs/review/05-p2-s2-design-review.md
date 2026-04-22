# Phase 2 S2 设计修改 Review — 第二轮

**审查日期**：2026-04-22
**审查范围**：docs/05-memory.md、docs/module-design/04-grafeo.md、docs/plan/plan-p2.md（基于 04-p2-s2-design-review.md 修改落地后的版本）
**审查目的**：验证第一轮 review（04-p2-s2-design-review.md）的修改是否正确落地，发现新引入的问题
**状态**：已完成

---

## 1. 整体评价

整体落地质量较好，第一轮 review 的核心决策均已反映到设计文档中。特别是以下几点做得扎实：

- **LLM 优先原则**已写成独立 §0.1，统一了整个文档的语义判断归属
- **memory_store 接口简化**（三元组→自然语言）方向正确，实现路径清晰
- **Purge 三条路径**替代了原来模糊的"90天"单一条件
- **4 级降级链路**有 SLA 数字，500ms 硬超时和各环节预算分配合理
- **purge_log 快照**支持 30 天恢复，数据结构定义完整

---

## 2. 发现的问题

### P0 — 逻辑矛盾（会直接导致实现错误）

#### 问题 1：`05-memory.md` §9 Phase 1 描述与新设计不一致（遗留旧描述）

`§9 Phase 1（分阶段实现路线）` 仍保留了旧版描述，与 §6.3、§4.1 产生矛盾：

**矛盾点 A**：§9 Phase 1 描述"硬限制 2 跳"，但 §6.3 已改为 3 跳 + 早期终止，`plan-p2.md S2.8` 也已是 3 跳。§9 是漏改的遗留文字，开发者按 §9 实现会做错。

**矛盾点 B**：§9 Phase 1 描述"Fact 自动按 (subject, predicate) 语义去重"，但 §4.1 已将去重分为"即时粗筛（embedding 相似度 > 0.85）"和"离线精确去重（三元组）"两个阶段，原来的同步三元组去重已从即时阶段移除。

**建议**：同步 §9 描述，将 2 跳改为 3 跳，将 Fact 去重描述改为分阶段机制。

---

#### 问题 2：`§5.2 Fact 去重`没有说清楚"发生在离线巩固阶段"

`§4.1` 说：写入 PendingKnowledgeNode 时做"冲突候选检测（embedding 相似度 > 0.85）"。但 `§5.2 Fact 语义去重`说："写入前检查：新 Fact 的 `(subject, predicate)` 是否与已有 Active 节点相同"。

两者有矛盾：PendingKnowledgeNode 阶段尚未提取三元组，无法做 `(subject, predicate)` 去重。§5.2 的三元组去重应发生在离线巩固阶段，但文字没有说清时机，读者容易误解为即时提取时也做三元组去重。

**建议**：在 §5.2 Fact 语义去重一节首行明确标注："以下去重逻辑发生在**离线巩固阶段**（三元组提取后），不是即时提取阶段。"

---

### P1 — 设计缺口（实现时会卡住）

#### 问题 3：`memory_hint` 字段名 `t` 与枚举值 `t` 同名冲突

`§1 Memory Hint 指令`中定义：

```
- t: s=语义联想 f=精确事实 t=时间相关 i=身份偏好
```

字段名为 `t`，枚举值中也有一个 `t`（代表"时间相关"）。写 parser 代码时 `{"e":["上海"],"t":"t"}` 中 key 和 value 均为 `"t"`，极易混淆，且 prompt 示例难以阅读。

**建议**：将"时间相关"枚举值改为 `r`（recency）或 `d`（datetime），避免与字段名 `t` 重名。

---

#### 问题 4：`AutobiographicalNode` 超上限降级与"永不 Purge"约束矛盾，且无实现路径

`§6.3` 定义 Autobiographical 节点上限 500 个，超出时"降级为 KnowledgeNode（从'身份'降为'知识'，仍可检索但可衰减）"。

但 `§5.2 遗忘策略` 和 `§3.3 自传体记忆` 明确：AutobiographicalNode 不参与衰减，始终 Active（schema 强制约束）。

矛盾在于：被降级的节点需要改变 `node_type`，而 schema 约束是基于 `node_type` 的。降级后节点变成可衰减的 KnowledgeNode，但 `04-grafeo.md` 的 `autobiographical.rs` 强制 `status=Active`——如果节点被降级，这个约束失效了。

另外，`04-grafeo.md` 中没有 `autobiographical_downgrade` 相关模块，这个功能目前没有实现路径。

**建议**：在 `04-grafeo.md` 中补充降级机制模块（例如 `autobiographical/downgrade.rs`），明确定义降级的触发条件、节点 type 变更逻辑、衰减参数初始化方式；或者修改上限策略为"最旧的降为 Episodic 而非 KnowledgeNode"以避免 schema 约束冲突。

---

#### 问题 5：`DecayConfig` 字段名在两个文档中不一致，且 `purge_importance_threshold` 字段缺失

`05-memory.md §10.3` 中 `DecayConfig` 结构体：

```rust
pub purge_after: Duration,  // Dormant → Purge 时长（默认 90 天）
```

`plan-p2.md S2.7` 中描述同一字段：

```
purge_days: u32（默认 90）— Dormant 节点保留天数
```

一个是 `purge_after: Duration`，一个是 `purge_days: u32`，描述同一字段却不同名，实现时会直接引发歧义。

另外，`plan-p2.md` 中提到的 `purge_importance_threshold` 参数，在 `05-memory.md §10.3` 的 `DecayConfig` 结构体里**没有对应字段**，不清楚是遗漏还是刻意放在别处。

**建议**：统一字段命名（建议采用 `purge_after: Duration`，表达更类型安全）；并在 `DecayConfig` 中补全 `purge_importance_threshold` 字段，或在代码注释中说明该参数由 Purge 逻辑内部处理。

---

#### 问题 6：`S2.10.1` 即时粗筛的文件路径指向错误

`plan-p2.md S2.10` 中：

```
S2.10.1 ConflictDetector 模块 — consolidation/conflict.rs
```

但 `04-grafeo.md` crate 结构中，**即时粗筛**归属 `semantic/conflict.rs`，**离线精确分类**归属 `consolidation/conflict.rs`。

S2.10.1 描述的是即时粗筛（embedding cosine > 0.85），应指向 `semantic/conflict.rs`，不是 `consolidation/conflict.rs`。

**建议**：修正 S2.10.1 的文件路径为 `semantic/conflict.rs`。

---

### P2 — 建议改进（不影响实现正确性）

#### 问题 7：历史裁剪与检索预算分配的顺序依赖未明确

`§6.8`（完整流程）描述步骤 ④ 为"检索触发决策"，发生在历史裁剪之后，即检索 Budget 是历史裁剪后剩余的空间。

但 `§1 预算分配`写的是"默认比例：history 75% / retrieval 25%"。两种描述隐含了不同的执行顺序：
- 方案 A：先按比例划分预算，再各自裁剪
- 方案 B：先裁剪 history 到满足为止，剩下的给 retrieval

两种顺序在极端情况下（如对话历史已超出 75% 配额且折叠无法解决）行为完全不同。

**建议**：在 §6.8 流程说明中明确是哪种顺序，并描述 history 超预算时的处理方式。

---

#### 问题 8：`S2.12.3 LongMemEval 集成测试`归属 Phase 2 与 review 决策矛盾

`plan-p2.md S2.12` 中：

```
S2.12.3 LongMemEval 集成测试 | tests/ | 5 维评估（IE/MR/TR/KU/Abs）
```

但第一轮 review（`04-p2-s2-design-review.md §6.11`）明确决策：

> **Phase 3：对接开源 Benchmark**（LongMemEval 评估脚本集成）

S2 应该只做"建标准 + 可观测基础设施"（S2.12.1/S2.12.2），LongMemEval 集成不应在 Phase 2 实施。

**建议**：将 S2.12.3 标注为 Phase 3 或 S5，与 review §6.11 决策保持一致。

---

#### 问题 9：`GrafeoConfig` 中备份相关配置字段不完整

第一轮 review（`04-p2-s2-design-review.md §6.16`）定义了完整的 manifest 配置：

```toml
[memory.backup]
enabled = true
schedule_hour = 3
daily_retention_days = 7
weekly_retention_weeks = 4
backup_dir = ""
```

但 `04-grafeo.md` 的 `GrafeoConfig` 中只新增了 `backup_enabled: bool`，其余字段（`schedule_hour`、`daily_retention_days`、`weekly_retention_weeks`、`backup_dir`）没有对应的结构体字段。

**建议**：在 `GrafeoConfig` 中补全备份配置字段，或提取为独立的 `BackupConfig` 结构体并在 `GrafeoConfig` 中引用，确保 manifest 和代码结构一致。

---

## 3. 问题汇总

| 编号 | 问题 | 级别 | 涉及文件 | 建议动作 |
|------|------|------|----------|---------|
| 1 | §9 Phase 1 仍描述旧版 2跳/同步三元组去重，与 §6.3/§4.1 矛盾 | P0 | 05-memory.md | 同步 §9 描述 |
| 2 | §5.2 Fact 去重未说明"发生在离线巩固阶段"，与 §4.1 即时阶段描述混淆 | P0 | 05-memory.md | 在 §5.2 首行加阶段说明 |
| 3 | memory_hint 字段名 `t` 与枚举值 `t`（时间相关）同名冲突 | P1 | 05-memory.md | 改时间类型为 `r` 或 `d` |
| 4 | Autobiographical 超上限降级为 KnowledgeNode，与"永不 Purge"约束矛盾，无实现路径 | P1 | 05-memory.md / 04-grafeo.md | 补充降级模块或修改降级策略 |
| 5 | DecayConfig 字段名 `purge_after` vs `purge_days` 不一致；`purge_importance_threshold` 缺失 | P1 | 05-memory.md / plan-p2.md | 统一字段名并补全缺失字段 |
| 6 | S2.10.1 即时粗筛指向 `consolidation/conflict.rs`，应为 `semantic/conflict.rs` | P1 | plan-p2.md | 修正文件路径 |
| 7 | 历史裁剪与检索预算分配的执行顺序未明确 | P2 | 05-memory.md | 在 §6.8 流程中说明顺序 |
| 8 | S2.12.3 LongMemEval 测试归属 Phase 2 与 review §6.11 决策矛盾 | P2 | plan-p2.md | 移到 Phase 3/S5 |
| 9 | GrafeoConfig 中 backup 相关字段不完整，仅有 `backup_enabled` | P2 | 04-grafeo.md | 补全备份配置字段或提取 BackupConfig |

---

## 4. 处理优先级建议

**优先处理 P0**：问题 1 和 2 涉及 `§9 分阶段实现路线`，这是开发者最容易参考的部分，描述错误会直接导致实现错误。

**同批处理 P1 中的问题 3、5、6**：字段名冲突（问题 3、5）和文件路径错误（问题 6）改动小、定位精确，可以一次性修完。

**问题 4（Autobiographical 降级）**需要在 `04-grafeo.md` 中补充模块设计，改动量较大，可单独一次 session 处理。

**P2 问题**可根据实际需要决定是否处理，不影响 Phase 2 实现的正确性。
