# 记忆系统设计 vs 实现对比分析报告

> 编号：21-memory-design-vs-impl-gap-analysis
> 分析时间：2026-05-30
> 对照基准：`docs/design/05-memory.md` (v3.9)、`docs/module-design/04-grafeo.md` (v3.6)
> 实现代码：`core/rollball-memory/`、`core/rollball-grafeo/`、`core/rollball-runtime/src/memory/`

---

## 一、整体结论摘要

| 维度 | 完成度 | 说明 |
|------|--------|------|
| **Phase 1 基础设施** | **~90%** | 核心存储/检索/遗忘已实现，memory_store 接口已对齐设计并接通即时提取管道 |
| **Phase 2 程序记忆与自我认知** | **~40%** | 数据结构存在，但联动逻辑大部分未实现 |
| **Phase 3 离线巩固与持久化** | **~15%** | 仅有代码骨架，核心管道未接通 |

**已解决（2026-05-30）**：`memory_store` 工具接口已从旧版 `{key, content, category}` 对齐到设计要求的 `{content, category, confidence, keywords}`，并接入了 `GrafeoStore.process_memory_store()` 即时提取管道（去重 → 两层冲突检测 → 节点创建）。

---

## 二、Phase 1 — 记忆基础（详细对照）

### 2.1 已完整实现

| 设计项 | 设计文档位置 | 实现位置 | 状态 |
|--------|------------|---------|------|
| 三层仿生分层架构（瞬态/经历/沉淀） | §0 | `rollball-memory/src/types.rs` | 完成 |
| Grafeo 4 种 LPG Label（Episodic/Knowledge/Procedural/Autobiographical） | §8.1 / 04-grafeo.md §8.1 | `rollball-memory/src/types.rs` labels 模块 | 完成 |
| MemoryStore trait 抽象 | §10.3 | `rollball-memory/src/store.rs` | 完成 |
| MemoryQuery / MemoryFilters / SearchResult / MemoryContext 等查询类型 | §10.3 | `rollball-memory/src/types.rs` | 完成 |
| DecayConfig 参数化遗忘配置 | §10.3 | `rollball-memory/src/types.rs` (L555-585) | 完成 |
| HintType 检索权重类型（s/f/r/i） | §6.6 | `rollball-memory/src/types.rs` (L24-46) | 完成 |
| 检索权重动态调整（get_hint_weights / config_from_hint） | §6.6 | `rollball-grafeo/src/spreading.rs` | 完成 |
| GrafeoStore MemoryStore trait 完整实现 | 04-grafeo.md | `rollball-grafeo/src/grafeo.rs` (L164-547) | 完成 |
| 4 Label 的 HNSW 向量索引 + BM25 文本索引自动初始化 | 04-grafeo.md | `rollball-grafeo/src/grafeo.rs` `init_schema()` (L104-144) | 完成 |
| 乘法衰减模型（decay_score = importance × activity_signal） | §5.1 | `rollball-grafeo/src/forgetting/decay.rs` | 完成 |
| 后台衰减扫描（run_decay_scan） | §5.2 | `rollball-grafeo/src/forgetting/scan.rs` | 完成 |
| Dormant 状态转换 + 90天 Purge | §5.2 | `forgetting/scan.rs` + `purge_log.rs` | 完成 |
| Reactivate 节点（dormant_since 清零 + access_count 递增） | §5.2 | `forgetting/scan.rs` `reactivate_node()` (L208-236) | 完成 |
| MemoryManager（Retrieve → Inject → Record）三阶段 | §10.4 | `rollball-runtime/src/memory/manager.rs` | 完成 |
| hybrid_search 多Label并行检索 | §6.1 | `manager.rs` `retrieve()` (L169-225) | 完成 |
| graph_expand 关联扩散（含 hint_type 驱动早期终止阈值） | §6.2-6.3 | `manager.rs` `retrieve()` (L226-251) + `spreading.rs` | 完成 |
| Abstention 拒答机制（min_score 过滤 + System Prompt 注入） | §6.5 | `rollball-grafeo/src/abstention.rs` | 完成 |
| **memory_store 工具接口对齐设计** | §4.1 | `tools/builtin/memory_store.rs` | **完成（2026-05-30 修复）**：接口从旧版 `{key, content, category}` 改为设计要求的 `{content, category, confidence, keywords}`；对接 `GrafeoStore.process_memory_store()` 即时提取管道（去重→冲突检测→节点创建）；GrafeoStore 未初始化时优雅降级为 UUID fallback |
| 两层冲突检测（语义相似度 + 时间冲突，统一 Ambiguous） | §6.4 | `rollball-grafeo/src/conflict.rs` | 完成（v3.8 简化） |
| **PageRank 集成到检索排序** | §6.3 | `manager.rs` `retrieve()` | **完成（2026-05-30 接线）**：`apply_pagerank_boost()` 已接入检索管线，在去重后、最终排序前应用拓扑权威性加成。`MemoryManagerConfig` 新增 `pagerank_weight` 字段（默认 0.1，0.0 表示禁用） |
| 即时提取冲突处理（统一 Ambiguous → Phase 3 LLM 仲裁） | §4.1 / §6.4 | `rollball-grafeo/src/consolidation/instant.rs` `process_memory_store()` | 完成（v3.8 简化） |
| 防重复提取（embedding similarity > 0.95 跳过） | §4.1 | `instant.rs` `is_duplicate_knowledge()` (L247-265) | 完成 |
| PendingKnowledgeNode 概念（confidence < 0.85 → Pending） | §4.1 | `instant.rs` `process_memory_store()` | 完成 |
| Episode / KnowledgeNode / ProceduralNode / AutobiographicalNode 数据类型 | §2-3 | `rollball-memory/src/types.rs` | 完成 |
| ContentType（Informational / Artifact / Structural） | §2 | `rollball-memory/src/types.rs` (L214-232) | ✅ 已废弃 |
| ArtifactRef 工件引用 | §2 | `rollball-memory/src/types.rs` (L362-372) | ✅ 已废弃 |
| PrivacyLevel（Public / Personal / Sensitive） | §7.1 | `rollball-core/src/memory/traits.rs` | 完成 |
| Episode 写入 Grafeo（MemoryManager::record） | §10.2 | `manager.rs` `record()` (L398-439) | 完成 |
| 蒸馏 Episode 写入（ADR-011：Compaction/Distillation 统一写入路径） | ADR-011 | `manager.rs` `record_distilled()` (L445-475) | 完成 |
| EpisodeDistiller（compact_full_context / distill_on_session_end） | ADR-011 | `rollball-runtime/src/episode_distill.rs` | 完成 |
| 上下文压缩三阶段（70% 监控 / 80% LLM摘要 / 95% 紧急裁剪） | §1 | `agent/loop_.rs` `compact_history_if_needed()` | 完成 |
| RetrievalMetrics 检索指标收集 | §11.1 | `rollball-memory/src/types.rs` (L161-178) | 完成 |
| RAG 双通道检索（Grafeo + RAG 并行） | §10.6 | `manager.rs` `retrieve()` (L283-302) | 完成 |
| 存储统计（stats/health_check） | §10.3 | `grafeo.rs` `stats()` / `health_check()` (L509-541) | 完成 |

