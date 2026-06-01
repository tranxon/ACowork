# 记忆系统竞品对标分析（11维度）

**审查日期**：2026-04-22
**审查范围**：RollBall v3.6 记忆设计 vs mem0/LightMem/HippoRAG/zeroclaw
**依据文档**：docs/reference/memory_system_comparison_11_dimensions.md
**审查文档**：docs/05-memory.md v3.6、docs/module-design/04-grafeo.md、docs/plan/plan-p2.md v1.4

---

## 执行摘要

基于对 RollBall 记忆设计、Grafeo 模块设计、Phase 2 计划与竞品对比文档（11维度分析）的深入研究，本报告从 **11 个工程维度** 逐一对标 RollBall 与 mem0/LightMem/HippoRAG/zeroclaw 的设计差异，评估 RollBall 的领先性与差距。

**关键发现**：
- **RollBall 的独特优势**（3 个维度明显领先）：关联扩散检索（基于 Grafeo 原生图遍历 + PageRank）、检索与注入拆分设计（Token 精细管理）、生命周期架构解耦
- **RollBall 的关键差距**（5 个维度落后）：质量评估体系、冲突处理成熟度、生命周期间流动智能化、分层容量管理、程序记忆与 Skill 系统联动
- **需立即补充的设计**（3 个高优先级）：标准化基准测试、冲突分类 LLM 精排、ProceduralNode 聚合策略

---

## A. 维度逐一对标

### 维度 1：记忆检索机制

#### 竞品概况
| 系统 | 主检索方式 | 混合检索权重 | 多跳推理 |
|------|----------|-----------|--------|
| mem0 | 向量相似度 + BM25 | 向量为主，BM25 融合 | 无原生支持 |
| LightMem | 三路检索（context/embedding/hybrid） | 依策略选择 | 无原生支持 |
| HippoRAG | PersonalizedPageRank 图遍历 + dense passage | 图遍历为主 | **核心能力** |
| zeroclaw | 三阶段管线：LRU → FTS → 向量 | 向量 0.7 + 关键词 0.3 | Knowledge Graph 五种关系遍历 |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §6（关联扩散检索）、`docs/module-design/04-grafeo.md` §检索能力

**关键设计**：
1. **混合检索**（§6.1）：`db.hybrid_search()` — Grafeo 原生 RRF 融合 + `topology_boost` 图连通性重排
2. **关联扩散**（§6.2-6.3）：GQL 原生图遍历 `MATCH (m)-[r*1..3]-(other)`，1-2 跳常规路径，3 跳为探索上限
3. **PageRank 集成**：`CALL grafeo.pagerank()` 评估节点全局重要性，作为手调 `importance` 补充
4. **社区检测**：`CALL grafeo.louvain()` 识别记忆社区，指导扩散优先级
5. **MMR 多样性**：`db.mmr_search()` 保证结果语义多样性

**判断**：**领先** ✅

**理由**：
- HippoRAG 的 PPR 是 1-3 跳推理，RollBall 的图遍历 + PageRank + topology_boost + 社区检测形成了"图+算法"的多重组合
- zeroclaw 的三阶段管线（缓存→FTS→向量）是工程级优化，RollBall 的检索降级策略（§6.1 Level 0-3）更完善
- HippoRAG 的图遍历专门针对多跳 QA，RollBall 的检索与注入拆分使其能灵活适配不同上下文需求

**存在的差距**：
- Level 0 混合检索的 RRF 权重配置写死，未参考 zeroclaw 的动态权重调整（需补充参数化）
- graph_expand 的早期终止阈值 [0.1, 0.15, 0.2] 是固定值，未基于查询动态调整

---

### 维度 2：记忆注入策略

#### 竞品概况
| 系统 | 注入位置 | 注入格式 | 注入时机 | 特点 |
|------|---------|---------|---------|------|
| mem0 | 调用方决定 | JSON {results: [...]} | 每次对话前调用 | 最灵活 |
| LightMem | Context 拼接用户消息前 | MemoryEntry 列表 | 检索结果自动拼接 | 无缝集成 |
| HippoRAG | RAG passage 拼接 | QuerySolution 对象 | retrieve() 独立调用 | 文档特定 |
| zeroclaw | `[Memory context]...[/Memory context]` 独立块 | `key: content` 列表 + score | 每轮对话前自动，time decay 过滤 | **最结构化** |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §1（瞬态层 Token 管理）、§6.1（检索降级）

**关键设计**：
1. **两条独立裁剪流水线**（§1）：
   - 流水线 A：对话历史三阶段渐进裁剪（折叠→FIFO→摘要）
   - 流水线 B：检索结果按优先级砍（从 episodic 到自传体）
2. **Token 预算分配**（§1）：固定区（system + output）+ 可分配区（history 75% / retrieval 25%）
3. **注入格式**（§1）：System Prompt 首部、Retrieved Memory 独立块、Conversation 折叠引用 + tool 结果
4. **内容折叠**（§1）：大段内容折叠为 `📎 path@L1..L150` / `📎 tool:name()` / `📎 inline#hash`

**判断**：**持平** ⚖️

**理由**：
- zeroclaw 的 `[Memory context]` 独立块是最干净的架构，RollBall 的多块级联（System/Retrieved/Conversation/Scratchpad）更复杂但信息密度更高
- RollBall 的三阶段渐进裁剪（折叠→FIFO→摘要）比 mem0/LightMem 的"返回原始 + 交给调用方"更细致
- RollBall 的内容折叠策略（文件→path+hash、代码→artifact_refs）体现了对"什么该住在记忆、什么该住在文件系统"的深层认知

