# Grafeo 图数据库项目详细分析

## 项目概述

**Grafeo** 是一个用 Rust 从零构建的高性能图数据库，专注于速度和低内存使用。它既可以作为嵌入式库运行，也可以作为独立服务器运行，支持内存和持久化存储，并提供完整的 ACID 事务支持。

### 基本信息
- **语言**: Rust (2024 edition, MSRV 1.91.1)
- **版本**: 0.5.12
- **许可证**: Apache-2.0
- **仓库**: https://github.com/GrafeoDB/grafeo
- **数据模型**: LPG (标签属性图) 和 RDF (资源描述框架)

### 核心特性

1. **双数据模型支持**: LPG 和 RDF，每种都有优化的存储
2. **多语言查询**: 支持 GQL、Cypher、Gremlin、GraphQL、SPARQL 和 SQL/PGQ
3. **零外部依赖**: 可嵌入使用，无需 JVM、Docker 或外部进程
4. **多语言绑定**: Python (PyO3)、Node.js/TypeScript (napi-rs)、Go (CGO)、WebAssembly (wasm-bindgen)
5. **向量化执行**: 推式向量化执行，自适应块大小
6. **并行处理**: Morsel 驱动的并行性，自动检测线程数
7. **列式存储**: 字典编码、Delta 编码、RLE 压缩
8. **向量搜索**: HNSW 索引，支持余弦、欧几里得、点积、曼哈顿距离
9. **全文搜索**: BM25 倒排索引
10. **混合搜索**: 文本 + 向量搜索结合

## 技术栈分析

### 核心依赖

#### 错误处理
- `thiserror 2.0` - 结构化错误处理
- `anyhow 1.0` - 简化的错误上下文

#### 并发与同步
- `parking_lot 0.12` - 高性能互斥锁和读写锁
- `crossbeam 0.8` - 无锁数据结构和通道
- `rayon 1.10` - 数据并行库

#### 数据结构
- `hashbrown 0.16.1` - 高性能哈希表
- `smallvec 1.13` - 小向量优化（栈分配）
- `bumpalo 3.20` - 竞技场分配器
- `indexmap 2.7` - 保持插入顺序的哈希表
- `dashmap 6.1` - 并发哈希映射
- `arcstr 1.2` - Arc 包裹的字符串（零拷贝）

#### 内存与 I/O
- `memmap2 0.9` - 内存映射文件
- `bytes 1.11.1` - 高效字节缓冲
- `byteorder 1.5` - 字节序处理

#### 校验和与哈希
- `crc32fast 1.5` - 快速 CRC32 校验
- `ahash 0.8` - 非 Crypto 高性能哈希

#### 序列化
- `serde 1.0` - 序列化框架
- `bincode 2.0` - 二进制序列化

#### Arrow & Polars
- `arrow 58` - Apache Arrow 列式格式
- `polars 0.53.0` - 数据处理库

#### 异步运行时
- `tokio 1.43` - 异步运行时

#### 可观测性
- `tracing 0.1` - 结构化日志
- `tracing-subscriber 0.3` - 日志订阅器

#### 正则表达式
- `regex 1.12.3` - 正则表达式引擎

#### 测试
- `proptest 1.10.0` - 属性测试
- `criterion 0.8.2` - 基准测试
- `tempfile 3.26` - 临时文件管理

#### 分配器（平台优化）
- `tikv-jemallocator 0.6` - TiKV 的内存分配器
- `mimalloc 0.1` - 高性能通用分配器

#### 快速哈希
- `rustc-hash 2.1` - 极快的非加密哈希

#### 向量索引支持
- `ordered-float 5.0` - 可排序的浮点数
- `rand 0.10` - 随机数生成

#### AI/嵌入功能（可选）
- `ort 2.0.0-rc.11` - ONNX Runtime
- `tokenizers 0.22` - 分词器
- `hf-hub 0.5` - Hugging Face Hub

## 整体架构

### 架构设计原则

| 目标 | 方法 |
|------|------|
| **性能** | 向量化执行、SIMD、列式存储 |
| **可嵌入性** | 零外部依赖、单一库 |
| **安全性** | 纯 Rust、内存安全设计 |
| **灵活性** | 插件架构、多存储后端 |

### 查询处理流程

