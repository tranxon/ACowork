# Grafeo 设计实现 Review

**审查日期**：2026-04-22
**审查范围**：`docs/module-design/04-grafeo.md`、`docs/05-memory.md`、`grafeo/`（v0.5.40）、`grafeo/docs/ecosystem/grafeo-memory.md`
**核心问题**：Rollball 的 Grafeo 使用方式与 Grafeo 实际能力存在系统性偏差
**状态**：方向已确认（引入 grafeo-engine），待执行集成规划

---

## 1. 核心发现：命名与实现的根本性错位

### 1.1 Grafeo 是什么

Grafeo 是一个**完整的 Rust 图数据库**（v0.5.40），不是 SQLite wrapper，也不是概念名称：

- **数据模型**：LPG（标签属性图）+ RDF，支持 GQL / Cypher / Gremlin / SPARQL / GraphQL / SQL-PGQ 六种查询语言
- **核心架构**：MVCC 事务 → Parser → Binder → Optimizer（CBO/DPccp）→ Executor（推式向量化 + Morsel 并行）→ Storage
- **索引能力**：HNSW 向量索引（内嵌）、BM25 全文索引（内嵌）、混合搜索 RRF（内嵌）
- **图算法**：PageRank、SSSP、中心性、社区检测（内嵌过程）
- **性能目标**：1M 节点/秒插入，< 1μs 点查询，< 10μs 1 跳遍历
- **生态**：grafeo-memory（AI 记忆层）、grafeo-mcp（MCP 服务）、grafeo-langchain（LangChain 集成）、grafeo-server（独立服务器）

### 1.2 Rollball 实际在用什么

`rollball-grafeo/Cargo.toml` 依赖：

```toml
rusqlite.workspace = true      # SQLite，非 Grafeo 数据库
```

实际实现（`grafeo.rs`）：
```rust
pub struct Grafeo {
    db_path: PathBuf,
    // TODO: Add graph database connection
}

pub fn open(_db_path: &PathBuf) -> Result<Self> {
    // TODO: Initialize SQLite connection (Phase 1 mock)
    unimplemented!()
}
```

**结论**：设计文档叫 Grafeo，依赖的是 rusqlite，实现是 TODO。Grafeo 项目源码躺在 `grafeo/` 子模块里，完全没有被引入。

---

## 2. 未利用的 Grafeo 核心特性

以下能力已在 Grafeo 中实现，Rollball 在重新发明轮子：

### 2.1 向量搜索（HNSW）

| 维度 | Rollball 当前设计 | Grafeo 原生能力 |
|------|------------------|----------------|
| 索引类型 | 自己实现 HNSW（M=16, ef=100/64） | 内嵌 HNSW，M/ef/beam_width 全可配置 |
| 距离函数 | 余弦相似度 | 余弦/欧几里得/点积/曼哈顿，SIMD 加速 |
| 搜索参数 | 硬编码参数 | 运行时可调 |
| 实现位置 | `vector/hnsw.rs`（自研） | `grafeo-core/index/vector/` |

**建议**：直接用 Grafeo 的 `HnswIndex`，删除 `vector/hnsw.rs` 自研实现。

### 2.2 全文搜索（BM25）

| 维度 | Rollball 当前设计 | Grafeo 原生能力 |
|------|------------------|----------------|
| 索引类型 | rusqlite FTS5 | 内嵌 BM25 倒排索引 |
| 分词器 | 无说明 | Unicode 分词器内置 |
| 实现位置 | `fulltext/bm25.rs`（自研） | `grafeo-core/index/text/` |

**建议**：Grafeo BM25 原生支持，直接用。

### 2.3 混合搜索 + RRF

| 维度 | Rollball 当前设计 | Grafeo 原生能力 |
|------|------------------|----------------|
| 融合方式 | 自研 RRF | 内嵌 RRF + 加权融合 |
| 搜索类型 | 向量 + BM25 | 向量 + BM25 + 图拓扑增强 |
| 特色 | 无 | `Topology boost`（图连通性重排序）|

**关键缺失**：`grafeo-memory` 文档明确提到 `topology_boost`——搜索结果按图连通性重新排序。这是 Grafeo 图数据库的独特优势，Rollball 完全未使用。

**建议**：引入 Grafeo 的 topology boost 作为 Rollball 检索排序的一个维度。

### 2.4 图遍历与关联扩散

| 维度 | Rollball 当前设计 | Grafeo 原生能力 |
|------|------------------|----------------|
| 图查询 | 自研 SQL 实现 | 原生 LPG，GQL/Cypher 查询 |
| 边遍历 | `memory_edges` 表 + SQL | 邻接索引，O(degree) 遍历 |
| 跳数限制 | 自研 3 跳 + 早期终止 | Grafeo 执行器原生支持限制和早期终止 |
| 优化器 | 无 | 谓词下推、DPccp 连接排序、基数估计 |

Rollball 的 `graph_expand` 是用 SQL 在关系表上模拟图遍历。Grafeo 的图遍历是**数据库原生操作**，有查询优化器支持。

### 2.5 MVCC 事务与 WAL

