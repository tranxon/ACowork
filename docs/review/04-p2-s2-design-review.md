# Phase 2 S2 阶段设计审查报告 — Grafeo 记忆系统

**审查日期**：2026-04-21
**审查范围**：docs/05-memory.md、docs/module-design/04-grafeo.md、docs/plan/plan-p2.md S2 部分
**参考对标**：Mem0、HippoRAG、LightMem、ZeroClaw Memory、LangGraph、Claude Memory
**状态**：持续更新中

---

## 1. 当前设计总结

**核心架构**：采用**仿生三层分层架构**，模拟人类认知系统：
- **瞬态层**（Transient）：工作记忆 → LLM 上下文窗口，生命周期单次会话
- **经历层**（Experiential）：情景记忆 → Grafeo episodes 表，HNSW 向量 + BM25 全文索引，生命周期天→周
- **沉淀层**（Consolidated）：语义/程序/自传体记忆 → Grafeo memory_nodes + memory_edges，LPG 知识图谱，生命周期长期→永久

**关键设计特点**：
1. **即时提取（Phase 1）**：通过 memory_store Tool Call 实现，LLM 自主判断是否调用，不需额外 API 调用
2. **Fact 语义去重**：按 (subject, predicate) 匹配，避免冗余知识
3. **工件性内容压缩**：代码/文件不存储全文，仅存摘要 + ArtifactRef 引用
4. **乘法衰减模型**：decay_score = importance × activity_signal，支持三种节点类型的差异化遗忘策略
5. **关联扩散检索**：1-2 跳图遍历，跨层扩散（episode ↔ knowledge_nodes）
6. **token 预算管理**：明确的裁剪优先级，自传体记忆和 Agent 身份不可裁剪

---

## 2. 设计缺失分析

### 2.1 记忆的生命周期管理 ⚠️ 中等缺失

**当前状态**：
- Episode 清理策略相对清晰（14天默认保留，7天自动清理）
- KnowledgeNode 遗忘规则明确（Active/Dormant/Purge 三态）

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 冲突处理 | 新旧记忆矛盾时的合并策略不详 | 高 | 定义冲突检测规则、confidence 优先级、用户确认机制 |
| 临界管理 | Dormant→Purge 的确切判断条件模糊 | 中 | 明确 90 天计时起点、异常情况处理（如 node 被频繁重激活） |
| 版本跟踪 | 已删除节点的恢复机制只有 purge_log，实现细节不全 | 中 | purge_log 持久化策略、恢复的 ID 生成方式、与 source_episode 的关联 |
| 批量操作 | 跨 session/agent 的垃圾清理策略不存在 | 低 | 后台定期 compact 任务设计 |

**为什么重要**：
- Agent 运行几个月后记忆库会变成"垃圾场"，重复节点、过期知识、冗余边导致检索效率下降 40%+
- 用户无法安心分享 Agent，因为不知道哪些记忆被真正删除了
- 多 Agent 共享资源时，缺乏全局生命周期协调会导致存储竞争

**建议方向**：
- 引入冲突检测的 `ConflictNode` 中间态，记录多个相互矛盾的候选值，供后续离线巩固处理
- 实现"记忆审计日志"（Memory Audit Log），追踪每个节点的完整生命周期（创建→激活→衰减→删除）

---

### 2.2 记忆的检索机制 ⚠️ 高度缺失

**当前状态**：
- hybrid_search（向量 + BM25 + RRF）+ graph_expand 框架已定义
- 但**缺乏实际排序策略、上下文融合、个性化调整**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 排序融合 | RRF 权重固定，无法应对不同查询类型 | 高 | 定义查询分类（事实型/情景型/推理型），按类型调整权重；引入 BM25 + 向量得分的动态加权 |
| 相关性判断 | 检索结果相关性阈值硬编码（min_score），无法自适应 | 中 | 实现"检索覆盖率"和"精准度"的折中算法，支持按 Agent 配置 |
| 多跳推理 | graph_expand 的 2 跳上限太严格，无法处理复杂问题 | 中 | 引入"推理深度自适应"：简单查询 1 跳，复杂查询动态扩展到 3 跳；限制扩展边数防止爆炸 |
| 时间优先 | 经历层检索无时间偏好权重 | 中 | 支持"最近优先"模式（recency_boost），对于"今天的计划"类查询生效 |
| 跨 Agent 检索 | 未定义如何检索其他 Agent 的共享知识 | 低 | 通过 Intent 机制查询其他 Agent 的 Public/Personal 节点，支持分级搜索 |
| 缓存策略 | 热查询无缓存，每次都走 hybrid_search | 低 | 实现 LRU 缓存层，支持按 Agent 配置缓存大小 |

**为什么重要**：
- 当前 RRF 是通用方案，但对于"查询用户的核心身份"（需精准匹配）和"查询相似情景"（需泛化）的表现差异巨大
- Mem0 研究表明，动态排序策略能将检索准确率提升 15-20%
- 缺乏时间优先会导致"很久以前的旧偏好"被过度激活

***设计决策（2026-04-21 讨论确定）**：

采用 **"LLM 结构化输出 + 轻量规则" 混合方案**，兼顾泛化能力与零额外 API 成本：

**1. LLM 侧：memory_hint 结构化输出（精简版）**

在 System Prompt 中添加约束（~40 tokens），要求 LLM 每轮回复末尾附加极简元数据：

```
<mh>{"e":["上海","出差"],"t":"s"}</mh>
```

仅保留 2 个字段：
- `e`（entities）：本轮核心实体，最多 3 个。LLM 在理解用户问题时已自然识别，几乎无额外认知负担
- `t`（type）：查询类型单字符分类。`s`=语义联想, `f`=精确事实, `t`=时间相关, `i`=身份偏好

每轮输出开销约 20-30 tokens。memory_hint 在存储对话历史时剥离，不占后续轮次上下文。

砍掉的字段及理由：
- `keywords`：与 entities 重叠，Runtime 可自动从 entities 构造 BM25 查询
- `topic_shift`：Runtime 通过比较相邻两轮 entities 重叠度自动判断（重叠 < 30% → 话题转换）
- `memory_feedback`：最分散注意力的字段，改为 Runtime 侧启发式推断（回复中引用的记忆标记 useful）

**2. Runtime 侧：规则补偿 + 自动推断**

```
Runtime 收到 LLM 回复后：
① 解析 <mh> 块（解析失败 → 静默降级，使用默认策略）
② 自动推断 topic_shift：entities 重叠率 < 30% → 清除上轮缓存
③ 自动推断 memory_feedback：回复中出现记忆实体 → referenced，连续 3 次 unused → 降权
④ 构造下一轮检索参数：
   - BM25 输入 = user_query + entities
   - 向量输入 = user_query
   - 权重 = WEIGHT_PRESETS[type]
     s → vector:0.7 / keyword:0.3
     f → vector:0.4 / keyword:0.6
     t → vector:0.3 / keyword:0.3 + recency:0.4
     i → 直接查 Autobiographical 层
```

**3. 第一轮对话的冷启动**

第一轮无历史 memory_hint，使用轻量规则兜底：
- 时间关键词检测（"今天/昨天/上周"）→ type=t
- 精确实体模式（邮箱/电话/地址）→ type=f
- 身份关键词（"我的/用户的/叫什么"）→ type=i
- 其余 → type=s（默认语义联想）

**4. 模型兼容性保障**

- System Prompt 提供 2-3 个 few-shot 示例
- 输出用正则宽松解析，容错处理
- 连续 3 轮解析失败 → 自动降级为纯规则模式

**设计原则**：LLM 只做"顺手就能做"的事（提取实体 + 单字符分类），Runtime 承担所有"额外推理"工作

---

### 2.3 记忆的存储格式 ⚠️ 中等缺失

**当前状态**：
- Episode 结构定义了 content_type 分类（Informational/Artifact/Structural）
- KnowledgeNode 采用 (subject, predicate, object) 三元组
- 但**缺乏具体的序列化、索引设计、存储优化**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 序列化格式 | 没有明确的二进制序列化方案，仅提及 JSON | 中 | 采用高效的序列化格式（MessagePack/protobuf），对于大量数据加载场景优化性能 |
| 索引设计 | HNSW 向量索引的具体参数（M、ef_construction、ef_search）未定义 | 中 | 根据数据规模定义参数（推荐 M=16, ef_construction=200, ef_search=50），支持运行时调整 |
| 向量维度 | 假设 all-MiniLM-L6-v2（384 维）但未讨论压缩 | 低 | 支持低秩分解/量化方案，当存储超过 100K episode 时自动启用 |
| 向量生成延迟 | 200ms 超时后"后台补生成"，但机制不详 | 中 | 明确延迟队列的优先级策略、失败重试、最大重试次数 |
| 增量持久化 | 向量索引的加载/卸载策略不详 | 中 | 定义 WAL（Write-Ahead Log）机制，Agent 重启时无需重建整个 HNSW 索引 |
| 存储压缩 | 无压缩方案，存储可能膨胀 | 低 | 支持对已巩固的 episode 进行块压缩，运行时解压 |