```
客户端 -> Session -> Parser -> Planner -> Optimizer -> Executor -> Storage -> 结果
  ↓           ↓        ↓        ↓          ↓          ↓
 execute()  parse()  plan()  optimize() execute()  scan/lookup
  ↓           ↓        ↓        ↓          ↓          ↓
 Query     AST     Logical  Physical   Results    Data
          语法树    计划      计划
```

### 关键组件

#### 1. 查询处理
- **Parser** - GQL/Cypher/SPARQL/Gremlin/GraphQL/SQL-PGQ 解析为 AST
- **Binder** - 语义分析和类型检查
- **Planner** - AST 转换为逻辑计划
- **Optimizer** - 基于成本的优化
- **Executor** - 推式执行

#### 2. 存储
- **LPG Store** - 节点和边存储
- **Property Store** - 列式属性存储
- **Indexes** - 哈希、邻接、Trie、向量 (HNSW)、文本 (BM25)、Ring
- **WAL** - 持久化和恢复

#### 3. 内存
- **Buffer Manager** - 内存分配
- **Arena Allocator** - 基于 Epoch 的分配
- **Spill Manager** - 大操作的磁盘溢出

### 线程模型

- **主线程** - 协调查询执行
- **工作线程** - 并行查询处理（Morsel 驱动）
- **后台线程** - 检查点、压缩

## 模块划分与详细分析

### 1. grafeo (顶层门面 Crate)

**路径**: `crates/grafeo`

**职责**: 重新导出公共 API 的顶层门面

**导出内容**:
```rust
use grafeo::GrafeoDB;

let db = GrafeoDB::new_in_memory();
```

**特点**:
- 简化用户 API
- 重新导出 grafeo-engine 的公共接口
- 作为用户入口点

---

### 2. grafeo-common (基础层 Crate)

**路径**: `crates/grafeo-common`

**职责**: 基础类型和工具，被所有模块使用

**核心模块**:

#### types/
- `NodeId` - 节点标识符
- `EdgeId` - 边标识符
- `Value` - 属性值类型
- `LogicalType` - 逻辑类型
- `PropertyKey` - 属性键
- `Timestamp` - 时间戳
- `TxId` - 事务 ID
- `EpochId` - Epoch 标识符

#### memory/
- Arena 分配器 - 高性能内存分配
- 内存池 - 重用内存缓冲
- BufferManager - 统一缓冲管理器

#### mvcc/
- `Version` - MVCC 版本
- `VersionChain` - 版本链
- `VersionInfo` - 版本信息
- 支持快照隔离

#### collections/
- 类型别名（带一致哈希的 Map/Set）
- 统一集合接口

#### utils/
- 哈希工具
- 错误类型定义

**特点**:
- 零抽象开销
- 提供所有模块共享的基础设施
- 特性门控的分层存储

---

### 3. grafeo-core (核心数据结构 Crate)

**路径**: `crates/grafeo-core`

**职责**: 核心数据结构、索引和执行原语

**核心模块**:

#### graph/

##### lpg/ (标签属性图)
- `LpgStore` - LPG 存储实现
- `Node` - 节点结构
- `Edge` - 边结构
- CSR 格式邻接列表

##### rdf/ (资源描述框架) - 特性门控
- `RdfStore` - RDF 三元组存储
- SPO/POS/OSP 索引

##### traits.rs
- `GraphStore` - 只读图存储接口
- `GraphStoreMut` - 可变图存储接口

#### index/

##### index/
- `HashIndex` - 哈希索引，O(1) 平均查找
- `adjacency.rs` - 邻接索引，O(degree) 遍历
- `trie.rs` - Trie 索引，多路连接优化

##### vector/ (向量索引) - 特性门控
- `HnswIndex` - HNSW 近似最近邻搜索
- `brute_force_knn` - 暴力 k-NN 搜索
- 支持距离函数:
  - 余弦相似度 (SIMD 加速)
  - 欧几里得距离
  - 点积
  - 曼哈顿距离

##### text/ (文本索引) - 特性门控
- `InvertedIndex` - BM25 倒排索引
- `Tokenizer` - Unicode 分词器
- `FusionMethod` - 混合搜索融合

##### ring/ (Ring 索引) - 特性门控
- `TripleRing` - RDF 三元组压缩
- `LeapfrogRing` - Leapfrog 连接算法
- 3 倍空间减少

##### zone_map.rs
- `ZoneMap` - 区域图
- `BloomFilter` - 布隆过滤器
- 智能数据跳过

#### execution/

