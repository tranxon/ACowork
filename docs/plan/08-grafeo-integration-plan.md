# Grafeo 集成规划

**状态**：已确认引入 `grafeo-engine`，待执行
**文件**：docs/plan/08-grafeo-integration-plan.md
**依赖**：docs/review/06-grafeo-design-review.md

---

## 1. 背景

06-grafeo-design-review.md 审查发现：rollball-grafeo 依赖 `rusqlite` 而非 Grafeo 数据库本体，`grafeo.rs` 全部为 `unimplemented!()`。

**用户确认决策**：
1. **grafeo-engine**：使用 crates.io 稳定版本，**不是本地 path 依赖**
2. **grafeo-memory**：作为设计参考（Extract/Reconcile 模式），**不是编译依赖**
3. **rollball-memory 改造**：参考 grafeo-memory 的 extract/reconcile 架构，改造 rollball-memory 的业务逻辑层

---

## 2. grafeo-engine 能力总览

### 2.1 核心定位

`grafeo-engine` 是 Grafeo 的 Rust 数据库引擎（v0.5.40），纯 Rust crate，无外部服务依赖，可直接通过 Cargo 引入。

```
入口类型：GrafeoDB
并发模型：Session（轻量 handle，支持多 Session 并发）
事务模型：MVCC 快照隔离
```

### 2.2 Feature Flags

按 Rollball 需求分级：

| Feature Flag | 作用 | Rollball 需求程度 |
|---|---|---|
| `lpg` | 标签属性图模型（核心） | **必须** |
| `vector-index` | HNSW 向量索引 | **必须** |
| `text-index` | BM25 全文索引 | **必须** |
| `hybrid-search` | 混合搜索（RRF 融合） | **必须** |
| `gql` | GQL 查询语言（ISO 39075:2024） | **必须** |
| `wal` | WAL 写前日志，崩溃恢复 | **必须** |
| `grafeo-file` | 单文件 `.grafeo` 持久化格式 | **必须** |
| `algos` | 图算法（PageRank / 社区检测 / 最短路径） | 推荐 |
| `cdc` | 变更数据捕获，history API | 推荐 |
| `temporal` | 时间版本化属性 | 可选 |
| `embed` | ONNX embedding 生成（+17MB） | 可选（LLM 调用方提供 embedding） |
| `encryption` | AES-256-GCM 静态加密 | 可选（Vault 已做密钥管理） |
| `parallel` | 并行查询执行（rayon） | 推荐 |
| `spill` / `mmap` | 磁盘溢出 / 内存映射 | 未来可选 |

**Rollball 推荐 feature 配置**（`core/rollball-grafeo/Cargo.toml`）：

```toml
# 使用 crates.io 稳定版本
grafeo-engine = { version = "0.5", features = [
    "lpg",
    "gql",
    "vector-index",
    "text-index",
    "hybrid-search",
    "wal",
    "grafeo-file",
    "algos",
    "cdc",
    "parallel",
] }
```

### 2.3 核心 API 模式

GrafeoDB 的基本使用模式：

```rust
use grafeo_engine::{GrafeoDB, Config};

// 内存数据库
let db = GrafeoDB::new_in_memory();

// 持久化数据库（单文件 .grafeo）
let db = GrafeoDB::open("agent_memory.grafeo").unwrap();

// 多 Session 并发访问
let mut session = db.session();
session.begin_transaction().unwrap();
session.execute("CREATE (:MemoryNode {node_id: $id, content: $c})").unwrap();
session.commit().unwrap();

// 向量索引
db.create_vector_index("MemoryNode", "embedding", Some(1536), Some("cosine"), None, None, None)?;
db.vector_search("MemoryNode", "embedding", &[0.1, 0.2, ...], 5, Some(50), None)?;

// BM25 全文索引
db.create_text_index("MemoryNode", "content")?;
db.text_search("MemoryNode", "content", "用户偏好", 10, None)?;

// 混合搜索
db.hybrid_search("MemoryNode", "content", "embedding", "用户设置", &[0.1, ...], 5, None)?;

// 图遍历（GQL）
let result = session.execute(
    "MATCH (m:MemoryNode)<-[:REFERENCES]-(related:MemoryNode) WHERE m.node_id = $id RETURN related"
).unwrap();

// 图算法（通过 CALL 过程）
let pagerank = session.execute("CALL grafeo.pagerank() YIELD node_id, score RETURN node_id, score ORDER BY score DESC").unwrap();
let shortest = session.execute("CALL grafeo.shortest_path($a, $b)").unwrap();

// CDC / 历史
let history = db.history(node_id).unwrap();  // 需要 cdc feature

// Time-travel 查询
let old_state = session.execute_at_epoch("MATCH (m) RETURN m", old_epoch).unwrap();
```