**存在的差距**：
- **缺少时间衰减过滤**：zeroclaw 在注入前做 time decay，低分结果自动过滤。RollBall 在裁剪阶段未考虑节点的 decay_score，可能注入"僵尸节点"
- **Memory Hint 指令复用性不足**：§1 中 `memory_hint.type` 区分 s/f/r/i 四类，但下轮检索参数调整（RRF 权重、BM25 加强）的逻辑未文档化

---

### 维度 3：记忆冲突处理

#### 竞品概况
| 系统 | 冲突检测 | 冲突解决 | 冲突范围 | 回退机制 |
|------|---------|---------|---------|----------|
| mem0 | LLM 判断冲突，输出 ADD/UPDATE/DELETE | LLM 决定更新内容，旧记忆标记 UPDATE | 全部记忆类型 | JSON 解析失败 fallback |
| LightMem | 向量相似度 0.9，LLM 仲裁 | merge/replace/delete/keep | 全部条目 | 单条失败不影响其他 |
| HippoRAG | 无冲突处理（只读） | 不适用 | 不适用 | 不适用 |
| zeroclaw | 向量 0.85 + Jaccard 双模式 | `superseded_by` 标记软删除 | 仅 Core 类 | 无向量嵌入时 Jaccard 降级 |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §6.4（冲突处理）、`docs/review/04-p2-s2-design-review.md` §6.9

**关键设计**：
1. **即时阶段检测**（§6.4）：embedding 相似度 > 0.85 时标记候选冲突，记录 conflict_pair
2. **离线阶段精排**（§6.4）：LLM 批量处理候选冲突，输出 evolution/correction/ambiguous
3. **自动解决**（§6.4）：
   - evolution：新值 Active，旧值 Dormant，写 conflict_log
   - correction：新值 Active，旧值 Dormant，降低旧来源可信度
   - ambiguous：两个都 Active，标记 conflict_group_id
4. **用户确认**（§6.4）：ambiguous 累计 3+ 时，Agent 在对话中自然询问用户

**判断**：**落后** ❌

**理由**：
- mem0 的 LLM 冲突判断在即时阶段就做，RollBall 延迟到离线巩固（效率低）
- zeroclaw 的 `superseded_by` 字段和双模式检测（向量 + Jaccard）比 RollBall 的单向量检测更鲁棒
- LightMem 的离线 `offline_update` 机制（睡眠巩固中批量处理冲突）与 RollBall 类似，但 LightMem 在冲突后续追踪中有更多细节（merge 策略）

**存在的差距**：
1. **冲突检测阈值固定**：0.85 是手调的，未根据 KnowledgeNode 类型（Fact/Preference）动态调整
2. **缺少相似度之外的冲突信号**：
   - 时间冲突：同一用户在短期内说出矛盾信息（"我喜欢咖啡" → 24h 后 "我讨厌咖啡"）应该比相似度 > 0.85 的两条陈述更优先触发冲突处理
   - 上下文冲突：新 fact 的 source_episode 中有明确的否定词（"不是"、"其实"）应该标记为 correction
3. **缺少冲突分类的自动决策逻辑**（目前完全依赖 LLM 仲裁）：
   - evolution 可由时间差和上下文词汇简单判断（"搬家了"、"改变主意"）
   - correction 可由否定词和反向量检测
4. **ambiguous 的"用户询问"流程未实现**：文档说"自然地询问"但具体机制未定义（何时触发、以什么形式注入 prompt）

---

### 维度 4：记忆与 LLM 的交互质量

#### 竞品概况
| 系统 | LLM 调用场景 | Prompt 工程 | Token 优化 | 容错 |
|------|----------|-----------|----------|------|
| mem0 | 事实抽取、冲突判断、procedural 生成、实体提取 | 动态构建 7 类用户信息模板 | 输入截断、UUID 映射 | JSON 失败 fallback |
| LightMem | 预压缩、话题分割、元数据、摘要、offline 仲裁 | 多视角 factual/relational | LLMLingua-2 压缩 | 配置化容错 |
| HippoRAG | OpenIE 三元组抽取、NER、QA 推理 | 模板化 | 批量 OpenIE 减调用 | 结果缓存 |
| zeroclaw | 仅 consolidation 提取 history_entry | 硬编码 CONSOLIDATION_SYSTEM_PROMPT | 截断 4000 字符 | markdown 包装容错 |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §0.1（LLM 优先原则）、§1（Memory Hint 指令）、§2（Tool Result 摘要）、§4.1-4.2（巩固管道）

**关键设计**：
1. **LLM 优先原则**（§0.1）：信任 LLM 超过规则——即时提取无预过滤，离线巩固用 LLM 在完整上下文下决策
2. **即时提取简化**（§4.1）：memory_store 工具改为自然语言 + 类型标签，不做三元组拆分
   - 工具定义约 40 tokens（含 Memory Hint 指引）
   - keywords 可复用 memory_hint.e，自动合并
3. **离线巩固深度提取**（§4.2）：
   - 发现隐式关联（同一主体多次出现但未显式存储）
   - 检测知识冲突、提炼跨 Skill 模式、评估现有 ProceduralNode、增强 Artifact 摘要
4. **Tool Result 摘要零 LLM**（§2）：纯 Runtime 模板提取（文件→"读取 X，共 N 行"、命令→"执行 X，退出码 N"）
5. **Token 分层计数**（§1）：Tier 1 精确（tiktoken-rs）、Tier 2 近似（采样）、Tier 3 启发式

**判断**：**持平** ⚖️