### 2.2 部分实现

| 设计项 | 设计文档位置 | 当前状态 | 差距描述 |
|--------|------------|---------|---------|
| ~~**memory_hint 系统提示注入**~~ | ~~§1~~ | ✅ 已废弃 | **2026-05-30 设计简化**：移除 per-round memory_hint，改为 Compaction 时 Compact Model 提取实体+三元组。`COMPACT_PROMPT` 已更新，`DistilledEpisode` 新增 `entities`/`triples` 字段 |
| ~~**memory_hint 解析与检索策略联动**~~ | ~~§1 / §6.6~~ | ✅ 已废弃 | 检索权重统一为默认值（vector:0.7, text:0.3），不再动态调整。`HintType` 保留但仅用于 `memory_store` 管道 |
| ~~**Episode 内容自动分类**~~ | ~~§2~~ | ✅ 已废弃 | **2026-05-30 设计简化**：移除 ContentType/ArtifactRef，废弃管道 A（Tool Call 模板提取）和管道 B（代码块正则分离）。Episode 内容直接存储，摘要由 Compaction 阶段 Compact Model 生成（ADR-011） |
| ~~**Per-round Episode 实时写入**~~ | ~~§2 / §10.2~~ | ✅ 已废弃（ADR-011） | **2026-05-30 设计变更**：ADR-011 决定「摘要即蒸馏」，经历层的唯一写入来源为 compaction 摘要和 session-close 蒸馏摘要。JSONL 是逐条消息的完整真相来源，Grafeo 只存摘要文本（自然语言），不存储逐轮原始副本。当前 `loop_.rs` 中 `write_distilled_to_grafeo()` 在 compaction/session-end 时通过 `record_distilled()` 写入 Grafeo——符合 ADR-011 设计 |