##### chunk.rs
- `DataChunk` - 批次化行处理（~1024 行/块）
- `ChunkZoneHints` - 区域提示

##### vector.rs
- `ValueVector` - 单列向量
- 列式存储

##### factorized_*
- `FactorizedChunk` - 多层分解块
- `FactorizedVector` - 分解向量
- 避免笛卡尔积

##### operators/
- 物理算子实现
- Scan、Filter、Join、Aggregate 等

##### pipeline/
- 推式执行管道
- `Pipeline`、`Source`、`Sink`

##### parallel/ - 特性门控
- Morsel 驱动并行
- `MorselScheduler`、`ParallelPipeline`

##### spill/ - 特性门控
- 磁盘溢出
- `SpillManager`、`SpillFile`

##### adaptive/
- 自适应执行
- 运行时基数反馈
- `AdaptivePipelineExecutor`

#### storage/

##### codec.rs
- 通用编解码器

##### dictionary.rs
- `DictionaryEncoding` - 字典编码
- `DictionaryBuilder` - 字典构建

##### delta.rs
- Delta 编码压缩

##### bitpack.rs
- 位打包压缩

##### runlength.rs
- `run_length_encoding` - RLE 压缩

##### bitvec.rs
- 位向量操作

##### epoch_store.rs
- Epoch 存储

##### succinct/ - 特性门控
- 精简数据结构
- rank/select 位向量
- Elias-Fano 编码
- 小波树

#### statistics/
- `ColumnStatistics` - 列统计
- `Histogram` - 直方图
- `LabelStatistics` - 标签统计
- `Statistics` - 统一统计接口
- 基数估计

#### cache/
- 二次机会 LRU 缓存

**特性**:
- `default = ["parallel", "spill", "mmap"]`
- `parallel` - rayon 并行
- `spill` - 磁盘溢出
- `rdf` - RDF 图模型
- `tiered-storage` - 分层热冷存储
- `succinct-indexes` - 精简数据结构
- `ring-index` - Ring 索引（需 rdf + succinct-indexes）
- `vector-index` - HNSW 向量索引
- `text-index` - BM25 文本索引
- `hybrid-search` - 混合搜索

---

### 4. grafeo-adapters (适配器 Crate)

**路径**: `crates/grafeo-adapters`

**职责**: 外部接口和适配器，解析器和存储后端

**核心模块**:

#### query/

##### gql/ - 特性门控
- `Parser` - GQL 词法分析器
- GQL 语法解析
- ISO/IEC 39075 标准

##### cypher/ - 特性门控
- Cypher 解析器
- openCypher 9.0

##### sparql/ - 特性门控
- SPARQL 解析器
- W3C SPARQL 1.1

##### gremlin/ - 特性门控
- Gremlin 解析器
- Apache TinkerPop

##### graphql/ - 特性门控
- GraphQL 解析器
- 规范兼容

##### sql_pgq/ - 特性门控
- SQL/PGQ 解析器
- SQL:2023 GRAPH_TABLE

#### storage/

##### wal/ (写前日志) - 特性门控
- `WalManager` - WAL 管理器
- `LpgWal` - LPG WAL
- `WalRecord` - WAL 记录
- `WalRecovery` - WAL 恢复
- 持久化和崩溃恢复

#### plugins/
- 插件系统
- 自定义函数注册
- 图算法插件

**特性**:
- `default = ["gql", "wal", "parallel"]`
- `gql` - GQL 解析器
- `cypher` - Cypher 解析器
- `sparql` - SPARQL 解析器
- `gremlin` - Gremlin 解析器
- `graphql` - GraphQL 解析器
- `sql-pgq` - SQL/PGQ 解析器（依赖 gql）
- `rdf` - RDF 支持
- `wal` - WAL 存储
- `parallel` - 并行算法
- `algos` - 图算法插件

---

### 5. grafeo-engine (引擎 Crate)

**路径**: `crates/grafeo-engine`

**职责**: 数据库门面和协调，查询处理管道

**核心模块**:

#### database/

##### mod.rs
- `GrafeoDB` - 主数据库结构
- 数据库生命周期管理
- 配置管理

##### query.rs
- 查询执行接口
- `execute()` - 执行查询

##### crud.rs
- 节点/边 CRUD 操作
- `create_node()` - 创建节点
- `create_edge()` - 创建边
- `delete_node()` - 删除节点
- `delete_edge()` - 删除边

##### index.rs
- 索引管理
- `create_index()` - 创建索引
- `drop_index()` - 删除索引