**建议方向**：
- 支持可插拔的序列化后端（trait EpisodicSerializer）
- 实现向量索引的增量持久化与恢复

---

### 2.4 记忆的层级与分类 ⚠️ 中等缺失

**当前状态**：
- 三层架构清晰（瞬态/经历/沉淀）
- KnowledgeNode 有三种子类型（Fact/Preference/Relation）
- AutobiographicalNode 有六个维度（Identity/Capability/Limitation/Preference/History/Relationship）
- 但**缺乏明确的分类决策树、类型转换规则、跨类型查询**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 分类决策 | LLM 在 Tool Call 时判断 type，但无明确指导原则 | 中 | 补充详细的分类示例库（50+ case），覆盖边界情况 |
| 类型转换 | "用户偏好"是否可升级为"身份特征"未定义 | 低 | 定义类型升级路径：Preference → Autobiographical.Preference（永不衰减） |
| 跨类型查询 | 查询"用户喜欢什么"时，Preference + Relation 如何融合 | 中 | 实现 `multi_type_search()` 方法，支持一次查询多种类型节点 |
| SkillExperience 隔离 | Skill 相关节点的生命周期与普通记忆分离 | 中 | 定义 SkillExperience 的衰减模型（与 Skill 发布状态相关联） |
| HypothesisNode 设计 | Phase 3 需要的"假设节点"的具体结构缺失 | 高 | 定义 HypothesisNode：包含假设、支持证据、反驳证据、confidence、验证期限 |

**建议方向**：
- 建立"类型分类的规范化指南"（Classification Canonical Format），供 LLM 参考
- 实现自动类型检查（compile-time check），防止不合规的类型组合

---

### 2.5 记忆的注入策略 ⚠️ 高度缺失

**当前状态**：
- Token 预算管理有清晰的裁剪优先级（8 级）
- 自传体记忆注入 System Prompt
- 检索结果按优先级裁剪
- 但**缺乏实际的 token 计数实现、注入时机决策、多轮对话的上下文衰减**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| Token 精确计数 | 文档说"字符数/3 近似"，但精确计数方案不全 | 高 | 集成 tiktoken-rs（OpenAI）+ tokenizers 库（Anthropic），支持多模型精确计数；误差要求 < 5% |
| 检索结果计数 | "预计算 token 数"的算法不详 | 中 | 定义采样策略：前 100 token 精确计数，后续按采样比例（如 1/20）计数 |
| 预算分配 | 总预算如何分配给 system/history/retrieval/output 未详 | 高 | 定义默认分配：system 20%、history 50%、retrieval 20%、output 10%，支持按 Agent 配置 |
| 多轮衰减 | 对话历史很长时，如何逐步衰减早期信息 | 中 | 按消息对向后衰减，衰减因子随轮数指数增长 |
| 注入时机 | 何时触发检索？每轮都检索还是按需检索 | 中 | 实现"检索需求预测"：每轮前计算 query 与历史的新鲜度，新鲜度低时触发检索 |
| 格式化方案 | 记忆注入到 System Prompt 的具体格式不明 | 中 | 定义统一格式，支持 Markdown 标记 |
| 冲突避免 | 检索结果与 System Prompt 中的既有信息冲突时如何处理 | 低 | 实现"冲突检测"：取 confidence 更高的 |
| 个性化调整 | 不同用户对记忆注入的敏感度不同 | 低 | 支持 manifest 配置：memory_injection_verbosity |

**建议方向**：
- 实现 `TokenBudgetPlanner` 模块，支持多种分配策略
- 引入"检索需求度"评分，动态决策何时触发检索
- 支持对检索结果的"融合总结"，在 token 有限时自动生成更紧凑的摘要

---

### 2.6 记忆的冲突处理 ⚠️ 高度缺失

**当前状态**：
- 文档提到"冲突解决策略"但实现方案空白
- 只有 Fact 语义去重，缺乏语义矛盾的检测

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 冲突检测 | 如何判断两条知识是否矛盾 | 高 | 定义语义距离阈值，当 embedding 相似度 > 0.85 but object 不同时触发冲突检测 |
| 冲突类型分类 | 直接矛盾 vs 软矛盾 vs 演进 | 中 | 实现冲突分类器，区分三类并采用不同的解决策略 |
| 优先级判断 | 冲突时如何判断哪个信息更可信 | 高 | 综合 confidence/recency/access_count 三个维度加权 |
| 用户确认机制 | 无法自动解决的冲突如何处理 | 中 | 生成"记忆冲突报告"，通过 Desktop App 让用户确认 |
| 演进历史 | 是否应保留旧值 | 中 | 定义"生成式"vs"替代式"更新策略 |
| 传播更新 | 当一个节点被更新时，关联节点是否应自动更新 | 低 | 实现"更新传播"机制 |

**建议方向**：
- 实现 `ConflictDetector` 模块，支持多种冲突判断策略
- 引入"冲突解决日志"（Conflict Resolution Log），供用户审计

---

### 2.7 记忆的隐私与隔离 ⚠️ 中等缺失

**当前状态**：
- PrivacyLevel (Public/Personal/Sensitive) 已定义
- 打包分享时会剥离 Personal/Sensitive
- 但**缺乏实际的访问控制、跨 Agent 隐私边界、数据隔离验证**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 访问控制策略 | PrivacyLevel 仅在"打包分享"时起作用，Runtime 中无访问控制 | 中 | 在 Memory Store trait 中加入 access_check 方法 |
| 隔离验证 | Agent A 是否真的无法访问 Agent B 的 Personal 节点 | 高 | 编写集成测试验证隔离 |
| 用户明确同意 | 何时记录用户数据缺乏明确的同意机制 | 高 | 记录每条 Personal/Sensitive 数据的来源和同意时间戳 |
| 跨 Zone 隐私 | Identity/Knowledge/Work Zone 的隐私级别独立 | 中 | 定义每个 Zone 的默认隐私级别 |
| 加密存储 | 敏感信息是否应加密存储 | 中 | 支持 feature flag：敏感数据加密（AES-256） |

**建议方向**：
- 实现 `AccessControl` 模块，支持 RBAC/ABAC 两种模型
- 编写隐私测试套件，定期验证隔离有效性

---

### 2.8 记忆的持久化 ⚠️ 中等缺失

**当前状态**：
- 存储位置明确：`<agent_workspace>/memory/private.grafeo`
- 异步写队列 + WAL 保证不丢失
- 但**缺乏备份策略、迁移方案、多设备同步**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 备份策略 | 无备份方案，单个 Grafeo 文件损坏即数据全失 | 高 | 实现定时备份（日级+周级），支持增量备份 |
| 损坏恢复 | 数据库损坏时的恢复机制不详 | 中 | 定义 Grafeo 自检 + 自动修复流程 |
| 版本迁移 | Schema 升级时如何保证向后兼容 | 中 | 使用数据库版本字段，每个 schema 版本有对应的迁移脚本 |
| 多设备同步 | Phase 3+ 的云端同步方案框架但实现缺失 | 低 | 支持 Zone-Based 差异化同步的具体协议设计 |

**建议方向**：
- 实现备份管理器（BackupManager），支持定时备份和手动备份
- 引入"记忆快照"概念，每天自动生成一个只读快照

---

### 2.9 记忆的评估与质量 ⚠️ 高度缺失

**当前状态**：
- 定义了 importance/confidence/decay_score 评分
- 但**缺乏实际的质量评估方法、衰减算法验证、效果度量**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 质量度量 | 无法衡量记忆系统的整体质量 | 高 | 定义"记忆有效性"指标 |
| 衰减验证 | 衰减公式（λ=0.03, FLOOR=0.05）未经过实际验证 | 高 | 设计实验验证参数 |
| 检索准确率 | 无法评估 hybrid_search 的准确率和覆盖率 | 中 | 建立"记忆检索基准"（50+ query-result 对） |
| 冲突率 | 不知道有多少记忆节点存在冲突 | 中 | 定期运行冲突检测，生成报告 |
| 重复度 | 无法检测大量重复/冗余的记忆 | 中 | 定义"语义重复度"检测 |
| 成本分析 | Token、存储、计算成本未量化 | 中 | 记录各项操作耗时和成本 |

**建议方向**：
- 建立"记忆系统可观测性"框架
- 实现"记忆质量评分"：综合准确率、覆盖率、冲突率、成本
- 支持 Desktop App 展示关键指标

---

### 2.10 与 LLM 的交互 ⚠️ 高度缺失

**当前状态**：
- Tool Call 机制明确
- 即时提取和离线巩固分工清晰
- 但**缺乏防止 LLM 幻觉的机制、记忆质量检查、自我审视**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 幻觉防止 | memory_store 工具是否会被 LLM 滥用 | 高 | 实现"三层检查"：Runtime 验证 → 语义合理性 → 离线复核 |
| 信心评分 | importance 由 LLM 主观赋予，无验证 | 中 | 引入"客观信心度"：用户明确说 1.0、推断 0.7、假设 0.3 |
| 提取质量 | LLM 的 memory_store 调用是否真的提取了关键信息 | 中 | 离线巩固时计算"提取遗漏率"和"过度提取率" |
| 模型特异性 | 不同模型的 Tool Call 准确率不同 | 中 | 记录模型与准确率映射，confidence 自动调整 |
| 循环依赖 | 检索→存储循环是否会强化幻觉 | 低 | 标记"新鲜"vs"衍生"记忆，限制衍生链长度 |