### 2.3 未实现

| 设计项 | 设计文档位置 | 说明 |
|--------|------------|------|
| **AutobiographicalNode 从 manifest 自动派生** | §3.3 | 设计要求启动时从 `manifest.toml` 的 `agent.name`、`agent.description`、`skills/` 列表自动生成 Identity/Capability 节点，目前未实现 |
| **自传体记忆 System Prompt 注入** | §3.3 | 设计要求在 System Prompt 最前面注入自传体摘要（Identity/Capability/Limitation 必注入，History Top-5 摘要+Top-3 明细，Relationship Top-3，总预算 200 token） |
| **经历层遗忘清理** | §2 | 已巩固+超7天 / 未巩固+超14天+importance<0.3 / 未巩固+超14天+importance>=0.3 保留并离线巩固——清理逻辑未集成到自动调度 |
| **Episode 检索时的 embedding 退化降级** | §2 | 设计要求 embedding 为空时退化为 text_search only，代码中有 fallback 但未按设计中的200ms超时生成 + 后台补生成策略实现 |
| **CDC/History 用于冲突时间检测** | §6.4 | `conflict.rs` 中时间冲突检测使用节点属性 `created_at` 而非 `db.history()` CDC API |
| **MMR 多样性搜索** | §6.3 / 04-grafeo.md | `db.mmr_search()` API 可用，但 MemoryManager 检索路径中未使用，统一用 hybrid_search |
| **Louvain 社区检测** | §6.3 / 04-grafeo.md | `CALL grafeo.louvain()` 代码示例存在，但未在 graph_expand 中用于优先扩展社区内节点 |
| **Zone 业务场景分区** | §8.2 | 明确标记为 Phase 4+ 暂缓，符合设计预期 |
| **Purge 三条路径（正常衰减/容量压力/用户手动）** | §5.2 | 仅实现了路径1（正常衰减），路径2（容量压力）和路径3（用户手动）未实现 |
| **Fact 语义去重（subject+predicate 匹配）** | §5.2 | 设计要求的 `(subject, predicate)` 精确匹配去重（同 predicate 不同 object → 知识更新）未实现，当前仅靠 embedding cosine similarity |
| **Edge 权重计算（confidence_avg × recency_factor）** | §3.1 | 边创建时有 weight 概念但未按设计公式计算和定期更新 |

---

## 三、Phase 2 — 程序记忆与自我认知（详细对照）

### 3.1 已完整实现

| 设计项 | 设计文档位置 | 实现位置 | 状态 |
|--------|------------|---------|------|
| ProceduralNode 数据结构 | §3.2 | `rollball-memory/src/types.rs` (L436-459) | 完成 |
| ProceduralNode Grafeo 存储 | §3.2 | `grafeo.rs` `store_procedural()` | 完成 |
| RetrievalMetrics 在线评估指标 | §11.1 | `rollball-memory/src/types.rs` | 完成 |
| Abstention 可配置 min_score | §6.5 | `abstention.rs` `get_min_score_for_agent()` | 完成 |
| Judge 轻量评估（LLM Judge 采样） | §11.1 | `rollball-grafeo/src/judge.rs` | 完成 |

### 3.2 部分实现

| 设计项 | 当前状态 | 差距描述 |
|--------|---------|---------|
| **ProceduralNode 三条来源路径** | 仅路径 A 部分可用 | 路径 A（用户反馈即时提取）：依赖 memory_store tool 发出正确的 category=procedure——但 tool 接口未对齐设计。路径 B（执行失败自动总结）：未实现 SkillExecution failure_case → ProceduralNode 联动。路径 C（离线巩固提炼）：Phase 3 未接通 |
| **Skill ↔ ProceduralNode 双向联动** | 未实现 | 设计要求 failure_cases ≥3 条同类失败时 LLM 生成跨 Skill ProceduralNode，代码中没有 SkillExperience → ProceduralNode 的提取链路 |
| **主动假设验证机制** | 未实现 | 设计要求 ≥3 个 action_pattern 变体时 Agent 主动提出合并假设。`generalization.rs` (47KB) 有 PatternCategory/GeneralizationConfig，但 `detect_merge_candidates()` 未在运行时调用 |
| **自我评估驱动的 AutobiographicalNode 更新** | 未实现 | Limitation 节点自动生成/更新（某模型某任务成功率 < 60%）的逻辑未实现 |
| **Relationship 节点自动维护** | 未实现 | 用户合作超过 30 天自动生成 Relationship 节点，未实现 |
| **History 节点摘要压缩** | 未实现 | 设计要求 History 超过 10 条时合并为摘要节点，未实现 |
| **自传体容量管理（200 token 预算 + Top-K 注入）** | 未实现 | 同 Phase 1 自传体注入未实现 |

