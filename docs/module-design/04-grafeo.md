# rollball-grafeo — 记忆存储引擎

**定位**：Agent 私有 Memory 的存储引擎实现。Grafeo 实现 `MemoryStore` trait（定义在 05-memory.md §10），作为 Phase 1 唯一的存储后端。每个 Agent Runtime 进程内嵌一个 GrafeoStore 实例。

**v3.4 变更**：从"直接被 Runtime 调用的存储模块"重构为"实现 MemoryStore trait 的可替换后端"。Runtime 和 MemoryManager 只依赖 trait，不依赖 Grafeo 的具体实现。

**v3.5 变更**：
- graph_expand_hops 默认值从 2 改为 3，支持早期终止机制
- 新增 semantic/conflict.rs（冲突候选检测）
- 新增 consolidation/conflict.rs（冲突分类）
- 新增 forgetting/purge_log.rs（Purge 恢复机制）
- 新增 backup.rs / recovery.rs（备份与恢复）
- 新增 embedding/fallback.rs（降级链路）
- 新增 vector/hnsw.rs（HNSW 参数定义：M=16, ef_construction=100, ef_search=64）

**v3.6 变更（当前）**：
- 存储后端从 `rusqlite` 全面迁移至 `grafeo-engine`（v0.5.39，crates.io）
- 删除自研 HNSW / BM25 / RRF 模块，复用 Grafeo 原生索引
- 删除 `embedding/` 目录，Embedding 生成上移至 Runtime 层
- 数据模型从关系型表结构迁移至 Grafeo LPG（标签属性图）
- 引入 PageRank、CDC/History、topology_boost、社区检测等图原生能力
- 新增 `conflict.rs`（三层信号冲突检测：语义 + 时间 + 上下文），即时阶段快速判定 Evolution / Correction / Ambiguous

---

## Crate 结构

```
crates/rollball-grafeo/
├── Cargo.toml
└── src/
    ├── lib.rs              # Export GrafeoStore + public types
    ├── grafeo.rs           # GrafeoStore init, GrafeoDB connection management
    ├── store.rs            # MemoryStore trait implementation for GrafeoStore
    ├── graph.rs            # LPG graph operations (CRUD based on grafeo-engine API)
    ├── retrieval.rs        # Retrieval entry point (calls grafeo-engine search APIs)
    ├── decay.rs            # Decay calculation (leverages CDC history API)
    ├── conflict.rs         # Multi-signal conflict detection (semantic + temporal + context)
    ├── types.rs            # Episode / KnowledgeNode / ProceduralNode / AutobiographicalNode
    │                       # and other data types
    ├── episodic/
    │   ├── mod.rs          # Episodic layer
    │   ├── store.rs        # Write interaction records (LPG node creation)
    │   ├── search.rs       # Semantic similarity retrieval (grafeo-engine vector_search)
    │   └── consolidate.rs  # Consolidation flag and cleanup
    ├── semantic/
    │   ├── mod.rs          # Semantic layer
    │   ├── knowledge.rs    # KnowledgeNode (Fact/Preference/Relation)
    │   ├── procedural.rs   # ProceduralNode
    │   ├── autobiographical.rs  # AutobiographicalNode (forced Active)
    │   ├── conflict.rs     # Conflict candidate detection (Phase 2)
    │   ├── inference.rs    # Knowledge inference and merge
    │   └── skill.rs        # Skill experience nodes
    ├── consolidation/
    │   ├── mod.rs          # Consolidation pipeline
    │   ├── instant.rs      # Instant extraction executor (PendingKnowledgeNode)
    │   ├── offline.rs      # Offline consolidation (Phase 3)
    │   └── conflict.rs     # Conflict classification (Phase 3)
    ├── forgetting/
    │   ├── mod.rs          # Forgetting mechanism
    │   ├── scan.rs         # Background decay scan
    │   └── purge_log.rs    # Purge recovery mechanism (Phase 2)
    └── error.rs            # Error types
```

**已删除模块**（Grafeo 原生提供，无需自研）：
- ~~`vector/hnsw.rs`~~ — 替换为 grafeo-engine 原生 HNSW 向量索引
- ~~`fulltext/bm25.rs`~~ — 替换为 grafeo-engine 原生 BM25 全文索引
- ~~`retrieval/rrf.rs`~~ — 替换为 grafeo-engine `hybrid_search()` 内置 RRF
- ~~`retrieval/hybrid_search.rs`~~ — 逻辑并入 `retrieval.rs`，直接调用 `db.hybrid_search()`
- ~~`embedding/`~~ — Embedding 生成移至 `rollball-runtime` 层
- ~~`backup.rs` / `recovery.rs`~~ — 替换为 grafeo-engine WAL + `grafeo-file` 原生持久化
- ~~`migration.rs`~~ — Grafeo LPG 无版本化 Schema 迁移概念，索引通过 API 动态创建
- ~~`schema.rs`~~ — 关系型表结构定义已废弃