##### search.rs
- 搜索接口
- 向量搜索
- 文本搜索
- 混合搜索

##### embed.rs - 特性门控
- 嵌入模型管理
- 注册和使用嵌入模型

##### persistence.rs
- 持久化操作
- `save()` - 保存到磁盘
- `load()` - 从磁盘加载
- 快照

##### admin.rs
- 管理接口
- 统计信息
- 诊断

#### query/

##### translator_common/
- 通用翻译器逻辑

##### gql_translator/ - 特性门控
- `translate_gql()` - GQL 到逻辑计划

##### cypher_translator/ - 特性门控
- `translate_cypher()` - Cypher 到逻辑计划

##### sparql_translator/ - 特性门控
- `translate_sparql()` - SPARQL 到逻辑计划

##### gremlin_translator/ - 特性门控
- `translate_gremlin()` - Gremlin 到逻辑计划

##### graphql_translator/ - 特性门控
- `translate_graphql()` - GraphQL 到逻辑计划

##### sql_pgq_translator/ - 特性门控
- `translate_sql_pgq()` - SQL/PGQ 到逻辑计划

##### graphql_rdf_translator/ - 特性门控
- GraphQL 到 RDF 的翻译

##### planner/
- `Planner` - 逻辑到物理计划
- `PhysicalPlan` - 物理计划

##### planner_rdf/ - 特性门控
- `RdfPlanner` - RDF 规划器

##### binder/
- 语义分析
- 变量绑定
- 类型检查

##### optimizer/
- `Optimizer` - 查询优化器
- `CardinalityEstimator` - 基数估计
- 基于成本的优化 (CBO)
- DPccp 连接排序

##### executor/
- `Executor` - 查询执行器
- 推式执行模型

##### cache/
- `QueryCache` - 查询缓存
- `CachingQueryProcessor` - 缓存查询处理器

##### processor/
- `QueryProcessor` - 统一查询处理接口
- `QueryLanguage` - 查询语言枚举
- `QueryParams` - 查询参数

#### transaction/
- `TransactionManager` - 事务管理器
- MVCC 实现
- 快照隔离
- `CommitInfo` - 提交信息
- `PreparedCommit` - 准备提交

#### session/
- `Session` - 会话管理
- 并发访问控制
- 轻量级会话句柄

#### catalog/
- `Catalog` - 目录管理
- `IndexDefinition` - 索引定义
- `IndexType` - 索引类型
- Schema 元数据

#### config/
- `Config` - 配置结构
- `ConfigError` - 配置错误
- `DurabilityMode` - 持久化模式
- `GraphModel` - 图模型选择

#### admin/
- `AdminService` - 管理服务
- `DatabaseInfo` - 数据库信息
- `DatabaseStats` - 数据库统计
- `DatabaseMode` - 数据库模式
- `WalStatus` - WAL 状态
- `IndexInfo` - 索引信息
- `SchemaInfo` - Schema 信息
- `LpgSchemaInfo` - LPG Schema 信息
- `RdfSchemaInfo` - RDF Schema 信息
- `DumpFormat` - 转储格式
- `DumpMetadata` - 转储元数据
- `ValidationError` - 验证错误
- `ValidationResult` - 验证结果
- `ValidationWarning` - 验证警告

#### cdc/ - 特性门控
- 变更数据捕获
- 历史记录 API
- 审计跟踪

#### embedding/ - 特性门控
- ONNX 嵌入生成
- 文本到向量转换

#### procedures/ - 特性门控
- 图算法过程
- SSSP（单源最短路径）
- PageRank
- 中心性
- 社区检测

**特性**:
- `default = ["gql", "parallel", "wal", "spill", "mmap"]`
- `gql`、`cypher`、`sparql`、`gremlin`、`graphql`、`sql-pgq` - 查询语言
- `rdf` - RDF 图模型
- `parallel` - 并行执行
- `algos` - 图算法
- `wal` - WAL 存储
- `spill` - 磁盘溢出
- `mmap` - 内存映射存储
- `vector-index` - HNSW 向量索引
- `text-index` - BM25 文本索引
- `hybrid-search` - 混合搜索
- `cdc` - 变更数据捕获
- `embed` - ONNX 嵌入生成
- `block-stm` - Block-STM 并行事务执行

---

### 6. grafeo-cli (命令行工具 Crate)

**路径**: `crates/grafeo-cli`

