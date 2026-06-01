# P2 Grafeo/Memory 代码审查报告

**审查日期**：2026-04-24
**审查范围**：`rollball-grafeo/`（32 文件，~290KB）、`rollball-core/src/memory/`、`rollball-runtime/src/memory/`
**对比基线**：设计文档 `docs/05-memory.md`、`docs/module-design/04-grafeo.md`
**前置审查**：`06-grafeo-design-review.md`（方向决策已确认：引入 grafeo-engine）

---

## 1. 总体评价

代码质量**远超预期**。对比 Phase 1 的 `unimplemented!()` 状态，Phase 2 已完成：

- GrafeoStore 基于 `grafeo_engine::GrafeoDB` 的完整集成（不再是 rusqlite）
- 三层五类型记忆模型（Episode、Knowledge、Procedural、Autobiographical、ArtifactRef）
- HNSW 向量搜索 + BM25 全文搜索 + RRF 混合搜索 + MMR 多样性搜索
- 图扩散检索（BFS + 评分衰减 + 早期终止）
- PageRank + 社区检测（Louvain/标签传播 fallback）
- 遗忘机制（指数衰减 + 访问提升 + 休眠转换）
- 冲突检测（三层：语义/时序/否定词）+ 自动解决
- 知识巩固管线（即时提取 + 离线巩固 + 模糊处理）
- Abstention 机制、Purge 日志（30 天恢复窗口）
- 容量管理、健康检查、SLA 监控
- 每个模块附带单元测试

但仍存在若干值得关注的问题，以下分级列出。

---

## 2. P0 级问题（必须修复）

### P0-1：MemoryStore trait 与 GrafeoStore 严重脱节

`rollball-core/src/memory/traits.rs` 定义了 `MemoryStore` trait：

```rust
pub struct MemoryNode {
    pub id: String,
    pub content: String,
    pub metadata: Value,
    pub zone: String,
    pub privacy_level: PrivacyLevel,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store(&self, node: MemoryNode) -> Result<()>;
    async fn retrieve(&self, id: &str) -> Result<Option<MemoryNode>>;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryNode>>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn list_by_zone(&self, zone: &str) -> Result<Vec<MemoryNode>>;
}
```

而 `GrafeoStore` 是同步 API，不实现此 trait，且数据模型完全不同：

- `MemoryNode` 用 `String` id，Grafeo 用 `NodeId`（u64）
- `MemoryNode` 只有 `content + metadata`，Grafeo 有类型化的 `Episode`/`KnowledgeNode`/`ProceduralNode`/`AutobiographicalNode`
- `MemoryStore::search` 是简单关键词搜索，Grafeo 有向量/文本/混合/图扩散
- `MemoryNode.zone` 和 `PrivacyLevel` 在 GrafeoStore 层不存在
- `MemoryStore` 是 async，`GrafeoStore` 全是同步

**影响**：Runtime 的 `MemoryManager` 绕过 `MemoryStore` trait，直接依赖 `GrafeoStore`。这违反了抽象层设计——如果未来需要切换存储后端，所有 Runtime 代码都要改。

**建议**：重新设计 `MemoryStore` trait，使其匹配 Grafeo 的实际能力，或在 GrafeoStore 之上建一个适配器实现 `MemoryStore` trait。

### P0-2：GQL 注入风险

`episodic/store.rs` 的 `find_or_create_session` 和 `retrieval.rs` 的 `graph_expand_simple`：

```rust
// episodic/store.rs:216
let gql = format!(
    "MATCH (s:Session) WHERE s.session_id = '{}' RETURN id(s)",
    crate::episodic::escape_gql_string(session_id)
);

// retrieval.rs:117
let gql = format!(
    "MATCH (m)-[r*1..{}]-(other) WHERE id(m) = {} RETURN DISTINCT id(other)",
    max_hops,
    start_id.as_u64()
);
```

`escape_gql_string` 只转义 `\` 和 `'`。如果 Grafeo GQL 支持双引号字符串、注释、分号等，该转义不够。`start_id.as_u64()` 是数字，这里安全，但模式不好。