**建议方向**：
- 实现 `MemoryQualityValidator` 模块
- 引入"记忆可信度"评分
- 支持用户"记忆审计"

---

### 2.11 实际的工程约束 ⚠️ 高度缺失

**当前状态**：
- 选择方案已定（all-MiniLM-L6-v2、Grafeo 原生存储、instant-distance）
- 但**缺乏性能指标、扩展瓶颈分析、应急方案**

**缺失点**：

| 维度 | 缺失项 | 重要性 | 建议补充 |
|------|--------|--------|---------|
| 存储容量 | 没有明确的单 Agent 存储上限 | 高 | 定义容量规划：10K episode + 5K nodes ≈ 200MB |
| 查询延迟 | 没有 hybrid_search 的 SLA | 高 | 定义 SLA：P99 < 100ms（1K nodes），P99 < 500ms（10K nodes） |
| 并发限制 | 没有并发查询的限制 | 中 | 限制最多 10 并发查询，超过后排队 |
| embedding 降级 | 本地 embedding 失败的降级策略 | 中 | 级联降级：ONNX → 远程 API → 仅 BM25 |
| Grafeo 故障 | Grafeo 不可用或 IPC 断开时的降级 | 中 | 启动"内存仅模式"，恢复后同步 |
| 内存压力 | HNSW 索引加载时内存飙升 | 中 | 支持"分片加载" |
| 模型兼容性 | 不同模型的 Tool Call 表现差异 | 中 | 记录各模型成功率，自动调整策略 |

**建议方向**：
- 编写性能基准测试（benchmark.rs）
- 实现"健康检查"系统
- 支持可配置的降级策略

---

## 3. 主流方案对比分析

### 3.1 对标方案汇总

| 方案 | 所属 | 核心特点 | 与 Grafeo 的对标点 |
|------|------|---------|------------------|
| **Claude Memory** | Anthropic | 对话级上下文管理，无显式记忆系统 | 主要依赖上下文窗口，缺乏长期记忆 |
| **Mem0** | 开源 | 即插即用的记忆库，支持多后端 | 类似 Phase 1，但缺乏离线巩固和知识图谱 |
| **MemOS** | 学术 | 操作系统假说，LLM-as-kernel | 理论宏大但实现复杂，不适合现阶段 |
| **LangGraph** | LangChain | 状态管理 + checkpointer 持久化 | 偏向工作流编排，记忆能力较弱 |
| **ZeroClaw Memory** | 本项目参考 | 模块化记忆 Trait，支持多实现 | 偏向轻量级，缺乏复杂推理 |
| **LightMem** | 学术 | 三级记忆 + 离线更新，成本低 | 压缩策略优秀，可参考 |
| **HippoRAG** | 学术 | 知识图谱 + PageRank，多跳推理 | 检索策略强，但构建成本高 |

### 3.2 检索策略对比

| 方案 | 检索方式 | 优点 | 缺点 | Grafeo 学习点 |
|------|---------|------|------|-------------|
| **Mem0** | 向量相似度 | 简单快速 | 无多跳推理 | 当前已采用，需加强 |
| **HippoRAG** | KG PageRank | 支持多跳 | 构建成本高 | 改进 graph_expand |
| **LightMem** | 分层检索 | 效率高、成本低 | 泛化有限 | 参考分层策略 |
| **Claude** | 滑动窗口 | 无外部依赖 | 丢失信息 | 不适合长期记忆 |

### 3.3 遗忘策略对比

| 方案 | 遗忘方式 | 优点 | 缺点 | Grafeo 学习点 |
|------|---------|------|------|-------------|
| **Grafeo** | 乘法衰减 | 科学合理 | 参数未验证 | 需实验验证 |
| **Mem0** | 时间 + 频率 | 简单易懂 | 缺语义重要性 | 已考虑 |
| **LightMem** | 分层策略 | 短期快遗忘 | 层级固定 | 参考分层 |
| **MemOS** | OS 虚拟内存 | 理论完整 | 实现复杂 | Phase 3+ |

### 3.4 巩固策略对比

| 方案 | 巩固方式 | 优点 | 缺点 | Grafeo 学习点 |
|------|---------|------|------|-------------|
| **Grafeo** | 即时 Tool Call + 离线 LLM | 两级分工清晰 | 离线巩固缺失 | 需完成 Phase 3 |
| **Mem0** | 自动提取 + 冲突检测 | 端到端自动化 | 误提取率高 | 参考冲突检测 |
| **LightMem** | 离线压缩+更新 | 成本低 | 需睡眠时间 | 参考"睡眠机制" |

### 3.5 隐私与隔离对比

| 方案 | 隐私策略 | 优点 | 缺点 | Grafeo 学习点 |
|------|---------|------|------|-------------|
| **Grafeo** | PrivacyLevel 标记 | 分级清晰 | 运行时无 AC | 需实现 AC 模块 |
| **Mem0** | 命名空间隔离 | 简单有效 | 无加密 | 已采用，需加强 |
| **Claude** | 会话级隔离 | 天然隔离 | 不支持跨会话共享 | 不适用 |

---

## 4. 建议优先级

### S2（Phase 2）必须解决的缺失

| 优先级 | 缺失项 | 工作量 | 影响 | 建议方案 |
|--------|--------|--------|------|---------|
| **P0** | 记忆检索机制（2.2）| 中 | 核心功能 | 完成 hybrid_search，支持 RRF 融合，SLA < 100ms |
| **P0** | Token 预算管理（2.5）| 中 | 核心功能 | 集成 tiktoken-rs，定义分配策略 |
| **P0** | 冲突检测与处理（2.6）| 中 | 数据一致性 | 实现冲突检测器，支持自动解决和用户确认 |
| **P1** | 质量评估框架（2.9）| 中 | 可观测性 | 实现记忆有效性度量，建立基准测试 |
| **P1** | 隐私访问控制（2.7）| 低-中 | 隐私安全 | 实现 AccessControl，编写隔离测试 |
| **P1** | 工程约束（2.11）| 中 | 稳定性 | 定义 SLA、容量规划、降级策略 |
| **P2** | 生命周期管理（2.1）| 低 | 长期稳定性 | 完善 purge_log，定义清理策略 |
| **P2** | 存储优化（2.3）| 低 | 性能优化 | 支持向量量化、压缩 |

### S3-S5（Phase 3+）应关注的缺失

| 优先级 | 缺失项 | Target Phase | 预期收益 |
|--------|--------|---------|----------|
| **P0** | 离线巩固完整实现 | Phase 3 | 处理隐式关联和记忆泛化 |
| **P0** | HypothesisNode 设计 | Phase 3 | 支持主动假设验证 |
| **P1** | LightMem 风格分层处理 | Phase 3 | 成本降低 50%+ |
| **P1** | 多模型兼容性适配 | Phase 3 | 支持 qwen/claude 等 |
| **P2** | 云端同步（Zone-Based）| Phase 6 | 跨设备一致性 |

---

## 5. 结论

RollBall Grafeo 的设计框架科学、分层明确，但在**实现细节、工程约束、质量评估**方面存在显著缺失。特别是：

1. **检索和注入策略**需要从"设计"升级到"经过验证的实现"
2. **冲突处理和隐私控制**是生产级系统的必需功能
3. **质量评估体系**是长期可维护性的基础

建议优先完成 P0 缺失项，确保 Phase 2 交付的 Grafeo 是**可用、可验证、可维护**的。同时为 Phase 3 的深度能力（离线巩固、假设验证）预留架构扩展空间。

---

## 6. 讨论记录

> 本节记录 S2 设计讨论的结论和决策，持续更新。

### 6.1 检索机制：查询分类与动态权重（2026-04-21）

**问题**：RRF 权重固定 vector:0.7/keyword:0.3，不同查询类型的最优权重差异大。纯规则引擎泛化能力弱，额外 LLM 调用成本高。

**决策**：采用 **"LLM memory_hint 结构化输出 + 轻量规则冷启动"** 混合方案。

**核心设计**：
- LLM 每轮回复末尾附加 `<mh>{"e":[实体],"t":类型}</mh>`（~20-30 tokens）
- 仅 2 个字段：`e`(entities, 最多 3 个) + `t`(type, 单字符 s/f/t/i)
- Runtime 侧自动推断 topic_shift 和 memory_feedback，不让 LLM 做额外推理
- 第一轮无历史时用规则兜底，解析失败静默降级

**关键考量**：
- 结构化输出必须极简，避免分散 LLM 注意力降低回复质量
- 零额外 API 成本，memory_hint 是正常回复的一部分
- 对话历史存储时剥离 memory_hint，不占后续上下文空间