**职责**: 数据库管理的命令行接口

**核心模块**:

#### commands/
- CLI 命令实现
- `info` - 数据库概览
- `stats` - 详细统计
- `schema` - Schema 信息
- `validate` - 完整性检查
- `backup` - 备份和恢复
- `wal` - WAL 管理

#### output.rs
- 输出格式化
- 表格格式
- JSON 格式

**使用示例**:
```bash
grafeo info ./mydb
grafeo stats ./mydb --format json
grafeo schema ./mydb
grafeo validate ./mydb
grafeo backup create ./mydb -o backup
grafeo wal status ./mydb
grafeo wal checkpoint ./mydb
```

---

### 7. 语言绑定

#### Python 绑定 (PyO3)

**路径**: `crates/bindings/python`

**核心模块**:
- `database.rs` - PyGrafeoDB 类
- `query.rs` - 查询执行
- `types.rs` - 类型转换

**使用示例**:
```python
import grafeo

db = grafeo.GrafeoDB()
db.execute("INSERT (:Person {name: 'Alice'})")
result = db.execute("MATCH (p:Person) RETURN p")
```

#### Node.js/TypeScript 绑定 (napi-rs)

**路径**: `crates/bindings/node`

**核心模块**:
- `database.rs` - JsGrafeoDB 类
- `query.rs` - 查询执行和结果转换
- `types.rs` - JavaScript ↔ Rust 类型转换

**使用示例**:
```javascript
const { GrafeoDB } = require('@grafeo-db/js');
const db = await GrafeoDB.create();
await db.execute("INSERT (:Person {name: 'Alice'})");
```

#### Go 绑定 (CGO)

**路径**: `crates/bindings/go` (通过 c 绑定)

**特点**:
- 通过 C FFI 层访问
- 零开销绑定

#### C 绑定 (C FFI)

**路径**: `crates/bindings/c`

**核心模块**:
- `lib.rs` - C 兼容函数导出
- `types.rs` - C 安全类型包装

**用途**:
- 跨语言互操作
- Go 绑定的基础

#### WebAssembly 绑定 (wasm-bindgen)

**路径**: `crates/bindings/wasm`

**核心模块**:
- `lib.rs` - WASM 兼容数据库 API
- `types.rs` - JavaScript ↔ WASM 类型转换

**使用示例**:
```javascript
import { GrafeoDB } from '@grafeo-db/wasm';
const db = new GrafeoDB();
```

---

## 完整工作流程

### 1. 数据库初始化流程

```
用户调用 GrafeoDB::new_in_memory()
    ↓
调用 with_config(Config::in_memory())
    ↓
配置验证 (validate())
    ↓
创建 LpgStore (图存储)
    ↓
[可选] 创建 RdfStore (RDF 存储)
    ↓
创建 TransactionManager (事务管理器)
    ↓
创建 BufferManager (缓冲管理器)
    ↓
[可选] 初始化 WAL (写前日志)
    ↓
创建 QueryCache (查询缓存)
    ↓
返回 GrafeoDB 实例
```

### 2. 查询执行流程

```
Session::execute(query_string)
    ↓
QueryProcessor::process(query_string, language)
    ↓
Translator (根据语言选择):
    ├─ translate_gql()
    ├─ translate_cypher()
    ├─ translate_sparql()
    ├─ translate_gremlin()
    ├─ translate_graphql()
    └─ translate_sql_pgq()
    ↓
返回 AST (抽象语法树)
    ↓
Binder (语义分析):
    ├─ 变量绑定
    ├─ 类型检查
    └─ 属性验证
    ↓
返回 LogicalPlan (逻辑计划)
    ↓
Optimizer (查询优化):
    ├─ 基数估计
    ├─ 谓词下推
    ├─ 连接重排序 (DPccp)
    └─ 成本优化
    ↓
返回 LogicalPlan (优化后)
    ↓
Planner (物理规划):
    ├─ 选择物理算子
    ├─ 确定执行策略
    └─ 生成 PhysicalPlan
    ↓
Executor (查询执行):
    ├─ 推式执行
    ├─ 向量化处理
    ├─ [可选] 并行执行
    └─ [可选] 自适应执行
    ↓
返回 QueryResult
```

### 3. 数据写入流程