**影响**：Agent 通过 session_id 可以注入 GQL 语句，修改/读取其他 Agent 的数据。

**建议**：改用参数化查询（如果 grafeo-engine 支持），或至少使用完整的 GQL 转义（双引号、分号、注释符）。

### P0-3：GrafeoStore 非线程安全

`GrafeoStore` 持有 `GrafeoDB` 实例，所有方法都是 `&self`（共享引用）。但 GrafeoDB 的方法是否线程安全取决于其内部实现。

```rust
pub struct GrafeoStore {
    pub(crate) db: GrafeoDB,
    hnsw_config: HnswConfig,
}
```

如果 `GrafeoDB` 不是 `Sync`，在 Runtime 多任务并发访问时会 UB。如果是 `Sync`，应在 struct 上显式标注或文档说明。

**影响**：Runtime 的 `MemoryManager` 可能被多个 tokio 任务并发调用。

**建议**：验证 `GrafeoDB` 是否 `Sync`。如果否，需要加 `Mutex` 或 `RwLock`。如果是，添加 `unsafe impl Sync for GrafeoStore` 并注释安全论证。

---

## 3. P1 级问题（应该修复）

### P1-1：hybrid_search_full 的权重调整逻辑错误

`retrieval.rs` 的 `hybrid_search_full` 和 `hybrid_search_weighted`：

```rust
// retrieval.rs:247
let weight_factor = f64::from((text_weight + vector_weight) / 2.0);
for (_, score) in &mut results {
    *score *= weight_factor;
}
```

对 RRF 融合后的分数做 `(text_weight + vector_weight) / 2` 的均匀缩放，这不正确：

- 如果 `text_weight=0.9, vector_weight=0.1`，`weight_factor=0.5`，RRF 分数被砍半，但文本和向量的相对权重并未改变——RRF 融合内部已经完成了排名融合
- 正确做法：**在 RRF 融合前**分别对文本排名和向量排名应用权重，或使用 Grafeo 内置的 `hybrid_search` 权重参数

**建议**：要么传入 `text_weight`/`vector_weight` 到 `GrafeoDB::hybrid_search`（如果其 API 支持权重），要么在 RRF 融合阶段自行实现加权融合，而不是对融合后分数做无意义的缩放。

### P1-2：graph_expand 的 BFS 内循环 break 不完整

`spreading.rs:198`：

```rust
for (neighbor_id, edge_id) in edge_refs {
    // ...
    if results.len() >= config.max_total_nodes {
        break;  // 只跳出内循环，外循环 while 可能继续
    }
    queue.push_back((neighbor_id, next_hop, accumulated_score, new_path));
}

// 外循环的 while 里也有检查：
if results.len() >= config.max_total_nodes {
    break;
}
```

虽然外循环有检查，但内循环 break 后当前节点的其余边被跳过——可能导致结果不完整（某些高分邻居被遗漏）。更重要的是，BFS 应该在添加到 queue 时做容量检查，而不是在弹出时。

**建议**：将容量检查移到 `queue.push_back` 之前，而不是在 `results.push` 之后。

### P1-3：consolidation/instant.rs 和 ambiguous.rs 缺乏 GrafeoStore 集成

审查了 `consolidation/mod.rs` 的导出，instant 和 ambiguous 模块定义了 `MemoryStoreInput`、`ConflictCandidate`、`AmbiguousConflict` 等数据结构，但这些模块没有 `impl GrafeoStore` 的方法——巩固管线的入口在哪？

对比 `episodic/store.rs` 有 `impl GrafeoStore`、`forgetting/scan.rs` 有 `impl GrafeoStore`，但 consolidation 三个子模块似乎没有绑定到 `GrafeoStore`。

**影响**：巩固管线是纯数据结构 + 逻辑，缺少与 GrafeoStore 的桥接。调用方（MemoryManager 或 Runtime）需要自己组装管线。

**建议**：至少为 `GrafeoStore` 添加 `process_instant_extraction` 和 `run_offline_consolidation` 方法，封装完整的巩固流程。