**理由**：
- mem0 的 LLM 频繁调用（每次对话都可能抽取 + 冲突判断 + 生成）导致高成本，RollBall 的"即时 + 离线"两分法更经济
- zeroclaw 最保守（仅 consolidation），RollBall 的 memory_hint 指引让即时提取更精准
- LightMem 的 LLMLingua-2 预压缩是 token 优化的独特方案，RollBall 没有对标

**存在的差距**：
1. **离线巩固 Prompt 设计不够精细**（§4.2 示例较粗糙）：
   - 应分离"显式冲突检测"（算法）和"隐式关联发现"（LLM）两个子流程
   - 当前 prompt 试图一次性处理 5 个任务（关联、冲突、模式、评估、增强），容易导致 LLM 漏掉某个方面
2. **缺少 Prompt 工程的演进机制**（无 A/B 测试框架，无结果评估反馈）
3. **Tool Result 摘要未考虑语义增强**：Phase 3 离线巩固时可以用 LLM 增强摘要（"这个文件实现了 X 模式"）但目前文档只定义了模板方式

---

### 维度 5：质量评估体系

#### 竞品概况
| 系统 | 内置评估 | 基准测试 | 质量指标 | 外部验证 |
|------|---------|---------|---------|---------|
| mem0 | LLM Judge 评分、生成式评估 | 自定义评估集 | LLM Judge 分数 | OpenMemory 生产级部署 |
| LightMem | LLM Judge 评估（LoCoMo/LongMemEval） | LoCoMo、LongMemEval | F1、BLEU、LLM Judge | 论文：成本↓117倍、API↓159倍、准度↑10.9% |
| HippoRAG | 检索召回率、QA 准确率 | MuSiQue、HotpotQA | Recall@K、EM、F1 | 论文：多跳推理显著优于标准 RAG |
| zeroclaw | **无内置评估** | **无** | **无** | 社区使用反馈 |

#### RollBall 当前设计覆盖
**文档位置**：`docs/plan/plan-p2.md` §2.5.6（S2.12 质量评估框架）

**关键设计**：
1. **可观测指标**（S2.12.1）：节点分布、检索统计、冲突统计、衰减统计
2. **SLA 定义**（S2.12.2）：hybrid_search P99 < 100ms（1K 节点）、P99 < 500ms（10K 节点）
3. **Phase 3 对接 LongMemEval**（S2.12.3）：5 维评估（IE/MR/TR/KU/Abs）

**判断**：**未覆盖** ❌

**理由**：
- 当前 Phase 2 计划仅定义了 SLA（性能指标）和基础可观测性
- 缺少语义质量评估框架（类似 mem0 的 LLM Judge、LightMem 的 F1 指标）
- zeroclaw 也缺评估体系，但 RollBall 作为平台级系统应该更重视

**存在的差距**：
1. **缺少在线评估框架**：应在每次检索后自动评估"返回结果是否对用户有帮助"
   - LLM Judge：用小型 LLM 评估检索结果相关性（快速）
   - 用户隐式反馈：Agent 的后续输出是否使用了检索结果、用户是否满意
2. **缺少离线基准测试集**：
   - 应建立"用户提问 + 期望答案 + 相关记忆"的测试集
   - 定期跑 Recall@K、MRR、NDCG 等指标
3. **缺少冲突处理的成功率评估**：ambiguous 类冲突用户确认时的准确率、evolution/correction 的自动判断准确率
4. **缺少衰减参数校准机制**：lambda、importance 阈值等目前是手调的，无数据驱动的调参流程

---

### 维度 6：工程约束

#### 竞品概况
| 系统 | 语言/运行时 | 并发模型 | 依赖量 | 配置复杂度 | 部署模式 |
|------|----------|---------|--------|----------|---------|
| mem0 | Python | 同步 + 部分 async | 重（20+ LLM/embedding/向量库 provider） | `MemoryConfig` 多子配置 | SDK + FastAPI 服务 |
| LightMem | Python | 多线程 offline_update | 中（Qdrant/HuggingFace/OpenAI/LLMLingua-2） | 高（6 层 Layer 配置） | SDK |
| HippoRAG | Python | 批量处理 | 中（igraph/embedding/LLM backend） | 全局配置 | SDK + CLI |
| zeroclaw | Rust | tokio 异步 | 轻（rusqlite/parking_lot/tokio/serde） | `SearchMode` 枚举 + `RetrievalConfig` | 单二进制 + CLI |

#### RollBall 当前设计覆盖
**文档位置**：`docs/module-design/04-grafeo.md`、`docs/05-memory.md` §10（生命周期架构）

**关键设计**：
1. **并发模型**（Grafeo）：GrafeoDB 内置 MVCC 快照隔离，多 Session 无需自研锁
2. **依赖量**（Phase 2 后）：
   - rollball-grafeo：grafeo-engine（含 lpg/gql/vector-index/text-index/hybrid-search/wal/algos/cdc）
   - rollball-runtime：tokio + LLM provider crates（OpenAI/Anthropic）
3. **配置复杂度**（§10）：
   - `MemoryStore` trait（轻）
   - `DecayConfig`（参数化）
   - `GrafeoConfig`（backing store 相关）
4. **部署模式**：
   - Phase 1：Agent 进程内嵌 GrafeoStore
   - Phase 3+：可选云端 RemoteMemoryStore

**判断**：**持平** ⚖️

**理由**：
- zeroclaw 的轻量级约束（Rust 编译期类型安全 + SQLite 单文件）vs RollBall 的灵活性（Grafeo LPG 支持多种查询模式 + 原生图算法）是不同的设计权衡
- RollBall 的配置通过 trait 参数化使其优于 Python 系统的"全局配置 vs SDK 配置"混乱
- Grafeo 的 CDC + PageRank + Louvain 等内置能力替代了 zeroclaw 需要自研的索引管理逻辑

