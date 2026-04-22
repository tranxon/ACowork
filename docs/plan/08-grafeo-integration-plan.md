# Grafeo 集成规划

**状态**：已确认引入 `grafeo-engine`，待执行
**文件**：docs/plan/08-grafeo-integration-plan.md
**依赖**：docs/review/06-grafeo-design-review.md

---

## 1. 背景

06-grafeo-design-review.md 审查发现：rollball-grafeo 依赖 `rusqlite` 而非 Grafeo 数据库本体，`grafeo.rs` 全部为 `unimplemented!()`。用户已确认：**弃用 rusqlite，引入 grafeo-engine**。

grafeo 源码已在 `agent-study/grafeo/` 本地，通过 path 依赖引入即可，无外部阻塞。

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

**Rollball 推荐 feature 配置**（`rollball-grafeo/Cargo.toml`）：

```toml
# grafeo 作为本地 path dependency
# grafeo/ 位于 agent-study/ 根目录，rollball-grafeo 位于 crates/rollball-grafeo/
# 相对路径：../../../grafeo/crates/grafeo-engine
[dependencies]
grafeo-engine = { path = "../../../grafeo/crates/grafeo-engine", features = [
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

## 4. 迁移路径

### 4.1 分阶段迁移

```
Phase 0: 基础设施（立即可做）
  ↓
Phase 1: 核心存储层（1-2天）
  ↓
Phase 2: 检索能力替换（2-3天）
  ↓
Phase 3: 高级特性集成（持续）
```

### Phase 0：替换 Cargo 依赖

**修改文件**：`crates/rollball-grafeo/Cargo.toml`

```toml
# 删除
rusqlite = { version = "0.32", features = ["bundled"] }

# 新增（path 引用本地 grafeo 仓库）
grafeo-engine = { path = "../../../grafeo/crates/grafeo-engine", features = ["lpg", "gql", "vector-index", "text-index", "hybrid-search", "wal", "grafeo-file", "algos", "cdc", "parallel"] }
```

**修改文件**：`crates/rollball-grafeo/src/lib.rs`

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

- `crates/rollball-grafeo/src/storage/hnsw.rs` — 删除
- `crates/rollball-grafeo/src/storage/bm25.rs` — 删除
- `crates/rollball-grafeo/src/storage/rerank.rs` — 删除
- `crates/rollball-grafeo/src/storage/search.rs` 中的自研混合检索逻辑 — 替换为 `hybrid_search`

### Phase 3：设计文档更新

**必须更新的设计文档**：

| 文档 | 改动内容 |
|---|---|
| `docs/module-design/04-grafeo.md` | 替换存储层描述，引入 Grafeo API，删除自研索引说明 |
| `docs/05-memory.md §4` | 检索流程图中的 HNSW/BM25/RRF 替换为 Grafeo 调用 |
| `docs/05-memory.md §5.5` | 重要性评分 → PageRank 集成方案 |
| `docs/05-memory.md §7.2` | CDC/history API 用于经验回溯 |
| `docs/05-memory.md §8.1` | 存储格式从 SQLite 改为 `.grafeo` 单文件 |

---

## 5. 关键设计决策

### Q1: embedding 由谁生成？

Grafeo 的 `embed` feature 支持 ONNX embedding 生成（+17MB binary）。Rollball 可以：

- **方案 A**：禁用 `embed`，embedding 由 LLM 调用方（Rollball Runtime）通过外部服务生成，存储到 Grafeo
- **方案 B**：启用 `embed`，Grafeo 内置生成 embedding

**建议**：方案 A。Rollball Runtime 已有 LLM 集成能力，embedding 生成不属于 Grafeo 的核心职责。保持存储层职责单一。

### Q2: Grafeo 单进程还是共享？

Rollball 每个 Agent 有独立 Grafeo 实例，还是多个 Agent 共享一个 Grafeo 实例？

```
方案 A（当前设计）：每个 Agent 一个 .grafeo 文件
  → GrafeoDB::open("~/.rollball/agents/{agent_id}/memory.grafeo")
  → 隔离性好，Agent 崩溃不影响其他 Agent

方案 B：所有 Agent 共用一个 Grafeo 实例（多-graph）
  → Grafeo named graphs 实现隔离
  → 节省资源，但耦合更高
```

**建议**：方案 A。Rollball 的 Agent 隔离原则要求存储层也隔离。Grafeo 的 `.grafeo` 单文件格式已经足够轻量。

### Q3: Rollball 内存管理逻辑在 Grafeo 上层还是下层？

Grafeo 负责存储和检索优化。Rollball 的业务逻辑（Decay、Forgetting、Merge、Importance 评分）仍然在 `MemoryManager` 层实现，Grafeo 只是底层存储引擎。

```
MemoryManager（Rollball 业务逻辑）
  │
  ├── importance_score 计算 → PageRank 增强（Grafeo algos）
  ├── Decay / Forget 决策 → Rollball 自主实现
  ├── GrafeoStore（GrafeoDB wrapper） → Rollball 自研
  │     ├── 节点/边 CRUD
  │     ├── 向量检索（HNSW）
  │     ├── 全文检索（BM25）
  │     ├── 混合检索（RRF）
  │     └── 图遍历（GQL）
  └── GrafeoDB（grafeo-engine） → 外部引入
        ├── WAL 持久化
        ├── MVCC 事务
        └── CDC history
```

---

## 6. 下一步行动

| 优先级 | 行动 | 负责 | 预计工时 |
|---|---|---|---|
| ~~P0~~ | ~~确认 grafeo-engine 的 git 源~~ → 已解决：本地 path 引用 | — | — |
| P0 | 替换 rollball-grafeo/Cargo.toml 的 rusqlite → grafeo-engine | 实现 | 1h |
| P0 | 重写 `GrafeoStore` 核心 API（CRUD + 索引） | 实现 | 2d |
| P1 | 删除自研 HNSW / BM25 / RRF 模块 | 实现 | 1d |
| P1 | 更新 `docs/module-design/04-grafeo.md` | 设计 | 2h |
| P1 | 集成 PageRank 作为 importance_score 的基础 | 实现 | 1d |
| P2 | 集成 CDC/history 用于经验回溯 | 实现 | 1d |
| P2 | 更新 `docs/05-memory.md` 相关章节 | 设计 | 2h |

---

## 7. 参考资源

- grafeo-engine crate: `grafeo/crates/grafeo-engine/`
- grafeo-engine Cargo.toml feature flags: `grafeo/crates/grafeo-engine/Cargo.toml`
- 关键测试（API 用法参考）：`tests/hybrid_query.rs`, `tests/vector_filtered.rs`, `tests/call_procedures.rs`, `tests/cdc_crud_api.rs`, `tests/time_travel.rs`
- grafeo-memory 参考架构: `grafeo/docs/ecosystem/grafeo-memory.md`
