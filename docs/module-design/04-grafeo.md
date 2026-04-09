# rollball-grafeo — Grafeo 图数据库引擎

**定位**：Agent 私有 Memory 的存储引擎。每个 Agent Runtime 进程内嵌一个 Grafeo 实例。支持情景记忆（向量索引）、语义记忆（知识图谱）、全文检索。

```
crates/rollball-grafeo/
├── Cargo.toml
└── src/
    ├── lib.rs                     # 公共 API 入口
    ├── grafeo.rs                  # Grafeo 主结构（open/close/query）
    ├── schema.rs                  # 数据库表结构定义
    ├── episodic/
    │   ├── mod.rs                 # 情景记忆（交互片段）
    │   ├── store.rs               # 写入交互记录
    │   └── search.rs              # 语义相似性检索（HNSW）
    ├── semantic/
    │   ├── mod.rs                 # 语义记忆（知识图谱）
    │   ├── graph.rs               # LPG 图操作（节点/边/属性）
    │   └── inference.rs           # 知识推理与合并
    ├── fulltext/
    │   ├── mod.rs                 # 全文检索
    │   └── bm25.rs                # BM25 倒排索引
    ├── hybrid/
    │   ├── mod.rs                 # 混合搜索（向量 + 全文 + RRF 融合）
    │   └── rrf.rs                 # Reciprocal Rank Fusion 排序
    ├── embedding/
    │   ├── mod.rs                 # Embedding 生成抽象
    │   ├── local.rs               # ONNX Runtime 本地生成（all-MiniLM-L6-v2）
    │   └── remote.rs              # 远程 embedding API（可选）
    ├── vector/
    │   ├── mod.rs                 # 向量索引抽象
    │   └── hnsw.rs                # HNSW 索引实现（rusqlite + 自定义）
    ├── migration.rs               # 数据库版本迁移
    └── error.rs                   # 错误类型
```

## 关键 API

```rust
pub struct Grafeo {
    db: rusqlite::Connection,
    embedding: Box<dyn EmbeddingProvider>,
}

impl Grafeo {
    /// 打开 Grafeo 实例（每个 Agent 独立文件）
    pub fn open(path: &Path, embedding: Box<dyn EmbeddingProvider>) -> Result<Self>;
    
    /// 情景记忆：写入交互片段
    pub fn store_episode(&self, episode: &Episode) -> Result<()>;
    
    /// 情景记忆：语义相似性检索
    pub fn search_episodes(&self, query: &str, limit: usize) -> Result<Vec<Episode>>;
    
    /// 语义记忆：写入知识节点
    pub fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()>;
    
    /// 语义记忆：图查询
    pub fn query_knowledge(&self, query: &str) -> Result<Vec<KnowledgeNode>>;
    
    /// 混合搜索：融合向量 + 全文检索
    pub fn hybrid_search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>>;
}
```

## 设计决策

- 基于 `rusqlite`（与 ZeroClaw 一致），避免额外数据库依赖
- HNSW 向量索引：初期用纯 Rust 实现或 `instant-distance` crate，不依赖外部服务
- ONNX Runtime 是可选依赖（feature flag `local-embeddings`），不可用时退化为远程 API
- 数据库文件路径：`<agent_workspace>/memory/private.grafeo`

## 依赖

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
