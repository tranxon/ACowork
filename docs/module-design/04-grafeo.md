# rollball-grafeo — 记忆存储引擎

**定位**：Agent 私有 Memory 的存储引擎实现。Grafeo 实现 `MemoryStore` trait（定义在 05-memory.md §10），作为 Phase 1 唯一的存储后端。每个 Agent Runtime 进程内嵌一个 GrafeoStore 实例。

**v3.4 变更**：从"直接被 Runtime 调用的存储模块"重构为"实现 MemoryStore trait 的可替换后端"。Runtime 和 MemoryManager 只依赖 trait，不依赖 Grafeo 的具体实现。

## Crate 结构

```
crates/rollball-grafeo/
├── Cargo.toml
└── src/
    ├── lib.rs                     # 导出 GrafeoStore + 公共类型
    ├── store.rs                   # GrafeoStore 结构（实现 MemoryStore trait）
    ├── schema.rs                  # 数据库表结构定义 + 迁移
    ├── types.rs                   # Episode / KnowledgeNode / ProceduralNode / AutobiographicalNode
    │                              # 等数据类型（定义在此，供 MemoryStore trait 引用）
    ├── episodic/
    │   ├── mod.rs                 # 经历层：情景记忆
    │   ├── store.rs               # 写入交互记录（content_type 分类 + 工件压缩）
    │   ├── search.rs              # 语义相似性检索（HNSW）
    │   └── consolidate.rs         # 巩固标记与清理
    ├── semantic/
    │   ├── mod.rs                 # 沉淀层：长期记忆
    │   ├── knowledge.rs           # KnowledgeNode（事实/偏好/关系）+ Fact 语义去重
    │   ├── procedural.rs          # ProceduralNode（行为模式/操作规则）
    │   ├── autobiographical.rs    # AutobiographicalNode（自我认知，强制 Active）
    │   ├── graph.rs               # LPG 图操作（节点/边/属性）
    │   ├── inference.rs           # 知识推理与合并
    │   └── skill.rs               # Skill 经验节点（Draft/Iteration/Execution/Experience）
    ├── consolidation/
    │   ├── mod.rs                 # 巩固管道
    │   ├── instant.rs             # 即时提取执行层（处理 memory_store 工具调用）
    │   └── offline.rs             # 离线巩固（Phase 3）
    ├── forgetting/
    │   ├── mod.rs                 # 遗忘机制
    │   ├── decay.rs               # 乘法衰减计算（DecayConfig 参数化）
    │   └── scan.rs                # 后台衰减扫描与状态转换
    ├── retrieval/
    │   ├── mod.rs                 # 检索入口
    │   ├── hybrid_search.rs       # 混合搜索（向量 + 全文 + RRF）
    │   ├── graph_expand.rs        # 关联扩散（LPG 图上 1-2 跳扩展）
    │   └── rrf.rs                 # Reciprocal Rank Fusion 排序
    ├── fulltext/
    │   ├── mod.rs                 # 全文检索
    │   └── bm25.rs                # BM25 倒排索引（rusqlite FTS5）
    ├── embedding/
    │   ├── mod.rs                 # Embedding 生成 trait + 实现
    │   ├── local.rs               # ONNX Runtime 本地生成（all-MiniLM-L6-v2）
    │   └── remote.rs              # 远程 embedding API（可选）
    ├── vector/
    │   ├── mod.rs                 # 向量索引 trait + 实现
    │   └── hnsw.rs                # HNSW 索引实现（rusqlite + 自定义）
    ├── migration.rs               # 数据库版本迁移框架
    └── error.rs                   # 错误类型
```

## GrafeoStore（MemoryStore trait 实现）