**存在的差距**：
1. **缺少 feature-gated 的轻量级模式**：
   - 应支持"最小化记忆"配置（仅 episodic 层，无图遍历、无衰减）用于轻量级 Agent
   - 当前设计没有这样的"配置档位"
2. **缺少性能 benchmarking 框架**：
   - 应定义不同规模（1K/10K/100K 节点）的性能基准
   - 当前仅有 SLA 定义，无实测数据
3. **向量嵌入的降级策略文档不完整**（§5.3 说有三级但具体条件未定）

---

### 维度 7：隐私访问控制

#### 竞品概况
| 系统 | 多租户隔离 | 数据访问控制 | 数据导出 | 删除权限 | 加密 |
|------|----------|-----------|---------|--------|------|
| mem0 | `user_id/agent_id/run_id` 三维度 | `filters` 查询限定 + metadata 9 种操作符 | `get_all()` 支持 filter 导出 | 单条+批量 | 无 |
| LightMem | `user_id` 参数化 | 向量数据库 payload 过滤 | 无内置导出 | 无显式 API | 无 |
| HippoRAG | 无多租户设计 | 无 | 无 | 无 | 无 |
| zeroclaw | `namespace` 隔离 + 装饰器强制 | `PolicyEnforcer` 策略引擎（只读/配额/保留期） | `export(filter)` GDPR Art.20 | 单条+批量+namespace | Vault 集成（P1 审查指出未接入） |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §3.1-3.3（PrivacyLevel）、§7（跨 Agent 知识共享）、`docs/plan/plan-p2.md` §2.5.11（S2.11 隐私访问控制）

**关键设计**：
1. **硬架构隔离**：独立进程 + 独立 Grafeo 文件（`<workspace>/memory/private.grafeo`）
2. **PrivacyLevel 标记**（§3.1）：Public / Personal / Sensitive
   - 用于打包分享时过滤（剥离 Personal/Sensitive）
   - 非用于网络同步（LLM 上下文内容无访问控制）
3. **跨 Agent 数据共享**（§7）：
   - Intent 查询（主路径）：主动查询，返回结果自动过滤敏感信息
   - 系统 Agent：托管身份信息，缓存 + 降级机制
   - 云端同步：Zone-Based（Identity/Preferences/Knowledge/Work）
4. **打包分享**：PrivacyLevel 独立于 Zone，Personal/Sensitive 一律剥离

**判断**：**持平** ⚖️

**理由**：
- zeroclaw 的 `NamespacedMemory` 装饰器和 `PolicyEnforcer` 提供了运行时的细粒度访问控制，RollBall 的"硬隔离 + 打包时过滤"是架构级设计
- 两者的权衡不同：zeroclaw 允许共享存储（多 namespace），RollBall 完全隔离。前者更复杂但支持共享，后者更简单但需 Intent 查询
- mem0 的 `user_id/agent_id/run_id` 三维度与 RollBall 的 Zone-Based 都是元数据维度的隔离，都不够强

**存在的差距**：
1. **缺少运行时访问控制**：当前所有隐私控制都在"打包分享"这个非运行时阶段
   - 应定义 Agent A 在运行时何时可以查询 Agent B 的记忆（基于 Intent 权限）
   - 当前文档未明确这一点
2. **缺少数据导出/GDPR 支持**：
   - zeroclaw 有 `export()` 接口支持 GDPR Art.20（数据可移植性）
   - RollBall 未定义导出格式和流程
3. **打包分享时的 Personal/Sensitive 剥离缺少审计**：
   - 应记录什么时候被分享给谁、剥离了哪些数据
   - 当前无审计日志

---

### 维度 8：存储格式

#### 竞品概况
| 系统 | 向量存储 | 关键词索引 | 知识图谱 | 元数据存储 | 记忆条目结构 |
|------|---------|----------|--------|----------|-----------|
| mem0 | Qdrant/Chroma/Milvus 等多选 | BM25（text_lemmatized） | Phase 7 实体链接+关系 | SQLite | {id/memory/event/hash/...} |
| LightMem | Qdrant | BM25（context_retriever） | graph_mem 可选 | JSON 文件 | MemoryEntry + 压缩版本 |
| HippoRAG | JSON 持久化 | 无独立索引 | igraph 原生 | JSON 文件 | 三层：chunk/entity/fact |
| zeroclaw | SQLite BLOB 余弦相似度 | SQLite FTS5 | 5 NodeType + 5 Relation | SQLite 单库 | {id/key/content/category/...} |

#### RollBall 当前设计覆盖
**文档位置**：`docs/module-design/04-grafeo.md` §LPG 数据模型、§索引说明、`docs/05-memory.md` §2-3（经历层/沉淀层）

**关键设计**：
1. **向量存储**：Grafeo 原生 HNSW，支持余弦/欧几里得/点积/曼哈顿，384 维（all-MiniLM-L6-v2）
2. **关键词索引**：Grafeo 原生 BM25，Unicode 分词
3. **知识图谱**：Grafeo LPG，7 个 Label（Episodic/Knowledge/Procedural/Autobiographical/SystemConfig/ToolInvocation/Session）+ 5 种 Edge Type
4. **元数据存储**：单 `.grafeo` LPG 文件，含 WAL + 自动恢复
5. **记忆条目**：
   - Episode：{episode_id/timestamp/role/content/embedding/metadata/session_id/consolidated/importance}
   - KnowledgeNode：{node_id/type/subject/predicate/object/confidence/source_episode/importance/status/privacy}
   - 边权重 = confidence_avg × recency_factor

