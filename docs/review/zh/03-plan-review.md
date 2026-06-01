# Phase 2 开发计划审查报告

> 审查日期：2026-04-20
> 审查对象：`docs/plan/plan-p2.md`（v1.0）
> 审查依据：`docs/05-memory.md`（v3.4）、`docs/07-system-agent.md`（v3.5）、`docs/module-design/04-grafeo.md`、`crates/rollball-core/src/memory/traits.rs`、现有 crate 实现

---

## 一、总体评价

plan-p2.md 框架完整、依赖链清晰、里程碑合理、技术选型有评估，与 `09-roadmap-and-scenarios.md` 的 Phase 2 描述基本一致。

但存在 **2 个必须修正的架构级问题**（MemoryStore trait 顶层设计缺失、MemoryManager 归属不清）以及 **5 个建议修正项**。这些问题不修复就直接实现，会导致 S2.1~S2.9 建立在错误抽象上，后期重构成本高。

---

## 二、必须修正的问题

### 问题 A：[架构风险] MemoryStore Trait 顶层设计与 Phase 1 实现存在巨大鸿沟

**严重度**：P0（架构级风险）

**证据**：

| 维度 | 设计文档要求（05-memory.md §10 + 04-grafeo.md） | 现有实现（rollball-core/src/memory/traits.rs） |
|------|---|---|
| API 规模 | `store_episode`, `search_episodes`, `mark_consolidated`, `store_knowledge`, `store_procedural`, `store_autobiographical`, `hybrid_search`, `graph_expand`, `run_decay_scan`, `reactivate_node`, `purge_expired`, `health_check`, `stats` 等 13+ 方法 | `store`, `retrieve`, `search`, `delete`, `list_by_zone` 仅 5 个方法 |
| 架构定位 | Trait 驱动，Grafeo 是实现，MemoryManager 只依赖 trait | Phase 1 简单 KV 存储，无分层抽象 |
| 遗忘机制 | `run_decay_scan`, `reactivate_node`, `purge_expired` 专门接口 | 无遗忘相关接口 |
| 向量检索 | `hybrid_search`, `graph_expand` | 无 |

**影响**：S2.1 "Grafeo 数据模型实现" 的验收标准写了"所有字段可序列化"，但**没有任何任务负责重新设计 MemoryStore trait**。Phase 2 必须先重构 trait（增删方法），否则 S2.1~S2.9 全都建立在错误的抽象上。

**建议修正**：在 S2.1 之前新增 **S2.0 MemoryStore Trait 重设计**，任务包括：
- 重构 `MemoryStore` trait，增加 `store_episode`, `hybrid_search`, `graph_expand`, `run_decay_scan` 等方法
- 或拆分为子 trait：`EpisodicStore`, `SemanticStore`, `ForgettingStore`
- 确保 04-grafeo.md 的 GrafeoStore 实现与 trait 对齐
- S2.0 应在 S2.1 开始前完成，或至少与 S2.1 并行

---

### 问题 B：[架构风险] MemoryManager 归属不清

**严重度**：P1

**证据**：

| 来源 | 说 |
|---|---|
| 04-grafeo.md | "GrafeoStore 实现 MemoryStore trait，MemoryManager 在 Runtime 调用 trait" |
| plan-p2.md S2.9 | MemoryManager 放在 `rollball-memory/src/manager.rs` |
| rollball-memory 现状 | 只有 3 个文件（lib.rs, store.rs, types.rs），是瘦 wrapper |
| 05-memory.md | MemoryManager 是协调者，协调 S2.1~S2.8 各 Grafeo 子系统 |

**问题**：MemoryManager 是"协调者"（协调经历层/沉淀层/遗忘/检索），放在 `rollball-memory` 这个瘦 wrapper crate 不合理。`rollball-memory` 现状只是一个 trait re-export，没有 manager.rs。

**建议修正**：二选一：
1. **推荐**：MemoryManager 放在 `rollball-runtime`，因为它是 Runtime 主循环和 Grafeo 存储之间的编排层
2. MemoryManager 放在独立的 `rollball-memory-manager` crate（但需要新建）

---

## 三、建议修正的问题

### 问题 1：S2.4 HNSW 任务缺少 Embedding 持久化/加载子任务

**严重度**：P2

**问题描述**：S2.4 列了 EmbeddingProvider trait、ONNX 本地生成、HNSW 实现、超时降级，但向量生成后的**持久化存储**和**从磁盘加载到 HNSW 结构**没有明确任务。S2.8 graph_expand 依赖 S2.4，但如果 S2.4 不输出可检索的向量存储，S2.8 无法工作。

**建议**：在 S2.4 增加子任务：
- S2.4.5 向量 Embedding 持久化（episode 写入时将向量 blob 存入 episodes 表）
- S2.4.6 HNSW 索引从磁盘加载（Agent 重启后恢复索引状态）

---

### 问题 2：S2.7 Decay 硬编码阈值 vs DecayConfig 可配置设计

**严重度**：P2

**问题描述**：plan-p2.md S2.7 写的是：
```rust
decay_score = importance × activity_signal  // ✅ 正确
Active → Dormant 阈值: 0.3  // ❌ 硬编码
Dormant > 90 天 → Purge  // ❌ 硬编码
```

但 05-memory.md §5.1 明确说：
> "遗忘参数通过 `DecayConfig` 注入（不再硬编码），支持按 Agent 定制"