详见 §2.2 的"设计决策"部分。

### 6.2 检索机制：graph_expand 自适应深度（2026-04-21）

**问题**：graph_expand 固定 2 跳，简单查询浪费（1 跳够用），复杂推理不足（需要 3 跳）。

**决策**：采用**渐进式扩散 + 早期终止**方案。

**核心设计**：
- max_hops 从 2 提高到 3，但通过早期终止实际大多数查询在 1-2 跳停止
- 每跳的 early_stop_threshold 随跳数递增（1跳: 0.1, 2跳: 0.15, 3跳: 0.2），越远越严格
- 扩散节点加入 PageRank 式加权：被多条路径经过的节点获得额外权重加成
- 早期终止条件：
  1. 本轮扩展最高分 < 阈值
  2. 累积结果已满足 token 预算
  3. 达到总节点上限（20）

### 6.3 检索机制：性能保障与降级策略（2026-04-21）

**问题**：无 SLA 定义，无降级策略。Autobiographical 节点增长后预热可能变慢。

**决策**：**Grafeo 内部负责索引恢复 + MemoryManager 层降级级联 + Autobiographical 增长控制**。

**1. Grafeo 启动与索引恢复**

Grafeo 作为图数据库引擎，自身负责启动时高效恢复内部索引（向量索引、全文索引、图索引）。Embedding 随节点一起持久化在 Grafeo 中，启动时从持久化数据加载而非重新生成。上层不关心 Grafeo 内部的索引实现细节。

**2. MemoryManager 层降级级联**

上层（MemoryManager）根据 Grafeo 的就绪状态和检索耗时决定降级：

```
Level 0（正常模式）
  前提：Grafeo 完全就绪
  策略：hybrid_search（向量+BM25+RRF）+ graph_expand
  SLA：P99 < 200ms

  ↓ 向量索引未就绪或 embedding 生成超时

Level 1（无向量模式）
  策略：BM25 only + graph_expand
  SLA：P99 < 100ms

  ↓ Grafeo 查询超时（>300ms）或索引异常

Level 2（缓存模式）
  策略：返回 Autobiographical 文本缓存 + 最近 5 条 Episode
  SLA：P99 < 10ms（纯内存读取）

  ↓ Grafeo 完全不可用

Level 3（内存模式）
  策略：仅返回当前会话工作记忆，无持久化检索
  SLA：P99 < 1ms
```

MemoryManager 在 Grafeo 启动完成前即可接受请求（从 Level 1/2 开始），Grafeo 就绪后自动升级到 Level 0。

**3. 单次检索预算分配（500ms 硬超时）**

```
① embedding 生成：≤200ms（超时→跳过向量，BM25 only）
② hybrid_search：≤150ms（向量和 BM25 并行，先返回的直接进融合）
③ graph_expand：≤100ms（早期终止，超时返回已扩展节点）
④ 排序+格式化：≤50ms
任一环节超时，使用已有部分结果继续后续步骤。
```

**4. Autobiographical 增长控制**

防止 Autobiographical 层无限膨胀影响性能：
- 硬上限：单 Agent 不超过 500 个 Autobiographical 节点
- 分维度软上限：Identity 50 / Capability 100 / Limitation 30 / Preference 150 / History 100 / Relationship 70
- 接近上限时触发合并（相似度 > 0.9 的节点自动合并）
- 超过上限时降级为 KnowledgeNode（从"身份"降为"知识"，仍可检索但可衰减）

### 6.4 检索机制：注入格式（2026-04-21）

**问题**：检索结果如何格式化注入 System Prompt 未定义。

**决策**：采用**分层结构化注入**方案。

**核心设计**：
- 自传体记忆始终注入且放在最前面，标记为不可裁剪
- 检索结果按类型分组（知识/经历/习惯），每条附带类型标签和时间信息
- Token 预算裁剪从最后一组（低相关度）开始

注入格式示例：
```markdown
## 用户记忆

### 身份信息
- 姓名：张三
- 常驻城市：北京

### 相关记忆
1. [知识] 经常去上海出差（相关度: 高）
2. [经历] 上次上海出差时淋过雨（2天前）
3. [习惯] 出差前会查目的地天气
```

### 6.5 注入策略：Token 计数与预算分配（2026-04-21）

**问题**：Token 计数方案不全（仅"字符数/3 近似"），预算如何分配给 system/history/retrieval/output 未定义。

**决策**：**分层计数 + 弹性分区**。

**1. Token 计数——分层实现 + 增量缓存**

```
TokenCounter trait：
  Tier 1: 精确计数（已知模型）
    OpenAI → tiktoken-rs，Anthropic → 官方 tokenizer
    误差 < 1%
  Tier 2: 近似计数（未知模型，有 endpoint）
    首次调用远程 tokenizer，后续用采样比推算
    误差 < 5%
  Tier 3: 启发式估算（兜底）
    英文 words×1.3，中文 字符×0.6，混合按语种比例加权
    误差 < 15%

增量缓存：
  System Prompt → 启动时计算一次，缓存
  每条消息 → 进入历史时计算一次，附加 _token_count
  Autobiographical → 内容变更时重新计算
  检索结果 → 返回时计算，附加到 SearchResult.context_tokens
```

**2. 预算分配——弹性分区 + 保底线**

```
total = 模型 context_window

固定区：
  System Prompt = system_base + autobiographical（不可裁剪）
  Output Reserve = manifest.max_output_tokens（默认 4096）

可分配空间 = total - system - output
  默认比例：history 75% / retrieval 25%
  自适应调整：
    对话轮数 < 5 → history 少，retrieval 可用空间更大
    memory_hint.type == "i" → retrieval 提升到 40%
    无检索结果时 → retrieval=0，history 自动扩展
  硬保底线：
    retrieval 最少 2048 tokens
    history 最少保留最近 3 轮对话
```

### 6.6 注入策略：对话历史三阶段裁剪（2026-04-21）

**问题**：对话超过预算时如何裁剪，特别是包含大量文件内容的代码开发/文档处理场景。

**决策**：**三阶段渐进裁剪：内容折叠 → FIFO → 摘要替代**。

**第一阶段：内容折叠（无损压缩，优先执行）**

扫描历史消息中的大段内容（单块 > 200 tokens），折叠为紧凑引用：

```
折叠规则：

1. 文件内容（tool_result 中的 file_read 输出）
   原文：[150行代码] → 折叠为：📎 src/main.rs@L1..L150

2. 工具长输出（grep/search 结果、命令输出等）
   原文：[35条匹配] → 折叠为：📎 tool:grep_code("pattern") → 35 matches

3. 用户粘贴的大段文本（> 500 tokens）
   原文：[长配置文件] → 折叠为：📎 inline#a3f2 (TOML, 2800 tokens)

统一引用格式：
  文件：  📎 <path>@L<start>..L<end>
  工具：  📎 tool:<name>(<args>) → <summary>
  内联：  📎 inline#<hash> (<type>, <tokens> tokens)

折叠元数据（不显示在上下文中，用于召回）：
  FoldedRef {
    ref_id, source_type, source_path,
    line_range, original_tokens,
    content_hash,    // 用于检测文件是否变更
    folded_at_turn,  // 在哪一轮被折叠
  }
```

**折叠时机**：不是消息产生时立即折叠，而是 token 预算紧张时才触发。最近 3 轮不折叠。优先折叠最早轮次中的最大内容块。

**召回机制**：
- 主要路径：System Prompt 告知 LLM "带 📎 标记的内容已折叠，如需查看请用 file_read 工具"，LLM 自然通过 tool_call 召回
- 补充路径：检索结果关联到 FoldedRef 源文件时，检查 content_hash，未修改则自动展开（如预算允许）

**Token 节省估算**：代码开发场景 30 轮对话，文件内容约占 60%+，折叠后节省 50-60% 的 history tokens，很可能不再需要触发 FIFO 裁剪。

**与 ArtifactRef 的关系**：FoldedRef 用于对话历史（瞬态层），ArtifactRef 用于经历层存储。两者格式可统一，会话结束后 FoldedRef 可转为 ArtifactRef 存入经历层。

**第二阶段：FIFO 裁剪（有损）**

折叠后仍超预算时，从最早的消息对（user+assistant）开始移除。保留：最近 3 轮对话 + 包含 tool_call 结果的消息对。

**第三阶段：摘要替代（Phase 3）**

Phase 2 不实现。Phase 3 对被 FIFO 裁剪的消息生成摘要（~100 tokens），保留核心决策信息。

### 6.7 注入策略：检索触发时机（2026-04-21）

**问题**：每轮都检索还是按需检索？

**决策**：**每轮默认触发 + 快速跳过优化**。

```
快速跳过条件（<1ms，纯规则）：
- 用户消息 < 10 字符（"好的"、"继续"等）
- 连续同一 topic（上轮 entities 重叠 > 70%）且上轮已有缓存
- manifest 明确配置不使用记忆

其余情况默认触发检索，缓存 TTL=300s。
理由："按需"判断本身需要理解用户意图，判断错了就会漏掉关键记忆。
有降级策略和 500ms 硬超时保障，检索的最坏代价可控。
```