---

## GrafeoStore（MemoryStore trait 实现）

```rust
use rollball_memory::MemoryStore;
use rollball_memory::{MemoryQuery, SearchResult, DecayConfig, StoreHealth, StoreStats};
use rollball_memory::{Episode, KnowledgeNode, ProceduralNode, AutobiographicalNode};
use rollball_memory::MemoryFilters;
use grafeo_engine::GrafeoDB;

/// Grafeo — MemoryStore implementation backed by grafeo-engine
/// One instance per Agent Runtime process, persisted as a single .grafeo file
pub struct GrafeoStore {
    db: GrafeoDB,
    config: GrafeoConfig,
}

pub struct GrafeoConfig {
    pub db_path: PathBuf,
    pub decay: DecayConfig,              // Forgetting params (injected from manifest)
    pub episode_retention_days: u32,     // Default episodic retention (14 days)
    pub graph_expand_hops: u8,          // Max graph expansion hops (default 3)
    pub graph_expand_per_hop: usize,    // Max edges per hop (default 5)
    pub graph_expand_max_nodes: usize,  // Max total expanded nodes (default 20)
    pub early_stop_thresholds: Vec<f32>, // Early stop thresholds (default [0.1, 0.15, 0.2])
    pub max_storage_mb: u64,            // Max storage capacity (default 5000MB)
    pub backup: BackupConfig,            // Auto backup config
}

/// Backup config (injected from manifest.toml [memory.backup])
pub struct BackupConfig {
    pub enabled: bool,                   // Backup switch (default true)
    pub schedule_hour: u8,              // Daily backup hour (default 3, i.e. 03:00)
    pub daily_retention_days: u8,        // Daily backup retention (default 7)
    pub weekly_retention_weeks: u8,      // Weekly backup retention (default 4)
    pub backup_dir: Option<PathBuf>,    // Backup dir (None = default <db_path>/../backups/)
}

impl GrafeoStore {
    /// Open a GrafeoStore instance (one independent .grafeo file per Agent)
    /// Auto-creates indexes if they do not exist
    pub fn open(config: GrafeoConfig) -> Result<Self> {
        let db = GrafeoDB::open(&config.db_path)?;

        // Initialize Grafeo native indexes on first open
        Self::init_indexes(&db)?;

        Ok(Self { db, config })
    }

    /// Create vector and text indexes for memory labels
    fn init_indexes(db: &GrafeoDB) -> Result<()> {
        // HNSW vector index for Episodic nodes
        db.create_vector_index("Episodic", "embedding", Some(384), Some("cosine"), None, None, None)?;
        // HNSW vector index for Knowledge nodes
        db.create_vector_index("Knowledge", "embedding", Some(384), Some("cosine"), None, None, None)?;
        // BM25 text index for Episodic content
        db.create_text_index("Episodic", "content")?;
        // BM25 text index for Knowledge content
        db.create_text_index("Knowledge", "content")?;
        Ok(())
    }
}

impl MemoryStore for GrafeoStore {
    // ── Episodic layer ──

    fn store_episode(&self, episode: &Episode) -> Result<()> {
        // Auto-classify content type (Informational / Artifact / Structural)
        // Artifact content is compressed to summary + artifact_refs
        // Embedding is generated by Runtime layer and passed in Episode.embedding
        episodic::store::write(&self.db, episode)
    }

    fn search_episodes(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        // Semantic search via grafeo-engine native HNSW
        let mut results = episodic::search::search(
            &self.db, query.embedding.as_slice(),
            query.filters.time_range.as_ref(), query.limit,
        )?;
        // Filter by MemoryQuery.filters
        apply_filters(&mut results, &query.filters);
        Ok(results)
    }

    fn mark_consolidated(&self, ids: &[String]) -> Result<()> {
        episodic::consolidate::mark(&self.db, ids)
    }

    fn cleanup_episodes(&self, older_than: Duration) -> Result<u64> {
        let days = older_than.as_secs() / 86400;
        episodic::consolidate::cleanup(&self.db, days as u32)
    }

    // ── Semantic layer ──

    fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()> {
        // Write PendingKnowledgeNode (Phase 2) or formal KnowledgeNode (Phase 3)
        // Instant phase does not do triplet extraction or semantic dedup;
        // those are moved to offline consolidation
        semantic::knowledge::store(&self.db, node)
    }

    fn store_procedural(&self, node: &ProceduralNode) -> Result<()> {
        semantic::procedural::store(&self.db, node)
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<()> {
        // Force status = Active (enforced by LPG property constraint)
        semantic::autobiographical::store(&self.db, node)
    }

    // ── Unified retrieval ──

    fn hybrid_search(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        // Parallel retrieval across Episodic + Knowledge labels,
        // fused via grafeo-engine native hybrid_search (RRF + optional topology boost)
        let results = retrieval::hybrid_search(
            &self.db, query,
        )?;
        apply_filters(&mut results, &query.filters);
        Ok(results)
    }

    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> Result<Vec<SearchResult>> {
        let hops = hops.min(self.config.graph_expand_hops); // clamp
        retrieval::graph_expand(
            &self.db, seeds, hops,
            self.config.graph_expand_per_hop,
            self.config.graph_expand_max_nodes,
        )
    }

    // ── Forgetting ──

    fn run_decay_scan(&self, config: &DecayConfig) -> Result<DecayScanResult> {
        forgetting::scan::run(&self.db, config)
    }

    fn reactivate_node(&self, node_id: &str) -> Result<()> {
        forgetting::scan::reactivate(&self.db, node_id)
    }

    fn purge_expired(&self, max_dormant_age: Duration) -> Result<PurgeResult> {
        forgetting::scan::purge(&self.db, max_dormant_age)
    }

    // ── Lifecycle ──

    fn health_check(&self) -> Result<StoreHealth> {
        let start = Instant::now();
        let result = self.db.session()
            .execute("MATCH (n) RETURN count(n) AS cnt");
        let latency = start.elapsed().as_millis() as u64;
        Ok(StoreHealth {
            is_healthy: result.is_ok(),
            latency_ms: latency,
            error_count: 0,
            details: result.err().map(|e| e.to_string()),
        })
    }

    fn stats(&self) -> Result<StoreStats> {
        let session = self.db.session();
        let episode_count: u64 = session.execute(
            "MATCH (n:Episodic) RETURN count(n) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let node_count: u64 = session.execute(
            "MATCH (n) WHERE n:Knowledge OR n:Procedural OR n:Autobiographical RETURN count(n) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let active_count: u64 = session.execute(
            "MATCH (n) WHERE n.status = 'Active' RETURN count(n) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let dormant_count = node_count - active_count;
        let edge_count: u64 = session.execute(
            "MATCH ()-[r]->() RETURN count(r) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let storage_size = std::fs::metadata(&self.config.db_path)
            .map(|m| m.len()).unwrap_or(0);
        Ok(StoreStats {
            episode_count, node_count, active_node_count: active_count,
            dormant_node_count: dormant_count, edge_count,
            storage_size_bytes: storage_size,
            index_count: 4, // HNSW(Episodic) + HNSW(Knowledge) + BM25(Episodic) + BM25(Knowledge)
        })
    }

    fn close(&self) -> Result<()> {
        // GrafeoDB auto-flushes WAL on drop; explicit close is optional
        Ok(())
    }
}
```