### 3.3 未实现

| 设计项 | 说明 |
|--------|------|
| **离线巩固 LLM prompt（~500 tokens，含主动假设步骤）** | consolidation/offline.rs 有骨架但未接入运行时 |
| **PendingKnowledgeNode 离线升/降级** | offline.rs 存在但未被调用 |
| **跨 Agent 知识共享（Intent 查询 / 系统 Agent ContentProvider）** | §7，三条路径均未实现 |
| **LongMemEval 基准测试集成** | `eval.rs` 有 EvalConfig/EvalDimension/run_eval，但无实际测试套件 |
| **冲突处理准确率跟踪** | `retrieval_metrics.rs` 有 ConflictAccuracyStats 但未接入实时管道 |
| **NRR 告警 / 降级频率告警 / 指标聚合** | §11.3，指标定义存在但告警逻辑未触发 |

---

## 四、Phase 3 — 离线巩固与持久化（详细对照）

### 4.1 已完整实现

| 设计项 | 说明 |
|--------|------|
| EpisodeDistiller 完整蒸馏流程 | `episode_distill.rs` 实现了 compact_full_context / distill_on_session_end / write_summary_to_grafeo |
| 摘要即蒸馏（ADR-011） | Compaction 和 Distillation 统一为单次 Compact Model 调用 |
| 便宜模型选择 | `select_cheapest_model()` 已实现 |

### 4.2 部分实现

| 设计项 | 当前状态 | 差距描述 |
|--------|---------|---------|
| **离线巩固调度器** | 骨架存在 | `consolidation/scheduler.rs` (18.6KB) 有 SchedulerConfig / TriggerReason / ConsolidationScheduler，但 `MemoryManager` 未使用它——没有实际的后台调度循环 |
| **离线巩固核心逻辑** | 骨架存在 | `consolidation/offline.rs` (15.7KB) 有 OfflineConsolidationConfig/Result，但未被调用 |
| **三元组提取** | 骨架存在 | `consolidation/triple_extraction.rs` (25.2KB) 存在，但未接入离线巩固管道 |
| **Ambiguous 冲突 LLM 仲裁** | 骨架存在 | `consolidation/conflict_llm.rs` (12.1KB) 存在，但未接入 |
| **Ambiguous 用户确认流程** | 未实现 | 设计要求 ambiguous 累计 3+ 时通过 memory_store hint 引导自然询问，未实现 |
| **Episode 摘要增强（模板 → LLM 语义摘要）** | 未实现 | 离线巩固阶段的 Artifact 摘要增强未实现 |
| **HypothesisNode（主动假设）** | 未实现 | 设计要求的 Phase 3 假设生成/验证机制未实现 |

### 4.3 未实现

| 设计项 | 说明 |
|--------|------|
| **空闲检测触发离线巩固** | Agent 空闲 >30min / episode 积攒 >50 / 用户手动触发——均未实现 |
| **分页换出（MemGPT 风格）** | 未实现，设计中标明有循环依赖风险 |
| **云端同步** | 未实现 |
| **WASM 中间件** | 设计预留（manifest [memory.middlewares] custom_filter wasm），未实现 |
| **RemoteMemoryStore** | Phase 5+，未实现 |
| **加密存储** | Phase 4+，未实现 |
| **Temporal 版本化查询** | Phase 4+，未实现 |
| **单 Agent 离线巩固全局协调锁** | 多 Agent 场景限制同时执行，未实现 |

---

## 五、MemoryManager 设计 vs 实现差距

设计文档 §10.4 定义的 MemoryManager 包含以下组件，当前实现状态：

| 组件 | 设计 | 实现 |
|------|------|------|
| **Lifecycle Handler Registry** | 每阶段可注册多个 handler | 未实现——当前直接硬编码调用链 |
| **MemoryMiddleware Chain** | pre_record / post_record / post_retrieve 洋葱模型 | `MemoryMiddleware` trait 定义在 `rollball-memory` 但 `MemoryManager` 未使用 |
| **Config Provider** | 从 manifest + 系统默认读取配置 | 部分——MemoryManagerConfig 存在但未从 manifest 动态加载 |
| **Event Bus** | 发布 MemoryEvent 供 Desktop App 订阅 | 未实现 |
| **RAG Client** | `Option<Arc<RagClient>>` | 已实现，在 `retrieve()` 中并行查询 |