**判断**：**领先** ✅

**理由**：
- Grafeo 的 LPG 模型（Label + Property + Edge）比 SQLite 表结构（zeroclaw）或分散式存储（mem0 向量+元数据分离）更原生支持图遍历
- Grafeo 的 CDC (Change Data Capture) 提供了完整的审计追踪，RollBall 利用它做"经验回溯"和"冲突调解"
- zeroclaw 的"全部存 SQLite BLOB"换取零外部依赖，RollBall 的 LPG 换取查询能力，权衡合理

**存在的差距**：
1. **缺少数据模型版本化机制**：
   - 文档说 LPG "无版本化 Schema 迁移概念"，但如果未来需要扩展 KnowledgeNode 的字段会怎么办？
   - 应定义向后兼容的扩展策略
2. **ArtifactRef 结构不完整**（§2）：
   - path/hash/description/line_range/modified_at 共 5 字段
   - 缺少"文件是否仍存在"的验证标志（重构后文件可能被删除）
3. **Episode 的 metadata 字段没有 schema**：
   - 当前说"上下文元数据（话题、情感倾向等）"但具体字段未定义
   - 这会导致后续查询 metadata 时的兼容性问题

---

### 维度 9：生命周期管理

#### 竞品概况
| 系统 | 创建触发 | 更新策略 | 遗忘机制 | 归档/压缩 | 保留策略 |
|------|---------|---------|---------|----------|---------|
| mem0 | `add()` 显式 | LLM ADD/UPDATE/DELETE | `delete()` 显式 | 无 | 无自动保留策略 |
| LightMem | `add_memory()` 显式 + 话题分割自动 | online（空）+ offline（非交互时批量） | offline 中 LLM 仲裁 | LLMLingua-2 预压缩 + compressed_memory | 无 |
| HippoRAG | `index()` 批量构建 | 无（只追加） | 无 | 无 | 无 |
| zeroclaw | `consolidate_turn()` 每轮自动 | LLM 提取 + 冲突检测 + superseded_by 标记 | Time decay（半衰期 7 天）+ 显式 delete + 保留策略 | snapshot + 分类保留期 | 按分类可配置保留天数 |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §4（巩固管道）、§5（遗忘机制）、§9（分阶段实现路线）

**关键设计**：
1. **创建触发**：
   - 即时：Tool Call `memory_store` 自主判断（显式）
   - 离线：空闲 30 分钟/未巩固积攒 50 条/用户手动触发（显式）
   - 对话记录：每轮对话后异步写入 episode（自动）
2. **更新策略**：
   - 即时：embedding 相似度 > 0.85 标记候选冲突，立即可检索
   - 离线：LLM 分类冲突、合并知识、提炼模式（Phase 3）
3. **遗忘机制**（§5）：
   - Active → Dormant：decay_score < 0.3
   - Dormant → Purge：
     - 路径 1：Dormant > 90天 AND importance < 0.5（Fact/Relation 永不 Purge）
     - 路径 2：容量压力（存储 > 90%）
     - 路径 3：用户手动
4. **归档/压缩**：
   - episodic：已巩固 + 超 7 天自动清理
   - 离线巩固：增强 Artifact 摘要（Phase 3）
5. **保留策略**（§9）：通过 DecayConfig 按分类可配置

**判断**：**持平** ⚖️

**理由**：
- zeroclaw 的"每轮自动 consolidate + time decay"最简单直接
- RollBall 的"即时 + 离线"两分法覆盖了更多场景（显式保存 + 深度提取）
- LightMem 的"offline_update 睡眠巩固"与 RollBall 的离线巩固本质相同，都是批量处理

**存在的差距**：
1. **即时 + 离线的协调机制不清楚**（§4.1 vs §4.2）：
   - 即时提取做了什么（PendingKnowledgeNode）、离线巩固做什么（正式 KnowledgeNode）的边界模糊
   - 应明确：是即时就升级为正式，还是离线时再升级？
2. **离线巩固触发的全局协调缺失**（§9 说"多 Agent 场景下增加全局协调限制"但未具体定义）：
   - 如果 Agent A 和 B 同时触发离线巩固，是否有竞争或冲突？
   - 当前文档未明确
3. **ProceduralNode 的聚合策略不完整**：
   - 同一 trigger_condition 出现 3 个 action_pattern 时应合并，但具体合并逻辑未定义
   - 应定义"何时合并"、"如何验证合并是否正确"的流程
4. **AutobiographicalNode History 的摘要压缩阈值硬编码**（10 条摘要）：
   - 应通过 manifest 配置，支持不同 Agent 的不同阈值

---

### 维度 10：持久化

#### 竞品概况
| 系统 | 默认后端 | 备选后端 | 持久化保证 | 迁移支持 | 跨进程 |
|------|---------|---------|----------|---------|--------|
| mem0 | Qdrant（向量）+ SQLite（元数据） | Chroma/Milvus/PgVector 等 | Qdrant WAL + SQLite ACID | 无 schema 迁移 | OpenMemory 独立服务 |
| LightMem | Qdrant + JSON 文件 | 无 | 依赖 Qdrant | 无 | MCP Server |
| HippoRAG | JSON 文件 + igraph 序列化 | 无 | 文件写入（无事务） | 无 | 无 |
| zeroclaw | SQLite（brain.db） | MarkdownMemory/QdrantMemory/NoneMemory | SQLite WAL + NORMAL 同步 + mmap | `safe_reindex` 原子性迁移 | 单进程（未来 Gateway IPC） |