**注意**：`MemoryStore` trait、`MemoryQuery`、`SearchResult`、`DecayConfig`、`StoreHealth`、`StoreStats`、`MemoryMiddleware` 等类型定义在独立的 `rollball-memory` crate 中（与 Runtime 共享），Grafeo crate 只实现 trait，不定义 trait。详见 05-memory.md §10。

**Embedding 职责上移**：GrafeoStore 不再持有 `EmbeddingProvider`。Embedding 向量由 `rollball-runtime` 的 LLM Provider 生成，以 `Vec<f32>` 形式传入 `Episode` / `MemoryQuery`，GrafeoStore 仅负责存储和索引。

---

## Grafeo LPG 数据模型

Rollball 的记忆类型直接映射为 Grafeo 的 **Label**，利用 Label 隔离实现类型区分，无需额外的 `node_type` 枚举字段。

### 8.1 节点类型（NodeType）— 认知功能分层

节点类型通过 **LPG Label** 实现，区分记忆的**认知功能**：

| Label | 含义 | 核心 Properties |
|-------|------|----------------|
| `Episodic` | 经历层节点 | `content`, `embedding`, `importance`, `timestamp`, `session_id`, `role`, `content_type`, `consolidated`, `metadata`, `artifact_refs` |
| `Knowledge` | 知识节点 | `content`, `embedding`, `sub_type` (Fact/Preference/Relation), `confidence`, `subject`, `predicate`, `object`, `status`, `privacy` |
| `Procedural` | 程序记忆节点 | `content`, `embedding`, `procedure_id`, `success_rate`, `invocation_count`, `status` |
| `Autobiographical` | 自传体记忆节点 | `content`, `embedding`, `sub_type` (Identity/Capability/Limitation/Preference/History/Relationship), `status` (forced Active) |
| `SystemConfig` | 系统配置节点 | `config_key`, `config_value`, `updated_at` |
| `ToolInvocation` | 工具调用记录 | `tool_name`, `input_hash`, `output_summary`, `timestamp`, `latency_ms` |
| `Session` | 会话节点 | `session_id`, `started_at`, `ended_at`, `agent_id` |

**NodeType 设计原则：**
- 通过 **Grafeo Label** 实现（而非枚举字段），利用 Label 隔离实现类型区分
- 每种 Label 有独立的 properties schema 和检索索引（HNSW/BM25）
- 认知分层与 LPG Label 一一映射（见 05-memory.md §0 分层原则）