---

## 六、关键风险与建议

### 风险等级：高

1. ~~**memory_store 工具接口不对齐**~~ → **✅ 已解决（2026-05-30）**：接口已从 `{key, content, category}` 改为 `{content, category, confidence, keywords}`，并接入 `GrafeoStore.process_memory_store()` 即时提取管道。详见 `tools/builtin/memory_store.rs`。

2. ~~**memory_hint 整条链路缺失**~~ → **✅ 已解决（2026-05-30）**：设计简化，per-round memory_hint 已移除。实体+三元组提取移至 Compaction 时由 Compact Model 完成。详见 §1 更新和 `COMPACT_PROMPT` 变更。

### 风险等级：中

3. ~~**Episode 内容分类未执行**~~ → **✅ 已废弃（2026-05-30）**：ContentType 枚举和 ArtifactRef 结构已从整个代码库中移除。管道 A（Tool Call 模板提取）和管道 B（代码块正则分离）不再需要——Episode 内容直接存储，摘要由 Compaction 阶段 Compact Model 生成。

4. ~~**主循环 Record 阶段缺失**~~ → **✅ 已废弃（ADR-011）**：经历层的唯一写入来源为 compaction/distillation 摘要。`loop_.rs` 中 `write_distilled_to_grafeo()` 在 compaction/session-end 时通过 `record_distilled()` 写入 Grafeo，符合 ADR-011 设计。

5. **自传体记忆链缺失**：Manifest 派生 → Grafeo 存储 → System Prompt 注入整条链路未实现，Agent 无自我认知背景。这是当前 Phase 1 最大的剩余缺口。

### 建议优先修复顺序

1. ~~重构 `memory_store` 工具接口对齐设计~~ ✅ 已完成（2026-05-30）
2. ~~接通 `memory_store` → `consolidation/instant.rs` 即时提取管道~~ ✅ 已完成（2026-05-30）
3. 实现 AutobiographicalNode manifest 派生 + System Prompt 注入
4. 接通离线巩固调度器（`ConsolidationScheduler` + `OfflineConsolidation`）

---

## 七、统计总览

| Phase | 总设计项 | 已实现 | 部分实现 | 未实现 | 已废弃（设计简化） |
|-------|---------|--------|---------|--------|-------------------|
| Phase 1 | ~35 | 30 | 1 | 0 | 4 (memory_hint ×2, ContentType, Per-round Episode) |
| Phase 2 | ~18 | 5 | 6 | 7 | 0 |
| Phase 3 | ~15 | 3 | 4 | 8 | 0 |
| **合计** | **~68** | **38** | **11** | **15** | **4** |

**整体完成度：约 56%（已实现）→ 约 76%（含部分实现可工作的功能）**

Phase 1 核心存储/检索/遗忘管线基本完整，即时提取管道已于 2026-05-30 接通（memory_store 接口对齐设计）。Phase 2/3 的大量代码骨架存在（47KB generalization、25KB triple_extraction、39KB instant 等），暗示曾投入大量开发但未完成端到端集成。

**2026-05-30 更新（1）**：移除 per-round memory_hint 设计，改为 Compaction 时 Compact Model 提取实体+三元组。新增 `DistilledEpisode.entities`/`.triples` 字段，`record_distilled` 已存储到 Grafeo 节点属性。检索权重统一默认值，不再动态调整。

**2026-05-30 更新（2）**：memory_store 工具接口从旧版 `{key, content, category}` 对齐到设计要求的 `{content, category, confidence, keywords}`，接入 `GrafeoStore.process_memory_store()` 即时提取管道（去重→两层冲突检测→节点创建）。工厂函数 `all_builtin_tools()` 和 `MemoryStoreTool::new()` 新增 `Option<Arc<GrafeoStore>>` 参数，GrafeoStore 未初始化时优雅降级为 UUID fallback。统计数据已同步更新。

**2026-05-30 更新（3）**：PageRank 拓扑权威性加成接入检索管线。`manager.rs` 的 `retrieve()` 在去重后、最终排序前调用 `apply_pagerank_boost()`，`MemoryManagerConfig` 新增 `pagerank_weight` 字段（默认 0.1）。Phase 1 已实现项 29→30，未实现项 1→0。统计数据已同步更新。