### 6.8 注入策略：完整流程串联（2026-04-21）

```
用户消息到达
    │
    ▼
① Token 计数（增量，<1ms）
   新消息 token 计数 → 更新 history_tokens
    │
    ▼
② 预算计算
   available = total - system - output_reserve
   history_budget = available × 0.75
   retrieval_budget = available × 0.25
   （根据对话轮数和 memory_hint.type 自适应调整）
    │
    ▼
③ 历史裁剪（如需要）
   第一阶段：内容折叠（无损）
   第二阶段：FIFO 裁剪（有损）
    │
    ▼
④ 检索触发决策
   快速跳过检查 → 跳过/触发检索
   检索时用 memory_hint.entities 增强 BM25
    │
    ▼
⑤ 注入组装
   System Prompt:
     [Agent 基础 Prompt]
     [Autobiographical — 不可裁剪]
     [检索结果 — 分层结构化，按 §6.4 格式]
     [memory_hint 指令 — ~40 tokens]
   Messages:
     [裁剪后的对话历史（含折叠引用）]
     [用户最新消息]
    │
    ▼
⑥ 发送给 LLM
```

### 6.9 冲突处理：实时轻量写入 + 离线精确结构化（2026-04-21）

**问题**：冲突检测依赖 `(subject, predicate)` 三元组匹配，但 LLM 实时拆三元组不可靠（同一事实可能拆出不同 predicate），且 memory_store 的三元组接口与 memory_hint 的实体提取存在功能重叠。

**决策**：**三元组提取从即时提取移到离线巩固阶段**，即时提取仅存自然语言 + 类型标签。

**1. memory_store 接口简化**

```
// 旧设计（三元组，LLM 负担重）
memory_store({ "subject": "用户", "predicate": "居住城市", "object": "上海" })

// 新设计（自然语言 + 类型标签）
memory_store({
  "content": "用户住在上海",
  "category": "fact",              // fact / preference / procedure
  "keywords": ["用户", "上海"]      // 可选，Runtime 自动从 memory_hint.e 补充
})
```

LLM 不再需要做三元组拆分，只需用自然语言描述要记住什么。keywords 可复用同轮 memory_hint.e 的值，Runtime 自动合并，LLM 甚至可以不填。

**2. 两阶段冲突检测**

| 阶段 | 时机 | 方式 | 精度 |
|------|------|------|------|
| **即时标记** | memory_store 写入时 | 新节点 embedding 与已有节点相似度 > 0.85 → 标记候选冲突 | 粗筛 |
| **离线解决** | 离线巩固时 | LLM 批量提取三元组 → `(subject, predicate)` 精确比对 → 分类 → 解决 | 精确 |

即时写入产生 `PendingKnowledgeNode`：
- 有 content、embedding、keywords → 立刻支持向量检索和 BM25
- 无三元组、无图边 → 暂不参与 graph_expand
- embedding 相似 > 0.85 的节点标记为候选冲突，不立即解决

**3. 离线巩固三元组提取**

```
离线巩固 LLM 输入：一批 PendingKnowledgeNode + 已有 KnowledgeNode
任务：
  1. 提取三元组：content → (subject, predicate, object)
  2. 标准化 predicate（匹配已有词表，找不到则新增）
  3. 构建图边：subject/object → 实体节点，predicate → 边标签
  4. 冲突检测：与已有三元组 (subject, predicate) 比对
  5. 合并去重：消除冗余节点
输出：标准化 KnowledgeNode + 图边 + ConflictLog
```

巩固后节点升级为正式 KnowledgeNode，参与 graph_expand 图遍历。

**4. 冲突分类与解决（离线阶段，LLM 判断）**

冲突分类是语义判断，遵循 LLM 优先原则，由离线巩固阶段的 LLM 完成（此时 LLM 已在场做三元组提取）。

```
离线巩固 LLM 冲突分类 prompt：

输入：冲突的两条记忆 + 各自的 source_episode 上下文
输出：
  - type: "evolution" | "correction" | "ambiguous"
  - action: "replace" | "keep_both" | "ask_user"
  - reasoning: 一句话解释判断理由

示例：
  "用户住在北京" vs "用户住在上海" + episode中用户说"我搬到上海了"
  → type:evolution, action:replace, reasoning:"用户明确表达了搬家"

  "用户生日 3月" vs "用户生日 5月" + episode中用户说"不是3月，是5月"
  → type:correction, action:replace, reasoning:"用户主动纠正了错误"

  "用户喜欢中餐" vs "用户喜欢西餐"
  → type:ambiguous, action:ask_user, reasoning:"可能都喜欢，无法确定"
```

| 类型 | LLM 判断依据 | 处理 |
|------|------------|------|
| **演进**（Evolution） | 理解上下文语义（如"我搬家了"） | 新值 Active，旧值 Dormant，写 conflict_log |
| **纠正**（Correction） | 理解否定语义（如"不是 X，是 Y"） | 新值 Active，旧值 Dormant，降低旧来源可信度 |
| **不确定**（Ambiguous） | 无法从上下文确定 | 两个都 Active，标记 conflict_group_id，等待用户确认 |

不确定类冲突累计 3+ 个时，Agent 在下次对话中自然地询问用户确认，不通过弹窗打扰。

**与旧方案的差异**：旧方案用规则判断（"时间差>30天→演进"），这是语义判断伪装成规则，泛化性差。新方案遵循 LLM 优先原则，冲突分类交给 LLM，它能理解对话上下文中的语义意图，准确率远高于规则。

**5. 三层职责分明，无重叠**

| 机制 | 目的 | 时机 | 来源 |
|------|------|------|------|
| memory_hint.e | 检索增强 | 每轮，实时 | LLM 回复末尾 |
| memory_store.keywords | 存储索引 | 偶尔，实时 | LLM tool_call + memory_hint.e 自动合并 |
| 离线巩固三元组 | 图结构构建 | 定期，异步 | LLM 批量提取 |

**设计理由**：三元组提取放在离线巩固阶段，符合三层五类记忆的设计初衷——即时提取负责快速写入（经历层→待巩固），离线巩固负责精确结构化（待巩固→沉淀层）。实时阶段不做重活，后台阶段有足够上下文和时间做精确判断。

### 6.10 LLM 交互质量：信任 LLM + 机械护栏 + 离线复核（2026-04-21）

**问题**：memory_store Tool Call 完全依赖 LLM 判断，缺乏防幻觉检查、客观信心评分、模型差异适配。

**设计原则**：信任 LLM 的判断能力，不用规则替代 LLM 做语义判断。长期来看 LLM 幻觉率在下降，规则缺乏泛化性不是长期方案。只在 LLM 做不了的事情上用规则（机械性限制）。

**1. memory_store 最终接口**

```
memory_store({
  "content": "用户住在上海",
  "category": "fact",              // fact / preference / procedure
  "confidence": "high",            // high(0.9) / medium(0.6) / low(0.3)，可选，默认 medium
  "keywords": ["用户", "上海"]      // 可选，Runtime 从 memory_hint.e 补充
})
```

- `confidence` 由 LLM 自己判断，用 enum 而非浮点数，降低决策负担
- LLM 比规则更擅长判断“用户说这话时有多确定”
- 不再需要 importance 字段，confidence 已涵盖其语义

**2. Runtime 机械护栏（非语义判断，<1ms）**

```
护栏规则（仅限 LLM 做不了的机械性限制）：
- content 非空且 < 500 字符（防止存入整段文章）
- 单轮 memory_store 调用 ≤ 5 次（防止批量滥调用）
- content 不得包含 system prompt 片段（防安全泄露）

不做的事：
- 不用规则判断“这条记忆是否真实”（LLM 更擅长）
- 不用规则计算 confidence（LLM 更准确）
- 不维护模型信心系数静态表（模型迭代快，静态表很快过时）
```

**3. 离线巩固复核（真正的防幻觉防线）**

幻觉的根本解决不在实时阶段，而在离线巩固：

```
离线 LLM 回看完整 episode 上下文：
- 每条 PendingKnowledgeNode 是否有对话证据支撑？
- 是否存在过度推断？
- 无证据支撑 → 降级 confidence 或标记 Dormant

与实时阶段的关键差异：
1. 有完整上下文——实时可能只看当前片段，离线看全貌
2. 批量对比——多条记忆放在一起，矛盾和过度推断更容易暴露
3. 不影响实时响应——后台异步执行，不阻塞用户交互
```

**4. 三层职责分工**

| 层 | 负责 | 方式 |
|---|---|---|
| **LLM** | 判断什么值得记、confidence 多高、自然语言描述 | 语义理解，泛化性强 |
| **Runtime** | 机械护栏、embedding 索引、冲突候选标记 | LLM 做不了的事 |
| **离线巩固** | 三元组提取、证据验证、confidence 校准、冲突解决 | 有充分上下文和时间 |