### 8.2 Zone 概念 — 业务场景分区（暂缓实现）

**⚠️ Zone 功能暂缓实现，本节仅做概念定义。**

Zone 用于区分记忆的**业务场景分区**，与 NodeType 正交：
- **NodeType** 回答"这是什么类型的记忆？"（认知功能：经历/语义/程序/自传体）
- **Zone** 回答"这个记忆属于哪个业务场景？"（业务分区：work/personal/system）

**⚠️ 当前状态（Phase 1-3）：**
- `rollball-core/src/memory/traits.rs` 中定义了 `MemoryNode.zone` 字段，但**暂未使用**
- `MemoryStore::list_by_zone()` 方法已定义，但 **GrafeoStore 未实现**
- Zone 功能推迟到 Phase 4+，当前所有节点默认属于 `default` zone

**Phase 4+ 实现方案（待定）：**
- Zone 将作为 **Grafeo Node Property** 存储（而非独立 Label）
- 在各节点结构体中增加 `zone: String` 字段
- 检索时可通过 zone 过滤（如 `filters.zone = Some("work")`）

**Zone 与 NodeType 的正交关系：**
```
一个 KnowledgeNode 可以同时属于：
  - NodeType: Knowledge（认知功能：语义记忆）
  - Zone: work（业务场景：工作相关）
  
一个 Episodic 可以同时属于：
  - NodeType: Episodic（认知功能：经历层）
  - Zone: personal（业务场景：个人生活）
```

### Edge Types

| Edge Type | 起点 | 终点 | 含义 |
|-----------|------|------|------|
| `HAS_MEMORY` | `Session` | `Episodic` / `Knowledge` / ... | 会话拥有记忆 |
| `REFERENCES` | `Knowledge` | `Knowledge` | 知识间引用关系 |
| `SELF_REFERENCES` | `Autobiographical` | `Autobiographical` | 自传体自我引用（身份关联） |
| `PRODUCED` | `ToolInvocation` | `Knowledge` / `Episodic` | 工具调用产生记忆 |
| `DERIVED_FROM` | `Knowledge` | `Episodic` | 知识来源于某段经历 |

### LPG 模型初始化示例

```rust
use grafeo_engine::GrafeoDB;

let db = GrafeoDB::open("agent_memory.grafeo")?;

// No explicit CREATE TABLE needed — LPG is schemaless
// Indexes are created via API (see GrafeoStore::init_indexes)

// Example: create an Episodic node
let mut session = db.session();
session.begin_transaction()?;
session.execute(
    "CREATE (e:Episodic { \
        episode_id: $id, \
        content: $content, \
        embedding: $emb, \
        importance: 0.5, \
        timestamp: $ts, \
        session_id: $sid, \
        role: 'user', \
        content_type: 'Informational', \
        consolidated: false \
    })",
)?;
session.commit()?;
```

---

## 检索能力（基于 grafeo-engine 原生 API）

| 能力 | 旧描述 | 新描述 |
|------|--------|--------|
| 语义检索 | 自研 HNSW (M=16, ef_c=100) | `db.vector_search(label, "embedding", &vec, k, Some(ef), filters)` — Grafeo 原生 HNSW，支持余弦/欧几里得/点积距离，SIMD 加速 |
| 关键词检索 | rusqlite FTS5 BM25 | `db.text_search(label, "content", query, k, filters)` — Grafeo 原生 BM25，内置 Unicode 分词器 |
| 混合检索 | 自研 RRF 融合 | `db.hybrid_search(label, "content", "embedding", query, Some(&vec), k, filters)` — 内置 RRF 融合，可选 topology boost |
| MMR 去重 | 无 | `db.mmr_search(label, "embedding", &vec, k, fetch_k, lambda, ef, filters)` — Maximal Marginal Relevance，保证结果多样性 |
| 图遍历 | SQL 模拟 | GQL: `MATCH (m)-[r*1..3]-(other) WHERE id(m) = $id RETURN other` — 原生 LPG 遍历，有查询优化器支持 |
| 冲突检测 | 无（Phase 2 新增） | `db.vector_search()` + `db.history()` — 三层信号融合（语义相似度 + 时间冲突 + 上下文否定），即时阶段快速判定 |

### 代码示例