#### RollBall 当前设计覆盖
**文档位置**：`docs/module-design/04-grafeo.md` §索引说明、`docs/05-memory.md` §5.2（Purge 流程）

**关键设计**：
1. **默认后端**：`.grafeo` 单文件，Grafeo 原生 WAL
2. **备选后端**（Phase 3+）：InMemoryStore（测试）+ RemoteMemoryStore（云端）
3. **持久化保证**：Grafeo WAL + MVCC 快照隔离
4. **迁移支持**：
   - 无 Schema 版本化（LPG 无固定 Schema）
   - 索引通过 API 动态创建
   - 数据迁移通过 GQL 导出/导入（待实现）
5. **跨进程**：Phase 1 单进程内嵌，Phase 3+ 云端 RemoteMemoryStore

**判断**：**持平** ⚖️

**理由**：
- zeroclaw 的 `safe_reindex` 原子性迁移是成熟的工程实践，RollBall 依赖 Grafeo 的 WAL 机制
- RollBall 的 GQL 导出/导入灵活性好，但未具体实现
- 两者都没有完整的"多版本共存"机制（如 A 机器跑 v1 Schema，B 机器升到 v2，如何同步）

**存在的差距**：
1. **迁移流程未具体定义**：
   - 应定义从 rusqlite（Phase 1）到 grafeo-engine（Phase 2）的迁移脚本
   - 当前代码审查报告 (01-code-review.md) 说数据迁移是 Phase 2 任务，但 plan-p2.md 中未见明确任务
2. **备份与恢复的 RTO/RPO 指标缺失**：
   - zeroclaw 的 `safe_reindex` 设计是对 RPO = 0 的追求
   - RollBall 的 Grafeo WAL 可做到 RPO = 0，但文档未量化
3. **跨进程通信的数据一致性**（Phase 3 云端）：
   - 多设备同时修改记忆时的冲突解决机制未定义
   - 当前文档说"单向同步（云端→Agent）"但未明确多设备场景如何处理

---

### 维度 11：记忆层级分类

#### 竞品概况
| 系统 | 层级模型 | 跨层流转 | 层级间检索 | 信息升级 |
|------|---------|---------|----------|---------|
| mem0 | 隐式两型：procedural + 通用 | 无自动流转 | 单层 | 无 |
| LightMem | 显式三层：Sensory → Short-term → Long-term + 6 种 Layer 实现 | 话题分割触发 | 可选单层或跨层 | 话题连贯性触发摘要 |
| HippoRAG | 隐式三层：chunk → entity → fact | chunk → OpenIE → 图索引 | 固定路径 | 无 |
| zeroclaw | 显式三分类：Core/Daily/Conversation + Custom | `consolidate_turn()` 自动流转 | 跨命名空间检索 + 统一注入 | 对话→Core（LLM 提取） |

#### RollBall 当前设计覆盖
**文档位置**：`docs/05-memory.md` §0（分层原则）、§1-3（瞬态层/经历层/沉淀层）

**关键设计**：
1. **四层模型**（§0）：
   - 瞬态层：工作记忆（LLM 上下文窗口）
   - 经历层：情景记忆（episodic，临时编码）
   - 沉淀层：
     - 语义记忆（Fact/Preference/Relation）
     - 程序记忆（行为模式）
     - 自传体记忆（自我认知）
   - dormant：衰减沉睡状态
2. **跨层流转**（§0 流动规则）：
   - 瞬态 → 经历：对话持久化
   - 经历 → 沉淀：即时提取 + 离线回放
   - 沉淀 → 瞬态：检索注入
   - 沉淀 → dormant：衰减
3. **层级间检索**（§6）：并行检索经历+沉淀，跨层关联扩散（episode → source_episode → knowledge）
4. **信息升级**（无专门章节）：经历层 episode 通过 Tool Call 升级为沉淀层 KnowledgeNode

**判断**：**领先** ✅

**理由**：
- RollBall 的"四层"模型（瞬态/经历/沉淀/dormant）比 zeroclaw 的"三分类"（Core/Daily/Conversation）更细致
- LightMem 的"六种 Layer 实现"是可插拔的，RollBall 的 MemoryStore trait 也支持可替换，但 RollBall 的层级设计更贴近认知科学
- HippoRAG 的"chunk→entity→fact"是知识组织，RollBall 的"瞬态→经历→沉淀"是记忆保留，不可直接对比

**存在的差距**：
1. **层级间的"升级"决策逻辑不透明**：
   - 经历层 episode 何时升级为沉淀层 KnowledgeNode？
   - 当前文档说"即时提取或离线巩固"但阈值未定义（是 importance > 0.5 还是其他？）
2. **瞬态→经历的折叠策略与沉淀层的关联不清**（§1 的三阶段裁剪 vs §6 的跨层扩散）：
   - 被裁剪的消息对转为 ArtifactRef 存入经历层，但 ArtifactRef 在沉淀层检索时如何利用？
   - 应明确"被动裁剪转换"与"主动提取"的协调机制
3. **Daily/Conversation 层级（对标 zeroclaw）在 RollBall 中缺失**：
   - 当前 RollBall 的经历层统一为 Episodic Label，无子分类
   - 应考虑区分"日志型 episode"和"对话型 episode"，便于不同的保留策略

---

## B. 综合评估

### B.1 RollBall 的独特优势