**设计理由**：规则缺乏泛化性，不是长期方案。LLM 幻觉率长期趋势下降，应该信任 LLM 的语义判断能力，仅在机械性限制（长度、频率、安全）上用规则。离线巩固是真正的质量防线，它有完整上下文、不受实时延迟约束、且可批量对比发现矛盾。

### 6.11 质量评估体系：借鉴开源 Benchmark + 可观测指标 + 衰减参数可调（2026-04-21）

**问题**：衰减参数（λ=0.03, FLOOR=0.05）未经实验验证；无法度量检索准确率和覆盖率；缺少性能基准测试。

**目标**：最终发布版本要通过多个开源记忆 Benchmark 工具（LongMemEval、BEAM、LoCoMo-Plus 等）的评测，并获得高分。

**参考框架调研**：详见 `docs/reference/research_memory_evaluation_frameworks.md`。

**1. 借鉴的评估维度**

| 框架 | 核心维度 | 对应 RollBall 设计 |
|------|---------|------------------|
| **LongMemEval** (ICLR 2025) | IE(信息提取) / MR(跨会话推理) / TR(时序推理) / KU(知识更新) / Abs(拒绝回答) | IE→memory_store提取质量、KU→冲突处理、Abs→不知道就说不知道 |
| **LoCoMo-Plus** | CCS(约束一致性评分)——隐式偏好推断准确率 | Preference / ProceduralNode 提取质量 |
| **BEAM** (ICLR 2026) | 10维能力 + Accuracy@Length 衰减曲线 | 矛盾检测、偏好学习、知识演化 + 验证 λ 参数 |

**2. 分阶段实施**

**Phase 2：建标准 + 可观测基础设施**

```
a) 采纳 LongMemEval 5 维作为 RollBall 记忆系统的评估标准
   集成测试按 5 维编写用例，确保每个维度有基础覆盖

b) 运行时可观测指标（日志输出，Phase 3 接入 Desktop App）
   - 节点分布：Active / Dormant / Pending 按类型统计
   - 检索统计：avg_latency / hit_rate / skip_rate
   - 冲突统计：pending / auto_resolved / user_confirmed
   - 衰减统计：本次新增 Dormant 数 / Dormant→Active 恢复数

c) 性能 SLA 定义 + 集成测试断言
   - hybrid_search: P99 < 100ms (1K nodes), < 500ms (10K nodes)
   - memory_store: < 50ms
   - embedding 生成: < 200ms
   - decay_scan: < 1s (5K nodes)
```

**Phase 3：对接开源 Benchmark + 参数校准**

```
a) 复用 BEAM 多长度评测数据（100K/500K/1M tokens）
   - 跑 RollBall 检索管线，绘制 Accuracy@Length 曲线
   - 与 BEAM 论文基线对比，目标超过纯 context-stuffing 方案

b) 集成 LongMemEval 评估脚本
   - 用 LongMemEval 500 个标注问题跑 5 维评分
   - 目标：IE/MR/TR/KU 各维准确率达到 SOTA 系统的 90%+

c) 用 LoCoMo-Plus CCS 评估隐式偏好提取
   - 验证离线巩固的 Preference/Procedural 推断质量

d) 基于真实数据校准衰减参数
   - 固定 λ=0.03 运行 3 个月积累数据
   - Dormant→Active 恢复率 > 20% → 调小 λ
   - Active 持续增长、Dormant 转化率 < 5% → 调大 λ 或调低 BOOST_CAP
   - 用 BEAM 的 Memory Degradation Slope 指标验证实际衰减曲线
```

**发布前评测流程（Release Quality Gate）**

```
正式版本发布前必须通过：

① LongMemEval 5 维评分
   - IE ≥ 85%、MR ≥ 75%、TR ≥ 75%、KU ≥ 80%、Abs ≥ 70%
   （具体阈值基于 Phase 3 实测结果调整）

② BEAM Accuracy@Length
   - 100K: ≥ 90%、500K: ≥ 80%、1M: ≥ 70%
   - 衰减斜率优于纯 context-stuffing 基线

③ LoCoMo-Plus CCS
   - 隐式偏好推断准确率 ≥ 70%

④ 性能 SLA
   - 所有 SLA 指标通过（hybrid_search P99 < 100ms 等）

⑤ 可观测指标健康
   - 无未解决冲突积压（pending_conflicts < 10）
   - Dormant 恢复率 < 20%（衰减参数合理）
   - 存储增长可控（Active 节点不超过 10K/Agent）

评测结果写入发布说明，作为记忆系统质量的公开背书。
```

**3. 衰减参数策略**

```
参数外置 + 可观测 + 运行时可调：

所有衰减参数通过 manifest 可配置（符合 RXT-03）：
  [memory.decay]
  lambda = 0.03
  floor = 0.05
  boost_cap = 0.5
  access_per_hit = 0.1
  dormant_threshold = 0.3

Phase 2 不做模拟实验（模拟和真实差异太大）。
Phase 3 用真实数据 + BEAM 衰减曲线验证，有说服力。
```

**设计理由**：直接对接开源 Benchmark 而非自建评估体系，原因：① 开源框架经过学术同行评审，维度定义比自建更严谨；② 评测结果可与其他系统横向对比，作为产品质量背书；③ 评测集开源可复用，不需大量人工标注。衰减参数不做预实验，因为没有真实用户数据前模拟验证缺乏说服力。

### 6.12 工程约束：存储容量 + 并发控制 + 降级策略 + 故障处理（2026-04-21）

**问题**：存储容量规划、并发查询限制、embedding 模型降级策略、Grafeo 故障降级模式均未明确。

**1. 存储容量规划**

```
存储构成（单 Agent，100K episode 估算）：
  Episode 元数据          ~50MB
  对话历史原文         ~800MB
  KnowledgeNode          ~200MB（去重后沉淀）
  ProceduralNode         ~50MB
  AutobiographicalNode   ~20MB
  Embedding 向量          ~600MB（768维×f32）
  图边（关系）            ~100MB
  FTS 索引              ~200MB
  合计                   ~2GB

容量参数（Agent manifest 可配置，用户可修改）：
  [memory.storage]
  max_episodes_per_agent = 100000     # 默认上限，用户可调
  max_storage_mb_per_agent = 5000     # 默认 5GB，用户可调
  episode_archive_after_days = 90     # 对话原文 90 天后归档压缩

归档策略：
  - 90 天以上 episode 原文 → zstd 压缩归档，不参与 FTS
  - 归档后保留：episode_id、summary、时间范围
  - 接近上限时通知用户（80% 警告，95% 停止写入新 episode）
```

**2. 并发查询控制**

```
场景分析：
  - 多 Agent 同时运行：各 Agent 操作自己的 Grafeo 实例，天然隔离
  - 同 Agent 内：hybrid_search（读）+ decay_scan（写）可能并发
  - 离线巩固 + 实时操作：两个写操作可能触及同一节点

方案：
  - Grafeo 内部 RwLock 语义：多读并行，写操作串行
  - 离线巩固优先级低于实时操作：写入前检查是否有等待中的检索，有则 yield
  - 单 Agent 最多 1 个写操作 + N 个读操作
  - 不需要连接池、乐观并发控制——本地单用户场景没这个复杂度
```

**3. Embedding 模型降级策略**

```
三级降级链路：

  Level 0（正常）：本地 ONNX 模型
    延迟 < 50ms，质量最优

  Level 1（本地不可用）：远程 Embedding API
    触发：ONNX 加载失败 / 内存不足 / 模型文件缺失
    延迟 100-300ms，静默降级，用户无感

  Level 2（网络不可用）：仅 BM25 关键词检索
    触发：远程 API 连续 3 次超时或无 API Key
    延迟 < 10ms，通知用户"记忆检索已降级为关键词模式"
    graph_expand 权重从 0.2 提高到 0.4（图结构弥补语义缺失）

  降级状态机：
    Local ─(加载失败/OOM)─→ Remote ─(超时/无Key)─→ Disabled
      ↑                            ↑
      └──(定期重试，每5分钟)────┘

  manifest 配置：
    [memory.embedding]
    provider = "auto"                # local / remote / auto
    local_model = "bge-small-zh"
    remote_endpoint = ""             # 可选
    fallback_retry_interval_secs = 300
```

**4. Grafeo 故障处理**

```
故障场景：
  - 文件损坏（意外断电、磁盘错误）
  - 磁盘空间满（写入失败）
  - Schema 迁移失败（版本升级）

策略：停用 + 备份恢复，不做临时文件重放

  Grafeo 支持原生快照和增量备份：
  - 每天自动增量备份（Grafeo 内置能力）
  - 备份保留策略：日备份 7 天 + 周备份 4 周

  故障时流程：
  1. Gateway 检测到 Grafeo 异常（启动校验 / 运行时心跳失败）
  2. 立即停用记忆系统，通知用户"记忆系统暂时不可用，对话可继续但不使用历史记忆"
  3. 尝试从最近备份自动恢复
  4. 恢复成功 → 重新启用记忆系统，通知用户
  5. 恢复失败 → 提示用户手动介入（Desktop App 提供备份管理界面）

  健康检查：
  - Gateway 启动时验证 Grafeo 完整性
  - 运行时每 5 分钟心跳检查
  - 异常时立即停用，不等用户操作触发
```