```rust
use grafeo_engine::GrafeoDB;

let db = GrafeoDB::open("agent_memory.grafeo")?;

// Semantic search — Episodic layer
let results = db.vector_search(
    "Episodic",           // label
    "embedding",          // property name of the vector
    &query_embedding,     // Vec<f32> generated by Runtime LLM Provider
    10,                   // top-k
    Some(64),             // ef_search
    Some(filters),        // optional property filters
)?;

// Keyword search — Knowledge layer
let results = db.text_search(
    "Knowledge",
    "content",
    "user preference",
    10,
    Some(filters),
)?;

// Hybrid search (RRF fusion + optional topology boost)
let results = db.hybrid_search(
    "Knowledge",
    "content",            // text property
    "embedding",          // vector property
    "dark mode setting",  // text query
    Some(&query_embedding),
    10,
    Some(hybrid_filters),
)?;

// MMR search for diverse results
let results = db.mmr_search(
    "Knowledge",
    "embedding",
    &query_embedding,
    5,                    // final k
    20,                   // fetch_k (over-fetch then re-rank)
    0.5,                  // lambda (relevance vs diversity balance)
    Some(64),
    Some(filters),
)?;

// Graph expansion via GQL
let gql = format!(
    "MATCH (m)-[r*1..{}]-(other) \
     WHERE id(m) = $id \
     RETURN other LIMIT {}",
    hops, max_nodes
);
let mut session = db.session();
let expanded = session.execute_with_params(&gql, [("id", seed_id.into())])?;
```

---

## 图算法增强

grafeo-engine 内置图算法过程（`algos` feature），Rollball 记忆系统可直接调用以提升记忆质量。

### PageRank 集成

Rollball 原有的 `importance_score` 是手调的 `f32`。Grafeo 的 PageRank 算法可以自动评估记忆节点的重要性——被更多边引用的节点 PageRank 更高，作为 `importance_score` 的补充或替代。

```rust
/// Compute PageRank scores for all memory nodes
/// Used to automatically rank node importance based on graph connectivity
pub fn compute_pagerank(&self) -> Result<HashMap<String, f64>> {
    let mut session = self.db.session();
    let result = session.execute(
        "CALL grafeo.pagerank({damping: 0.85, max_iterations: 20}) \
         YIELD node_id, score \
         WHERE score > 0.001 \
         RETURN node_id, score ORDER BY score DESC"
    )?;
    // Parse result rows into HashMap<node_id, score>
    let mut scores = HashMap::new();
    for row in result.rows() {
        let id: String = row.get("node_id");
        let score: f64 = row.get("score");
        scores.insert(id, score);
    }
    Ok(scores)
}
```

**使用场景**：
- 检索排序：将 PageRank 分数作为 `topology_boost` 的输入权重
- 遗忘保护：PageRank 高于阈值的节点跳过衰减扫描
- 重要性校准：替代手调 `importance_score`，减少人工干预

### CDC / History

Grafeo 内置 CDC（Change Data Capture）记录每个节点的完整变更历史。通过 `db.history()` 可以追溯任何记忆节点的创建、修改、删除全过程。

```rust
use grafeo_engine::EntityId;

/// Retrieve full change history of a memory node
/// Enables experience backtracking after decay
pub fn node_history(&self, node_id: &str) -> Result<Vec<ChangeEvent>> {
    // Grafeo CDC tracks every create / update / delete as a ChangeEvent
    let history = self.db.history(EntityId::Node(node_id))?;
    Ok(history)
}

/// Example: restore a node to a previous state after decay
pub fn restore_node_at_epoch(&self, node_id: &str, epoch: u64) -> Result<NodeSnapshot> {
    let mut session = self.db.session();
    let snapshot = session.execute_at_epoch(
        "MATCH (n) WHERE id(n) = $id RETURN n",
        epoch,
    )?;
    // Parse snapshot and optionally write back as a new node
    Ok(snapshot)
}
```

**使用场景**：
- 经验回溯：每次 Decay 修改节点属性后，可通过 `history()` 查看原始状态
- 冲突调解：对比同一节点在不同时间点的版本，辅助 LLM 判断合并策略
- 审计追踪：追踪记忆从 Episodic → Knowledge 的完整演化链路

### topology_boost

Grafeo `hybrid_search()` 支持 `topology_boost` 选项——搜索结果按图连通性重新排序。被更多边引用的节点在检索中获得更高权重，这是图数据库的独特优势。

```rust
/// Hybrid search with topology boost enabled
/// Nodes with more incoming edges are ranked higher
pub fn search_with_topology_boost(
    &self,
    query_text: &str,
    query_embedding: &[f32],
    k: usize,
) -> Result<Vec<SearchResult>> {
    let mut filters = SearchFilters::default();
    filters.topology_boost = true; // Enable graph connectivity re-ranking

    let results = self.db.hybrid_search(
        "Knowledge",
        "content",
        "embedding",
        query_text,
        Some(query_embedding),
        k,
        Some(filters),
    )?;
    Ok(results)
}
```

**原理**：
- 向量/文本检索返回候选集后，Grafeo 执行器计算每个候选节点的图中心性（degree、PageRank 等）
- 最终排序 = RRF 分数 × topology_boost 系数
- 高连通性节点通常是"枢纽记忆"（核心事实、高频工具调用模式），应当优先召回

### 社区检测

Grafeo 内置 Louvain 社区检测算法，可自动发现记忆间的隐性群组。