| 维度 | Rollball 当前设计 | Grafeo 原生能力 |
|------|------------------|----------------|
| 事务模型 | rusqlite 单写 | MVCC 快照隔离，原生多版本 |
| WAL | rusqlite WAL | Grafeo WAL（`grafeo-adapters/storage/wal/`） |
| 恢复 | 自研 `recovery.rs` | WAL 重放，崩溃恢复内置 |
| 备份 | 自研 `backup.rs` | grafeo-cli backup 命令 |

Rollball 重新实现了 Grafeo 原生提供的持久化机制。

### 2.6 并行执行

| 维度 | Rollball 当前设计 | Grafeo 原身能力 |
|------|------------------|----------------|
| 并行模型 | 无（单线程） | Morsel 驱动并行，自动检测线程数 |
| 向量化 | 无 | 推式向量化执行，~1024 行/批次，SIMD |
| 溢出处理 | 无 | 透明磁盘溢出（Spill Manager） |

---

## 3. grafeo-memory：与 Rollball 设计高度重叠

这是最需要认真对待的部分。Grafeo 官方提供了 `grafeo-memory`（AI 记忆层），其架构与 Rollball 的 GrafeoMemoryManager **惊人地相似**：

### 3.1 核心循环对比

```
grafeo-memory:
  Extract → Search → Reconcile → Execute

Rollball (05-memory.md §6):
  memory_store → hybrid_search → LLM判断 → store
```

两者本质上是同一套哲学："用 LLM 做语义判断，存储在图里"。

### 3.2 功能对比

| 功能 | grafeo-memory | Rollball | 差距 |
|------|--------------|----------|------|
| 事实提取（LLM） | ✅ pydantic-ai Agent | ✅ memory_store tool | 同 |
| 向量语义搜索 | ✅ HNSW | ✅ hybrid_search | 同 |
| 图结构（节点+边） | ✅ Memory/Entity/HAS_ENTITY/RELATION | ✅ memory_nodes/memory_edges | 同 |
| 冲突检测与调解 | ✅ reconciliation（ADD/UPDATE/DELETE/NONE） | ✅ 冲突处理（evolution/correction/ambiguous） | 同 |
| 记忆摘要 | ✅ summarize() | ✅ 离线巩固（Phase 3） | 同期 |
| 程序记忆 | ✅ procedural memory | ✅ ProceduralNode | 同 |
| 情景记忆 | ✅ episodic memory | ✅ Episode | 同 |
| 重要性评分 | ✅ importance scoring | ✅ importance + decay | 同 |
| 图连通性排序 | ✅ topology_boost（opt-in） | ❌ 未使用 | **关键缺失** |
| MMR 多样性搜索 | ✅ MMR search | ❌ 未使用 | 缺失 |
| MCP 暴露 | ✅ grafeo-memory-mcp 内嵌 | ❌ 未使用 | 缺失 |
| 多用户隔离 | ✅ user_id 隔离 | ✅ Agent 私有 Grafeo | 同 |
| 变化历史 | ✅ history() API | ❌ 未实现 | 缺失 |
| LLM 提供者 | ✅ pydantic-ai（多提供者） | ✅ 通用 LLM | 同 |

### 3.3 Rollball 与 grafeo-memory 的取舍建议

Rollball 面临两个选择：

**路径 A（深度集成 Grafeo）**：直接使用 grafeo-memory 作为记忆层，Rollball 只需做少量 wrapper 和扩展：
- 优点：免费获得所有功能，复用成熟实现，Grafeo 团队维护
- 缺点：依赖 Python生态（grafeo-memory 是 Python 库），Rollball 是 Rust 项目

**路径 B（Rust-first 集成 Grafeo）**：用 Rust 调用 Grafeo 数据库（`grafeo-engine`），在 Rollball 的 MemoryManager 层重新实现记忆逻辑：
- 优点：Rust 原生，无 Python 依赖，性能可控
- 缺点：需要自己实现 grafeo-memory 的语义提取、调解、摘要等功能

**路径 C（混合，当前设计）**：继续 rusqlite + 自研，但这放弃了 Grafeo 的核心价值

**建议**：路径 B 是最合理的——用 Grafeo 数据库的能力（向量/HNSW、BM25、图遍历、WAL、MVCC），在 Rust 层实现记忆语义。这比 rusqlite 好，但需要放弃"从零实现所有记忆功能"的野心。

---

## 4. 未使用的 Grafeo 特色能力

### 4.1 图算法

Grafeo 内嵌了图算法过程，Rollball 完全未提及：

- **PageRank**：用于评估记忆节点的重要性（补充 importance_score）
- **社区检测**（Louvain）：识别记忆群组，发现隐性知识结构
- **最短路径**：追踪两个记忆之间的关联链路
- **中心性**：识别"枢纽记忆"（被大量边引用的节点）

这些可以增强 graph_expand 的语义质量。

### 4.2 MCP 集成

`grafeo-mcp` 将 Grafeo 数据库暴露给 AI Agent。如果 Rollball 用 Grafeo 作为存储，MCP 服务可以直接复用。

### 4.3 多查询语言