```
用户调用 db.create_node(labels, properties)
    ↓
TransactionManager::begin()
    ↓
生成 TxId (事务 ID)
    ↓
LpgStore::add_node(labels, properties)
    ↓
分配 NodeId
    ↓
[可选] 属性压缩存储
    ↓
创建 Version (MVCC 版本)
    ↓
添加到 VersionChain
    ↓
[可选] 写入 WAL (write-ahead log)
    ↓
更新索引 (如果有)
    ↓
TransactionManager::commit()
    ↓
生成 CommitInfo
    ↓
[可选] WAL 检查点
    ↓
返回 NodeId
```

### 4. 图遍历流程

```
用户查询: MATCH (a:Person)-[:KNOWS]->(b:Person)
    ↓
解析为 LogicalPlan:
    ├─ NodeScan: Person label
    ├─ EdgeTraversal: KNOWS relationship
    └─ Filter: b label = Person
    ↓
优化为 PhysicalPlan:
    ├─ use adjacency index (邻接索引)
    ├─ use hash index for label (标签哈希索引)
    └─ early filter pushdown (早期谓词下推)
    ↓
Executor 执行:
    ├─ 获取所有 Person 节点 (通过标签索引)
    ├─ 对每个节点遍历 KNOWS 边 (通过邻接索引)
    ├─ 过滤目标节点标签
    ↓
构建 DataChunk (向量化批次)
    ↓
流式返回结果
```

### 5. 向量搜索流程

```
用户查询: 寻找相似向量
    ↓
[如果没有索引] brute_force_knn():
    ├─ 计算所有向量距离
    ├─ 排序
    └─ 返回 top-k
    ↓
[如果有 HNSW 索引] HnswIndex::search():
    ├─ 入口层搜索
    ├─ 图遍历
    ├─ 候选筛选
    └─ 精确重排
    ↓
计算距离 (SIMD 加速):
    ├─ 余弦相似度
    ├─ 欧几里得距离
    └─ 点积
    ↓
返回 k-NN 结果
```

### 6. 混合搜索流程

```
用户查询: 文本 + 向量相似度
    ↓
文本搜索 (BM25):
    ├─ 分词
    ├─ 查询倒排索引
    ├─ 计算 BM25 分数
    ↓
向量搜索 (HNSW):
    ├─ 搜索 k-NN
    ├─ 计算相似度分数
    ↓
结果融合:
    ├─ Reciprocal Rank Fusion (RRF)
    ├─ 或加权融合
    ↓
返回混合排序结果
```

### 7. 事务管理流程 (MVCC)

```
开始事务
    ↓
生成 TxId 和 ReadVersion (读版本)
    ↓
读取数据
    ↓
查找 VersionChain
    ↓
根据 ReadVersion 选择合适的 Version
    ↓
修改数据
    ↓
创建新 Version
    ↓
添加到 VersionChain
    ↓
提交
    ↓
检查冲突
    ↓
成功: 更新 CommitVersion
    ↓
失败: 回滚 (丢弃新 Version)
    ↓
后台垃圾回收旧版本
```

### 8. 并行查询执行流程

```
查询提交
    ↓
并行检测
    ↓
MorselScheduler 分割任务
    ↓
生成 Morsels (工作单元)
    ↓
工作线程池
    ├─ Worker 1
    ├─ Worker 2
    ├─ Worker 3
    └─ Worker N
    ↓
每个 Worker 处理一个 Morsel
    ↓
向量化执行 (~1024 行/批次)
    ↓
结果合并
    ↓
返回最终结果
```

### 9. 持久化流程

```
用户修改数据
    ↓
[启用 WAL] 写入 WAL:
    ├─ 序列化为 WalRecord
    ├─ 追加到 WAL 文件
    ├─ 计算 CRC32 校验
    └─ [可选] fsync (根据配置)
    ↓
内存中数据更新
    ↓
[定期] 检查点:
    ├─ 脏页刷盘
    ├─ 创建快照
    ├─ 清理旧 WAL
    └─ [可选] 压缩
    ↓
崩溃恢复:
    ├─ 读取最新检查点
    ├─ 重放 WAL 记录
    └─ 重建内存状态
```

## 性能特性

### 执行特性
- **推式向量化执行**: 批次化处理 ~1024 行，启用 SIMD
- **Morsel 驱动并行性**: 自动检测线程数
- **列式存储**: 字典编码、Delta 编码、RLE 压缩
- **基于成本的优化器**: DPccp 连接排序、直方图
- **区域图**: 智能数据跳过（包括向量区域图）
- **自适应查询执行**: 运行时重新优化
- **透明溢出**: 核外处理支持
- **布隆过滤器**: 高效成员测试