### P1-4：rollball-memory crate 的公共 API 过于简陋

`rollball-memory` crate 暴露了 `ConflictSignal`、`ConflictType`、`RetrievalMetrics`、`MemoryQuery` 等类型，但没有文档说明这些类型之间的关系和使用场景。

```rust
// lib.rs 的 pub use
pub use rollball_memory::{ConflictSignal, ConflictType};
pub use rollball_memory::{MemoryQuery, RetrievalMetrics};
```

**建议**：为 `rollball-memory` 添加 `lib.rs` 的模块级文档，说明各类型在记忆生命周期中的角色。

### P1-5：Episodic search 的 GQL 查询可能返回大量结果

`episodic/search.rs` 使用 GQL 查询来搜索 Episodic 节点。如果图中有大量 Episode 节点（设计上限 100,000），全表扫描会非常慢。

**建议**：确保搜索走 HNSW 或 BM25 索引，而不是 GQL 全表扫描。如果是 GQL 过滤（如按 session_id），需要确保有对应的索引。

---

## 4. P2 级问题（建议改进）

### P2-1：cosine distance → similarity 转换散落在多处

`retrieval.rs:197` 和 `spreading.rs:248` 都有相同的转换公式：

```rust
let similarity = (2.0 - f64::from(dist)) / 2.0;
```

这假设 cosine distance ∈ [0, 2]，但有些 Grafeo 配置可能返回 [0, 1] 的余弦相似度而非距离。

**建议**：抽取为公共函数 `cosine_distance_to_similarity(dist: f64) -> f64`，并文档说明假设。

### P2-2：DecayConfig 的 `scan_interval_hours` 未使用

`DecayConfig` 有 `scan_interval_hours: u64` 字段，但 `run_decay_scan` 不使用它——调用方决定何时调用。这个字段是给定时器用的，但 GrafeoStore 没有后台任务。

**建议**：删除此字段，或在 GrafeoStore 层面提供 `start_decay_background_task` 方法。

### P2-3：~~Negation keywords 硬编码中英双语~~ ✅ 已在 v3.8 解决

`conflict.rs` 的 `NEGATION_KEYWORDS` 和 `EVOLUTION_KEYWORDS` 硬编码了中英文关键词。不支持日韩法等其他语言。

**v3.8 解决方案**：已完全移除关键词匹配层（Layer 3）和启发式快速路径。所有冲突统一标记 Ambiguous，交由 Phase 3 LLM 离线仲裁，从根源上解决了多语言覆盖问题。

### P2-4：PageRank fallback 的 O(V^2) 遍历效率

`compute_pagerank_fallback` 每次迭代遍历所有节点，对 100K 节点的图来说会很慢。

**建议**：在 `compute_pagerank` 中添加 `max_nodes` 参数，超过阈值时拒绝计算或采样。同时考虑增量 PageRank。

### P2-5：compress_artifact_content 的 UTF-8 截断可能 panic

```rust
pub fn compress_artifact_content(content: &str) -> (String, Vec<ArtifactRef>) {
    let summary = if content.len() > 200 {
        format!("{}...", &content[..200])  // 可能在多字节字符中间截断
    } else {
        content.to_string()
    };
```

`&content[..200]` 按 byte 截断，如果第 200 字节在 UTF-8 多字节字符中间，会 panic。

**建议**：使用 `char_indices` 或 `.floor_char_boundary(200)` 做安全截断。

### P2-6：测试中缺少并发安全测试

所有测试都是单线程顺序执行。没有测试并发读写、并发搜索+写入等场景。

**建议**：添加并发测试（`#[tokio::test]` + 多 task 并发调用 GrafeoStore）。

---

## 5. 架构层面观察

### 5.1 GrafeoStore 的 "God Object" 倾向

`GrafeoStore` 通过 `impl` 块散布在 8+ 个文件中（grafeo.rs、graph.rs、retrieval.rs、spreading.rs、episodic/store.rs、episodic/search.rs、forgetting/scan.rs、engineering.rs）。这是 Rust 的 extension trait 模式，但导致：