---

## 3. 能力映射：Rollball Memory → Grafeo 原生特性

### 3.1 节点模型映射

| Rollball 概念 | Grafeo 实现 | Label |
|---|---|---|
| AutobiographicalNode | Node | `Autobiographical` |
| EpisodicNode | Node | `Episodic` |
| KnowledgeNode | Node | `Knowledge` |
| GrafeoConfig / SystemConfig | Node + CDC history | `SystemConfig` |
| 工具调用记录 | Node | `ToolInvocation` |
| SessionHistory | Edge + temporal | `(:Session)-[:HAS_MEMORY]->(:MemoryNode)` |
| 记忆引用关系 | Edge | `(:MemoryNode)-[:REFERENCES]->(:MemoryNode)` |
| 自我引用（身份） | Edge | `(:MemoryNode)-[:SELF_REFERENCES]->(:MemoryNode)` |
| 工具→记忆 | Edge | `(:ToolInvocation)-[:PRODUCED]->(:MemoryNode)` |

Rollball 的"六层记忆类型"直接映射为 Grafeo 的 **Label**，利用 Grafeo 的 Label 隔离实现类型区分，无需额外的 `node_type` 枚举字段。

### 3.2 检索能力映射

| Rollball 设计 | Grafeo 原生实现 |
|---|---|
| 语义检索（HNSW 自研） | `db.create_vector_index()` + `db.vector_search()` |
| 关键词检索（BM25 自研） | `db.create_text_index()` + `db.text_search()` |
| 混合检索（RRF 自研） | `db.hybrid_search()` — 内置 RRF 融合 |
| MMR 去重 | `db.mmr_search()` |
| 图扩展（SQL 模拟） | GQL `MATCH (m)-[r*1..3]-(other)` 原生图遍历 |
| 图折叠（降维） | GQL 聚合查询 |
| 跨 Agent 消息关联 | GQL 多跳模式匹配 |

**核心收益**：删除全部自研检索代码（HNSW / BM25 / RRF / MMR），复用 Grafeo 经过生产验证的索引和查询优化。

### 3.3 生命周期管理

| Rollball 需求 | Grafeo 原生能力 |
|---|---|
| 衰减（Decay） | Rollball 自主实现：Grafeo 只负责存储和检索，不处理业务语义 |
| 遗忘（Foget） | `db.delete_node()` 或标记 `superseded=true` 属性 |
| 合并（Merge） | Rollball 自主实现：Grafeo 提供 `history()` API 查询原始节点 |
| 经验积累（Accumulate） | Rollball 自主实现：Grafeo 提供 `cdc` history 和 `temporal` |
| 情景升级（Situation Escalation） | Rollball 自主实现 |

### 3.4 高价值未充分利用的能力

以下 Grafeo 特性 Rollball 设计文档未提及，但可显著提升记忆质量：

**PageRank（必须使用）**：

Rollball 的 `importance_score` 是手调的 `f32`。Grafeo 的 PageRank 算法可以自动评估记忆节点的重要性——被更多边引用的节点 PageRank 更高。

```
CALL grafeo.pagerank({damping: 0.85, max_iterations: 20})
YIELD node_id, score
WHERE score > 0.001
RETURN node_id, score ORDER BY score DESC
```

**社区检测（推荐使用）**：

Rollball 的六层记忆是手动的分层。Louvain 社区检测可以发现记忆间的隐性群组，自动识别"能力块"、"偏好簇"、"关系网络"。

**CDC + Time-Travel（推荐使用）**：

Grafeo 内置 CDC，记录每个节点的完整变更历史。通过 `db.history(node_id)` 可以追溯任何记忆节点的创建、修改、删除全过程。这对 Rollball 的"经验积累"机制至关重要——每次 Decay 后可以回溯原始记忆。

**Temporal 版本化（可选）**：

Grafeo 支持 append-only 版本化属性。Rollball 的 `created_at` / `updated_at` 可以升级为带版本的时间属性，支持"记忆在某个时间点的状态"查询。

---

## 4. grafeo-memory 设计参考

### 4.1 核心架构

grafeo-memory 采用 **Extract → Reconcile → Execute** 循环：

```
┌──────────────────────────────────────────┐
│             grafeo-memory                │
│                                          │
│  Extractor -> Reconciler -> Executor     │
│  (LLM)       (LLM)        (GrafeoDB)     │
└──────────────────┬───────────────────────┘
                   │
         ┌─────────┴──────────┐
         │      GrafeoDB      │
         │  Graph + Vector    │
         │  + Text (optional) │
         └────────────────────┘
```

**核心流程（`add()` 方法）**：
1. **Extract**: 调用 LLM 从对话中提取 facts
2. **Search Similar**: 向量搜索找相似记忆
3. **Reconcile**: LLM 决定 ADD/UPDATE/DELETE/NONE
4. **Execute**: 执行决策，写入 GrafeoDB