```rust
/// Detect memory communities via Louvain algorithm
/// Identifies "capability blocks", "preference clusters", "relationship networks"
pub fn detect_memory_communities(&self) -> Result<Vec<Community>> {
    let mut session = self.db.session();
    let result = session.execute(
        "CALL grafeo.louvain() \
         YIELD community_id, node_id \
         RETURN community_id, collect(node_id) AS members"
    )?;

    let mut communities = Vec::new();
    for row in result.rows() {
        communities.push(Community {
            id: row.get("community_id"),
            members: row.get("members"),
        });
    }
    Ok(communities)
}
```

**使用场景**：
- 能力块识别：发现围绕特定技能的程序记忆群组，辅助 Skill 系统升级
- 偏好簇：发现用户偏好的隐性关联（如"暗色模式 + 快捷键 + 夜间勿扰"）
- 关系网络：识别 Autobiographical 节点之间的社交/身份关系网络
- graph_expand 优化：社区内节点在关联扩散时优先扩展，社区间延迟扩展

---

## 冲突检测（Multi-Signal Conflict Detection）

冲突检测采用**三层信号融合**设计，在即时提取阶段（memory_store Tool Call 时）快速识别候选冲突，为离线巩固的精确分类提供输入。该模块对应 `conflict.rs`，独立于 `semantic/conflict.rs`（纯语义候选检测）和 `consolidation/conflict.rs`（离线分类），负责多信号融合的即时判定。

### 三层冲突信号

| 信号层 | 数据源 | 判定逻辑 | 动态阈值 |
|--------|--------|---------|---------|
| **语义相似度** | `db.vector_search()` 返回的候选节点 embedding | 新节点与已有 Active 节点的余弦相似度 | Fact 0.85 / Preference 0.80 / Relation 0.90 |
| **时间冲突** | `db.history()` CDC 历史 API | 同一 subject 在 24h 内的矛盾陈述 | 时间差 < 24h 且 predicate 相同但 object 不同 |
| **上下文冲突** | `source_episode` 原始内容 | 来源 episode 含否定关键词 | "不是" / "其实" / "actually" / "错了" / "纠正" 等 |

**语义阈值差异化设计**：
- **Fact（事实）**：阈值 0.85 — 事实要求高精度匹配，避免误判
- **Preference（偏好）**：阈值 0.80 — 偏好的表达方式多样，适当放宽
- **Relation（关系）**：阈值 0.90 — 关系涉及多实体，误匹配成本高

**时间冲突检测**：
利用 Grafeo CDC `db.history()` 获取同一 subject 的近期变更记录。当检测到同一 subject 在 24 小时内有多个 object 值时，触发时间冲突信号。例如："用户住北京"（上午） vs "用户住上海"（下午）。

**上下文否定检测**：
扫描 `source_episode` 内容中的否定关键词（中文："不是"、"其实"、"错了"、"纠正"；英文："actually"、"wrong"、"correct"、"instead"）。命中否定词表明用户可能在修正之前的陈述，提升冲突置信度。

### 启发式规则加速

三层信号融合后，通过启发式规则实现**快速路径**（无需 LLM）和**慢速路径**（LLM 离线仲裁）的分流：

| 规则 | 条件 | 自动判定 | Confidence | 处理路径 |
|------|------|---------|------------|---------|
| **Evolution（演进）** | 时间差 > 7天 + 含变化词（"搬家了"、"换工作"、"现在"） | 新值 Active，旧值 Dormant | 0.8 | 快速路径，无需 LLM |
| **Correction（纠正）** | 时间差 < 24h + 含否定词（"不是 X，是 Y"） | 新值 Active，旧值 Dormant，降低旧来源可信度 | 0.9 | 快速路径，无需 LLM |
| **Ambiguous（不确定）** | 不满足以上任一规则 | 标记 conflict_group_id，两个都 Active | — | 慢速路径，LLM 离线仲裁 |

**设计理由**：Evolution 和 Correction 有明确的语义模式，规则足以判定；Ambiguous 需要 LLM 理解完整上下文，留给离线巩固阶段处理。这与 05-memory.md §6.4 的两阶段冲突处理设计一致。

### ConflictSignal 结构

```rust
/// Multi-signal conflict detection result
/// Produced by the immediate-phase conflict detector (conflict.rs)
pub struct ConflictSignal {
    /// Semantic similarity score (cosine similarity of embeddings)
    pub semantic_score: f32,

    /// Whether temporal conflict is detected (same subject, <24h, different object)
    pub temporal_conflict: bool,

    /// Whether source_episode contains negation keywords
    pub context_negation: bool,

    /// Suggested conflict type based on heuristic rules
    pub suggested_type: ConflictType,

    /// Heuristic confidence (0.0-1.0). Higher = more certain, no LLM needed.
    /// Evolution: 0.8, Correction: 0.9, Ambiguous: 0.5 (requires LLM arbitration)
    pub heuristic_confidence: f32,
}

/// Conflict classification types
/// Fast-path types (Evolution, Correction) are resolved immediately.
/// Ambiguous types are deferred to offline consolidation LLM arbitration.
pub enum ConflictType {
    /// Natural knowledge evolution over time (e.g., user moved)
    Evolution,
    /// User explicitly corrected a previous statement
    Correction,
    /// Cannot determine from signals alone — requires LLM arbitration
    Ambiguous,
}
```