**设计理由**：本地单用户场景不需要服务器级别的高可用设计。存储上限作为 Agent 配置参数允许用户根据磁盘空间自行调整。并发控制采用最简单的 RwLock，不过度设计。Embedding 降级保证无网络环境也能工作。Grafeo 故障利用其原生备份能力直接恢复，不做临时文件重放等复杂方案——降级不是灾难，保证对话能继续是底线，记忆事后可恢复。

### 6.13 隐私访问控制：架构级硬隔离 + Intent 响应过滤 + 隔离验证测试（2026-04-21）

**问题**：PrivacyLevel 只在打包分享时起作用，Runtime 中无实际访问控制；跨 Agent 隔离缺乏验证测试。

**结论：当前设计已经足够，Runtime 内部不需要增加访问控制**

```
RollBall 的隔离模型是进程级 + 存储级硬隔离，不是 prompt 级软约束：

  1. 每个 Agent 运行在独立进程中
  2. 每个 Agent 有独立的 Grafeo 数据目录
  3. Agent 之间只能通过 Intent 通信（Gateway 路由）
  4. Intent 查询的响应内容由被查询 Agent 自己决定返回什么

  Agent A 物理上读不到 Agent B 的 Grafeo 文件
  Agent A 无法绕过 Intent 直接访问 Agent B 的数据
```

**1. PrivacyLevel 在各层的实际作用**

| 层面 | PrivacyLevel 作用 | Phase |
|------|------------------|-------|
| 打包分享 | Personal/Sensitive 节点自动剥离（已设计） | Phase 2 |
| Intent 响应 | Gateway 层自动剥离 Sensitive 节点（新增） | Phase 2 |
| 云端同步 | Sensitive 不上传，Personal 仅同设备 | Phase 3 |
| Runtime 内部 | 不做过滤——Agent 查自己的记忆不需要权限检查 | N/A |

**2. Intent 响应过滤（Phase 2 新增）**

```
Agent B 响应 Intent 查询时：
  Agent B → 查 Grafeo → 返回结果
    → Gateway 检查响应中的 PrivacyLevel
    → Sensitive 节点自动剥离
    → 转发给 Agent A

为什么在 Gateway 而不是 Agent B 自己过滤？
  - Agent B 的 LLM 可能在回复中无意泄露 Sensitive 内容
  - Gateway 是信任边界——Agent 不可信，Gateway 可信
  - 与打包分享复用同一套 PrivacyLevel 过滤机制

局限性：
  - Gateway 只能过滤结构化的 PrivacyLevel 标记节点
  - LLM 生成的自由文本中可能包含敏感信息的复述
  - 这个问题靠规则解决不了（回到 LLM 优先原则）
  - Phase 3 可考虑离线审计：检查 Intent 响应中是否包含 Sensitive 关键词
```

**3. 跨 Agent 隔离验证测试（Phase 2 必做）**

```
隔离验证测试矩阵：

a) 存储隔离
   - Agent A 写入数据 → 验证 Agent B 的 Grafeo 中不存在
   - Agent A 的数据目录路径不包含 Agent B 的任何引用

b) 进程隔离
   - Agent A 崩溃 → Agent B 不受影响
   - Agent A 内存超限 → 不影响其他 Agent

c) Intent 隔离
   - Agent A 直接构造对 Agent B Grafeo 路径的文件访问 → 被 Gateway 拒绝
   - Intent 响应中 Sensitive 节点被 Gateway 自动剥离

d) 打包分享隔离
   - 打包后的 .agent 文件中不包含 Personal/Sensitive 节点
   - 提供验证工具可独立检查打包文件
```

**4. 不做的事**

```
- 不在 hybrid_search 中按 PrivacyLevel 过滤
  （Agent 查自己的 Grafeo，所有级别都应该可见）
- 不做 RBAC/ACL 权限模型
  （单用户本地场景，没有多用户角色）
- 不做 LLM 输出的实时敏感信息检测
  （回到 LLM 优先原则：规则检测泛化性差）
```

**设计理由**：RollBall 的隔离是架构级硬隔离（独立进程 + 独立 Grafeo），不是 prompt 级软约束。唯一需要补充的是 Gateway 层的 Intent 响应过滤和隔离验证测试。Runtime 内部不需要访问控制——Agent 查自己的记忆不需要权限检查。

### 6.14 存储格式：HNSW 参数 + Embedding 补生成 + 持久化语义（2026-04-21）

**问题**：HNSW 参数未定义；向量生成延迟的后台补生成机制不详；缺乏增量持久化（WAL）详细设计。

**1. HNSW 参数定义**

```
HNSW 参数（Grafeo 引擎全局配置，不暴露到 Agent manifest）：

  构建参数：
    M = 16                    # 每层连接数
    ef_construction = 100     # 构建时搜索宽度
  
  查询参数：
    ef_search = 64            # 查询时搜索宽度
  
  距离函数：
    distance_metric = "cosine"

参数选择理由：
  - M=16：<100K 向量的标准推荐值
    M=8 召回率下降明显，M=32 对小数据集无额外收益但内存翻倍
  - ef_construction=100：构建时多花一点时间换更好图质量
    对 memory_store 单条写入影响极小（<1ms 级别差异）
  - ef_search=64：查询精度和延迟的平衡点
    10K 向量规模下，ef=64 vs ef=128 召回率差异 <1%，延迟可达 2x

  ⚠️ 这些参数是 Grafeo 引擎内部默认值，不暴露给 Agent manifest
  理由：HNSW 参数是底层引擎实现细节，遵循 Grafeo 架构分层规范
  如需调优，通过 Grafeo 全局配置调整
```

**2. Embedding 后台补生成机制**

```
写入时流程：
  1. memory_store / episode 写入时同步尝试生成 embedding
  2. 超时 200ms → embedding 置为 None，标记 embedding_pending = true
  3. 写入 Grafeo 成功（节点立刻可被 BM25 检索，但不参与向量检索）

后台补生成（BackgroundEmbeddingWorker）：
  - 触发：写入时标记了 embedding_pending 的节点
  - 执行：按写入时间顺序，逐条生成 embedding 并更新节点
  - 频率：每 30s 扫描一次 pending 队列（空则跳过）
  - 并发：单线程串行，不与实时操作争资源
  - 失败处理：
    - Embedding 降级到 Level 2（Disabled）→ 暂停补生成
    - Embedding 恢复后 → 自动恢复
    - 最多重试 3 次，仍失败则保持 None（该节点永远走 BM25）

  可观测指标：
  - embedding_pending_count 纳入监控
  - 正常情况下 pending 数应趋近 0
  - 持续积压 → embedding 生成能力不足，可能需切换更小模型
```

**3. 增量持久化（WAL）语义**

```
记忆系统视角的持久化语义（WAL 实现是 Grafeo 引擎内部责任）：

  写入确认语义：
  - memory_store tool call 返回 Ok → 数据已持久化到 Grafeo WAL
  - 即使进程立刻崩溃，WAL 恢复后数据不丢失

  一致性保证：
  - hybrid_search 可读到所有已确认的写入（read-your-writes）
  - 后台任务（decay_scan / consolidation）的写入同样有 WAL 保护

  不在记忆设计文档中定义的（Grafeo 引擎内部）：
  - WAL 文件格式
  - Checkpoint 算法和策略
  - WAL 压缩和回收

  依据：Grafeo 架构分层规范——设计文档讨论引擎语义，不暴露存储实现细节
```

**设计理由**：记忆系统设计文档关注语义和行为承诺，Grafeo 引擎内部参数（HNSW、WAL）作为引擎层默认配置，遵循架构分层规范不向上暴露实现细节。HNSW M=16/ef_construction=100/ef_search=64 是小规模数据集的标准推荐。Embedding 补生成保证延迟不影响写入可用性，节点立刻可被 BM25 检索，后台补全向量能力。

### 6.15 生命周期管理：Dormant→Purge 精确化 + purge_log 恢复机制（2026-04-21）

**问题**：Dormant→Purge 判断条件模糊；purge_log 恢复机制实现细节不全。

**1. Purge 触发条件（三条路径，满足任一即可）**