### 4.2 Rollball 改造策略

| grafeo-memory 组件 | Rollball 实现方式 |
|---|---|
| pydantic-ai Agent | Rollball LLM Provider + 结构化输出 |
| extract_facts() | 新建 `rollball-memory/src/extraction.rs` |
| reconcile_async() | 新建 `rollball-memory/src/reconciliation.rs` |
| GrafeoDB | rollball-grafeo（实现 MemoryStore trait）|

**设计参考来源**（`ref-repo/grafeo-memory/`）：
- `src/grafeo_memory/extraction/facts.py` — fact extraction prompt 模板
- `src/grafeo_memory/reconciliation/memories.py` — reconciliation prompt 模板
- `src/grafeo_memory/prompts.py` — 所有 prompt 定义
- `src/grafeo_memory/schemas.py` — 结构化输出 schema

---

## 5. 迁移路径

### 5.1 分阶段迁移

```
Phase 0: 基础设施
  │
Phase 1: grafeo-engine 存储层（grafeo-engine → MemoryStore trait）
  │
Phase 2: rollball-memory 业务逻辑层（Extract/Reconcile）
  │
Phase 3: 高级特性（PageRank / CDC / topology_boost）
```

### Phase 0：依赖替换

**修改文件**：`core/rollball-grafeo/Cargo.toml`

```toml
# 删除
rusqlite.workspace = true

# 新增（crates.io 稳定版本）
grafeo-engine = { version = "0.5", features = [...] }
```

**修改文件**：`core/rollball-grafeo/src/lib.rs`

```rust
// 旧
pub mod rusqlite_storage;  // 删除

// 新
pub mod grafeo_store;       // 基于 GrafeoDB 的存储实现
```

### Phase 1：重写 MemoryStore trait 实现

`rollball-grafeo/src/grafeo_store.rs` 替代 `rusqlite_storage.rs`：

```rust
use grafeo_engine::{GrafeoDB, Config, GraphModel};

pub struct GrafeoStore {
    db: GrafeoDB,
    agent_id: AgentId,
}

impl GrafeoStore {
    pub fn new(agent_id: AgentId, path: &Path) -> Result<Self> {
        let db = GrafeoDB::open(path)?;
        Ok(Self { db, agent_id })
    }

    // 节点操作
    pub fn create_node(&mut self, label: &str, props: HashMap<&str, Value>) -> NodeId {
        let id = self.db.create_node(&[label]);
        for (k, v) in props {
            self.db.set_node_property(id, k, v);
        }
        id
    }

    // 向量索引初始化
    pub fn init_vector_index(&self, label: &str, property: &str, dim: usize) {
        self.db.create_vector_index(label, property, Some(dim), Some("cosine"), None, None, None)
            .expect("vector index created");
    }

    // 全文索引初始化
    pub fn init_text_index(&self, label: &str, property: &str) {
        self.db.create_text_index(label, property).expect("text index created");
    }

    // 语义检索
    pub fn semantic_search(&self, label: &str, emb: &[f32], k: usize) -> Vec<(NodeId, f32)> {
        self.db.vector_search(label, "embedding", emb, k, Some(50), None)
            .expect("vector search")
    }

    // 关键词检索
    pub fn keyword_search(&self, label: &str, query: &str, k: usize) -> Vec<NodeId> {
        self.db.text_search(label, "content", query, k, None)
            .expect("text search")
    }

    // 混合检索
    pub fn hybrid_search(&self, label: &str, query: &str, emb: &[f32], k: usize) -> Vec<NodeId> {
        self.db.hybrid_search(label, "content", "embedding", query, emb, k, None)
            .expect("hybrid search")
    }

    // CDC 历史
    pub fn node_history(&self, node_id: NodeId) -> Vec<ChangeEvent> {
        self.db.history(node_id).unwrap_or_default()
    }

    // PageRank 重要性评分
    pub fn compute_pagerank(&self) -> HashMap<NodeId, f64> {
        let result = self.db.session()
            .execute("CALL grafeo.pagerank() YIELD node_id, score RETURN node_id, score")
            .unwrap();
        // parse result into HashMap
    }

    // 图扩展（GQL）
    pub fn graph_expand(&self, start_id: NodeId, depth: usize) -> Vec<NodeId> {
        let gql = format!(
            "MATCH (m)-[r*1..{}]-(other) WHERE id(m) = $id RETURN other",
            depth
        );
        self.db.session()
            .execute_with_params(&gql, [("id", start_id.into())])
            .unwrap()
    }
}
```

### Phase 2：删除自研检索代码

以下模块整体废弃：

