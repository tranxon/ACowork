# Code Review + 修复计划：ADR-068（记忆两轴正交化与离线蒸馏器重构）

**Reviewer**: Senior Software Engineer
**日期**: 2026-09-05
**范围**: 提交 `c994b5ae`..`9964b7ac`（M1–M8 共 8 个 commit，31 文件，+3594 / −563）
**基线**: HEAD = `9964b7ac`（ADR-068 M5-M8: clippy cleanups）
**对照文档**: [ADR-068](../adr/zh/ADR-068-memory-layer-promotion-two-axis-orthogonal.md)

> 本文档所有"红/绿"结论均来自本机实测（macOS aarch64，`cargo test` 于基线 HEAD 执行），
> 归因依据为提交文件清单与失败签名一致。

---

## 0. 二次复核与修复记录（2026-09）

### 0.1 复核基线

复核 HEAD = `f1a8cd37`（本修复提交）之前的工作区状态。复核方法：逐条对照 §3 架构问题（A1–A6）、§5.2 验收矩阵与本机 `cargo test` 复跑。

### 0.2 A1–A6 复测结论

| ID | 原结论 | 复测结果 | 证据 |
|---|---|---|---|
| A1 | R3 未彻底执行，旧 LLM→沉淀层路径整段存活 | **部分修复 → 本修复后闭环** | grafeo 侧已清（`instant.rs` 仅剩 helper + 单测；`offline.rs` 的 `compress_history_nodes` / `auto_generate_relationship_nodes` 真实实现已删；W3 `grep` = 0）。**但跨 crate 残留**：`acowork-memory/src/manager.rs::run_relationship_generation` 仍在 compaction/session-end 无条件直写 Relationship 节点（见 0.3），本次已删除 |
| A2 | 提交说明与代码事实不符 | ✅ 已修复 | [provider.rs:227-237](../../../core/acowork-memory/src/provider.rs#L227) 改为 `#[deprecated]` no-op 且注释属实；[provider_impl.rs:509-516](../../../core/acowork-grafeo/src/provider_impl.rs#L509) 如实声明 |
| A3 | Judge Skip 不粘滞 → 无限重试 | ✅ 已修复 | `mark_episodes_skipped`（[provider.rs:64-77](../../../core/acowork-memory/src/provider.rs#L64)）+ `DISTILLER_SKIP_METADATA_KEY` 过滤闭环；`test_a3_skip_verdict_is_sticky_no_llm_on_second_run` 绿 |
| A4 | 审计无法映射到节点 | ✅ 已修复 | `MemoryProvider::store_*` 返回 `u64`（[provider.rs:125](../../../core/acowork-memory/src/provider.rs#L125)）；`promoted_node_id` 写入链路真实（[distiller.rs:786-817](../../../core/acowork-grafeo/src/consolidation/distiller.rs#L786)） |
| A5 | Autobio 聚簇退回字符串相等 | ✅ 已修复 | [distiller.rs:600-616](../../../core/acowork-grafeo/src/consolidation/distiller.rs#L600) embedding cosine 合并，无 embedding 时回退字符串；`test_a5_autobio_key_hint_variants_merge_via_embedding` 绿 |
| A6 | 死配置与缺失护栏 | ✅ 已修复 | `max_cluster_size` 生效（`test_a6_*` 绿）；`llm_temperature` 死配置已删（commit `1fa12e41`） |

### 0.3 复测新发现（本修复消除）

**A1 跨 crate 残留——Relationship 沉淀层仍有第二生产者（高）**

复核发现 §3.2 A1 的修复只覆盖了 `acowork-grafeo`，**漏掉了 `acowork-memory/src/manager.rs`**（不同 crate）：

- [manager.rs:1007](../../../core/acowork-memory/src/manager.rs#L1007)（已删）`run_relationship_generation` 完整实现 30 天规则，直写/更新 `AutobiographicalNode{category=Relationship, key=collaboration_span}`，被 `run_post_compaction_tasks` 调用；
- 真实调度路径：compaction 后 [loop_context.rs:1019](../../../core/acowork-runtime/src/agent/loop_context.rs#L1019)（已改）与 session 关闭 [loop_session.rs:238](../../../core/acowork-runtime/src/agent/loop_session.rs#L238)（已改）；
- 该路径**不经过 distiller、不受 `[memory.distiller].enabled`（opt-in）门控**，创建节点**无 `promotion_metadata`、无审计条目**；
- 与 distiller `promote_autobio_relationship` 的幂等检查（`find_autobiographical_by_category` 非空即跳过）互踩：manager 先写 → distiller 永远跳过 → M8 单测全绿（测试环境无竞争）但真实部署走无审计直写；
- 双写格式不一致：manager 中文 value（"已合作 X 天"）vs distiller 英文 value（"collaborated X days"）。

违反条款：ADR-068 §3.6"promote_autobio_relationship 是 Relationship 唯一生产者"、§5.1 W4 opt-in 语义、D10 审计一一对应。且为 review §3.2 A1 原文点名项（"manager.rs session-end run_relationship_generation 仍写 Relationship 节点"）。

### 0.4 修复内容（commit `f1a8cd37`）

| 改动 | 文件 |
|---|---|
| 删除 `run_relationship_generation`（含 30 天规则直写） | [manager.rs](../../../core/acowork-memory/src/manager.rs) |
| `run_post_compaction_tasks` 收敛为 generalization + no-op history compression，doc 注明 ADR-068 M8 移交 | 同上 |
| 调用点注释同步（Relationship 已移交 distiller 后台 step） | [loop_context.rs](../../../core/acowork-runtime/src/agent/loop_context.rs)、[loop_session.rs](../../../core/acowork-runtime/src/agent/loop_session.rs)、[loop_memory.rs](../../../core/acowork-runtime/src/agent/loop_memory.rs) |
| 回归 e2e：`post_compaction_tasks_do_not_write_relationship_nodes` —— 铺 45 天前 episode → 跑 `run_post_compaction_tasks` → 断言无 Relationship 节点；再调 `promote_autobio_relationship` → 断言节点出现且带 `promotion_metadata`（证明能力移交而非消失） | [memory_adr068_e2e.rs](../../../core/acowork-runtime/tests/memory_adr068_e2e.rs) |

### 0.5 修复后验证（本机实测）

- `acowork-memory`：30 passed；`acowork-runtime --lib`：1332 passed（1 个预存在失败 `agent::e2e_prompt_cache::restart_after_compression_preserves_todo_state`，stash 复测确认基线即红，属 §附 未深究项，与本次无关）；
- `memory_adr068_e2e`：10 passed（含新增回归）；`memory_p1p2_e2e` 17 / `memory_e2e` 4 / `memory_m4_bench` 1 / `memory_m5_bench` 1 / `memory_m4_probe` 1 全绿；
- W3 红线：`grep -rn "process_memory_store\|process_autobiographical\|process_procedure" core --include="*.rs"` = 0 命中。

### 0.6 遗留风险（未在本轮处理）

1. **Step 2a / Step 4 prompt 无 golden 快照测试**：`EXTRACTION_SYSTEM_PROMPT` / `JUDGE_SYSTEM_PROMPT` 仅被生产代码引用，mock LLM 从不校验 prompt 内容 —— prompt 措辞改动无测试护栏（R-R2 高风险区）。
2. **`consolidation_event` MQTT topic 仍为设计占位**：`HistoryMilestoneEvent` 只能经 `promote_event` 程序内触发，全库无 `acowork/consolidation/event` 订阅入口 → History 晋升在真实部署缺事件触发面（ADR §3.4.3）。
3. **e2e 走 scripted mock LLM**：未串联真实 `ProviderLlmAdapter` JSON 序列化路径。
4. **promote_autobio_relationship 创建后不刷新 value**：协作天数信息冻结在创建时刻（ADR 幂等语义，可接受；如需刷新需扩展 distiller 侧而非 manager 侧）。
5. **`triple_extraction` 保留为 manual/batch 导入路径**：注释明确标注非生产链路（LLM 提取 + 落库），但其晋升**不写 `promotion_metadata`、不经 distiller 审计**——如需正式化应改为经 distiller 或补审计，见 0.7 决策。

### 0.7 Path C 伪规则残留下线 + 文档同步（2026-09，new Phase）

**决策（用户拍板）**：沉淀层节点只允许两类可信语义生产者 —— (a) 图数据库统计归纳（节点/边关系）、(b) LLM 分析归纳（EpisodicDistiller）；规则式替代 LLM 的"数据沉淀"手段全部下线。经全量排查，残留的 rule-based 归纳 = `generalization.rs`（Path C），本轮清除：

**下线对象（commit 详见 git log `refactor(memory): retire Path C…`）**
- `MemoryManager::run_post_compaction_tasks` / `run_generalization_step` / `run_history_compression`（[manager.rs](../../../core/acowork-memory/src/manager.rs) 已删，含 `GeneralizationConfig`/`Arc` import 清理）；
- runtime 三个调用点：`loop_context.rs`（compaction 后）、`loop_session.rs`（session 关闭）、`loop_memory.rs::run_post_compaction_memory_tasks` 已删；
- `GrafeoStore::run_offline_consolidation_with_generalization` Step 4 不再执行 generalization（参数保留 deprecated 兼容，consolidation_bg.rs 传 `None`）；
- 回归测试改名强化：`offline_consolidation_does_not_write_relationship_nodes`（原 `post_compaction_tasks_do_not_write_relationship_nodes`）。

**保留对象**：`triple_extraction.rs::extract_triples` —— 标注为 manual/batch LLM 导入路径（LLM 驱动、非生产活跃、非规则 hack），按"LLM 归纳可信"原则保留；缺口（无审计）记入 0.6-5。

**Path C 为何不可信（代码级证据）**：(1) 扫描对象非真实经验——真实对话轮次不落库，可扫描的 assistant 轮次仅 memory_store 记录，tool_call 结构化信息从不进入 Episode.content；(2) 特征提取 = 文本 hack（首非空行前 100 字符 = action；`"name": "xxx"` 正则 = tool）；(3) "归纳" = 字符串精确全等 + 计数，措辞差一字符即不合并；(4) 消费过的 episode 从不标记 consolidated（`mark_consolidated` 无调用），每轮重复 boost、`success_count` 无限虚增。

**验证**：`acowork-memory` 30 / `acowork-grafeo`(consolidation) 110 / `acowork-runtime --lib` 1332（1 个预存在基线失败，同上）/ `memory_adr068_e2e` 10 全绿；clippy 0 警告。

**文档同步（保持 doc-code 一致）**：ADR-068 §1.2/§3.4.3/§3.4.4/§3.7/M7/R-R4/§7 就地修订（`run_generalization` 唯一来源与 fallback 语义移除）；[05-memory.md](../../design/zh/05-memory.md) 分层图/§3.3 自传体来源/§4.1 工具 schema（4 类 Episode-only）/§4.2 重写为 EpisodicDistiller/§6.4 冲突仲裁收敛到 Judge/§8.1.1 子分类写入语义/§9 覆盖声明；[memory-write-entrypoints.md](../../memory-write-entrypoints.md) 有效入口表（A→Episodic，F→distiller）+ 追加 I/J 废弃行。

---

## 1. 总体结论

| 维度 | 评级 | 说明 |
|---|---|---|
| 架构合理性 | **B-** | 核心两轴正交模型落地正确；但 R3"沉淀层只由 EpisodicDistiller 产生"未彻底执行——旧 LLM→沉淀层写入路径整段存活于 grafeo 固有 API，且 M6/M8 提交说明与代码事实不符 |
| 功能完整性 | **C+** | M1–M5 主体完成；M7 默认开关在真实 manifest 解析路径失效（实际 OFF）；M6/M8 只完成一半；D8 History 事件晋升路径完全缺失，ADR 后 History 沉淀层无运行时生产者 |
| 测试覆盖率（单元） | **A-** | D1–D18 单测基本齐备；`acowork-grafeo` 315 过 / `acowork-memory` 30 过 / runtime lib 1332 过 |
| 测试覆盖率（e2e） | **D** | ADR-068 **零新增**端到端测试（E1–E7 均无自动化覆盖）；且 M5 改坏 4 个既有真实链路 e2e/bench 文件，**HEAD 上 11 个用例红** |
| 编译/clippy | **A** | 基线 `cargo test` 编译通过；`9964b7ac` 为 clippy 清理提交 |

**总评建议**：**先修复 11 个红测（P0），再按 §6 计划补齐 e2e 与完成 M6/M7/M8 未竟项**；M6 声称"cargo test 全绿"与 HEAD 实测不符，需在修复后重新全量验证再签收。

---

## 2. 提交清单与 ADR 里程碑对照

| Commit | 里程碑 | 内容 | 完成度 |
|---|---|---|---|
| `c994b5ae` | M1–M3 | Episode schema + EpisodicDistiller 6 步流水线 + 19 单测 | ✅ 主体完成 |
| `9bc597c4` | M4 | 挂入 ConsolidationBgTask + `[memory.distiller]` manifest 配置 | ✅ |
| `17bb9bec` | M5 | memory_store 工具重写为纯 Episode 写入 | ⚠️ 功能完成，**破坏 4 个既有 e2e/bench 文件未修复** |
| `97f81ea7` | M6 | trait 层 deprecate 旧写入路径 | ❌ 与 ADR"删除"不符，grafeo 层整段存活 |
| `493e6410` | M7 | distiller 默认开启 | ❌ 仅在结构体默认值生效，agent_core 解析路径实际 OFF |
| `2582b0da` | M8 | bootstrap 范围收窄 | ⚠️ 仅注释/日志，无测试；Relationship 移交 distiller 未做 |
| `9964b7ac` | — | clippy 清理 | ✅ |

---

## 3. 架构合理性

### 3.1 符合 ADR 的设计（✅）

- **组件分层正确**：类型与配置在 `acowork-memory`（[consolidation.rs](../../../core/acowork-memory/src/consolidation.rs)），流水线在 `acowork-grafeo`（[distiller.rs](../../../core/acowork-grafeo/src/consolidation/distiller.rs)），消费 `dyn MemoryProvider` + `dyn TripleExtractorLlm`，符合 ADR-051 解耦与 ADR §3.4.1 组件位置约定。
- **Episode 收敛为单字段** `knowledge_subtype`（[types.rs:393-419](../../../core/acowork-memory/src/types.rs#L393)），无 subject/predicate/object/autobio 字段泄漏；`KnowledgeSubType` 仅 4 类。
- **6 步流水线**结构清晰：单次批量提取（Step 2a）→ 余弦聚簇 + 字符串回退（Step 2b）→ 分桶证据门槛（Step 3）→ LLM Judge（Step 4）→ 写节点 + 标记 consolidated（Step 5）→ DistillerResult 审计（Step 6）。阈值默认值与 ADR 一致（fact=2 / preference=3 / relation=2 / procedure=5 / autobio=3+14d / judge=0.85）。
- **LLM 缺失时优雅降级为 no-op**，episode 保持原状留待重试，符合 ADR Step 2 失败处理。

### 3.2 架构问题

**A1. R3 未彻底执行——旧 LLM→沉淀层写入路径整段存活（高）**

M6 仅将 `MemoryProvider` trait 方法改为 deprecated no-op，grafeo 层**固有方法原样保留且可调用**：

- [instant.rs:173](../../../core/acowork-grafeo/src/consolidation/instant.rs#L173) `GrafeoStore::process_memory_store` 完整旧流水线仍在，**第 182/187 行仍是 autobiographical / procedure 直写分支**；
- [offline.rs:228](../../../core/acowork-grafeo/src/consolidation/offline.rs#L228) `compress_history_nodes` 完整实现（月度合并 + Dormant）仍在，配套单测仍在（grafeo 315 个用例全绿含它们）；
- [offline.rs:316](../../../core/acowork-grafeo/src/consolidation/offline.rs#L316) `auto_generate_relationship_nodes`（30 天规则）在 runtime 离线巩固中**每次触发都会执行**，不经 distiller；
- `manager.rs` session-end `run_relationship_generation` 仍写 Relationship 节点。

后果：Relationship 沉淀层现有 **3 个生产者**（distiller / offline 30 天规则 / session-end）；`eval.rs`（`pub mod eval`，编入生产 lib）与 `ambiguous.rs`/`instant.rs` 单测仍直接消费旧路径。R3 只在"runtime 工具调用"这一条链路上成立。

**A2. 提交说明与代码事实不符（高）**

[provider.rs:222](../../../core/acowork-memory/src/provider.rs#L222) 与 [provider_impl.rs:511](../../../core/acowork-grafeo/src/provider_impl.rs#L511) 注释声称 grafeo 实现"已删除 (M6)"——均不实。且 runtime 巩固循环 [offline.rs:100](../../../core/acowork-grafeo/src/consolidation/offline.rs#L100) 实际仍在调用固有 `compress_history_nodes`（真实实现），注释"History-node compression is gone"与执行行为矛盾。违反"事实驱动"评审原则，误导后续维护者。

**A3. Judge Skip 不粘滞 → 无限重试 + 重复 LLM 开销（中）**

[distiller.rs:571-596](../../../core/acowork-grafeo/src/consolidation/distiller.rs#L571)：judge 返回 skip/defer 时不落任何标记，episode 保持 unconsolidated，下轮重新提取/聚类/judge。ADR Step 4 明确要求 skip 标记"永不晋升（防止无限重试）"，实现缺失（无 tombstone、无 episode 标记、无聚簇哈希注册表）。与 R-R2/R-R7 缓解设计矛盾。

**A4. 审计可回滚性打折（中）**

`store_knowledge()` 返回 `()`，故 `PromotionEvaluation.promoted_node_id` 恒为 `None`（[distiller.rs:601](../../../core/acowork-grafeo/src/consolidation/distiller.rs#L601)）。ADR 承诺"promotion_evaluations 提供完整审计、人工可回滚"（R-R3/R-R6），但审计条目无法映射到实际创建的节点。

**A5. Autobio 聚簇退回字符串相等（中）**

knowledge 聚簇用 embedding（R8），autobio 聚簇却按 `aspect + key_hint` 全等字符串分组（[distiller.rs:404](../../../core/acowork-grafeo/src/consolidation/distiller.rs#L404)）。key_hint 为 LLM 自由生成（"verbose_response" vs "verbosity"），分裂即永不达 3 条证据门槛——正是本 ADR 二次修正从谓词聚簇移除的缺陷在 autobio 维度的回潮。

**A6. 死配置与缺失护栏（低）**

`llm_temperature` / `max_cluster_size` 声明并进入 manifest 映射，但 distiller 从不读取（`chat()` 无 temperature 参数；无 OOM 护栏）。R-R5 的 `promotion_in_flight` 锁未实现（单 agent 场景风险低）。

---

## 4. 功能完整性

| ADR 要求 | 状态 | 证据 / 缺口 |
|---|---|---|
| M1 Episode 扩展（向后兼容） | ✅ | `#[serde(default)]`，仅 1 新字段 |
| M3 五类晋升 + defer/skip | ⚠️ | 代码齐备；skip 不粘滞（A3） |
| M5 工具 schema 收窄 | ✅ | enum 4 值、无 aspect/key/source；`category=autobiographical` 报错有单测 |
| M6 删除旧路径（W3: grep = 0） | ❌ | 见 A1；实测 grep 命中数十处（含生产 lib `eval.rs`） |
| M7 默认开启（W4） | ❌ | `ManifestDistillerConfig::default().enabled = true`（[manifest.rs:394](../../../core/acowork-core/src/manifest.rs#L394)）且 `SchedulerConfig::default().distiller_enabled = true`（[consolidation.rs:378](../../../core/acowork-memory/src/consolidation.rs#L378)），但 [agent_core.rs:1105-1111](../../../core/acowork-runtime/src/agent/agent_core.rs#L1105) 用 `unwrap_or(false)`：**manifest 无 `[memory.distiller]` 段时实际 OFF**。现有示例 .agent 全无此段 → 线上默认关闭。manifest.rs 注释自称"缺段=ON"与 agent_core 注释"缺段=disabled"自相矛盾；M7 两个单测只测结构体默认值，未测 manifest→SchedulerConfig 解析路径 |
| M8 Relationship 移交 distiller | ❌ | 只收窄 bootstrap（Identity/Capability）+ 注释；`promote_autobio_relationship()` 不存在；30 天 Relationship 由旧 offline 路径产生（A1） |
| D8 History 事件晋升 | ❌ | `AutobioAspect::History` 在聚类时被显式排除（[distiller.rs:409](../../../core/acowork-grafeo/src/consolidation/distiller.rs#L409)）；全库无 `consolidation/event` MQTT 消费者；无事件入口 API。ADR 后 History 沉淀层节点无任何运行时生产者——功能性回退 |
| generalization 改造 / 降级为 fallback | ❌ | M1–M3 仅给 `ProceduralNode` 机械补两个字段；扫描逻辑未改"只扫 `knowledge_subtype=Procedure`"；runtime 每次巩固仍并行跑全量 generalization（第二个 Procedural 生产者） |

**附带发现（同批引入）**：memory_store 成功消息不再返回存储侧 id（[memory_store.rs:370](../../../core/acowork-runtime/src/tools/builtin/memory_store.rs#L370)，`store_episode` 返回 `()`），任何依赖返回 id 的调用方（含 desktop 调试面板）静默失去句柄。

---

## 5. 测试覆盖率

### 5.1 实测数据（HEAD = `9964b7ac`）

| 测试目标 | 结果 | 与 ADR-068 关系 |
|---|---|---|
| `cargo test -p acowork-grafeo` | 315 过 / 0 失败 | distiller 19 个用例（D1–D18）全过 |
| `cargo test -p acowork-memory` | 30 过 / 0 失败 | — |
| `cargo test -p acowork-runtime --lib` | 1332 过 / **1 失败** | 失败项为 todo/压缩路径（`e2e_prompt_cache`），**非** ADR-068 引入 |
| `memory_store` 工具单测 | 18 过 | M5 新增 inmemory 写入语义断言，质量好 |
| `consolidation_bg` 单测 | 9 过 | 含 M4 集成级 `test_distiller_step_promotes_facts_when_enabled`（真 GrafeoStore + 脚本化 Mock LLM），是当前最接近"新链路 e2e"的用例 |
| `tests/memory_p1p2_e2e.rs` | **9 过 / 8 失败** | M5 破坏 |
| `tests/memory_m4_bench.rs` | **1 失败** | M5 破坏 |
| `tests/memory_m4_probe.rs` | **1 失败** | M5 破坏 |
| `tests/memory_m5_bench.rs` | **1 失败** | M5 破坏 |
| `tests/prompts_api_e2e.rs` | 2 失败 | prompt 注册表 6 vs 9（ADR-063 契约），ADR-068 提交未触碰，疑似既有漂移（未深究） |

**红测根因**（可精确归因）：ADR-068 系列（`c994b5ae^..9964b7ac`）**未触碰任何 `tests/` 文件**；M5 将工具结果从 `Stored fact: …(id: 5)` 改为 `Stored episode: …(subtype: …)` 且写入语义从 KnowledgeNode 变 Episode。[memory_p1p2_e2e.rs:126](../../../core/acowork-runtime/tests/memory_p1p2_e2e.rs#L126) 的 `node_id_from_tool_result` 直接 panic；后续 `get_knowledge`/`export` 断言落空。

### 5.2 ADR 验收矩阵逐项对照

**W（写入路径）**：W1 ✅（enum 断言）· W2 ✅（无 aspect/key/source）· **W3 ❌**（grep 非 0）· **W4 ❌**（解析路径实际 false）

**D（蒸馏质量）**：D1/D2/D3/D5/D7/D9/D11/D12/D13/D14/D15/D17/D18 ✅（有同名/等价单测）；D4/D6 仅在 D1 内联断言（无独立用例，可接受）；**D8 ❌ 无测试无实现**；D10 仅计数器断言，无"audit ↔ 节点一一对应"集成测试（且 A4 使该对应在数据上不可能成立）；**D16 ❌**（无字段反射测试，数据面虽满足）；D17 用"全同向量假 embedding"验证聚类机制，非真实同义谓词召回；**D19 ❌**——工具 description 实际包含 "Autobiographical feedback …"（[memory_store.rs:78](../../../core/acowork-runtime/src/tools/builtin/memory_store.rs#L78)），与 ADR 字面验收冲突（R-R1(b)"迁移指引"与 D19 的文档内矛盾，至少需一个文本断言测试明确取舍）。

**E（e2e）——核心缺口**：**ADR-068 自身零 e2e 测试**。

- E1（agent 启动 → Identity/Capability 从 manifest 写入）：无任何层级测试（M8 只改注释与日志文案）；
- E2/E3/E4：仅有拆到单测的 mock 覆盖；**无**"用户话 → LLM 调 memory_store → Episode(knowledge_subtype) → bg 巩固 → distiller 晋升 → 沉淀层节点 → system prompt 注入"完整链路（E5 亦无）；
- E6（新 schema 下 14 天衰减回归）：无；
- E7 ✅（`test_memory_store_rejects_autobiographical_category` 存在且通过）。

M7 里程碑要求的"e2e：跑 100 个 episode → 沉淀层节点出现 + 审计完整"无任何自动化对应。全仓 `tests/` 目录无 distiller/ADR-068 专用 e2e 文件。

**补充风险**：Step 2a / Step 4 的 prompt 与 `RawExtract` / `RawJudge` 反序列化之间无 golden 测试，mock 返回的 JSON 与真实 prompt 输出格式靠目测一致（R-R2 高风险点无测试护栏）。

---

## 6. 修复计划

> 编号沿用 A1–A6 / 验收编号沿用 ADR §5。每项含：目标、改动点、验收标准。
> 建议按 P0 → P1 → P2 顺序独立提交，每提交保持可编译、可测试、可回滚。

### P0-1 修复 M5 破坏的 4 个既有 e2e/bench 文件

- **目标**：恢复 `cargo test --all` 全绿基线（消除 11 个红测）。
- **改动点**：
  - `core/acowork-runtime/tests/memory_p1p2_e2e.rs`：A1–A4 改为断言"写入的是 Episode（`knowledge_subtype` 正确、unconsolidated）而非 KnowledgeNode"，随后**通过 distiller/离线巩固制造沉淀层节点**再验证 privacy/importance/keywords 落点（若需保留原契约语义，可改用 `provider.store_knowledge` 直接铺数据，但必须注明已脱离 LLM 工具链路）；`node_id_from_tool_result` 删除或改为读取 episode 存储 id（需先让 `store_episode` 返回 id，见 P1-3）。
  - `memory_m4_bench.rs` / `memory_m4_probe.rs` / `memory_m5_bench.rs`：同步改为 Episode 写入契约（关键字/隐私/importance 现落在 Episode.metadata，按新契约断言）。
- **验收**：`cargo test -p acowork-runtime`（含全部 integration tests）全绿；`cargo test --all` 无新增红测。

### P0-2 补齐 ADR-068 自身的 e2e（对应 E1–E5、D10、M7 验收）

- **目标**：为"LLM 工具 → Episode → distiller → 沉淀层 → 注入"建立真实链路回归。
- **改动点**（新增 `core/acowork-runtime/tests/memory_adr068_e2e.rs`）：
  1. E2 链路：`MemoryStoreTool`（category=preference, content 关于 agent 反馈）→ 断言 Episode 落库（subtype=Preference、无 autobio 字段、unconsolidated）；
  2. E3/E4 链路：铺 3 个跨 14 天 limitation 候选 episode → 触发 `run_episodic_distiller_step`（真 GrafeoStore + `ProviderLlmAdapter` 或严格 golden Mock LLM，固定 judge/extract 输出）→ 断言 `AutobiographicalNode{category=Limitation, key="verbose_response"}` 落库 + `promotion_evaluations` 一一对应（D10）+ episode consolidated；
  3. E5 链路：断言该 AutobiographicalNode 被 `MemoryManager::retrieve` 注入（可复用 manager 现有注入断言模式）；
  4. E1 链路：agent 启动（或等效初始化）后 Identity/Capability 节点存在——需要先为 `bootstrap_autobiographical_from_manifest` 补可测出口（见 P1-2）；
  5. D16 反射测试：编译期/运行期断言 `Episode` 不存在结构化与 autobio 字段。
- **验收**：新增 e2e 全绿；D8 之外的 D/W/E 验收项均有自动化对应。

### P1-1 完成 M6 真删除（A1/A2）

- **目标**：R3 在 grafeo 层也成立；消灭"注释与事实不符"。
- **改动点**：
  - 摘除 `instant.rs::process_memory_store` / `process_procedure` / `process_autobiographical` / `derive_autobiographical_key`（或整体转为 `#[cfg(any(test, feature="eval"))]` 并在注释如实说明）；
  - 摘除 `offline.rs::compress_history_nodes` 固有实现（统一走 episodic retention），同步删其单测；
  - `auto_generate_relationship_nodes` / `run_relationship_generation` 收编为 distiller 的 relationship 晋升实现细节（见 P1-4），或随 ADR 变更单显式保留；
  - 迁移 `eval.rs`（IE/Abs 维度）改为"store_episode + distiller"后再评估；修正 provider.rs / provider_impl.rs 的失实注释。
- **验收**：`grep -rn "process_memory_store\|process_autobiographical\|process_procedure" core --include="*.rs"` 命中为 0（W3）；全量测试全绿。

### P1-2 修正 M7 默认开关并补解析单测（W4）

- **目标**：运行时默认行为与 ADR W4 一致，且 manifest.rs 与 agent_core.rs 注释一致。
- **改动点**（二选一，需 ADR 决策）：
  - 方案 A（按 ADR）：[agent_core.rs:1105-1111](../../../core/acowork-runtime/src/agent/agent_core.rs#L1105) 改为 `unwrap_or(true)`（缺段 = ON），存量 agent 若需关闭显式配 `enabled: false`；
  - 方案 B（按现状）：承认"缺段 = OFF、显式段默认 ON"是刻意的 opt-in 策略，**回改 ADR W4/M7 文案**与 manifest.rs 注释。
- **验收**：新增单测覆盖"无 distiller 段 → distiller_enabled = 预期默认值；有段缺 enabled → true；enabled=false → false"三条路径；示例 agent 文档同步。

### P1-3 让写入/晋升返回节点 id（A4 + P0-1 前置）

- **目标**：audit 可映射到实际节点，工具/e2e 可引用 episode id。
- **改动点**：`MemoryProvider::store_episode` / `store_knowledge` 等返回 `Result<u64>`（或新增 `*_with_id`），贯通 Grafeo/InMemory 实现；distiller 将 id 填入 `PromotionEvaluation.promoted_node_id`；memory_store 成功消息回填真实 id。
- **验收**：D10 集成测试断言 `promotion_evaluations[*].promoted_node_id.is_some()` 且 `get_knowledge(id)` 可查回。

### P1-4 M8 未竟项 + Relationship 单生产者化

- **目标**：Relationship/Limitation/Preference/History 沉淀层收敛到 distiller（含事件型 History）。
- **改动点**：
  - 30 天 Relationship：将 `auto_generate_relationship_nodes` 改为由 distiller 在 consolidation 周期内显式调用（作为 `promote_autobio_relationship()` 实现细节），移除 offline 固化步骤；`run_relationship_generation` 同理收编或移除；
  - History（D8）：在 `EpisodicDistiller` / bg 任务增加事件 hint 入口（MQTT `acowork/consolidation/event` 或等价调度信号），聚类阶段放行 `AutobioAspect::History` 的事件型路径；
  - 为 bootstrap 增加可测出口并补 E1 测试。
- **验收**：D8 集成测试（无 episode 输入 + hint → History 节点）通过；Relationship 无重复/多生产者测试（幂等）。

### P2-1 Judge Skip 粘滞（A3）

- **目标**：skip 不再导致同批 episode 无限重试与重复 LLM 开销。
- **改动点**：judge skip 时在 episode `metadata["distiller_skip"] = {cluster_key, reason, at}` 落粘滞标记（或独立 tombstone 注册表）；Step 1 扫描排除带 skip 标记的 episode；`defer` 仍保留重试语义。
- **验收**：单测：skip 后二次 run 不再对该聚簇发起 judge 调用（mock LLM 调用计数断言）。

### P2-2 Autobio 聚簇改用 embedding（A5）

- **目标**：消除 key_hint 字符串分裂导致的永不晋升。
- **改动点**：autobio 候选按 `aspect` + key_hint embedding 余弦 ≥ cluster_threshold 聚簇（复用 knowledge 聚簇逻辑）；无 embedding 时回退字符串相等（与 knowledge 路径一致）。
- **验收**：单测：`verbose_response` / `verbosity` 两 key_hint 同向量 → 1 聚簇（沿用 D17 模式）。

### P2-3 清理死配置与补护栏（A6）

- **目标**：配置项可观测、护栏生效。
- **改动点**：`llm_temperature` 接入 LLM 适配器或从配置/文档移除；`max_cluster_size` 在 Step 2b 聚类处实现截断护栏；评估 R-R5 锁（多 agent 并发时补 `promotion_in_flight`）。
- **验收**：clippy 0 警告；对应单测。

### P2-4 文档一致性收尾

- **目标**：消除 ADR 内部矛盾与失实注释。
- **改动点**：D19 与 R-R1(b)（迁移指引）矛盾 → 在 ADR 记录取舍（保留"Autobiographical"一词的指引文案，D19 改为"不出现 candidate_autobio_aspect / aspect 字段"）；修正 provider.rs / provider_impl.rs 失实注释；ADR 状态推进并记录本 review 结论。
- **验收**：ADR 与实现行为逐条可核。

---

## 7. 修复完成验收清单

```bash
# 1. 全量单元 + 集成
cd core && cargo test --all                 # 必须全绿（含 memory_p1p2_e2e 等 4 个文件）
# 2. 新增 e2e 单独确认
cargo test -p acowork-runtime --test memory_adr068_e2e
# 3. W3 红线
grep -rn "process_memory_store\|process_autobiographical\|process_procedure" core --include="*.rs"   # 期望 0
# 4. 静态
cargo clippy --all-targets -- -D warnings  # 0 警告
# 5. 行为抽查
#   - 无 [memory.distiller] 段的 agent：distiller 按决策后默认值生效（W4 单测覆盖）
#   - E2–E5 全链路 e2e 绿
```

---

## 附：评审置信度与未决项

- **置信度高**的结论：所有测试红/绿状态（本机实测）；11 个红测归因于 M5（提交文件清单 + 失败签名吻合）；A1/A2 双实现事实。
- **未深究项**：`tests/prompts_api_e2e.rs`（2 失败，ADR-063 prompt 注册表漂移，非 ADR-068 提交可触及）；`e2e_prompt_cache::restart_after_compression_preserves_todo_state`（1 失败，todo 压缩路径，非 ADR-068 可触及）。建议各自归口处理。
- 修复完成后建议由 ADR 决策者（大鱼）复核 P1-2 的 A/B 方案取舍与本报告 §6 优先级。