### 冲突检测 API

```rust
/// Detect conflict between a new memory node and existing Active nodes
///
/// # Arguments
/// - `semantic_score`: Cosine similarity from vector_search (0.0-1.0)
/// - `threshold`: Dynamic threshold based on KnowledgeType (Fact 0.85, Preference 0.80, Relation 0.90)
/// - `time_diff_hours`: Hours between new node and existing node creation
/// - `source_content`: Raw source_episode content for negation keyword scanning
///
/// # Returns
/// - `Some(ConflictSignal)` if conflict is detected (semantic_score > threshold + temporal/context signals)
/// - `None` if no conflict detected
///
/// # Fast-path behavior
/// - Evolution: auto-resolved, old node marked Dormant
/// - Correction: auto-resolved, old node marked Dormant + source credibility reduced
/// - Ambiguous: deferred to offline consolidation (conflict_group_id marked)
pub fn detect_conflict(
    semantic_score: f32,
    threshold: f32,
    time_diff_hours: f32,
    source_content: &str,
) -> Option<ConflictSignal> {
    // 1. Semantic gate: must exceed type-specific threshold
    if semantic_score < threshold {
        return None;
    }

    // 2. Temporal signal: check if same subject within 24h with different object
    let temporal_conflict = time_diff_hours < 24.0;

    // 3. Context signal: scan for negation keywords
    let context_negation = has_negation_keywords(source_content);

    // 4. Heuristic classification
    let (suggested_type, heuristic_confidence) = if time_diff_hours > 168.0 && has_change_keywords(source_content) {
        // > 7 days + change keywords → Evolution
        (ConflictType::Evolution, 0.8)
    } else if time_diff_hours < 24.0 && context_negation {
        // < 24h + negation → Correction
        (ConflictType::Correction, 0.9)
    } else {
        // Otherwise → Ambiguous, needs LLM arbitration
        (ConflictType::Ambiguous, 0.0)
    };

    Some(ConflictSignal {
        semantic_score,
        temporal_conflict,
        context_negation,
        suggested_type,
        heuristic_confidence,
    })
}

/// Check if content contains negation keywords (Chinese + English)
fn has_negation_keywords(content: &str) -> bool {
    const NEGATION_KEYWORDS: &[&str] = &[
        "不是", "其实", "错了", "纠正", "不对", "更正",
        "actually", "wrong", "correct", "instead", "not", "never",
    ];
    NEGATION_KEYWORDS.iter().any(|&kw| content.to_lowercase().contains(kw))
}

/// Check if content contains change/evolution keywords
fn has_change_keywords(content: &str) -> bool {
    const CHANGE_KEYWORDS: &[&str] = &[
        "搬家", "换工作", "换城市", "现在", "已经", "改了",
        "moved", "changed", "now", "recently", "new",
    ];
    CHANGE_KEYWORDS.iter().any(|&kw| content.to_lowercase().contains(kw))
}
```

### 与离线巩固的衔接

`conflict.rs`（即时阶段）和 `consolidation/conflict.rs`（离线阶段）的分工：

| 阶段 | 模块 | 职责 | 输出 |
|------|------|------|------|
| **即时** | `conflict.rs` | 三层信号融合 + 启发式快速判定 | `ConflictSignal` + 快速路径自动处理 |
| **离线** | `consolidation/conflict.rs` | LLM 仲裁 Ambiguous 类型 + 精确分类 | `conflict_group_id` + 用户确认策略 |

即时阶段处理 Evolution 和 Correction（无需 LLM），Ambiguous 类型标记 `conflict_group_id` 后交由离线巩固的 LLM 做最终判定。这与 05-memory.md §6.4 的两阶段设计完全一致。

---

## 索引说明

| 索引类型 | 旧实现 | 新实现 |
|----------|--------|--------|
| 向量索引 | 自研 HNSW (`vector/hnsw.rs`) | Grafeo 原生 HNSW 向量索引，通过 `db.create_vector_index()` 创建 |
| 全文索引 | rusqlite FTS5 (`fulltext/bm25.rs`) | Grafeo 原生 BM25 全文索引，通过 `db.create_text_index()` 创建 |
| 混合检索 | 自研 RRF (`retrieval/rrf.rs`) | Grafeo 原生 `hybrid_search()`，内置 RRF 融合 + topology boost |
| 图遍历索引 | SQL JOIN 模拟 | Grafeo 原生邻接索引，O(degree) 遍历，查询优化器支持谓词下推 |
| 事务隔离 | rusqlite WAL | Grafeo MVCC 快照隔离，原生多版本并发控制 |
| 崩溃恢复 | 自研 `recovery.rs` | Grafeo WAL 重放机制，内置崩溃恢复 |
| 备份 | 自研 `backup.rs` | `grafeo-file` 单文件格式 + 文件级备份 |