#### 1. 关联扩散检索（图算法 + 重要性评估）
- **维度**：维度 1（记忆检索机制）、维度 8（存储格式）
- **表现**：RollBall 的 GQL 原生图遍历 + PageRank + topology_boost + 社区检测形成了"多层次的关联发现"
- **对标差异**：
  - HippoRAG 的 PPR 专为多跳 QA 优化，RollBall 的图遍历用途更广泛（可用于任何类型的记忆扩散）
  - zeroclaw 的 KG 遍历是关系类型的显式路由，RollBall 的边权重系统更灵活
- **价值**：从"查询" → "推理" 的升级，让记忆检索不止是检索，还能发现"隐性关联"

#### 2. 检索与注入的拆分设计
- **维度**：维度 2（记忆注入策略）、维度 5（质量评估体系）
- **表现**：
  - Retrieve 阶段关注"查什么"（hybrid_search + graph_expand）
  - Inject 阶段关注"怎么放"（Token 预算、优先级、内容折叠）
  - 两个独立的裁剪流水线（历史 + 检索）
- **对标差异**：mem0/LightMem 返回原始结果交调用方处理，zeroclaw 的注入是一体的，RollBall 的拆分设计最灵活
- **价值**：支持不同的检索和注入策略独立演化，未来可灵活实现"RAG 结果与本地记忆的混合排序"

#### 3. 生命周期架构解耦
- **维度**：维度 6（工程约束）
- **表现**：MemoryStore trait + MemoryManager + 生命周期阶段定义，使 Runtime 和存储后端完全解耦
- **对标差异**：
  - mem0/LightMem/HippoRAG 都是 SDK 模式，直接调用存储 API
  - zeroclaw 也是硬依赖特定后端
  - RollBall 的 trait 设计允许未来无缝替换存储引擎
- **价值**：架构可维护性和可扩展性最高，未来可支持多种存储后端（内存/本地/云端）

---

### B.2 RollBall 的关键差距

#### 1. 质量评估体系缺失
- **严重度**：🔴 高
- **维度**：维度 5（质量评估体系）
- **表现**：当前仅定义 SLA（性能），无语义质量评估
- **竞品对标**：
  - mem0 有 LLM Judge 评分框架
  - LightMem 有 F1/BLEU/LLM Judge + LoCoMo/LongMemEval 基准
  - HippoRAG 有 Recall@K/EM/F1 + MuSiQue/HotpotQA 基准
- **影响**：无法量化记忆系统的实际表现，参数调优无数据支撑
- **补充方案**（§C.1 详述）

#### 2. 冲突处理的自动决策不够成熟
- **严重度**：🟠 中
- **维度**：维度 3（记忆冲突处理）
- **表现**：
  - 检测阈值固定（0.85），无动态调整
  - 缺少时间/上下文的冲突信号检测
  - 冲突分类完全依赖 LLM（无启发式规则加速）
  - ambiguous 用户确认流程未实现
- **竞品对标**：zeroclaw 的向量 + Jaccard 双模式更鲁棒，mem0 的 LLM 判断虽费时但精准
- **影响**：误判的冲突可能导致错误的知识更新，长期记忆的准确性难以保证
- **补充方案**（§C.2 详述）

#### 3. 程序记忆与 Skill 系统的联动缺失
- **严重度**：🟠 中
- **维度**：维度 9（生命周期管理）
- **表现**：ProceduralNode 的三条来源路径只有"用户反馈"明确实现，"执行失败"和"跨 Skill 模式提炼"都在 Phase 3（未实现）
- **竞品对标**：PlugMem 有"处方式知识"框架实现了 Skill ↔ 记忆的双向联动
- **影响**：Agent 无法从失败中学习，跨 Skill 的行为模式无法自动泛化
- **补充方案**（§C.2.4 详述）

#### 4. 即时提取与离线巩固的边界模糊
- **严重度**：🟡 中-低
- **维度**：维度 9（生命周期管理）、维度 4（LLM 交互质量）
- **表现**：
  - 即时提取生成 PendingKnowledgeNode，离线巩固升级为正式 KnowledgeNode，但升级条件未定义
  - 两个阶段的 LLM prompt 各自独立，未明确分工（是否会重复抽取？）
- **竞品对标**：zeroclaw 的单一 consolidate_turn 最简洁，LightMem 的 online/offline 有明确界线
- **影响**：可能导致重复提取或遗漏关键信息
- **补充方案**（§C.1.2 详述）

#### 5. AutobiographicalNode 容量管理不完整
- **严重度**：🟡 低
- **维度**：维度 1（记忆层级分类）
- **表现**：History 节点摘要压缩阈值硬编码 10 条，注入上限 200 token，无灵活配置
- **竞品对标**：zeroclaw 的所有参数都通过 manifest 配置，RollBall 的设计有改进空间
- **影响**：长期运行的 Agent 的自我认知可能被无限膨胀，历史事件丢失
- **补充方案**（§C.1.3 详述）

---

### B.3 优先级排序的补充项清单