```rust
use rollball_memory::MemoryStore;
use rollball_memory::{MemoryQuery, SearchResult, DecayConfig, StoreHealth, StoreStats};
use rollball_memory::{Episode, KnowledgeNode, ProceduralNode, AutobiographicalNode};
use rollball_memory::MemoryFilters;

/// Grafeo — 基于 rusqlite 的 MemoryStore 实现
/// 每个 Agent Runtime 进程内嵌一个实例，存储在独立文件中
pub struct GrafeoStore {
    db: rusqlite::Connection,
    embedding: Box<dyn EmbeddingProvider>,
    config: GrafeoConfig,
}

pub struct GrafeoConfig {
    pub db_path: PathBuf,
    pub decay: DecayConfig,          // 遗忘参数（可从 manifest 注入）
    pub episode_retention_days: u32, // 情景记忆默认保留期（默认 14 天）
    pub graph_expand_hops: u8,       // 关联扩散最大跳数（默认 2）
    pub graph_expand_per_hop: usize, // 每跳最大扩展边数（默认 5）
    pub graph_expand_max_nodes: usize,// 扩展节点总数上限（默认 20）
}

impl GrafeoStore {
    /// 打开 GrafeoStore 实例（每个 Agent 独立文件）
    /// 自动执行数据库迁移（如果需要）
    pub fn open(config: GrafeoConfig, embedding: Box<dyn EmbeddingProvider>) -> Result<Self> {
        let db = Connection::open(&config.db_path)?;
        migration::run(&db)?;
        // ... 初始化索引
        Ok(Self { db, embedding, config })
    }
}

impl MemoryStore for GrafeoStore {
    // ── 经历层 ──

    fn store_episode(&self, episode: &Episode) -> Result<()> {
        // 自动分类内容类型（Informational / Artifact / Structural）
        // 工件性内容压缩为摘要 + artifact_refs
        // 生成 embedding（超时 200ms 则后台补生成）
        episodic::store::write(&self.db, episode, &*self.embedding)
    }

    fn search_episodes(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        let mut results = episodic::search::search(
            &self.db, &*self.embedding, query.query_text.as_str(),
            query.filters.time_range.as_ref(), query.limit,
        )?;
        // 按 MemoryQuery.filters 过滤
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

    // ── 沉淀层 ──

    fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()> {
        // Fact 自动语义去重（subject+predicate 匹配）
        semantic::knowledge::store(&self.db, node)
    }

    fn store_procedural(&self, node: &ProceduralNode) -> Result<()> {
        semantic::procedural::store(&self.db, node)
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<()> {
        // 强制 status = Active（schema 约束）
        semantic::autobiographical::store(&self.db, node)
    }

    // ── 统一检索 ──

    fn hybrid_search(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        // 并行检索经历层 + 沉淀层，RRF 融合排序
        let results = retrieval::hybrid_search::search(
            &self.db, &*self.embedding, query,
        )?;
        apply_filters(&mut results, &query.filters);
        Ok(results)
    }

    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> Result<Vec<SearchResult>> {
        let hops = hops.min(self.config.graph_expand_hops); // clamp
        retrieval::graph_expand::expand(
            &self.db, seeds, hops,
            self.config.graph_expand_per_hop,
            self.config.graph_expand_max_nodes,
        )
    }

    // ── 遗忘 ──

    fn run_decay_scan(&self, config: &DecayConfig) -> Result<DecayScanResult> {
        forgetting::scan::run(&self.db, config)
    }

    fn reactivate_node(&self, node_id: &str) -> Result<()> {
        forgetting::scan::reactivate(&self.db, node_id)
    }

    fn purge_expired(&self, max_dormant_age: Duration) -> Result<PurgeResult> {
        forgetting::scan::purge(&self.db, max_dormant_age)
    }

    // ── 生命周期 ──

    fn health_check(&self) -> Result<StoreHealth> {
        let start = Instant::now();
        let result = self.db.execute_batch("SELECT count(*) FROM memory_nodes WHERE status='Active'");
        let latency = start.elapsed().as_millis() as u64;
        Ok(StoreHealth {
            is_healthy: result.is_ok(),
            latency_ms: latency,
            error_count: 0,
            details: result.err().map(|e| e.to_string()),
        })
    }

    fn stats(&self) -> Result<StoreStats> {
        // 查询各表行数、文件大小
        let episode_count: u64 = self.db.query_row(
            "SELECT count(*) FROM episodes", [], |r| r.get(0),
        )?;
        let node_count: u64 = self.db.query_row(
            "SELECT count(*) FROM memory_nodes", [], |r| r.get(0),
        )?;
        let active_count: u64 = self.db.query_row(
            "SELECT count(*) FROM memory_nodes WHERE status='Active'", [], |r| r.get(0),
        )?;
        let dormant_count = node_count - active_count;
        let edge_count: u64 = self.db.query_row(
            "SELECT count(*) FROM memory_edges", [], |r| r.get(0),
        )?;
        let storage_size = std::fs::metadata(&self.config.db_path)
            .map(|m| m.len()).unwrap_or(0);
        Ok(StoreStats {
            episode_count, node_count, active_node_count: active_count,
            dormant_node_count: dormant_count, edge_count,
            storage_size_bytes: storage_size, index_count: 3, // HNSW + BM25 + SQL
        })
    }

    fn close(&self) -> Result<()> {
        // 刷写 WAL，关闭连接
        Ok(())
    }
}
```

**注意**：`MemoryStore` trait、`MemoryQuery`、`SearchResult`、`DecayConfig`、`StoreHealth`、`StoreStats`、`MemoryMiddleware` 等类型定义在独立的 `rollball-memory` crate 中（与 Runtime 共享），Grafeo crate 只实现 trait，不定义 trait。详见 05-memory.md §10。

## 数据库 Schema（核心表）