```
路径 1：正常衰减（后台 decay_scan）
  条件：Dormant > dormant_purge_days AND importance < purge_importance_threshold
  默认：90天 + importance < 0.5
  含义：低价值 + 长期沉睡 = 清理候选
  高价值偏好（importance ≥ 0.5）即使 Dormant 也不 Purge，只沉睡

路径 2：容量压力（存储接近上限时触发）
  触发条件：Agent 存储用量 > max_storage_mb 的 90%
  执行顺序：
    ① 先 Purge 所有已满足路径 1 条件的节点
    ② 仍不够 → 放宽条件：Dormant 节点按 decay_score 升序逐批 Purge
       （最不活跃的优先删除，不管 importance）
    ③ 仍不够 → 通知用户手动清理（不自动删 Active 节点）

  关键约束：
    - Fact/Relation 和 AutobiographicalNode 即使容量压力也不自动 Purge
    - 只对 Preference/ProceduralNode 的 Dormant 节点动手
    - 每次强制 Purge 都记 purge_log（reason = CapacityPressure）

路径 3：用户手动
  用户可随时 Purge 任意节点（Desktop App Memory 管理面板）

参数（manifest 可配置）：
  [memory.lifecycle]
  dormant_purge_days = 90
  purge_importance_threshold = 0.5
```

**状态转换更新（替代原 05-memory.md §5.2 的纯时间条件）：**

| 节点类型 | Dormant → Purge 条件 | 永不 Purge |
|---------|---------------------|------------|
| Fact / Relation | — | 是（容量压力也不删） |
| Preference | 路径 1：Dormant > 90天 AND importance < 0.5；路径 2：容量压力 | importance ≥ 0.5 时仅容量压力可触发 |
| ProceduralNode | 同 Preference | 同 Preference |
| AutobiographicalNode | — | 是（容量压力也不删） |

**2. purge_log 恢复机制**

```
purge_log 数据结构：

  PurgeLogEntry {
    id: PurgeLogId,
    purged_at: DateTime,
    node_id: NodeId,
    node_type: NodeType,              // Preference / ProceduralNode
    node_content: String,             // 节点完整内容的 JSON 快照
    node_embedding: Option<Vec<f32>>, // 原始 embedding（恢复后不用重新生成）
    importance: f32,
    purge_reason: PurgeReason,        // DormantExpired / CapacityPressure / UserManual
    related_edges: Vec<EdgeSnapshot>, // 被同时删除的边的快照
    source_episode_ids: Vec<EpisodeId>,
    restored: bool,                   // 是否已被恢复
  }

恢复流程（用户在 Desktop App 点击"恢复"）：
  1. 从 purge_log 取出 PurgeLogEntry
  2. 重建节点（用快照中的完整内容，含 embedding）
  3. 重建被删除的边（用 related_edges 快照）
  4. 节点状态设为 Active，last_accessed 设为当前时间
  5. access_count 保留原值（历史使用频率不丢失）
  6. dormant_since 清除
  7. 标记 purge_log entry.restored = true

边界条件：
  - source_episode 已被清理 → 恢复节点但 source_episode 置空
  - 关联节点也已被 Purge → 不恢复该边（单向恢复，不级联）
  - purge_log 已超过 30 天 → 不可恢复（彻底删除）

purge_log 存储：
  - 存在 Grafeo 内部（专用 collection，不参与 hybrid_search）
  - 30 天后自动清理（decay_scan 顺带处理）
```

**3. 完整生命周期状态图**

```
写入（memory_store / 离线巩固）
        │
        ▼
  PendingKnowledge ──(离线巩固)──→ Active
  （实时写入，无三元组）       （正式节点，有图结构）
        │                        │
        │ (检索命中)           │ decay_score < 0.3
        │ ↓ 也可被 BM25 检索    ▼
        │                    Dormant
        │                      │  ↑
        │                      │  │ (被重新引用)
        │                      │  └─────────
        │                      │
        │            ┌───────┼───────┐
        │            │       │       │
        │         路径 1  路径 2  路径 3
        │         衰减    容量    手动
        │            │       │       │
        │            └───────┼───────┘
        │                      ▼
        │                Purge 流程
        │                (检查关联节点 + 转移引用)
        │                      │
        │                      ▼
        │              purge_log（30天可恢复）
        │                      │
        │                      ▼
        │              彻底删除（不可逆）

特殊路径：
  - Fact/Relation：Active ↔ Dormant，永不 Purge
  - AutobiographicalNode：始终 Active，不参与衰减
```

**关于 LLM 与 Purge 判断的关系：**

```
Purge 判断不让 LLM 参与，理由：
  - Purge 是批量操作（decay_scan 可能涉及数百个节点）
  - 逐个调 LLM 判断成本太高且阻塞后台扫描
  - importance 已经是 LLM 在写入时给出的判断
  - Purge 条件是机械性的（时间 + 数值阈值 + 容量）
  - 符合 LLM 优先原则边界：规则能解决的机械性判断不需要 LLM
```

**设计理由**：原设计的纯时间条件（90天）会错误清理高价值偏好。新增 importance 保护确保重要偏好只沉睡不删除。容量压力路径是系统自保机制，按 decay_score 升序逐批清理，保证最不活跃的优先删除。purge_log 存储完整快照（含 embedding + 边），支持 30 天内一键恢复。

### 6.16 持久化：备份策略 + Schema 版本迁移（2026-04-21）

**问题**：缺乏备份策略和 Schema 版本迁移方案。

**1. 备份策略**

```
自动备份（Grafeo 原生能力）：
  - 频率：每天一次增量备份（默认凌晨 3:00，可配置）
  - 触发：Gateway 后台调度，不阻塞 Agent 运行
  - 类型：增量备份（只备份自上次以来的变更）

手动备份：
  - 用户可通过 Desktop App 手动触发全量快照
  - 场景：大版本升级前、重要对话后

保留策略：
  - 日备份：保留最近 7 天
  - 周备份：保留最近 4 周（每周日的日备份自动提升为周备份）
  - 超出保留期的备份自动清理

存储位置：
  - 默认：Agent 数据目录下的 backups/ 子目录
  - 可配置为外部路径（如外接硬盘）

备份大小估算：
  - 增量备份通常很小（每天变更量 << 全量）
  - 全量快照 ≈ Grafeo 存储大小（zstd 压缩后约 50-70%）
  - 7 日增量 + 4 周备份 ≈ 全量的 2-3 倍空间

manifest 配置：
  [memory.backup]
  enabled = true
  schedule_hour = 3
  daily_retention_days = 7
  weekly_retention_weeks = 4
  backup_dir = ""                # 空=默认路径
```

**2. Schema 版本迁移**

```
版本标识：
  - Grafeo 数据库中存储 schema_version（整数，递增）
  - 每个 rollball-grafeo crate 版本声明其支持的 schema 版本范围

启动时检查流程：
  ┌─ 读取数据库 schema_version
  │
  ├─ == 代码期望版本 → 正常启动
  │
  ├─ < 代码期望版本 → 自动向前迁移
  │   ├─ 先触发自动全量备份（迁移前安全网）
  │   ├─ 逐版本执行迁移脚本：v1→v2→v3→...
  │   └─ 全部成功 → 更新 schema_version → 正常启动
  │
  ├─ > 代码期望版本 → 拒绝启动
  │   └─ 提示"数据库版本高于当前软件，请升级软件"
  │
  └─ 迁移失败 → 回滚到迁移前备份
      └─ 提示"迁移失败，已恢复原数据"

迁移脚本管理：

  trait SchemaMigration {
      fn version(&self) -> u32;                       // 目标版本号
      fn up(&self, store: &GrafeoStore) -> Result<()>; // 向前迁移
      fn description(&self) -> &str;                   // 迁移说明
  }

  迁移注册表示例：
  migrations: vec![
      MigrateV1ToV2 { /* 新增 embedding_pending 字段 */ },
      MigrateV2ToV3 { /* 新增 PurgeLogEntry collection */ },
      MigrateV3ToV4 { /* 调整 HNSW 索引参数 */ },
  ]

安全保障：
  - 迁移前自动全量备份（不依赖日常备份）
  - 迁移在事务中执行（Grafeo WAL 保护）
  - 单次迁移失败 → 整个迁移链回滚
  - 迁移日志记录到独立文件

不做降级迁移（down）：
  - 降级场景极少且风险高
  - 替代方案：回滚到迁移前的自动备份
```

**3. 架构分层**

```
备份和迁移的架构位置：

  MemoryStore trait：不变
    - 备份/迁移不是记忆操作，不属于 MemoryStore 职责

  GrafeoStore 实现层：内部实现
    impl GrafeoStore {
        fn create_backup(&self, backup_type: BackupType) -> Result<BackupInfo>;
        fn restore_from_backup(&self, backup_id: &str) -> Result<()>;
        fn check_schema_version(&self) -> Result<SchemaStatus>;
        fn run_migrations(&self) -> Result<MigrationResult>;
    }

  Gateway 层：调度入口
    - 启动时：check_schema_version → 需要则 run_migrations
    - 定时：按配置执行 create_backup
    - Desktop App API：手动备份 / 查看备份列表 / 恢复

  MemoryManager：不感知备份和迁移
    - 只管记忆的读写和生命周期
    - 备份和迁移在它"下面"透明完成
```

**设计理由**：备份和迁移是存储引擎的责任，不是记忆系统的责任。遵循 Grafeo 架构分层规范，MemoryStore trait 保持纯粹的记忆操作接口。迁移采用逐版本递进策略，迁移前自动备份是安全网，失败则回滚。不做降级迁移，用备份替代——简单可靠。