- 读代码时需要跳转多个文件才能理解 GrafeoStore 的完整 API
- 所有功能都通过 `&self`（共享引用）调用，没有访问控制
- 任何模块都可以给 GrafeoStore 添加方法

**建议**：考虑将 GrafeoStore 拆分为多个 facade（如 `EpisodicStore`、`SemanticStore`、`RetrievalEngine`），通过组合而非 extension 实现。

### 5.2 缺少 GrafeoStore → MemoryStore trait 适配

如 P0-1 所述，`MemoryStore` trait 与 GrafeoStore 完全脱节。建议的修复方案：

```rust
pub struct GrafeoMemoryAdapter {
    store: GrafeoStore,
}

#[async_trait]
impl MemoryStore for GrafeoMemoryAdapter {
    async fn store(&self, node: MemoryNode) -> Result<()> {
        // 将 MemoryNode 转为 Episode 或 KnowledgeNode 并调用 GrafeoStore
    }
    // ...
}
```

### 5.3 rollball-grafeo 对 grafeo-engine 的依赖范围过大

`Cargo.toml` 同时依赖 `grafeo-engine` 和 `grafeo-core` 和 `grafeo-common`。如果 Grafeo 团队升级了 `grafeo-core` 的内部 API，Rollball 可能意外破坏。

**建议**：只依赖 `grafeo-engine`（公共 API），避免直接使用 `grafeo_core::graph::*`。如果必须用，添加版本锁定注释。

---

## 6. 与设计文档的一致性

| 设计规格 | 代码实现状态 | 备注 |
|---------|------------|------|
| 三层五类型 | ✅ 完整 | Episode/Knowledge/Procedural/Autobiographical/ArtifactRef |
| 遗忘机制 | ✅ 完整 | 指数衰减 + 访问提升 + 休眠转换 |
| 冲突检测 | ✅ 完整 | 三层检测 + 自动解决 + 模糊升级 |
| 即时巩固 | ⚠️ 部分 | 数据结构和逻辑有了，但缺少 GrafeoStore 集成 |
| 离线巩固 | ⚠️ 部分 | 同上 |
| Abstention | ✅ 完整 | 分级阈值 + prompt 注入 |
| Purge 日志 | ✅ 完整 | 30 天恢复窗口 + 三条清理路径 |
| graph_expand | ✅ 超出 | BFS + 评分 + 早期终止 + PageRank + 社区检测 |
| hybrid_search | ⚠️ 有缺陷 | 权重调整逻辑不正确 |
| PrivacyLevel | ❌ 缺失 | GrafeoStore 不支持 zone/PrivacyLevel 过滤 |
| memory_store tool | ❌ 缺失 | LLM 工具层的 memory_store 尚未实现 |

---

## 7. 修复优先级建议

| 优先级 | 编号 | 工作量 | 说明 |
|-------|------|-------|------|
| 紧急 | P0-1 | 3d | 重做 MemoryStore trait 或建适配器 |
| 紧急 | P0-2 | 0.5d | 参数化 GQL 查询或加固转义 |
| 紧急 | P0-3 | 1d | 验证 GrafeoDB Sync 安全性 |
| 高 | P1-1 | 1d | 修正 hybrid search 权重逻辑 |
| 高 | P1-2 | 0.5d | 修正 graph_expand 容量检查位置 |
| 高 | P1-3 | 2d | 为 consolidation 添加 GrafeoStore 集成 |
| 高 | P1-4 | 0.5d | 为 rollball-memory 添加文档 |
| 高 | P1-5 | 1d | 确保搜索走索引 |
| 中 | P2-1~6 | 2d | 代码质量改进 |

**总工作量估计**：约 12 人天

---

## 8. 结论

Phase 2 的 Grafeo/Memory 实现从 0 到 1 完成了非常扎实的核心功能，特别是图扩散、遗忘、冲突检测等模块质量很高。主要问题集中在**接口层**：MemoryStore trait 与 GrafeoStore 的脱节、GQL 注入风险、并发安全未验证。修复这些 P0 问题后，代码即可进入集成测试阶段。