05-memory.md §5.2 的阈值表是**默认值**而非硬编码要求。

**建议修正**：S2.7 改为 "DecayConfig 参数化实现"，验收标准：
- DecayConfig 包含 `dormant_threshold: f32`、`purge_days: u32`、`decay_lambda: f32` 等字段
- 阈值从配置读取，支持按 Agent 定制
- 后台扫描使用配置的阈值

---

### 问题 3：S2.2.1 Episode 内容分类缺少"零 LLM 调用"约束

**严重度**：P2

**问题描述**：05-memory.md §2 明确：
> "内容分类压缩**不需要额外 LLM 调用**，全部由 Runtime 的确定性逻辑完成"

plan-p2.md S2.2.1 验收标准是"分类正确"，没有强调"零 LLM 调用"。这可能导致实现时用 LLM 做分类（增加 API 调用成本）。

**建议修正**：S2.2.1 验收标准增加：
- **"纯 Runtime 模板逻辑，无 LLM 调用"**
- 在 S2.2.2 工件性内容压缩也加同样约束

---

### 问题 4：S5.3.4 远程 Embedding API 降级方案未指定

**严重度**：P2

**问题描述**：plan-p2.md 说"本地失败时降级"，但没有说用哪个远程 API。Phase 2 结束时应该有明确可工作的远程降级方案。

**建议修正**：S5.3.4 明确：
- 降级 API 选型（如 OpenAI text-embedding-3-small 兼容接口）
- 或使用 agentskills.io 生态的公共 embedding 服务
- 超时阈值（建议 2s）

---

### 问题 5：S5.4 Token 计数 CJK 处理与设计文档不一致

**严重度**：P2

**问题描述**：

| 来源 | Token 估算方式 |
|------|--------------|
| 05-memory.md §1 | 检索结果按**字符数 / 3**近似估算 |
| plan-p2.md S5.4.3 | **中文 1.5 字符/token** |

两个文档用了不同的估算值。Phase 2 实现时应验证哪个更准确，并统一。

**建议修正**：S5.4.3 验收标准改为：
- "与目标 LLM 的 tokenizer 误差 < 5%"
- CJK 估算比例通过实际采样校准
- 文档要求不一致的部分应先统一

---

## 四、Minor Issues（备注）

### 问题 6：S4.1 Intent 路由 "spawn + 等待就绪" 机制未详细说明

**严重度**：P2

**问题描述**：S4.1.2 说"目标 Agent 未运行时拉起"，S4.1.3 说"同步 Intent 超时处理（默认 30s）"，但：
- "等待就绪"是什么机制？轮询 health check？Socket 连接建立？
- 如果目标 Agent 需要冷启动（加载 Grafeo、连接 LLM），30s 是否够？
- S3.3 的"冷启动身份注入"和 S4.1 的"spawn on Intent" 是否共享同一套拉起逻辑？

**建议**：S4.1 增加子任务说明 spawn-on-Intent 的就绪判断机制。

---

### 问题 7：测试数量估算偏低

**严重度**：P3

**问题描述**：plan-p2.md §3 说"预期 219+ 测试"，但 Phase 1 实际 281 tests。plan 的测试估算基数偏低。

**建议**：更新预期测试数为 250+。

---

## 五、建议修正汇总

| # | 严重度 | 问题 | 位置 | 建议 |
|---|--------|------|------|------|
| A | **P0** | MemoryStore trait 与 Phase 2 设计不匹配 | S2.1 前缺失 | **新增 S2.0** 重设计 trait |
| B | **P1** | MemoryManager 归属不清 | S2.9 | 明确放在 rollball-runtime |
| 1 | P2 | HNSW 缺少 embedding 持久化/加载 | S2.4 | 增加 S2.4.5, S2.4.6 |
| 2 | P2 | Decay 硬编码阈值 | S2.7 | 改为 DecayConfig 参数化 |
| 3 | P2 | 缺"零 LLM 调用"约束 | S2.2.1 | 验收标准加此条 |
| 4 | P2 | 远程 Embedding 降级未指定 | S5.3.4 | 明确降级 API |
| 5 | P2 | Token 计数 CJK 与文档不一致 | S5.4.3 | 统一估算方式 |
| 6 | P2 | spawn-on-Intent 就绪机制未说明 | S4.1 | 增加子任务说明 |
| 7 | P3 | 测试数量估算偏低 | §3 | 更新为 250+ |

---

## 六、审查结论

**plan-p2.md 整体框架良好，但存在 2 个必须修正的架构级问题**。

### 必须修正后才能开始 Phase 2：
1. **新增 S2.0**：MemoryStore trait 重设计（问题 A）
2. **明确 MemoryManager 归属**（问题 B）

### 建议在开始前澄清或修正：
- S2.4 增加 embedding 持久化子任务（问题 1）
- S2.7 改为 DecayConfig 参数化（问题 2）
- S2.2.1 验收标准加"零 LLM 调用"（问题 3）

### 可以边实施边修正：
- S5.3.4 明确降级 API（问题 4）
- S5.4.3 统一 token 估算（问题 5）
- S4.1 说明 spawn-on-Intent 机制（问题 6）
- 测试数量估算（问题 7）

**下一步行动**：补充 S2.0 和明确 MemoryManager 归属后，plan-p2.md 可作为 Phase 2 实施依据。