Grafeo 支持 GQL/Cypher 等多种查询语言。Rollball 的检索完全依赖 SQL-Like 操作，GQL 查询能力未使用。

---

## 5. 问题汇总

| 编号 | 问题 | 级别 | 说明 |
|------|------|------|------|
| 1 | rollball-grafeo 依赖 rusqlite 而非 Grafeo 数据库本体 | P0 | 设计叫 Grafeo，用的是 SQLite |
| 2 | 自研 HNSW 实现与 Grafeo 内嵌 HNSW 重复 | P1 | Grafeo `grafeo-core/index/vector/` 已有 |
| 3 | 自研 BM25 与 Grafeo 内嵌 BM25 重复 | P1 | Grafeo `grafeo-core/index/text/` 已有 |
| 4 | 自研 RRF 与 Grafeo 内嵌 RRF 重复 | P1 | Grafeo hybrid-search 已有，含 topology boost |
| 5 | 图遍历用 SQL 模拟，未使用 Grafeo 原生 LPG | P1 | Grafeo 的图查询有优化器支持 |
| 6 | MVCC 和 WAL 自研，Grafeo 原生支持未用 | P1 | 重复实现，且自研方案不如原生完善 |
| 7 | grafeo-memory 与 Rollball 设计高度重叠但未参考 | P1 | grafeo-memory 的 extract/reconcile 模式已成熟 |
| 8 | topology_boost 未使用 | P2 | Grafeo 独特优势，图连通性重排序 |
| 9 | MMR 多样性搜索未实现 | P2 | grafeo-memory 有，Rollball 无 |
| 10 | history() 变化历史 API 未实现 | P2 | grafeo-memory 有 |
| 11 | 图算法（PageRank/社区检测）未使用 | P2 | Grafeo 内嵌，Rollball 未提及 |
| 12 | MCP 集成未规划 | P2 | grafeo-mcp 可复用 |
| 13 | 实现是 TODO stub | P0 | `grafeo.rs` 里 unimplemented!() |

---

## 6. 建议

### 6.1 根本决策（先确认方向）

Rollball 的 Grafeo 层有三条路：

1. **继续 rusqlite 路线**：放弃"Grafeo"这个名字和 Grafeo 项目，改名为 `rollball-sqlite`，专注实现记忆逻辑。这条路不差，但命名有误导。

2. **引入 grafeo-engine（Rust）**：放弃 rusqlite，用 Grafeo 数据库作为存储后端。这能获得所有 Grafeo 特性，但需要大幅重构 MemoryStore trait 和 rollball-grafeo 模块。

3. **参考 grafeo-memory 重新设计**：如果走路线 2，MemoryManager 的设计应大量参考 grafeo-memory 的 API（add/search/reconcile/summarize/history）和模式，而不是另起炉灶。

**建议走路线 2**，理由：
- Grafeo 的存储能力（向量/HNSW、BM25、图遍历、WAL、MVCC）远超 rusqlite
- Grafeo 的性能目标（1M 节点/秒插入）与 Rollball 的资源约束匹配
- Rust 原生集成，无生态依赖

### 6.2 立即可做的改动

**不依赖方向决策，独立可做**：

- 将 `04-grafeo.md` 中自研的 `vector/hnsw.rs`、`fulltext/bm25.rs` 标注为"参照 Grafeo 原生实现预留占位，后续集成时删除"
- 引入 `grafeo-mcp` 的设计参考，让记忆 API 可以通过 MCP 暴露给 Agent
- 参考 grafeo-memory 的 `topology_boost` 设计，在 graph_expand 中加入图连通性权重

**方向确认后需要做的重构**：

- 替换 rusqlite → grafeo-engine
- 统一 memory_nodes/memory_edges → Grafeo LPG 模型
- 删除重复实现（HNSW、BM25、RRF、WAL）
- MemoryStore trait 适配 Grafeo 的事务模型

---

## 7. 方向决策

**决策**：✅ 引入 `grafeo-engine`，弃用 rusqlite。

**理由**：
- Grafeo 原生支持 HNSW / BM25 / RRF / WAL / LPG 图查询，无需重复实现
- `grafeo-memory` 与 Rollball 记忆管理架构高度重叠，可参考设计思路
- grafeo-engine 是纯 Rust crate，无外部依赖，可直接引入 Cargo.toml
- 当前 rusqlite 实现为 `unimplemented!()`，迁移成本极低

**弃用组件**：
- `rollball-grafeo/Cargo.toml` → 删除 `rusqlite`，引入 `grafeo-engine`
- `rollball-grafeo/src/grafeo.rs` → 基于 grafeo-engine 重新实现
- 所有自研 HNSW / BM25 / RRF 代码 → 删除，复用 Grafeo 原生能力

**待办（依赖决策确认后可启动）**：
1. 确定 grafeo-engine feature flags 配置（vector-index? text-index? wal? parallel?）
2. 设计 grafeo-engine 与 MemoryStore trait 的适配层
3. 参考 grafeo-memory 架构优化 Rollball MemoryManager 层
4. 评估 PageRank / 社区检测等图算法集成点