---

## 设计决策

- **MemoryStore trait 抽象**：GrafeoStore 实现 `rollball-memory` crate 定义的 `MemoryStore` trait，Runtime 和 MemoryManager 只依赖 trait。未来可无缝替换为其他存储后端（Sled / LMDB / 远程服务 / 内存 mock）
- **存储后端**：`grafeo-engine`（v0.5.39，crates.io），纯 Rust 图数据库，支持 LPG + GQL + HNSW + BM25 + WAL + MVCC
- **向量索引**：Grafeo 原生 HNSW，M/ef/beam_width 全可配置，距离函数支持余弦/欧几里得/点积/曼哈顿，SIMD 加速
- **全文索引**：Grafeo 原生 BM25，内置 Unicode 分词器
- **混合检索**：Grafeo 原生 `hybrid_search()`，内置 RRF 融合，支持 `topology_boost` 图连通性重排序
- **图遍历**：原生 GQL 查询，有 CBO/DPccp 优化器支持，替代 SQL 模拟
- **Embedding 职责分离**：Embedding 由 Runtime LLM Provider 生成，以 `Vec<f32>` 传入 GrafeoStore。GrafeoStore 仅负责存储和索引，不持有 `EmbeddingProvider`
- **数据库文件路径**：`<agent_workspace>/memory/private.grafeo`（单文件 `.grafeo` 格式）
- **遗忘参数**通过 `DecayConfig` 注入（不再硬编码），支持按 Agent 定制
- **遗忘扫描**在 Agent 空闲时后台运行，不阻塞正常检索
- **关联扩散**参数可配置（hops / per_hop / max_nodes），带默认值
- **巩固管道**的即时提取通过 Tool Call 机制（memory_store 工具）实现，离线巩固 Phase 3 使用专用 LLM 调用
- **Fact 节点**写入时自动语义去重（按 subject+predicate 匹配）
- **Episode 写入**时自动分类内容类型，工件性内容压缩为摘要 + artifact_refs
- **PageRank 重要性**：替代或补充手调 `importance_score`，自动评估节点图重要性
- **CDC 历史**：利用 `db.history()` 追踪记忆变更，支持经验回溯和审计
- **社区检测**：Louvain 算法自动发现记忆群组，增强 graph_expand 语义质量

---

## 依赖

```toml
[dependencies]
rollball-memory = { workspace = true }    # MemoryStore trait + shared types
grafeo-engine = { workspace = true }      # v0.5.39, features: lpg, gql, vector-index, text-index, hybrid-search, wal, grafeo-file, algos, cdc, parallel
grafeo-common = { workspace = true }      # Shared types from Grafeo ecosystem
serde = { workspace = true }              # Serialization
serde_json = { workspace = true }         # JSON handling
thiserror = { workspace = true }          # Error definitions
tokio = { workspace = true }              # Async runtime
async-trait = { workspace = true }        # Async trait support
chrono = { workspace = true }             # DateTime handling
```

**Workspace 声明**（`core/Cargo.toml`）：

```toml
[workspace.dependencies]
grafeo-engine = { version = "0.5.39", features = [
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
grafeo-common = { version = "0.5.39" }
```

---

## Feature Flags

`rollball-grafeo` 本身 feature flags 极简 —— 复杂能力由 `grafeo-engine` 的 feature flags 控制：

```toml
[features]
default = []
```

**注**：如需禁用 Grafeo 图算法以减小编译体积，可在 workspace 中移除 `"algos"` feature；如需禁用 CDC，移除 `"cdc"` feature。

---

## 未来扩展方向

| 方向 | 说明 | Phase |
|------|------|-------|
| InMemoryStore | 基于 `GrafeoDB::new_in_memory()` 的 mock 实现，用于单元测试和集成测试 | Phase 3 |
| RemoteMemoryStore | 基于 Grafeo Server 的云端分布式存储，支持多设备实时共享 | Phase 5+ |
| 增量同步 | 基于 Grafeo CDC + WAL 的跨设备增量同步协议 | Phase 5+ |
| Temporal 版本化 | 启用 `grafeo-engine` `"temporal"` feature，支持记忆时间版本化查询 | Phase 4+ |
| 加密存储 | 启用 `grafeo-engine` `"encryption"` feature（AES-256-GCM），与 Vault 密钥管理集成 | Phase 4+ |
| MCP 暴露 | 通过 `grafeo-mcp` 将记忆 API 暴露给外部 Agent | Phase 4+ |
