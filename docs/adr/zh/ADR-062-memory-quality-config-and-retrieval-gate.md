# ADR-062：记忆质量参数集中化与检索质量门禁（MemoryQualityConfig）

> ✅ **M5 已全绿落地（方案 A）**——auto_inject 首轮触发（§6.1）+ keyword 写时质量门（§6.2.1）+ `keyword_index` 写时拼入 `object`（§6.2 Plan Y / §6.5 步骤 2a/2b）。
>
> - 当前代码：`auto_inject_enabled` 默认 `false`（per-agent 经 manifest `[memory.quality].auto_inject_enabled = true` 显式开启，开启后每 session 首轮触发一次）；`keyword::sanitize` 质量门 always-on（memory_store LLM 边界 + instant 持久边界双向调用）；`quality.keyword_index` 默认 `false`（per-agent manifest 显式开启）。
> - **M5 后修正**：M5 曾将 `auto_inject_enabled` 默认 `false → true`；因与 LLM 自主 `memory_recall` 双路径召回重复（两条路径同以 user 消息为 query，核心节点必然重叠，见 [05-memory.md §0](../design/zh/05-memory.md#0-分层原则) 检索注入行），默认值回退为 `false`（per-agent opt-in），`memory_recall` 工具描述已加防重复召回提示（"do NOT re-run the same query"）。
> - M5 benchmark（`memory_m5_bench.rs`）：keyword hit@5 0.0000→1.0000，Precision@5 0.5000→0.8750，Recall@5 0.6250→1.0000，MRR 0.6250→1.0000，无回归。
> - 提交历史：`15654af0`（含 M5 草案的安全快照）→ `b117f901`（仅撤回 M5 代码增量）→ `6de0caf2`（§6.2.1 质量门文档）→ **M5 落地提交**（本实施）。
> - 回滚路径：所有开关参数化（`quality.auto_inject_enabled` / `quality.keyword_index`），回滚成本 = 改配置。
>
> ✅ **检索参数接通（M5 后续增量）**——盘点 `memory_recall` 未生效参数后实施两项：
> - **时间过滤接通（#1）**：`since`/`until` 此前只校验、从不生效（全代码无 `filters.time_range` 写入点）。现打通三层——`MemoryProvider::get_node_created_at`（新 trait 方法，GrafeoProvider 读节点 `created_at` 属性 + InMemoryProvider 实现）、`MemoryManager::retrieve` post-filter（镜像 `exclude_session_id` 的 keep-on-unknown 策略）、`memory_recall` 工具写入 `filters.time_range`（单边补界：`since` 单边 → `[since, now]`，`until` 单边 → `[epoch, until]`）。
> - **配置一致性（#6）**：`memory_recall` 此前硬编码 `MemoryManagerConfig::default()`（`memory_recall.rs:184`），不读 agent 的 manifest quality 配置。现 `MemorySessionHandle` 持有 agent 的 `MemoryManagerConfig`，工具从 handle 读取——与 auto_inject 同一份配置（min_score / graph_expand / …），双路径行为一致。
> - 测试：memory_recall 工具 20 个测试全绿（新增：since/until 双 provider e2e——InMemoryProvider + GrafeoStore、manager 层 time_range 过滤、`get_node_created_at` trait 直接测试、spec 描述断言）；memory 30 / grafeo 291 / runtime 1138 / memory_e2e 4 / p1p2 17 / m4 1 / m5 1 全绿，clippy 0 警告。
> - 待接通（后续）：`search_mode` 三种策略、`privacy_levels`、`session_id` 过滤、加权 RRF——多数有 ADR-062 P3 决策约束（先 benchmark 证明需要），见 §9。

**状态**：已实施（2026-09）
**日期**：2026-09
**决策者**：大鱼
**前置**：
- [ADR-051](./ADR-051-runtime-memory-provider-decoupling.md)（Runtime 与 Grafeo 解耦）
- [ADR-057](./ADR-057-compaction-distillation-into-graph.md)（压缩蒸馏入图，triples 删除）
- [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md)（上下文块重排，检索注入位置）
- [05-memory.md](../design/zh/05-memory.md)（记忆系统设计文档，P1/P2 数据质量改动基线）

---

## 1. 决策摘要

P1/P2 记忆数据质量改动（`bugfix/memory`，G1-G13/G20）已落地并全绿，**写入质量明显提升、可验证**；但 **检索质量仅有局部改进，尚不足以支撑重新打开 `auto_inject` / 让 `keywords` 参与检索**。核心缺口：

1. **检索路径未排除 `Dormant` 节点**——设计 §5.2 明确"Dormant 不参与常规检索"，但全链路（`manager.rs` → `provider_impl.rs` → `grafeo.rs` → `grafeo-engine`）均无 `status` 过滤。衰减/清理只减少了存储量，**没有减少检索结果里的垃圾**——这正是当初关闭 auto_inject 的原因。
2. **`hint_weights`（RRF 权重）实际不参与排序**——`retrieval.rs` 标注 `_text_weight`/`_vector_weight` 为 "Reserved"，四种 hint 权重配置空转。
3. **检索质量参数散落 5+ 个文件、无 benchmark 量化**，无法调优、无法对比、无法回滚。
4. **`memory_store` 工具描述中的默认值造成 LLM 锚定**——confidence/importance 分布坍缩，衰减与巩固门控失去区分度。

本 ADR 决定：

1. **P0：检索路径排除 Dormant 节点**（`exclude_dormant` 过滤，默认开启）。这是"让衰减/清理真正反哺检索质量"的关键一行，也是 auto_inject 重新打开的前提。
2. **P1：新增 `MemoryQualityConfig`**（memory 层，可被 `.agent` 包覆盖），把"生效且写死"的检索/写入质量参数集中化，消除散落硬编码。
3. **P2：benchmark 门禁**——重新打开 `auto_inject` 和让 `keywords` 参与检索，**必须以 `eval.rs` / `retrieval_metrics.rs` 的量化指标达到阈值为前提**，不做无依据的开关。
4. **P3：`memory_store` 工具描述去锚定**——schema 已先行落地为零数字版（§6.2），本 ADR 确认现状并推进 M3.6 数据校准：以证据维度引导 LLM 主动推理使分数离散化，阈值合理性由分布数据重标定（详见 §6.6）。

**非目标**（本 ADR 不讨论）：
- 记忆内容本身的语义质量（LLM 打分准确性）——本 ADR 只定义捕获/过滤/调参机制。
- embedding 模型替换与向量质量——独立话题。
- `hint_weights` 的加权 RRF 完整实现——若 P2 benchmark 证明需要才做。

---

## 2. 背景与现状盘点（代码级事实）

### 2.1 P1/P2 改动概览与质量结论

| 维度 | 结论 | 依据 |
|---|---|---|
| 写入质量 | ✅ **明显提升** | typed `privacy`/`importance`/`source`/`keywords` 全量落库（`grafeo/consolidation/instant.rs:238,266`）；dedup 门控 `0.95`/`0.90`（`instant.rs:52,58`）；离线巩固 confidence 门控 `<0.3→Dormant / ≥0.7→Active`（`offline.rs:132-148`）；episode 三规则清理（`offline.rs:392`） |
| 检索质量 | ⚠️ **局部改进** | graph expand 真正消费边权重 + 阈值 `[0.1,0.15,0.2]`（`spreading.rs:159-163`）；Identity 全 label 检索（`manager.rs:319`）；Abstention prompt 注入（`manager.rs:507`） |
| 检索质量缺口 | 🔴 **未闭合** | **Dormant 未排除**；**RRF 权重空转**；**无 benchmark 数字** |

### 2.2 决定性事实：检索链路无 Dormant 过滤

设计 §5.2："Dormant 不参与常规检索但保留"（**并非删除**）。全链路核对结果：

| 层 | 文件 | status 过滤 |
|---|---|---|
| 检索编排 | `acowork-memory/src/manager.rs` `retrieve` | ❌ 仅 session 排除 + dedup（`manager.rs:380-545`） |
| Provider | `acowork-grafeo/src/provider_impl.rs` | ❌ |
| 原生检索 | `acowork-grafeo/src/grafeo.rs` `search_with_filter` | ❌ 仅 score 过滤 |
| 引擎 | `grafeo-engine search.rs` | ❌ text/vector 索引含全部节点 |

**含义**：G2（衰减→Dormant）与 G13（episode 清理）当前只减少存储量，Dormant 节点仍会出现在检索结果中。auto_inject 当初因"垃圾进上下文"关闭，**这个理由目前仍然成立**。

### 2.3 决定性事实：hint_weights 空转

`manager.rs:1024` 定义四套权重（Semantic 0.8/0.2、Identity 0.3/0.7 等），传入 `hybrid_search_full`，但 `grafeo/retrieval.rs:239-240` 明确标注：

```rust
_text_weight: f32,   // Reserved for future weighted RRF implementation
_vector_weight: f32, // Reserved for future weighted RRF implementation
// Weight scaling after RRF is meaningless...
```

即排序只靠 **RRF rank + PageRank boost**，设计 §6.6 的"动态权重"未生效。

### 2.4 决定性事实：检索质量参数散落盘点

| 参数 | 当前值 | 位置 | 是否生效 | 是否可配置 |
|---|---|---|---|---|
| RRF k | 60 | grafeo-engine 硬编码 | ✅ | ❌ |
| hint_weights（4 套） | 0.8/0.2 等 | `manager.rs:1024` | ❌ 空转 | ❌ |
| min_score（RRF 域，默认） | 0.0 | `MemoryManagerConfig` | ✅ | ✅ 已有 |
| min_score（auto_inject） | 0.3 | `memory/types.rs:132` | ✅ | ❌ 写死 |
| graph expand 阈值 | `[0.1,0.15,0.2]` | `spreading.rs:42` | ✅ | ✅（builder） |
| min_edge_weight | 0.1 | `spreading.rs` | ✅ | ✅（builder） |
| DECAY_PER_HOP | 0.7 | `spreading.rs:105` | ✅ | ❌ 写死 |
| edge weight λ / cap | 0.01 / 0.8 | `semantic/graph.rs:12` | ✅ | ❌ 写死 |
| decay 参数 | 7 字段 | `DecayConfig`（`memory/types.rs:606`） | ✅ | ✅ 已有 |
| dedup 阈值 | 0.95 / 0.90 | `instant.rs:52,58` | ✅ | ❌ 写死 |
| 巩固门控 | 即时 0.85 / 离线 0.7+0.3 / 泛化 0.8 | `instant.rs:68`、`offline.rs:132,138`、`generalization.rs:427,473` | ✅ | ❌ 写死（三套） |
| PageRank weight | 0.1 | `MemoryManagerConfig` | ✅ | ✅ 已有 |
| **Dormant 检索排除** | **无** | **缺失** | — | — |

### 2.5 决定性事实：memory_store 工具描述的锚定问题与现状

> **2026-09 复核**：本节描述的"默认值锚定 schema"曾是代码现状；复核时 `memory_store.rs` 的 `confidence`/`importance` schema 文本**已先行改为零数字版**（与 §6.2 目标形态一致），即 D4 的 schema 改动已提前落地。本节保留历史事实与代码兜底盘点，D4（§6）相应重定位为"确认已落地 + M3.6 阈值校准"。

历史锚定文本（曾为 LLM 实际看到的 schema）：

```jsonc
"confidence": { "description": "... High confidence (>=0.85) creates an Active node;
    lower creates Pending for later verification. Default 0.7 for knowledge/procedure,
    0.85 for autobiographical." }          // ← 锚定点 1
"importance":  { "description": "... Higher importance resists forgetting. Default 0.5." }
                                             // ← 锚定点 2
```

代码兜底（LLM 完全不提供时，保持不变）：
- `memory_store.rs:20,26`：`DEFAULT_CONFIDENCE=0.7` / `AUTOBIO_DEFAULT_CONFIDENCE=0.85`
- `instant.rs:71`：`DEFAULT_CONFIDENCE=0.7`；`instant.rs:266`：`importance.unwrap_or(0.5)`

**锚定效应的由来**：若 schema 含 "Default 0.7/0.5"，LLM 在不确信时会直接采信"保险的默认值"，导致 confidence/importance 分布坍缩到少数几个值、失去区分度，下游衰减（FLOOR/BOOST_CAP 依赖 importance）与巩固门控（0.7/0.3 依赖 confidence）随之失效。当前 schema 已消除此风险，但**分布是否真正离散化、阈值是否需重标定，仍需 M3.6 用数据验证**（§6.6）。

---

## 3. 决策 D1（P0）：检索路径排除 Dormant 节点

### 3.1 决策

在检索链路的合并结果阶段增加 `status != "Dormant"` 过滤，作为 `MemoryQualityConfig.exclude_dormant`（默认 `true`）。

### 3.2 实现位置（首选）

`grafeo.rs search_with_filter` 的 `all_results.retain(...)` 处，或 `manager.rs` 合并后 `best_by_id` 构建前。**优先在 manager 层**（对所有 Provider 实现生效，含测试桩），其次在 grafeo 原生层。

**前置条件（M1 第一步）**：`MemoryProvider` trait 目前只有 `get_node_content` / `get_node_session_id`（`acowork-memory/src/provider.rs:246,256`），**没有 `get_node_status`**。manager 层按 status 过滤必须先在 trait 新增 `fn get_node_status(&self, node_id: u64) -> Result<Option<NodeStatus>>`，并同步实现于 `GrafeoProvider`（`provider_impl.rs`）与测试桩——否则过滤无从落地。

**与 graph_expand 的关系**：graph expansion 的种子取自 `manager.rs:385-406` 的 `all_results`（含 Dormant），而 Dormant 过滤在 `best_by_id` 构建前（`:408`）执行。由此确定的语义为：**Dormant 节点可继续作为图扩展种子（保留图桥接），但不会出现在最终检索结果中**。

### 3.3 语义细节

- **Purged** 节点已物理删除，无需处理。
- **Pending** 节点（低置信待验证）：设计上属于"可检索但低置信"。默认**参与检索但通过 confidence 排序自然降权**；不单独过滤（避免过度收紧召回）。
- 命中 `Dormant` 时是否计入 `access_count`/恢复 Active 属后续行为问题，本 ADR 默认**不恢复**（避免检索自身造成"假活跃"），见 §9 开放问题。

### 3.4 验收

- 新增测试：写入后 `transition_to_dormant` → 检索结果不包含该节点；Active/Pending 仍返回。
- 回归：既有 `memory_p1p2_e2e.rs` 与 `memory_e2e.rs` 全绿。

---

## 4. 决策 D2（P1）：新增 `MemoryQualityConfig`

### 4.1 决策

在 memory 层新增 `MemoryQualityConfig`，收拢"生效且写死"的质量参数，支持 per-agent 覆盖（随 `.agent` 包 manifest 注入，复用 ADR-051 的解耦通道）。

```rust
pub struct MemoryQualityConfig {
    // ── 检索 ──
    pub exclude_dormant: bool,                 // 默认 true（D1）
    pub min_score: f32,                        // RRF 域；auto_inject 与默认统一走这里
    pub graph_expand: GraphExpandQuality {     // thresholds / min_edge_weight / decay_per_hop
        early_stop_thresholds: Vec<f32>,       // 默认 [0.1, 0.15, 0.2]
        min_edge_weight: f32,                  // 默认 0.1
        decay_per_hop: f64,                    // 默认 0.7
    },
    pub edge_weight: EdgeWeightQuality {       // lambda / cap
        lambda: f64,                           // 默认 0.01
        cap: f32,                              // 默认 0.8
    },
    pub pagerank_weight: f64,                  // 默认 0.1（合并现有 MemoryManagerConfig 字段）
    // ── 写入 ──
    pub dedup: DedupQuality {
        knowledge_threshold: f32,              // 默认 0.95
        procedure_threshold: f32,              // 默认 0.90
    },
    pub consolidation: ConsolidationQuality {
        direct_active_threshold: f32,          // 默认 0.85（instant.rs:68 即时提取→Active）
        pending_upgrade_threshold: f32,        // 默认 0.7（offline.rs:138 离线巩固 Pending→Active）
        dormant_confidence: f32,               // 默认 0.3（offline.rs:132 离线巩固→Dormant）
        min_pending_age_hours: u64,            // 默认 1（offline.rs:640）
    },
    pub keyword_index: bool,                   // 默认 false（P2 门禁通过后再开）
}
```

### 4.2 边界与取舍

- **不迁移**：`DecayConfig`（已 7 字段集中，§2.4）与 `MemoryManagerConfig`（检索预算/注入预算）保持独立，避免大爆炸。
- **与 `MemoryManagerConfig` 字段重叠**：`MemoryQualityConfig.min_score` / `pagerank_weight` 与 `MemoryManagerConfig.default_min_score` / `pagerank_weight`（`manager.rs:167,170`）同义。规则：**`MemoryQualityConfig` 为新源，旧字段标记 deprecated（运行时优先读 `MemoryQualityConfig`，字段未设置时回退 `MemoryManagerConfig` 旧值），后续单独清理**。两者不并存生效。
- **巩固阈值现状（三套而非一套）**：代码中存在三套独立阈值——即时提取 `≥0.85→Active`（`instant.rs:68`）、离线巩固 `≥0.7→Active / <0.3→Dormant`（`offline.rs:132,138`）、经验泛化 Pending→Active `≥0.8`（`generalization.rs:427,473`，ProceduralNode 路径）。D2 的 `ConsolidationQuality` 拆为 `direct_active_threshold` / `pending_upgrade_threshold` / `dormant_confidence` 三字段以覆盖现实。**泛化路径 0.8 与离线 0.7 是否统一属待决问题**——不同节点类型用不同置信度线（程序模式误判成本更高）在语义上说得通，本 ADR 先各自参数化、不强行统一，由 M3.6 校准后决定。
- **RRF k 参数化**（P3）依赖 grafeo-engine 版本支持，暂不纳入，先记录。
- **hint_weights**：当前空转。若 P2 benchmark 证明加权有意义再实现；否则**删除空转代码**（Rule of three / YAGNI）。
- **per-agent 注入机制（M2 待办）**：当前 `MemoryManagerConfig` 仅 `Default` 构造（`agent_core.rs:810-812`），**不存在 manifest→config 注入管线**。M2 需新增 `.agent` manifest `[memory.quality]` 节解析 + Provider 工厂注入——ADR-051 只解决了 Runtime↔Provider 解耦，未覆盖配置注入。
- 每个字段默认值必须与现状代码一致，保证"零配置 = 现状行为"，可平滑落地。

### 4.3 验收

- 所有参数可通过配置覆盖并生效；默认值与现状行为一致（快照对比测试）。
- `cargo test -p acowork-memory -p acowork-grafeo` 全绿。

---

## 5. 决策 D3（P2）：benchmark 门禁

### 5.1 决策

`auto_inject` 重新打开 与 `keyword_index` 开启，**均以 benchmark 达标为前提**，不因"感觉质量好了"而开启。

### 5.2 指标与阈值（初版，可校准）

复用现有 `grafeo::eval`（`eval_information_extraction` / `eval_abstraction`）与 `retrieval_metrics`（NRR / Precision@k / Recall@k）：

| 指标 | 阈值（建议初值） | 测量方式 |
|---|---|---|
| 检索 Precision@5（含 Dormant 排除后） | 比现状提升 ≥ 20% 且 ≥ 0.5 | `retrieval_metrics` + 固定查询集 |
| Dormant 垃圾进上下文比例 | = 0（D1 生效即保证） | 命中样本抽样 |
| confidence/importance 分布方差 | 显著增大（锚定消除） | `memory_store` 写入样本统计 |
| auto_inject 注入命中率 | ≥ 60% 返回结果非空 | 打开前后对比 |

### 5.3 门禁流程

1. 合并 D1 + D2 + D4（提示词去锚定）后跑一轮基准，记录 before 数字。
2. 仅当 D1 生效且 benchmark 达到 §5.2 阈值，才打开 `auto_inject`（同时修复其 `min_score=0.3` 分数域问题，见 §6.4）。
3. `keyword_index` 同理，有数据支撑再开。

### 5.4 验收

- 出具一份 benchmark 报告（before/after 对比表），作为开启开关的依据存档。

---

## 6. 决策 D4（P3）：memory_store 工具描述去锚定

### 6.1 决策

**2026-09 复核状态：schema 去锚定已先行落地**——当前 `memory_store.rs:102-114` 的 `confidence`/`importance` 描述已是零数字、纯证据引导版（与 §6.2 目标形态一致）。本决策从"待实施"重定位为：

1. **确认现状已符合 §6.2 目标形态**（作为验收依据，见 §6.5）；
2. **代码兜底常量保留**（`DEFAULT_CONFIDENCE=0.7` / `AUTOBIO_DEFAULT_CONFIDENCE=0.85` / `importance.unwrap_or(0.5)`，作为"LLM 完全未提供"时的防御，不构成锚定）；
3. **M3.6 阈值校准**（§6.6）仍需执行——schema 已去锚定，但分布是否离散化、后端阈值是否需重标定，必须用真实写入数据验证。

### 6.2 提示词改写（目标形态，零数字版）

**关键原则：LLM 不需要知道后端的判定阈值（0.85/0.3/0.7）**——Active/Pending/Dormant 判定是 `instant.rs`/`offline.rs` 的后端规则，把阈值写进提示词只会制造两种锚定：默认值锚定（偷懒采信）与趋利锚定（知道 0.85 触发"立即 Active"后系统性地抬高分数）。因此提示词**零数字、纯证据引导**：

```jsonc
"confidence": {
    "type": "number",
    "description": "Your confidence in this knowledge (0.0-1.0), reflecting how certain
        you actually are. Anchor on evidence, not on a target value: base it on whether
        the statement is direct, explicit, recent, and from the user personally (higher),
        versus inferred, stale, or speculative (lower). Most routine observations are
        moderately certain — score them accordingly. Reserve very high scores for facts
        you would bet on; use very low scores for uncertain or contradicting signals.
        Do not inflate scores to make a memory seem more certain than it is."
},
"importance": {
    "type": "number",
    "description": "How critical is this memory to long-term value (0.0-1.0)?
        Higher resists forgetting. Distinguish core identity facts (near 1.0) from
        transient preferences (~0.3-0.5) from trivia (~0.1)."
}
```

### 6.3 要点

> 以下要点均为当前代码已实现的形态（2026-09 复核），本节作为设计意图的正式记录。

- **去锚定 ≠ 无指导**：不提供"默认取多少"，也不把后端判定阈值（0.85/0.3/0.7）泄漏给 LLM——那是 `instant.rs`/`offline.rs` 的规则，LLM 只需输出连续的确信度，后端自行判定。以**证据维度**（direct/explicit/recent/personal vs inferred/stale/speculative）引导推理，既避免默认值锚定，也避免趋利锚定（"给 0.85 就能立即 Active"）。
- **信任 LLM 的保守性**：零数字版下 LLM 天然保守，给出 ≥0.85 的记忆必然很少；**能给出 0.85 本身就是"确信非垃圾"的信号，应信任该信号**。因此**不做悲观默认对冲**（不临时收紧即时生效线），高分信号直接采信。
- **保留代码兜底**：`DEFAULT_CONFIDENCE=0.7` / `importance.unwrap_or(0.5)` 是"LLM 完全没填"的防御，不是提示词锚定，二者不冲突。
- 测试 `test_memory_store_default_confidence`（`memory_store.rs:586`）与 autobio 0.85 断言（:915-918）**不受影响**——它们验证的是代码兜底路径，而非 schema 文本。
- **阈值合理性由数据决定**：去锚定后分数绝对尺度会漂移，代码中 `≥0.85→Active`/`<0.3→Dormant`/`≥0.7→Active` 等阈值与 LLM 分数的映射关系随之变化。这不靠猜，靠 §6.6 的 M3.6 阈值校准用分布数据重标定。

### 6.4 顺带修复：auto_inject 的 min_score

`memory/types.rs:132` `auto_inject` 设 `min_score: Some(0.3)`，落在 RRF 分数域（`1/(k+rank)`，k=60 → 最高约 0.016）会过滤掉几乎全部结果。D2 落地后改为走 `MemoryQualityConfig.min_score`（默认 0.0）。

### 6.5 验收

- schema 描述不含 "Default 0.7/0.5/0.85" 字样，且**不含后端判定阈值数值**（0.85/0.3/0.7 等触发线）。
- 对同一批记忆写入样本，confidence/importance 的**标准差显著大于**改动前（锚定消除的量化证据）。
- 全量测试绿。

### 6.6 阈值校准（M3.6）

去锚定让分数离散化、有区分度，但也使绝对尺度漂移。因此阈值合理性必须由数据决定，而非拍脑袋：

1. 去锚定落地后，先采集真实 confidence/importance 分布（复用 §5.2 的"分布方差"指标，作为校准输入）。
2. 用分布数据**重新标定** `MemoryQualityConfig` 中的 `direct_active_threshold`/`pending_upgrade_threshold`/`dormant_confidence`/`dedup` 阈值默认值（D2 已参数化，校准=改配置，可回滚）。
3. 阈值语义不变（"0.85 以上 = 值得立即生效"），仅数值随校准调整。
4. **不做分布相对分位数**（如"当次 top 25% 才 Active"）：Active 应反映绝对可信度，相对主义会扭曲语义——若某次对话全是低确信信息，最高分 0.4 不应被强行判 Active。

---

## 7. 落地优先级与里程碑

| 里程碑 | 内容 | 依赖 |
|---|---|---|
| M1（P0） | Dormant 检索过滤 + 测试 | 无 |
| M2（P1） | `MemoryQualityConfig` 落地 + 参数收拢 + manifest `[memory.quality]` 注入 + auto_inject min_score 修复 | M1 |
| M3（P3） | memory_store 提示词去锚定——**schema 已先行落地**，本里程碑收敛为"验收确认（§6.5）+ 写入路径分布埋点" | M2 |
| M3.6（P3） | 采集 confidence/importance 分布（需先在写入路径加轻量分布埋点，当前无），校准阈值默认值（§6.6） | M3 |
| M4（P2） | 跑 before/after benchmark，出具报告 | M1-M3.6 |
| M5（P2 门禁） | ✅ **已落地**：auto_inject 首轮触发机制 + keyword 质量门 + `keyword_index` 写时拼入 object（per-agent 开启）；benchmark 达标。**后续修正**：auto_inject 默认值回退 `true → false`（per-agent opt-in，与 `memory_recall` 双路径召回重复），见状态行 | M4 |

每个里程碑独立可合并、可回滚（参数化保证回滚成本 = 改配置）。M5 落地后仍开放的事项见 §9 与 benchmark 报告 §6.7（P1：pagerank 非确定性修复、vector 索引填充验证；P3：加权 RRF、Block C 注入等）。

---

## 8. 风险与缓解

| 风险 | 缓解 |
|---|---|
| Dormant 排除导致召回下降（高价值沉睡节点消失） | `exclude_dormant` 默认 true 但可配置；benchmark 对比开关前后 |
| 去锚定后 LLM 系统性给低 confidence → 记忆过早 Pending/Dormant | **信任 LLM 保守性**：零数字版下能给出高分本身就说明确信，不做悲观默认对冲（§6.3）；若实测分布系统性偏低，由 M3.6 用数据校准阈值，而非在提示词里加数值引导 |
| 参数化引入配置爆炸 | 只收拢"生效且写死"的参数；默认值 = 现状行为；不迁移已集中配置 |
| 加权 RRF 未实现导致调参空间有限 | 明确 P3 保留；若 benchmark 显示 rank 融合是瓶颈再实现 |

---

## 9. 开放问题

1. **Dormant 命中是否应视为一次访问并自动恢复 Active**？设计 §5.2 说"被引用即恢复"，但检索自身触发恢复会造成"假活跃"。倾向：检索不恢复，仅用户/对话显式引用恢复。
2. **RRF k 是否需要参数化**？依赖 grafeo-engine 升级，暂记 P3。
3. **hint_weights 加权 RRF 是否值得实现**？待 M4 benchmark 决定；当前空转代码建议删除或标注 deprecated。
4. **keyword_index 的接入方式**：是拼入 `content` 参与 BM25，还是建 `metadata["keywords"]` 独立索引？待 benchmark 有正面信号后细化。

---

## 10. 关联文档

- [05-memory.md](../design/zh/05-memory.md)（§5.2 Dormant 语义、§6.5 Abstention、§6.6 检索权重）
- [ADR-051](./ADR-051-runtime-memory-provider-decoupling.md)
- [review 报告](../_internal/archive/review/zh/)（Gap 分析来源）