| 优先级 | 项目 | 维度 | 补充内容 | 目标文档 | 涉及任务 | 对 Phase 2 影响 |
|--------|------|------|--------|---------|---------|---------|
| **P0** | 质量评估框架完整化 | 5 | 标准化基准测试（LongMemEval 5 维）+ LLM Judge 评分 + 在线评估 | 05-memory.md + 新增 12-evaluation.md | 新增 S2.12.x | 新增 5-10 测试，阻塞 Phase 2 Grafeo 验收 |
| **P0** | 冲突处理精排流程 | 3 | 启发式规则 + LLM 分层仲裁 + ambiguous 用户询问实现 | 05-memory.md §6.4 + 04-grafeo.md | 新增 S2.10.x + S2.14 | 涉及 consolidation/conflict.rs 实现 |
| **P1** | ProceduralNode 聚合策略 | 9 | 三条来源路径完整实现 + Skill ↔ ProceduralNode 双向联动 | 05-memory.md §3.2 + §9（Phase 2 补充） | 新增 S2.x.x（Skill 联动部分） | 需延迟至 Phase 2.5，影响 S3 |
| **P1** | 即时/离线巩固边界明确化 | 9 | 定义 PendingKnowledgeNode 的升级条件 + LLM prompt 分工 | 05-memory.md §4 + 04-grafeo.md | 更新 S2.6.x + S2.9.x | 无新增，仅文档澄清 |
| **P1** | 向量/关键词权重动态调整 | 1 | memory_hint.type 驱动的 RRF 权重参数化 | 05-memory.md §1 + plan-p2 S2.8 | 新增 S2.8.x 子任务 | 新增 3-5 测试 |
| **P1** | 冲突检测的多信号融合 | 3 | 时间、上下文、相似度三层检测 | 04-grafeo.md §semantic/conflict.rs | 更新 S2.10.1 | 实现复杂度增加 20% |
| **P2** | 隐私审计日志 | 7 | 打包分享的数据剥离审计、Intent 查询权限日志 | 新增章节 05-memory.md §7.1 + 04-gateway 更新 | 新增 S2.11.x | 新增 3-5 测试 |
| **P2** | Embedding 降级链路完整化 | 4/6 | 三级降级（Local→Remote→Disabled）的具体触发条件和熔断机制 | 05-memory.md + plan-p2 S5.3 | 更新 S5.3.3 | 无新增，仅补充实现细节 |
| **P2** | Episode 的 metadata schema 定义 | 8 | 标准化 episode 元数据字段（话题、情感、置信度等） | 04-grafeo.md §LPG 数据模型 + 05-memory.md §2 | 新增 S2.1.x 子任务 | 新增 2-3 测试 |
| **P2** | 数据迁移脚本（rusqlite→Grafeo） | 10 | Phase 1→2 的数据格式转换脚本 | 新增章节 04-grafeo.md + 补充 plan-p2 S2.14 | 新增 S2.14.x | 新增 3-5 测试 + 1 周工作 |
| **P3** | 跨进程多设备冲突解决 | 10 | Phase 3 云端同步的冲突解决策略 | 09-roadmap-and-scenarios.md + 05-memory.md §9 | Phase 3 规划 | 无，属 Phase 3 |
| **P3** | AutobiographicalNode 参数化 | 11 | History 摘要阈值 + 注入上限通过 manifest 配置 | 05-memory.md §3.3 | 新增 S2.x.x 或 Phase 2.5 | 新增 2-3 测试 |

---

## C. 每个补充项的详细方案

### C.1 质量评估框架完整化 [P0]

#### 当前状态
- 文档定义：`plan-p2.md` §2.5.12（S2.12）仅涉及两个子任务
  - S2.12.1 可观测指标（节点分布/检索统计/冲突统计/衰减统计）
  - S2.12.2 SLA 定义（hybrid_search P99）
  - S2.12.3 占位：Phase 3 对接 LongMemEval
- 缺失内容：
  - 无在线评估（retrieval 完成后自动评分）
  - 无标准基准测试集
  - 无冲突处理准确率评估
  - 无参数校准数据流

#### 补充方案

##### C.1.1 在线评估框架
```rust
// rollball-memory/src/evaluation.rs（新增）

pub trait RetrievalEvaluator: Send + Sync {
    /// 评估本次检索结果是否对 Agent 有帮助
    /// 返回 [0.0, 1.0] 分数
    fn evaluate_retrieval(&self, 
        query: &str,
        results: &[SearchResult],
        agent_response: &str,  // 用户后续回复或 Agent 的使用情况
    ) -> f32;
}

pub struct LLMJudgeEvaluator {
    // 使用轻量级 LLM（如 qwen3:1.7b）评分，避免成本过高
    model: String,
}

impl RetrievalEvaluator for LLMJudgeEvaluator {
    fn evaluate_retrieval(...) -> f32 {
        // Prompt: "检索结果是否与查询相关（1-5分）？"
        // 返回标准化分数
    }
}
```
- **部署**：用轻量级 LLM（1-2B 参数）评分，不用主模型
- **触发**：每次 retrieval 后自动运行，异步不阻塞主流程
- **指标**：NRR (Normalized Retrieval Relevance)、MRR (Mean Reciprocal Rank)

##### C.1.2 标准化基准测试集
```markdown
# LongMemEval 5 维集成

RollBall 记忆系统评估基准采纳 LongMemEval 的 5 个维度：

1. **IE (Information Extraction)**：从对话中提取关键信息的完整性
   - 评估指标：F1（与人工标注对比）
   - 测试场景：10 轮以上的多轮对话，包含 5+ 个不同类型的信息

2. **MR (Memory Retrieval)**：检索到相关信息的准确性
   - 评估指标：Recall@K、MRR、NDCG
   - 测试场景：给定查询，评估前 K 个检索结果的相关性

3. **TR (Temporal Reasoning)**：时间相关信息的处理能力
   - 评估指标：时序准确率、冲突检测率
   - 测试场景：包含时间矛盾的对话（如"上月我搬到北京"，1 周后"我在上海工作"）

4. **KU (Knowledge Utilization)**：Agent 利用检索信息回答新问题的能力
   - 评估指标：QA 准确率（F1@Question）
   - 测试场景：基于前面对话中存储的事实，回答新问题

5. **Abs (Abstraction)**：从具体信息中抽象通用规则的能力
   - 评估指标：ProceduralNode 提取的通用性评分（LLM Judge）