- `core/rollball-grafeo/src/storage/hnsw.rs` — 删除（复用 grafeo-engine HNSW）
- `core/rollball-grafeo/src/storage/bm25.rs` — 删除（复用 grafeo-engine BM25）
- `core/rollball-grafeo/src/storage/rerank.rs` — 删除（复用 grafeo-engine hybrid-search）

### Phase 3：rollball-memory Extract/Reconcile 实现

新增模块（参考 grafeo-memory）：

```
core/rollball-memory/src/
├── lib.rs
├── store.rs              # MemoryStore trait（已有）
├── types.rs              # MemoryNode 等类型（已有）
├── extraction.rs         # 新增：fact extraction 逻辑
├── reconciliation.rs      # 新增：冲突调解逻辑
├── prompts.rs            # 新增：LLM prompt 模板
└── schemas.rs            # 新增：结构化输出 schema
```

### Phase 4：设计文档更新

**必须更新的设计文档**：

| 文档 | 改动内容 |
|---|---|
| `docs/module-design/04-grafeo.md` | 替换存储层描述，引入 Grafeo API，删除自研索引说明 |
| `docs/05-memory.md §4` | 检索流程图中的 HNSW/BM25/RRF 替换为 Grafeo 调用 |
| `docs/05-memory.md §5.5` | 重要性评分 → PageRank 集成方案 |
| `docs/05-memory.md §7.2` | CDC/history API 用于经验回溯 |
| `docs/05-memory.md §8.1` | 存储格式从 SQLite 改为 `.grafeo` 单文件 |

---

## 6. 关键设计决策

### Q1: embedding 由谁生成？

- **方案 A（推荐）**：embedding 由 Rollball Runtime 生成，存储到 Grafeo
- **方案 B**：启用 grafeo-engine `embed` feature，内置 ONNX embedding

**选择**：方案 A。Rollball Runtime 已有 LLM 集成能力，保持存储层职责单一。

### Q2: Rollball Memory 层级架构

```
MemoryManager（rollball-runtime）
  │
  ├── extraction.rs — LLM fact extraction（参考 grafeo-memory）
  ├── reconciliation.rs — LLM conflict resolution（参考 grafeo-memory）
  │
MemoryStore trait（rollball-memory）
  │
GrafeoStore（rollball-grafeo，实现 MemoryStore）
  │
  └── GrafeoDB（grafeo-engine，crates.io）
        ├── HNSW 向量索引
        ├── BM25 全文索引
        ├── WAL 持久化
        └── MVCC 事务
```

### Q3: grafeo-memory vs 自研 Extract/Reconcile

| 方案 | 优点 | 缺点 |
|------|------|------|
| **A. 参考改造（推荐）** | 复用成熟设计，prompt 模板可用 | 需要适配 Rust |
| B. 全部自研 | 完全可控 | 工作量大，容易踩坑 |
| C. 移植 Python 代码 | 逻辑完整 | grafeo-memory 依赖 pydantic-ai，无法直接移植 |

**选择**：方案 A。参考 grafeo-memory 的 prompt 模板和流程，用 Rust + Rollball LLM Provider 重写。

---

## 7. 下一步行动

| 优先级 | 行动 | 依赖 | 预计工时 |
|---|---|---|---|
| P0 | 替换 `core/rollball-grafeo/Cargo.toml` rusqlite → grafeo-engine（crates.io） | — | 1h |
| P0 | 重写 `GrafeoStore` 实现 MemoryStore trait | P0 | 2d |
| P1 | 删除自研 HNSW / BM25 / RRF 模块 | P0 | 1d |
| P1 | 实现 `rollball-memory/src/extraction.rs`（参考 grafeo-memory） | P0 | 2d |
| P1 | 实现 `rollball-memory/src/reconciliation.rs`（参考 grafeo-memory） | P1 | 2d |
| P2 | 集成 PageRank 作为 importance_score 补充 | P0 | 1d |
| P2 | 更新 `docs/module-design/04-grafeo.md` | P0 | 2h |
| P2 | 更新 `docs/05-memory.md` 相关章节 | P1 | 2h |

---

## 8. 参考资源

**grafeo-engine（crates.io 依赖）**：
- 版本：0.5.x（稳定版）
- Feature flags：见 §2.2

**grafeo-memory（设计参考，非编译依赖）**：
- 源码：`ref-repo/grafeo-memory/src/grafeo_memory/`
- 关键文件：
  - `extraction/facts.py` — fact extraction 实现
  - `reconciliation/memories.py` — reconciliation 实现
  - `prompts.py` — 所有 prompt 模板
  - `schemas.py` — 结构化输出 schema
  - `manager.py` — MemoryManager 完整实现

**Rollball 相关文件**：
- `core/rollball-memory/src/` — 待改造
- `core/rollball-grafeo/src/` — 待重写