```sql
-- 经历层：情景记忆
CREATE TABLE episodes (
    episode_id   TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    timestamp    DATETIME NOT NULL,
    role         TEXT NOT NULL,          -- user / agent / tool
    content      TEXT NOT NULL,         -- 信息性内容原样存储；工件性内容仅存摘要
    content_type TEXT DEFAULT 'Informational',  -- Informational / Artifact / Structural
    embedding    BLOB,                  -- f32 向量（基于 content 而非原始代码生成）
    importance   REAL DEFAULT 0.5,
    consolidated BOOLEAN DEFAULT FALSE,
    metadata     TEXT,                  -- JSON
    artifact_refs TEXT                  -- JSON Array，ArtifactRef[]（仅 Artifact 类型有值）
);

-- 经历层：工件引用（独立表便于查询）
CREATE TABLE artifact_refs (
    ref_id       TEXT PRIMARY KEY,
    episode_id   TEXT NOT NULL REFERENCES episodes(episode_id),
    path         TEXT NOT NULL,
    hash         TEXT NOT NULL,         -- 内容 sha256
    description  TEXT NOT NULL,
    line_start   INTEGER,
    line_end     INTEGER,
    modified_at  DATETIME
);

-- 沉淀层：统一节点表
CREATE TABLE memory_nodes (
    node_id      TEXT PRIMARY KEY,
    node_type    TEXT NOT NULL,          -- Knowledge / Procedural / Autobiographical / SkillDraft / SkillIteration / SkillExecution / SkillExperience
    sub_type     TEXT,                   -- Knowledge: Fact/Preference/Relation; Autobiographical: Identity/Capability/Limitation/Preference/History/Relationship
    content      TEXT NOT NULL,          -- JSON，按 node_type 不同结构不同
    created_at   DATETIME NOT NULL,
    updated_at   DATETIME NOT NULL,
    status       TEXT DEFAULT 'Active',  -- Active / Dormant / Purged（Autobiographical 强制 Active）
    dormant_since DATETIME,

    -- 遗忘字段（Autobiographical 不使用）
    importance   REAL DEFAULT 0.5,
    access_count INTEGER DEFAULT 0,
    last_accessed DATETIME,
    decay_score  REAL DEFAULT 1.0,

    -- 隐私
    privacy      TEXT DEFAULT 'Personal' -- Public / Personal / Sensitive（打包分享时过滤）
);

-- 沉淀层：关系边
CREATE TABLE memory_edges (
    edge_id      TEXT PRIMARY KEY,
    source_id    TEXT NOT NULL REFERENCES memory_nodes(node_id),
    target_id    TEXT NOT NULL REFERENCES memory_nodes(node_id),
    relation     TEXT NOT NULL,
    weight       REAL DEFAULT 1.0,
    created_at   DATETIME NOT NULL,
    source_episode TEXT
);

-- 向量索引（HNSW，由 rusqlite 扩展或自定义实现）
-- 全文索引（BM25，由 rusqlite FTS5 实现）
```

## 设计决策

- **MemoryStore trait 抽象**：GrafeoStore 实现 `rollball-memory` crate 定义的 `MemoryStore` trait，Runtime 和 MemoryManager 只依赖 trait。未来可无缝替换为其他存储后端（Sled / LMDB / 远程服务 / 内存 mock）
- 基于 `rusqlite`（与 ZeroClaw 一致），避免额外数据库依赖
- HNSW 向量索引：初期用纯 Rust 实现或 `instant-distance` crate，不依赖外部服务
- ONNX Runtime 是可选依赖（feature flag `local-embeddings`），不可用时退化为远程 API
- Embedding 已有 `EmbeddingProvider` trait 抽象（local / remote）
- 数据库文件路径：`<agent_workspace>/memory/private.grafeo`
- 遗忘参数通过 `DecayConfig` 注入（不再硬编码），支持按 Agent 定制
- 遗忘扫描在 Agent 空闲时后台运行，不阻塞正常检索
- 关联扩散参数可配置（hops / per_hop / max_nodes），带默认值
- 巩固管道的即时提取通过 Tool Call 机制（memory_store 工具）实现，离线巩固 Phase 3 使用专用 LLM 调用
- Fact 节点写入时自动语义去重（按 subject+predicate 匹配）
- Episode 写入时自动分类内容类型，工件性内容压缩为摘要 + artifact_refs

## 依赖

- `rollball-memory` — MemoryStore trait + 共享类型定义
- `rusqlite` — 存储引擎
- `serde`, `serde_json` — 数据序列化
- `tokio` — 异步封装
- `ort` (feature-gated) — ONNX Runtime

## Feature Flags

```toml
[features]
default = []
local-embeddings = ["dep:ort"]     # 本地 embedding（增加 ~50MB 编译体积）
```

## 未来扩展方向

| 方向 | 说明 | Phase |
|------|------|-------|
| Sled 后端 | 替换 rusqlite，获得更好的并发性能和 MVCC 支持 | Phase 3+ |
| InMemoryStore | 用于单元测试和集成测试的 mock 实现 | Phase 3 |
| RemoteMemoryStore | 云端分布式存储，支持多设备实时共享 | Phase 5+ |
| WAL 增量同步 | 基于 SQLite WAL 的跨设备增量同步协议 | Phase 5+ |