### 索引优化
- **哈希索引**: O(1) 平均查找
- **邻接索引**: O(degree) 遍历
- **Trie 索引**: 多路连接优化
- **HNSW 向量索引**: O(log n) 近似最近邻
- **BM25 文本索引**: 全文搜索
- **Ring 索引**: RDF 3 倍空间减少

### 内存优化
- **Arena 分配器**: 高性能分配
- **分层存储**: 热冷数据分离
- **内存映射**: 大向量磁盘支持
- **压缩**: 位打包、RLE、Delta

## 性能目标

| 指标 | 目标 |
|------|------|
| 插入吞吐量 | 1M 节点/秒 |
| 边插入 | 500K 边/秒 |
| 点查询 | < 1μs |
| 1 跳遍历 | < 10μs |
| 2 跳遍历 | < 100μs |
| 三角查询 | < 1ms/1K 三角形 |
| PageRank (1M 节点) | < 1s |
| 内存开销 | < 100 字节/节点 |

## 实现状态

### Phase 1: 基础 ✅
- 区域图
- 字典编码
- 基于成本的连接 (DPccp)
- 统计收集

### Phase 2: 内存与执行 ✅
- 统一缓冲管理器
- 推式执行
- 自适应块大小
- 邻接压缩

### Phase 3: 并行 ✅
- Morsel 调度器
- 透明溢出
- 自动线程检测

### Phase 4: 优化 ✅
- 整数压缩
- 布隆过滤器
- 直方图
- RLE
- 属性压缩
- 自适应执行

## 查询语言与数据模型支持

| 查询语言 | LPG | RDF |
|----------|-----|-----|
| GQL | ✅ | - |
| Cypher | ✅ | - |
| Gremlin | ✅ | - |
| GraphQL | ✅ | ✅ |
| SPARQL | - | ✅ |
| SQL/PGQ | ✅ | - |

### 数据模型
- **LPG**: 节点带标签和属性，边带类型和属性。适合社交网络、知识图谱
- **RDF**: 三元组存储 (subject-predicate-object)，SPO/POS/OSP 索引。适合语义网络、链接数据

## 生态系统

| 项目 | 描述 |
|------|------|
| [grafeo-server](https://github.com/GrafeoDB/grafeo-server) | HTTP 服务器 & Web UI: REST API、事务、单一二进制 (~40MB Docker) |
| [grafeo-web](https://github.com/GrafeoDB/grafeo-web) | 基于 WebAssembly 的浏览器 Grafeo，IndexedDB 持久化 |
| [grafeo-langchain](https://github.com/GrafeoDB/grafeo-langchain) | LangChain 集成: 图存储、向量存储、Graph RAG 检索 |
| [grafeo-llamaindex](https://github.com/GrafeoDB/grafeo-llamaindex) | LlamaIndex 集成: PropertyGraphStore、向量搜索、知识图谱 |
| [grafeo-mcp](https://github.com/GrafeoDB/grafeo-mcp) | MCP 服务器: 为 LLM 代理暴露 Grafeo 工具 |
| [grafeo-memory](https://github.com/GrafeoDB/grafeo-memory) | AI 内存层: 事实提取、去重、语义搜索 |
| [anywidget-graph](https://github.com/GrafeoDB/anywidget-graph) | Python 笔记本交互式图可视化 |
| [anywidget-vector](https://github.com/GrafeoDB/anywidget-vector) | Python 笔记本 3D 向量/嵌入可视化 |
| [graph-bench](https://github.com/GrafeoDB/graph-bench) | 基准测试套件，25+ 基准测试比较图数据库 |

## 总结

Grafeo 是一个设计精良的高性能图数据库，具有以下优势：

1. **架构清晰**: 分层设计，职责明确，无循环依赖
2. **性能优化**: 向量化执行、列式存储、多种压缩算法
3. **灵活性**: 多查询语言、多数据模型、多存储后端
4. **可嵌入**: 零外部依赖，可作为库使用
5. **可扩展**: 插件架构，支持自定义函数和算法
6. **多语言**: 支持多种编程语言的绑定
7. **现代化**: 采用最新的 Rust 特性和最佳实践

该项目展示了如何在 Rust 中构建一个高性能、可扩展、用户友好的数据库系统，其架构和实现值得学习和借鉴。
