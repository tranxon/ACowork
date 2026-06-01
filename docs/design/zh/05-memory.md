# Memory 仿生分层架构

> 版本：v3.7 | 更新日期：2026-04-22

> 本文档基于 docs/review/07/08-memory review 的设计补充。主要变更：新增 Abstention 拒答机制（§6.5）、冲突检测升级为三层信号模型（§6.4）、新增质量评估框架章节（§11）、即时/离线巩固边界明确化（§4）、检索权重动态调整（§6.6）。

> **v3.8 变更（2026-05-28）**：上下文压缩策略大幅简化，程序化折叠策略全部放弃——见 [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md)。核心变更：移除内容折叠（Phase 1）、三阶段渐进裁剪、检索结果 8 级优先级、弹性预算分区。瞬态层压缩简化为：70% 告警 → 80% LLM 摘要（完整上下文） → 95% emergency_trim 安全网。

> **v3.9 变更（2026-05-28）**：经历层写入来源简化——见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)。核心变更：移除每轮对话实时写入 Grafeo，经历层仅通过 Compaction 摘要和 Session 关闭蒸馏写入。Compaction 与 Distillation 统一为单次 Compact Model 调用（"摘要即蒸馏"）。

---

Memory 采用**仿生分层**设计，以人类认知科学为参照，以 Grafeo 图数据库为存储引擎。每个 Agent 拥有完全独立的私有 Memory，不存在 Gateway 维护的公共数据库。跨 Agent 的数据共享通过 Intent 查询和系统 Agent 服务实现，而非共享存储。

**设计哲学**：记忆不是存储，是认知。一个没有遗忘的记忆系统是垃圾场，一个没有巩固的记忆系统是碎片堆，一个没有自我认知的记忆系统是数据库。Memory 模块要回答的不是"怎么存"，而是"怎么记、怎么忘、怎么想"。

```
┌─────────────────────────────────────────────────────────┐
│  瞬态层（Transient）                                     │
│  ───                                                    │
│  工作记忆 — LLM 上下文窗口                               │
│  当前对话、推理链、注意力焦点                             │
│  生命周期：单次会话                                      │
│  仿生对应：前额叶持续放电                                 │
├─────────────────────────────────────────────────────────┤
│  经历层（Experiential）                                  │
│  ───                                                    │
│  情景记忆 — Grafeo episodic                              │
│  交互片段、对话快照、感知原始记录                         │
│  Grafeo 原生 HNSW 向量索引 + BM25 全文检索                │
│  生命周期：天→周，巩固后晋升至沉淀层                      │
│  仿生对应：海马体临时编码                                 │
├─────────────────────────────────────────────────────────┤
│  沉淀层（Consolidated）                                  │
│  ───                                                    │
│  语义记忆 — 事实、偏好、关系（KnowledgeNode）             │
│  程序记忆 — 行为模式、操作规则（ProceduralNode）          │
│  自传体记忆 — 自我认知、能力边界（AutobiographicalNode）  │
│  LPG 知识图谱 + GQL 原生关联扩散检索                      │
│  生命周期：长期至永久，遗忘衰减但不轻易删除               │
│  仿生对应：新皮层长期存储                                 │
└─────────────────────────────────────────────────────────┘

         ┌─── 巩固管道 ───┐
         │                 │
    经历层 ──(即时提取)──→ 沉淀层    ← LLM 自主 tool call（memory_store）
         │                 │
    经历层 ──(离线回放)──→ 沉淀层    ← 空闲时专用 LLM 调用（Phase 3）
         │                 │
    沉淀层 ──(遗忘衰减)──→ dormant → (可选 purge)
         │                 │
    沉淀层 ──(关联扩散)──→ 多跳检索结果
```

## 0. 分层原则

**为什么按认知功能分而不是按存储位置分？**

旧版三层（工作记忆 / 私有记忆 / 云端同步）混淆了两个维度——"工作记忆"是认知功能，"私有记忆"是存储位置，"云端同步"是同步机制。仿生分层统一按认知功能划分，每层有明确的职责边界和信息流动规则。

**三层之间的流动规则：**

| 流动方向 | 机制 | 触发条件 |
|---------|------|---------|
| 瞬态层 → 经历层 | 摘要写入 | Compaction 触发（80% token 使用）或 Session 关闭时，LLM 摘要异步写入 Grafeo。不再每轮写入，避免与 JSONL 冗余 |
| 经历层 → 沉淀层 | 巩固管道 | 即时提取（LLM 自主 tool call）+ 离线回放（空闲时专用调用） |
| 沉淀层 → 瞬态层 | 检索注入 | 用户输入到达时，检索相关记忆注入上下文 |
| 沉淀层/经历层内流动 | 关联扩散 | 检索时沿图边 1-2 跳扩展 |
| 沉淀层 → Dormant | 遗忘衰减 | 后台定期计算 decay_score |

**不可逆的单向门：** 经历层 → 沉淀层是信息精炼过程（原始片段 → 结构化知识），天然单向。但沉淀层 → 经历层可以通过"回忆"机制实现——用户或 Agent 主动触发时，从沉淀层提取关联知识，作为新的情景上下文注入瞬态层。

**分层与 Grafeo 存储的映射：**

| 认知层 | 内容 | Grafeo 存储 | 说明 |
|--------|------|-------------|------|
| 瞬态层 | 工作记忆 | 不在 Grafeo 中 | LLM 上下文窗口，纯进程内存 |
| 经历层 | 情景记忆 | `Episodic` Label | Grafeo 原生 HNSW + BM25 + metadata |
| 沉淀层 | 语义/程序/自传体/Skill 经验 | `Knowledge` / `Procedural` / `Autobiographical` Label + Edge | LPG 知识图谱 |

不存在"经历层节点存在 Grafeo semantic 中"的歧义——认知分层和 LPG Label 是一一映射的，存储格式为 `.grafeo` 单文件。

## 0.1 LLM 优先原则

**信任 LLM 超过信任规则——除非规则能解决 LLM 不能解决的问题。**

记忆系统中涉及大量语义判断（什么值得记、confidence 多高、是否冲突、如何分类），这些判断由 LLM 完成而非规则引擎。具体应用：

- **即时提取**：LLM 自主判断是否调用 memory_store、评估 confidence（high/medium/low），Runtime 不做语义层面的二次检查
- **离线巩固**：三元组提取、冲突分类、证据验证由 LLM 在有完整上下文时执行，而非实时阶段的规则近似
- **Runtime 仅做机械护栏**：内容长度限制、调用频率限制、安全过滤——这些是 LLM 无法自我约束的机械性限制
- **摘要时实体/三元组提取**：Compaction 触发时，Compact Model 在生成摘要的同时提取实体和三元组。不再每轮提取（v3.10 简化）

## 1. 瞬态层：工作记忆

工作记忆是 Agent 当前正在"思考"的内容，直接映射到 LLM 的上下文窗口。

```
┌─ System Prompt ──────────────────────────┐
│  Agent 身份定义                            │
│  自传体记忆摘要（来自沉淀层注入）           │
│  Skill Instructions                       │
│  工具定义                                  │
├─ Retrieved Memory ───────────────────────┤
│  巩固层检索结果（语义/程序/自传体）         │
│  经历层检索结果（相似情景）                 │
│  关联扩散结果                              │
├─ Conversation ──────────────────────────┤
│  用户消息 + Agent 回复                     │
│  工具调用与结果                            │
│  （工具调用与结果保存在对话历史中）       │
├─ Scratchpad ────────────────────────────┤
│  Agent 内部推理链                          │
└──────────────────────────────────────────┘
```

**实体与三元组提取（v3.10 简化）：**

不再每轮通过 memory_hint 提取实体——改为在 Compaction 触发时由 Compact Model 一次性完成。Compact Model 输出格式：

```
<summary>
自然语言摘要文本...
</summary>
<entities>
Entity1, Entity2, Entity3
</entities>
<triples>
subject | predicate | object
subject | predicate | object
</triples>
```

- **entities**：跨轮持续出现的核心实体（人、地点、技术、项目、概念），最多 10 个，逗号分隔
- **triples**：显式事实知识，subject|predicate|object 格式，一行一个

设计理由：
- 每轮提取的成本（~65 tokens/轮）在 ADR-011 之后不再合理——经历层不再逐轮写入，存储目的已消失
- Compact Model 有完整对话上下文，实体和三元组的提取质量高于逐轮快照
- Compaction 是低频操作（每 80% 触发），边际成本可忽略
- 检索策略始终使用默认权重，不做类型驱动的动态调整——经评估 f/r 类型的微调收益未被验证

**检索策略（v3.10 简化）：**

所有检索统一使用默认 RRF 权重（vector: 0.7, text: 0.3），不再基于 memory_hint 类型动态调整。HintType 枚举保留但仅在 `memory_store` 工具调用时由 LLM 显式指定（用于即时提取管道的 sub_type 分类）。

**瞬态层的管理策略（v3.8 简化）**：

上下文压缩是一个语义理解任务，只有 LLM 能可靠判断哪些信息可以丢弃。程序化策略（字符截断、FIFO、角色折叠）本质是用 proxy 指标替代语义理解，必然失效。因此所有日常程序化折叠策略已被放弃，压缩简化为三阶段：

| 阶段 | 触发条件 | 行为 |
|------|---------|------|
| Stage 1: 监控 | 70% context 使用率 | 日志记录，不干预 |
| Stage 2: LLM 摘要 | 80% context 使用率 | Compact Model 对完整上下文做 LLM 摘要。不做任何折叠/截断预处理。保护 system prompt + 最近 2-3 轮，中间段压缩。完整历史归档至临时文件。 |
| Stage 3: 紧急裁剪 | 95% / API ContextOverflow | emergency_trim（保留最后 N 条非 system），作为安全网 |

> **设计决策**：详见 [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md)。

## 2. 经历层：情景记忆

情景记忆存储 Agent 与用户的交互片段，是记忆的"原始素材"。

```
Grafeo Episodic Store
├── episode_id: String              // 唯一 ID
├── timestamp: DateTime             // 发生时间
├── role: Role                      // user / agent / tool
├── content: String                 // 内容（对话原文或 Compaction 后的摘要）
├── embedding: Vec<f32>             // 语义向量
├── metadata: HashMap<String, Value>  // 上下文元数据（话题、情感倾向等）；不预存关联 node_id，跨层扩散通过 source_episode 反向查询实现
├── session_id: String              // 所属会话
├── consolidated: bool              // 是否已巩固到沉淀层
└── importance: f32                 // 重要性评分（写入时 LLM 打分 0.0-1.0）
```

> **v3.10 设计简化**：已移除 Episode 的 `content_type`（ContentType）和 `artifact_refs`（ArtifactRef）字段。
> Episode 内容不再做分类压缩——原始对话直接存储，Compaction 时由 Compact Model 输出摘要。
> 理由详见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)：Compaction = Distillation，摘要即蒸馏。

**关键设计决策：Episode 内容存储策略**

- **v3.10**：Episode 内容不再做分类压缩。对话原文直接完整存储，摘要由 Compaction 阶段的 Compact Model 生成。
- **Compaction**：当上下文使用率达 80% 时，Compact Model 对完整上下文做自然语言摘要（含实体和三元组提取），摘要写入 Grafeo 蒸馏 Episode。
- 理由详见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)：Compaction = Distillation，摘要即蒸馏。

**检索能力（基于 grafeo-engine 原生 API）：**

- **语义检索**：`db.vector_search()` — Grafeo 原生 HNSW 向量索引，支持余弦/欧几里得/点积距离，SIMD 加速
- **关键词检索**：`db.text_search()` — Grafeo 原生 BM25 全文索引，内置 Unicode 分词器
- **混合检索**：`db.hybrid_search()` — Grafeo 原生 RRF 融合排序，支持 `topology_boost` 图连通性重排序
- **MMR 去重**：`db.mmr_search()` — Maximal Marginal Relevance，保证结果多样性，避免重复语义
- **时间过滤**：按时间范围缩小检索空间
- **跨层关联扩散**（§6）：检索到的 episode 通过沉淀层 KnowledgeNode 的 `source_episode` 字段反向查询关联节点，沿 GQL 原生图遍历扩展到沉淀层知识和其他经历层 episode。例如：用户问"上次去上海住的酒店"，episodic 检索到出差记录 → 反向查到沉淀层"用户常住锦江之星" → 通过 `MATCH (m)-[r*1..3]-(other)` 图遍历扩展到同一酒店的另一次出差 episode。

**Embedding 生成策略：**

Embedding 由 Runtime 层通过 `EmbeddingProvider` trait 生成（而非 GrafeoStore 内部），以 `Vec<f32>` 形式传入 `Episode` / `MemoryQuery`。

**Provider 降级链**：Ollama local（primary，`nomic-embed-text`，768d）→ Remote API（fallback，OpenAI-compatible `/embeddings`，512-1536d）。`FallbackEmbeddingProvider` 自动管理 primary→fallback 切换（2 次连续失败 + 200ms 超时）。

**生成时机**：
- 检索时：`MemoryManager.retrieve()` 方法头部自动生成 embedding（200ms 超时），超时/失败则 `query.embedding = None`，退回 `text_search` 纯文本检索
- 写入时：episode 蒸馏写入时同步生成 embedding，同样 200ms 超时降级

GrafeoStore 仅负责存储和索引，不持有 `EmbeddingProvider`。

**经历层的遗忘：**

情景记忆的遗忘比沉淀层更激进——这是自然的，因为海马体本身就是临时编码区。

- **默认保留期**：14 天（可配置）
- **巩固标记**：已被提取到沉淀层的情景标记 `consolidated = true`
- **清理策略**：
  - 已巩固 + 超过 7 天 → 自动清理（知识已转移到沉淀层，原始片段不再需要）
  - 未巩固 + 超过 14 天 + importance < 0.3 → 清理（低价值且未被提取的碎片）
  - 未巩固 + 超过 14 天 + importance >= 0.3 → 保留并尝试离线巩固

## 3. 沉淀层：长期记忆

沉淀层是 Agent 的"知识根基"，包含三种记忆类型，全部存储在 Grafeo 的语义记忆图谱中。

### 3.1 语义记忆（KnowledgeNode）

存储从交互中提取的结构化知识——事实、偏好、关系。

```rust
struct KnowledgeNode {
    node_id: String,
    node_type: KnowledgeType,        // Fact / Preference / Relation
    subject: String,                 // 知识主体（通常是"用户"）
    predicate: String,               // 关系/属性
    object: String,                  // 值/目标
    confidence: f32,                 // 置信度 0.0-1.0
    source_episode: Vec<String>,     // 来源情景 ID（可追溯）
    created_at: DateTime,
    updated_at: DateTime,

    // === 遗忘机制字段 ===
    importance: f32,                 // 写入时 LLM 打分 0.0-1.0
    access_count: u32,               // 检索命中次数
    last_accessed: DateTime,         // 最后一次被检索
    decay_score: f32,                // 运行时计算的衰减分数
    status: NodeStatus,              // Active / Dormant / Purged
    dormant_since: Option<DateTime>, // 进入 Dormant 状态的时间（Purge 90 天计时起点）

    // === 隐私级别 ===
    privacy: PrivacyLevel,           // Public / Personal / Sensitive
}

enum KnowledgeType {
    Fact,        // 事实："用户住在北京"
    Preference,  // 偏好："用户喜欢简洁的回复"
    Relation,    // 关系："用户的经理是王五"
}

enum NodeStatus {
    Active,     // 正常参与检索
    Dormant,    // 衰减低于阈值，不参与常规检索但保留
    Purged,     // 已清除（仅 purge 操作）
}

enum PrivacyLevel {
    Public,     // 可跨 Agent 共享（如用户姓名）
    Personal,   // Agent 私有（如用户偏好风格）
    Sensitive,  // 敏感信息，打包分享时剥离
}
```

**节点之间的关系边：**

```
KnowledgeNode:张三 ──[LIVES_IN]──→ KnowledgeNode:北京
KnowledgeNode:张三 ──[PREFERS]───→ KnowledgeNode:简洁回复
KnowledgeNode:张三 ──[MANAGED_BY]→ KnowledgeNode:王五
KnowledgeNode:北京 ──[IS_CAPITAL_OF]→ KnowledgeNode:中国
```

边也有属性——权重（strength）、来源、创建时间。边的权重影响关联扩散的传播强度。

**边权重计算规则：**

```
edge_strength = min(0.8, confidence_avg × recency_factor)

其中：
- confidence_avg = (source_node.confidence + target_node.confidence) / 2
- recency_factor = exp(-0.01 × days_since_edge_created)
  （边的衰减比节点慢，半衰期约 69 天，因为关系比事实更持久）
- 上限 0.8 防止任何单条边权重过高导致扩散偏向
```

边的权重在创建时计算，后续 decay_scan 时同步更新。边不独立存储 decay_score——边的存亡取决于两端节点：任一端被 purge 时，相关边自动删除。

### 3.2 程序记忆（ProceduralNode）

存储"在什么情况下该怎么做"的行为模式，与 Skill 系统互补。

```
Skill 系统的程序记忆：SkillExperience（Skill 级别，特定技能的执行经验）
Grafeo 的程序记忆：ProceduralNode（跨 Skill 的通用行为模式）
```

```rust
struct ProceduralNode {
    node_id: String,
    trigger_condition: String,       // 触发条件："用户连续两次纠正格式"
    action_pattern: String,         // 行为模式："停止使用 Markdown 表格，改用纯文本列表"
    confidence: f32,                 // 置信度
    activation_count: u32,           // 被激活应用的次数
    source_skill: Option<String>,    // 来源 Skill（如有）
    learned_from: String,            // "用户反馈" / "执行失败" / "自我评估"

    // 遗忘字段（同 KnowledgeNode）
    importance: f32,
    access_count: u32,
    last_accessed: DateTime,
    decay_score: f32,
    status: NodeStatus,
    dormant_since: Option<DateTime>,  // 进入 Dormant 的时间（Purge 90 天计时起点）

    created_at: DateTime,
    updated_at: DateTime,
}
```

**与 SkillExperience 的关系：**

| 维度 | ProceduralNode | SkillExperience |
|------|---------------|-----------------|
| 作用域 | 跨 Skill 的通用行为 | 特定 Skill 的执行经验 |
| 来源 | 用户反馈 / 执行失败总结 | Skill 每次执行的记录 |
| 注入位置 | System Prompt 的行为准则 | Skill Instruction 的经验补充 |
| 示例 | "用户不喜欢长回复" | "weekly-report Skill 在 qwen3:8b 上需要扁平化指令" |

**程序记忆与 Skill 经验的联动：**

当一个 ProceduralNode 的 `source_skill` 非空时，它与对应 Skill 的 SkillExperience 形成交叉引用。例如，weekly-report Skill 多次因"输出太长"被用户纠正 → SkillExperience 记录 failure_case → 巩固管道提取出通用 ProceduralNode："此用户偏好简洁输出" → 这个 ProceduralNode 会影响所有 Skill 的执行，不只是 weekly-report。

### 3.3 自传体记忆（AutobiographicalNode）

存储 Agent 对自身的认知——"我是谁、我能做什么、我的边界在哪"。这是人格连续性的基础。

```rust
struct AutobiographicalNode {
    node_id: String,
    aspect: AutobiographicalAspect,  // 自我认知的维度
    content: String,                 // 具体内容
    confidence: f32,
    source: String,                  // "manifest" / "self_evaluation" / "user_statement"
    updated_at: DateTime,

    // 自传体记忆不参与遗忘衰减——这是 Agent 的核心身份
    // 但可以被更新（如用户改名、Agent 学会了新 Skill）
    //
    // ⚠️ status 始终为 Active，遗忘扫描跳过此类型节点
    // schema 中 status 列对 AutobiographicalNode 不可修改
}

enum AutobiographicalAspect {
    Identity,           // 身份声明："我是天气助手，帮助你了解天气信息"
    Capability,         // 能力范围："我能查询全球城市天气、给出穿衣建议"
    Limitation,         // 能力边界："我无法预测超过 7 天的天气"
    Preference,         // 自身偏好："我倾向于先给结论再解释原因"
    History,            // 重要经历："2026-04-14 用户教我生成周报，这是我的第一个 Skill"
    Relationship,       // 与用户的关系："我和张三合作了 3 个月，他喜欢简洁风格"
}
```

**自传体记忆的来源：**

1. **Manifest 派生**（自动）：从 `manifest.toml` 的 `agent.name`、`agent.description`、`skills/` 列表自动生成 Identity 和 Capability 节点
2. **自我评估**（定期）：Agent 空闲时，根据 SkillExperience 的 model_compatibility 和执行统计，生成/更新 Limitation 节点（"在 qwen3:8b 上，复杂推理任务的成功率约 60%"）
3. **用户陈述**（即时）：用户直接表达对 Agent 的评价（"你太啰嗦了"），巩固管道提取为 Preference 节点
4. **重要事件**（即时）：关键交互（首次学会新 Skill、重大错误修正、用户表达强烈情绪）记录为 History 节点

**自传体记忆的注入：**

自传体记忆摘要始终注入 System Prompt 的最前面（在 Agent 身份定义之后），作为 Agent 的"自我认知背景"：

```
## 关于你自己

你是「天气助手」，帮助用户了解天气信息。
你能查询全球城市天气、给出穿衣建议，但无法预测超过 7 天的天气。
你和张三合作了 3 个月，他偏好简洁的回复风格。
你在 qwen3:8b 模型上复杂推理的成功率约 60%。
```

**自传体容量管理：**

AutobiographicalNode 不参与遗忘，但需要容量控制防止无限膨胀：

- **History 节点摘要压缩**：当 History 节点超过 10 条时，离线巩固阶段自动将多条旧 History 合并为一条摘要节点（"2026 年 4-6 月主要事件：学会了周报和代码审查两个 Skill，与用户磨合期结束"），原始节点转为 Dormant（不 purge）
- **注入上限**：自传体摘要注入 System Prompt 时，按重要性取 Top-K（Identity / Capability / Limitation 必注入，History 取最近 5 条摘要 + 最近 3 条明细，Relationship 取 Top-3）
- 总 token 预算：自传体不超过 200 token（约 150 个中文字符）

## 4. 巩固管道

巩固管道是经历层→沉淀层的信息精炼过程，模拟海马体→新皮层的记忆巩固。

### 4.1 即时提取（Phase 1）

即时提取通过 **Tool Call 机制**实现——`memory_store` 作为 Agent 的内置工具之一，LLM 在生成回复时自主判断是否调用。无需额外的 LLM 调用、异步管道或预过滤规则。

**即时提取产出定义（v3.7 明确化）**：

即时提取阶段产出 **PendingKnowledgeNode**，与正式 KnowledgeNode 有明确区分：

```
PendingKnowledgeNode：
  confidence = 0.7（默认值）
  status = Pending（立即可检索但标记为"待确认"）
  参与常规 hybrid_search，但结果中标注 [待确认] 标记
  不参与 graph_expand（待确认节点不做关联扩散）

高置信度直接生效：
  如果即时提取的 confidence >= 0.85（即 LLM 输出 confidence="high"），
  直接创建正式 KnowledgeNode（status = Active），无需离线确认
  → 适用于用户明确表达的事实（如"我住在北京"），避免高确定性信息被不必要地标记为待确认
```

**设计决策：Tool Call 而非单独调用**

| 维度 | 单独调用 LLM | Tool Call（当前选择） |
|------|-------------|---------------------|
| 额外 API 成本 | 每轮 0-1 次额外调用 | 零额外调用 |
| 架构复杂度 | 高（异步管道 + 队列 + WAL + 预过滤） | 低（工具定义天然集成） |
| 预过滤 | Runtime 硬编码规则 | LLM 自主判断（天然过滤器） |
| 上下文共享 | 需重新输入对话 | 共享当前对话上下文 |
| 用户可观测性 | 黑箱（异步管道不可见） | 透明（tool call 在对话历史中可见） |
| Token 开销 | 0（按需调用） | 每轮多 ~150 token（工具定义 + 提取指引） |

选择 Tool Call 的核心理由：即时提取的目标是"能用"而非"完美"。LLM 天然具备判断"什么值得记住"的能力——"今天天气如何"不值得存，它自己就知道。Phase 3 的离线巩固再用专用 prompt 做深度提取补漏。

**memory_store 工具定义（Phase 2 简化版）**：

```json
{
  "name": "memory_store",
  "description": "存储值得长期记住的用户信息或行为模式。仅在对话中包含新的、重要的、非临时性信息时调用。不要存储显而易见的常识或临时性信息。",
  "parameters": {
    "type": "object",
    "properties": {
      "content": {
        "type": "string",
        "description": "要记住的内容，用自然语言描述（如「用户住在上海」），不需要拆分成三元组"
      },
      "category": {
        "type": "string",
        "enum": ["fact", "preference", "procedure"],
        "description": "信息类型：fact=客观事实, preference=用户偏好, procedure=行为模式"
      },
      "confidence": {
        "type": "string",
        "enum": ["high", "medium", "low"],
        "description": "置信度：high=用户明确表达的, medium=推测的, low=不确定的。LLM 自己判断，可选，默认 medium"
      },
      "keywords": {
        "type": "array",
        "items": { "type": "string" },
        "description": "关键词，可选。Runtime 会自动从 memory_hint.e 补充，通常不需要填写"
      }
    },
    "required": ["content", "category"]
  }
}
```

**接口简化设计理由**：
- 旧设计要求 LLM 拆分三元组 `{subject, predicate, object}`，负担重且不可靠（同一事实可能拆出不同 predicate）
- 新设计让 LLM 用自然语言描述要记住什么，Runtime 负责后续的结构化处理
- keywords 可复用同轮 memory_hint.e 的值，LLM 甚至可以不填，由 Runtime 自动合并
- 三元组提取移至离线巩固阶段（Phase 3），此时 LLM 有完整上下文和充裕时间

详见 docs/review/04-p2-s2-design-review.md §6.9

**即时阶段 Prompt 职责（v3.7 明确化）**：

即时提取的 Prompt 职责限定为**轻量操作**，与离线巩固的深度操作有明确分工：

```
即时阶段 Prompt（约 100 tokens）：
  ─ 事实识别：判断本轮是否包含值得记住的新信息
  ─ 类型标注：category = fact / preference / procedure
  ─ 关键词提取：提取核心实体（复用 memory_hint.e）
  ─ 置信度评估：confidence = high / medium / low

  不做的事：
  ✗ 关联发现（无法跨轮次）
  ✗ 冲突判定（缺乏完整上下文）
  ✗ 模式提炼（需要多轮数据）
  ✗ 质量评估（需要对比已有知识）
```

**System Prompt 中的提取指引（Phase 2 简化版）**：

```
## 记忆管理

你可以使用 memory_store 工具存储值得长期记住的信息。使用原则：
- 用户透露了新的个人信息（住址、职业、家庭成员等）→ 存为 category: fact
- 用户表达了偏好或风格（"我喜欢简洁的回复"）→ 存为 category: preference
- 用户反复纠正你的行为模式（"别用表格了"）→ 存为 category: procedure

不要存储：临时性信息、已存储的重复知识、显而易见的常识。
confidence 由你判断：用户明确表达的 → high，推测的 → medium，不确定的 → low。
```

**即时提取流程（Phase 2 更新）**：

```
用户消息到达
   │
   ▼
LLM 生成回复（含 tool call 判断）
   │
   ├─ LLM 判断"无值得记住的信息"
   │   → 仅生成自然语言回复
   │   → 对话内容仍写入经历层（episode）
   │
   └─ LLM 判断"有值得记住的信息"
       → 生成自然语言回复
       → 同时调用 memory_store({content, category, confidence?, keywords?})
       → Runtime 执行工具调用：
           ├─ confidence >= 0.85（high）→ 直接创建正式 KnowledgeNode
           ├─ confidence < 0.85 → 写入 PendingKnowledgeNode（待离线确认）
           ├─ 冲突候选检测（三层信号，§6.4）
           └─ 标记相关 episode

离线巩固时（Phase 3）：
  → LLM 批量处理 PendingKnowledgeNode
  → confidence >= 0.85 → 升级为正式 KnowledgeNode
  → confidence 在 [0.5, 0.85) → 保持 Pending，等待更多证据
  → confidence < 0.5 → 标记为 Dormant（可能是噪声）
  → 构建图边、执行冲突分类
```

**关键行为保证：**

- **不强制提取**：LLM 有权不在每轮调用 memory_store。简单问候、天气查询等不存储，这比预过滤规则更智能
- **Fact 自动去重**：Runtime 在执行 memory_store 时检查 (subject, predicate) 是否已存在，避免重复
- **对话始终记录**：每轮对话内容写入 JSONL 文件（瞬态层），经历层仅通过 Compaction 摘要 / Session 关闭蒸馏写入（ADR-011）
- **工具调用可见**：memory_store 的调用记录在对话历史中，用户知道 Agent 记住了什么
- **防重复提取机制**（v3.7 新增）：离线巩固前先查询已有 KnowledgeNode，embedding 相似度 > 0.95 的跳过（避免重复提取相同事实）。这一阈值比冲突检测的 0.85 更严格——0.95 意味着几乎完全相同的语义内容

### 4.2 离线巩固（Phase 3）

> **实现状态**：离线巩固的完整实现（空闲检测 + 批量回放 + 关联发现 + 冲突分类）标记为 **Phase 3**。Phase 2 仅实现了即时提取（Tool Call）+ 按需遗忘计算。离线巩固的触发机制和专用 LLM 调用将在 Phase 3 实现。

即时提取（Tool Call）覆盖了"显式信息的即时记忆"，但有两类信息它处理不了：

1. **隐式关联**：用户三次提到上海，但每次都没说"我住在上海"——Tool Call 不会触发，但离线回放可以发现"用户可能常住上海"
2. **跨片段模式**：多个 Skill 都因"输出太长"被纠正——单个 Tool Call 只记录 ProceduralNode，但离线巩固能发现跨 Skill 的通用模式
3. **主动假设验证**（Phase 3 补充）：LLM 在回放时主动提出"如果…会怎样"类型的假设，生成 HypothesisNode 暂存供后续验证——这是困境三（记忆泛化与抽象）在 Phase 3 的具体补全

离线巩固用**专用 LLM 调用**（非 Tool Call），因为它的输入是批量情景记忆而非实时对话，需要独立的 prompt 和推理空间。

**离线巩固升级条件（v3.7 明确化）**：

离线巩固对 PendingKnowledgeNode 的处理有明确的升级/降级规则：

```
PendingKnowledgeNode 离线处理：
  LLM 重新评估 PendingKnowledgeNode，根据完整上下文重新打分：
    confidence >= 0.85 → 升级为正式 KnowledgeNode（status = Active）
    confidence 在 [0.5, 0.85) → 保持 Pending，等待更多证据
      → 同一事实在后续离线巩固中被再次提及 → 累加证据，confidence 递增
    confidence < 0.5 → 标记为 Dormant（可能是噪声，如 LLM 误判的临时信息）

防重复提取（§4.1）：
  离线巩固前先查询已有 KnowledgeNode，embedding 相似度 > 0.95 的跳过
  → 避免重复提取相同事实，浪费 LLM 调用
```

**离线阶段 Prompt 职责（v3.7 明确化）**：

```
离线阶段 Prompt（约 500 tokens）：
  ─ 关联发现：同一主体在多个 episode 中出现但未被显式存储
  ─ 冲突判定：新提取的知识与已有 KnowledgeNode 是否矛盾（三层信号，§6.4）
  ─ 模式提炼：多个 Skill 的 failure_cases 是否指向同一根本原因
  ─ 质量评估：重新评估 PendingKnowledgeNode 的 confidence
  ─ Artifact 摘要增强：模板摘要 → LLM 语义摘要
  ─ 主动假设：提出"如果…会怎样"类型的假设

  与即时阶段的区别：
  ✓ 有完整的多轮上下文（而非单轮）
  ✓ 有时间做深度推理（而非实时响应的延迟约束）
  ✓ 可以对比已有知识（而非仅做单点判断）
  ✓ 可以跨 episode 发现隐式关联
```

```
Agent 空闲（无对话超过 N 分钟）
   │
   ▼
① 扫描未巩固的情景记忆（consolidated = false, importance >= 0.3）
   │
   ▼
② 批量回放：将情景记忆按时间分组，LLM 提取跨片段的关联知识
   │
   ▼
③ 知识合并：检查是否与已有 KnowledgeNode 冲突
   - 新旧一致 → 更新 confidence
   - 新旧冲突 → LLM 判断哪个更准确，或标记为待确认
   - 全新知识 → 创建新节点
   │
   ▼
④ 程序记忆提炼：从多个 SkillExecution 的失败模式中提炼通用 ProceduralNode
   │
   ▼
⑤ 自我评估更新：根据近期执行统计更新 AutobiographicalNode
   │
   ▼
⑥ 标记已巩固 + 清理过老的已巩固情景
```

**离线巩固的触发条件（OR 关系，任一满足即触发）：**

- Agent 空闲超过 30 分钟（可配置）
- 未巩固情景积攒超过 50 条
- 用户手动触发

三者取 OR 而非 AND——避免"既要空闲又要积攒"导致长期不触发的情况。

**离线巩固与即时提取的区别：**

| 维度 | 即时提取（Tool Call） | 离线巩固（专用调用） |
|------|----------------------|---------------------|
| 触发 | LLM 在回复时自主调用 | Agent 空闲时系统触发 |
| 粒度 | 单轮对话中的显式信息 | 多轮对话的隐式关联 |
| 能力 | 事实识别 + 类型标注 + 关键词提取 + 置信度评估 | 关联发现 + 冲突判定 + 模式提炼 + 质量评估 |
| 产出 | PendingKnowledgeNode（confidence=0.7）/ 正式 KnowledgeNode（confidence≥0.85） | 升级/降级 PendingKnowledgeNode + 新建正式 KnowledgeNode |
| 成本 | 零额外 API 调用（每轮 ~150 token 工具定义开销） | 批量 LLM 调用（空闲时进行） |
| 可靠性 | 中（依赖 LLM 自主判断，可能遗漏） | 高（专用 prompt，深度推理） |
| Prompt 规模 | ~100 tokens | ~500 tokens |

## 5. 遗忘机制

遗忘不是记忆的失败，是记忆的优化。没有遗忘的记忆系统会退化——检索效率下降、无关信息干扰决策、存储资源无限增长。

### 5.1 衰减公式

沉淀层每个节点（KnowledgeNode / ProceduralNode）的衰减分数由两个维度决定：

- **importance（固有价值）**：写入时 LLM 打分（0.0-1.0），静态不变，代表这条知识的内在重要性。可作为 Grafeo PageRank 的补充——PageRank 基于"被多少边引用"自动评估重要性，与手调 importance 正交互补：手调 importance 反映语义重要性（"用户姓名很重要"），PageRank 反映结构重要性（"被大量边引用的枢纽节点"）
- **activity_signal（当前活跃度）**：综合近期访问和历史使用频率的动态信号，随时间衰减

公式采用**乘法模型**——importance 作为天花板，activity_signal 决定了"当前保留了多少"：

```
decay_score = importance × activity_signal

activity_signal = clamp(recency_boost + access_boost, FLOOR, 1.0)

其中：
- recency_boost = exp(-λ × days_since_last_access)
  λ = 0.03（半衰期 ≈ 23 天：23 天后衰减到 0.5，46 天后 0.25）
- access_boost = min(BOOST_CAP, access_count × ACCESS_PER_HIT)
  ACCESS_PER_HIT = 0.1（每次检索命中加 0.1）
  BOOST_CAP = 0.5（历史访问最多贡献 0.5，避免"翻旧账"的访问次数补偿一切）
  ⚠️ 权衡：0.5 的上限意味着被访问 5 次以上的节点，即使长期不用，activity_signal 底线仍有 0.55（假设 recency 完全衰减到 0.05），结合 importance >= 0.5 的节点永远不会进入 Purge 候选。这是有意为之——历史高频知识（如核心身份事实）应该有强抗遗忘能力。代价是低 importance 但被频繁访问的偏好（如临时项目相关）可能"卡"在 Active 状态更久。如果实际运行中发现 Dormant 转化率过低，可调低 BOOST_CAP 至 0.3。
- FLOOR = 0.05（即使完全不用，也保留 5% 的 activity）
```

**直觉解释：**

| 场景 | importance | recency | access | activity | decay_score | 状态 |
|------|-----------|---------|--------|----------|-------------|------|
| 核心事实，昨天刚用 | 0.9 | 0.97 | 0.5 | 1.0 | 0.9 | Active |
| 核心事实，60天未用 | 0.9 | 0.17 | 0.5 | 0.67 | 0.6 | Active |
| 中等偏好，90天未用 | 0.6 | 0.07 | 0.1 | 0.17 | 0.1 | Dormant 边界 |
| 低价值碎片，60天未用 | 0.2 | 0.17 | 0 | 0.22 | 0.04 | Purge Candidate |
| 用户姓名，从不查询 | 1.0 | 0.05 | 0 | 0.05 | 0.05 | Dormant 但不 purge |

**为什么用乘法而不是加法？**

加法公式（旧版 `importance × 0.5 + recency × 0.2 + access × 0.3`）的问题：
1. importance 是静态语义属性，recency 是动态时序信号，两者本质不同，放同一层级加权没有认知意义
2. 加法下高 importance 节点的最低 decay_score = importance × α = 0.5（假设 α=0.5），意味着重要知识永远不会真正"沉睡"——这不符合人类认知（即使用户的名字，长期不用也会"一时想不起来"）
3. 加法下低 importance 节点的最低值也是 importance × α，低价值知识的衰减幅度不够

乘法模型解决了这些问题：importance 决定了"这条知识值多少"，activity 决定了"当前还记得多少"，两者正交且直觉清晰。高 importance 知识的绝对衰减幅度更大（0.9 → 0.045），但因为 Fact/Relation 类型的 purge 保护（§5.2），它只是 Dormant 不会丢失。

**λ 的选择（0.03）：**

λ = 0.05 时半衰期约 14 天，对于 Agent 记忆来说太激进——用户两周前提到的事不应该就"沉睡"了。λ = 0.03 给出约 23 天半衰期，约 2 个月降到 0.17——这意味着一个中等重要的偏好如果不被引用，大约 2-3 个月进入 Dormant，符合"过时偏好应该沉睡"的预期。可通过配置调整。

### 5.2 遗忘策略

遗忘策略分两步：**先统一计算 decay_score，再按节点类型决定动作**。

**所有节点类型的 Active → Dormant 阈值统一为 0.3。** 区别仅在于 Dormant 之后是否有 Purge 路径。

> **Phase 2 实现说明**：遗忘机制采用**按需计算模型**而非后台定期扫描。每个 Agent 拥有私有 Grafeo，后台 decay 扫描会重复扫描所有 Agent 的全部节点，在 Agent 数量增长时资源开销过大。按需计算模型在查询时（`hybrid_search`）实时计算 decay_score 并过滤，语义等价但资源效率更高。后台扫描作为 Phase 3 可选优化项。

```
后台定期扫描（每小时一次，可配置）
   │
   ▼
第一步：计算每个 Active 节点的 decay_score（§5.1 公式）
   │
   ▼
第二步：按节点类型 + decay_score 决定状态转换
   │
   ├─ KnowledgeNode（Fact / Relation）
   │   ├─ decay_score >= 0.3 → Active
   │   └─ decay_score < 0.3 → Dormant
   │       ⚠️ 永不进入 Purge。事实性知识只沉睡不删除。
   │       （即使用户搬家了，"曾经住北京"也是历史事实，不应被系统自动删除）
   │
   ├─ KnowledgeNode（Preference）
   │   ├─ decay_score >= 0.3 → Active
   │   └─ decay_score < 0.3 → Dormant
   │       → Dormant 持续超过 90 天 → 进入 Purge 流程
   │       （偏好可能已过时，90 天的沉睡期足够长）
   │
   ├─ ProceduralNode
   │   ├─ decay_score >= 0.3 → Active
   │   └─ decay_score < 0.3 → Dormant
   │       → Dormant 持续超过 90 天 → 进入 Purge 流程
   │       （行为模式可能不再适用）
   │
   └─ AutobiographicalNode
       └─ 不参与衰减，始终 Active（schema 强制约束）
```

**状态转换规则总结（Phase 2 更新）**：

| 节点类型 | Active → Dormant 阈值 | Dormant → Purge 条件 | 永不 Purge |
|---------|----------------------|---------------------|-----------|
| Fact / Relation | 0.3 | —（无 Purge 路径） | 是 |
| Preference | 0.3 | 路径1: Dormant > 90天 AND importance < 0.5；路径2: 容量压力 | importance ≥ 0.5 时仅容量压力可触发 |
| ProceduralNode | 0.3 | 同 Preference | 同 Preference |
| AutobiographicalNode | —（不参与衰减） | — | 是 |

**Purge 三条路径**：

路径 1 — 正常衰减（后台 decay_scan）：
- 条件：Dormant > dormant_purge_days AND importance < purge_importance_threshold
- 默认：90天 + importance < 0.5
- 含义：低价值 + 长期沉睡 = 清理候选

路径 2 — 容量压力（存储接近上限时）：
- 触发：Agent 存储用量 > max_storage_mb 的 90%
- 执行：先 purge 满足路径1的节点，仍不够则按 decay_score 升序清理

路径 3 — 用户手动（Desktop App Memory 管理面板）

详见 docs/review/04-p2-s2-design-review.md §6.15

**Fact 语义去重（发生在离线巩固阶段）：**

Fact/Relation 永不 Purge 可能导致存储膨胀（如用户每天说"今天天气不错"生成大量低价值重复节点）。即时提取和离线巩固阶段对 Fact 节点执行语义去重：

- 写入前检查：新 Fact 的 `(subject, predicate)` 是否与已有 Active 节点相同
- 相同且 object 一致 → 更新 `confidence`（取最新值）和 `last_accessed`，不创建新节点
- 相同但 object 不同 → 视为知识更新（如"用户住北京"→"用户住上海"），创建新节点并将旧节点标记为 Dormant（历史事实仍保留）
- 用户可手动触发"归档非核心 Fact"：将 2 年以上未访问的非身份类 Fact 标记为 Dormant

**Dormant 节点的处理：**

- 不参与常规检索（`hybrid_search` 默认过滤 `status != Dormant`）
- 如果被其他机制重新引用（如用户再次提及、关联扩散路径经过），自动恢复为 Active，更新 `last_accessed`、`access_count += 1`（恢复引用算一次访问）并清除 **dormant_since**（90 天计时器归零）
- 注意：恢复时不清零 `access_count` 的历史累积值，只增量 +1——历史使用频率是乘法模型的一部分，重置会抹杀抗遗忘能力
- Dormant 节点仍然占用存储，但不在检索路径上，不影响检索效率

**Purge 流程（真正删除）：**

进入 Purge 流程不等于立即删除。Purge 前执行以下检查：

```
节点进入 Purge 流程
   │
   ▼
① 检查是否有 Active 关联节点
   - 有 → 不做自动知识合并（避免 LLM 判断的不确定性）
     仅将该节点的 source_episode 引用转移到关联节点
     （确保关联节点仍然知道这条知识的来源）
   - 无 → 直接删除
   │
   ▼
② 同时删除相关的图边（Grafeo LPG Edge 自动级联删除）
   │
   ▼
③ 记录 purge_log（节点 ID、类型、内容摘要、purge 原因、关联节点 ID）
   - purge_log 保留 30 天，用于调试和"找回被遗忘的记忆"
   - purge_log 支持手动回滚：用户可从 purge_log 恢复任意已删除节点
   - ⚠️ 未来可迁移至 Grafeo CDC history() 作为更可靠的审计机制
```

**用户手动操作：**

- 用户可随时手动 purge 指定节点或所有 Dormant 节点（Desktop App → Memory 管理面板）
- 用户可手动恢复任意 Dormant 节点为 Active（等同于"我想起来了"）

**经验回溯 / 变更历史（基于 Grafeo CDC）：**

Grafeo 内置 CDC（Change Data Capture）记录每个节点的完整变更历史。通过 `db.history(EntityId::Node(id))` 可以追溯任何记忆节点的创建、修改、删除全过程。

使用场景：
- 经验回溯：每次 Decay 修改节点属性后，可通过 `history()` 查看原始状态
- 冲突调解：对比同一节点在不同时间点的版本，辅助 LLM 判断合并策略
- 审计追踪：追踪记忆从 Episodic → Knowledge 的完整演化链路
- Purge 恢复：被 purge 的节点可通过 CDC 历史找回（替代自研 purge_log）

```rust
// Retrieve full change history of a memory node
let history = db.history(EntityId::Node(node_id))?;

// Restore a node to a previous state after decay
let snapshot = session.execute_at_epoch(
    "MATCH (n) WHERE id(n) = $id RETURN n",
    epoch,
)?;
```

详见 docs/module-design/04-grafeo.md §CDC / History

### 5.3 不参与遗忘的节点

| 节点类型 | 是否遗忘 | 原因 |
|---------|---------|------|
| AutobiographicalNode | 否 | 核心身份，遗忘 = 人格断裂 |
| KnowledgeNode（identity 类） | 否 | 用户姓名、语言等基础身份 |
| SkillExperience | 专用衰减 | 按 Skill 系统规则管理 |
| SkillDraft / Iteration / Execution | 开发期保留 | 调试完成后归档 |

## 6. 关联扩散检索

传统检索是"查到什么就是什么"，关联扩散是"查到一个，带出一串"——模拟海马体的模式完成和激活扩散。关联扩散基于 Grafeo 原生 GQL 图遍历实现（`MATCH (m)-[r*1..3]-(other) WHERE ...`），无需 SQL 模拟图查询，有查询优化器支持谓词下推和早期终止。

### 6.1 检索流程（Phase 2 更新）

检索同时查询经历层和沉淀层，并支持跨层关联扩散。MemoryManager 根据 Grafeo 的就绪状态和检索耗时决定降级：

```
Level 0（正常模式）
  前提：Grafeo 完全就绪
  策略：hybrid_search（Grafeo 原生 RRF + topology_boost）+ graph_expand（GQL 原生图遍历）
  SLA：P99 < 200ms

  ↓ 向量索引未就绪或 embedding 生成超时

Level 1（无向量模式）
  策略：text_search only（Grafeo 原生 BM25）+ graph_expand
  SLA：P99 < 100ms

  ↓ Grafeo 查询超时（>300ms）或索引异常

Level 2（缓存模式）
  策略：返回 Autobiographical 文本缓存 + 最近 5 条 Episode
  SLA：P99 < 10ms

  ↓ Grafeo 完全不可用

Level 3（内存模式）
  策略：仅返回当前会话工作记忆，无持久化检索
  SLA：P99 < 1ms
```

**单次检索预算分配（500ms 硬超时）**：

```
① embedding 生成：≤200ms（超时→跳过向量，text_search only）
② hybrid_search：≤150ms（grafeo-engine 原生 RRF 融合 + topology_boost，向量和 BM25 并行）
③ graph_expand：≤100ms（GQL 原生图遍历，早期终止，超时返回已扩展节点）
④ 排序+格式化：≤50ms
任一环节超时，使用已有部分结果继续后续步骤。
```

**检索流程**：

```
用户输入 / Agent 内部查询
   │
   ▼
① 并行检索两层数据
   ├─ 经历层 hybrid_search（Episodic Label）：Grafeo 原生向量 + 全文 + RRF
   │   返回 Top-K 相似情景
   └─ 沉淀层 hybrid_search（Knowledge/Procedural/Autobiographical Label）：Grafeo 原生向量 + 全文 + RRF
       返回 Top-K 匹配知识（Knowledge / Procedural / Autobiographical）
   │
   ▼
② graph_expand：从所有匹配节点出发，沿 GQL 原生图遍历做跨层扩展
   - 经历层 episode → 通过 KnowledgeNode.source_episode 反向查询关联的沉淀层节点
   - 沉淀层 node → 通过 GQL `MATCH (m)-[r*1..3]-(other)` 扩展到其他沉淀层节点
   - 1 跳：直接关联（边权重 > 0.3）
   - 2 跳：间接关联（累积路径权重 > 0.1）
   - 3 跳：复杂推理（累积路径权重 > 0.05），通过早期终止实际大多在 1-2 跳停止
   │
   ▼
③ 去重 + 评分
   - 直接匹配节点分数 = RRF 分数（含 topology_boost 权重加成）
   - 扩展节点分数 = 路径权重 × 源节点 RRF 分数
   - 多路径节点获得 Grafeo PageRank 额外权重加成
   - 同一节点可能同时被经历层和沉淀层命中，取最高分
   │
   ▼
④ 截断返回
   - 总结果数受 Token 预算限制
   - 优先返回直接匹配，扩展节点作为补充
```

### 6.2 示例

```
用户问："上海明天下雨吗？"

① 并行检索：
   经历层 → Episode: "上周用户说下周要去上海出差"（语义相似）
   沉淀层 → KnowledgeNode: "用户住在北京"（"上海"关键词匹配）
            KnowledgeNode: "用户经常去上海出差"（"上海"关键词匹配）

② graph_expand 跨层扩散：
   "上周出差提到" episode → 反向查 source_episode → KnowledgeNode: "出差时关心天气"（经历→沉淀）
   "经常去上海出差" KnowledgeNode ──[PREFERS]→ ProceduralNode: "出差时查天气"（沉淀内）
   "用户住北京" KnowledgeNode ──[PREFERS]→ "简洁的回复风格"（沉淀内）
   "经常去上海出差" KnowledgeNode → 反向扩展 → Episode: "上次上海出差淋了雨"（沉淀→经历）

③ 最终注入上下文：
   - 核心事实：用户住北京、经常去上海出差
   - 跨层扩展：出差时关心天气、上次上海出差淋了雨
   - 关联扩散：偏好简洁回复
   → Agent 回答："上海明天小雨，15-20°C。需要带伞——上次你上海出差淋过雨。"
```

没有关联扩散，Agent 只知道用户"住北京"或"去上海出差"，但不知道这两者之间的关联，也不知道用户出差时关心天气、上次淋过雨。跨层关联扩散让检索从"关键词匹配"升级到"语义推理"。

Grafeo GQL 原生图遍历相比旧版 SQL 模拟的优势：
- 邻接索引 O(degree) 遍历，替代 SQL JOIN 模拟
- CBO/DPccp 查询优化器支持谓词下推、基数估计、早期终止
- `topology_boost` 利用图连通性重排序检索结果，高连通性节点优先召回

### 6.3 性能保障（Phase 2 更新）

- 扩展深度硬限制 **3 跳**（通过早期终止实际大多在 1-2 跳停止）
- early_stop_threshold 随跳数递增（1跳: 0.1, 2跳: 0.15, 3跳: 0.2），越远越严格
- 每跳最多扩展 5 条边（按权重 Top-5）
- 扩展节点总数上限 20（防止 Token 膨胀）
- 只对 Active 节点做扩展，Dormant 节点不参与
- 经历层和沉淀层并行检索，扩展阶段串行（避免并发复杂度）
- 扩散阈值可配置（默认 0.2），在首次运行时可根据实际效果调整
- 多路径节点获得 Grafeo PageRank 额外权重加成

详见 docs/review/04-p2-s2-design-review.md §6.2、§6.3

**Grafeo 图算法增强（基于 grafeo-engine 原生 API）：**

Grafeo 内置图算法过程（`algos` feature），可直接调用以提升记忆质量：

- **PageRank 重要性评估**：`CALL grafeo.pagerank()` 自动评估记忆节点的图连通性重要性——被更多边引用的节点 PageRank 更高，作为手调 `importance` 的补充或替代。使用场景：检索排序（topology_boost 权重输入）、遗忘保护（PageRank 高于阈值的节点跳过衰减扫描）、重要性校准
- **MMR 多样性搜索**：`db.mmr_search()` — Maximal Marginal Relevance，在检索结果中保证语义多样性，避免返回大量重复语义的节点
- **社区检测**（Louvain）：`CALL grafeo.louvain()` 自动发现记忆间的隐性群组，增强 graph_expand 的语义质量——社区内节点优先扩展，社区间延迟扩展

详见 docs/module-design/04-grafeo.md §图算法增强

### 6.4 冲突处理（Phase 2 新增，v3.8 简化为两层信号）

冲突处理采用**两阶段方案**：即时阶段做粗筛（两层信号候选冲突标记），离线阶段做精确分类与解决。

> **v3.8 简化**：移除了 Layer 3（上下文否定关键词）和启发式快速路径（Evolution/Correction 自动判定）。理由见 docs/module-design/04-grafeo.md §冲突检测。

**两层冲突信号模型**：

冲突检测不再仅依赖语义相似度，而是综合两层信号提高冲突识别的准确性：

| 层级 | 信号 | 检测机制 | 说明 |
|------|------|---------|------|
| **Layer 1** | 语义相似度 | embedding cosine similarity > threshold | 已有机制，保留并细化 |
| **Layer 2** | 时间冲突 | 同一主体 24h 内矛盾陈述 | 24h 内置信度 0.7，24h 外 0.5 |

**Layer 1 — 语义相似度（已有，保留并细化）**：

embedding cosine similarity 超过动态阈值即标记为候选冲突。阈值按知识类型区分：

| 知识类型 | 冲突检测阈值 | 理由 |
|---------|------------|------|
| Fact | 0.85 | 事实性知识需较严格的语义匹配 |
| Preference | 0.80 | 偏好更容易变化，稍低阈值可捕获更多潜在冲突（如“喜欢简洁”vs“喜欢详细”在不同上下文中） |
| Relation | 0.90 | 关系需更严格匹配，避免误报（如“经理是王五”vs“同事是王五”） |

**Layer 2 — 时间冲突（v3.7 新增）**：

同一主体（subject 字段匹配）在短时间窗口内出现矛盾陈述，优先触发冲突检测：

```
时间冲突检测：
  条件：新节点与已有节点的 subject 字段匹配
        AND 两者 object 不同
        AND 时间差 < time_window（默认 24h）
  动作：标记为高优先级冲突候选

  利用 Grafeo CDC history API 获取节点创建时间：
    let history = db.history(EntityId::Node(node_id))?;
    let created_at = history.first().timestamp;

  时间窗口可配置（默认 24h）：
    → 24h 内矛盾陈述更可能是纠正（correction）
    → 超过 24h 更可能是演进（evolution）
```

**即时阶段（每轮 memory_store 时，两层信号联合检测）**：

```
PendingKnowledgeNode 写入时：
  1. Layer 1：计算新节点 embedding 与已有 Active 节点的余弦相似度
     → 相似度超过动态阈值（Fact: 0.85 / Preference: 0.80 / Relation: 0.90）
       → 标记为候选冲突（conflict_candidate = true）
  2. Layer 2：检查新节点与已有节点的创建时间差
     → 时间差 < 24h → 置信度 0.7
     → 时间差 ≥ 24h → 置信度 0.5
  3. 所有冲突统一标记 ConflictType::Ambiguous，共享 conflict_group_id
  4. 新节点立即可被检索，但暂不参与 graph_expand
```

**离线阶段（巩固管道，Phase 3）**：

```
离线巩固时，LLM 批量处理候选冲突：

输入：冲突的两条记忆 + 各自的 source_episode 上下文 + 两层信号标注
输出：
  - type: "evolution" | "correction" | "ambiguous"
  - action: "replace" | "keep_both" | "ask_user"
  - reasoning: 一句话解释判断理由
```

| 类型 | LLM 判断依据 | 处理 |
|------|------------|------|
| **演进**（Evolution） | 理解上下文语义（如"我搬家了"） | 新值 Active，旧值 Dormant，写 conflict_log |
| **纠正**（Correction） | 理解否定语义（如"不是 X，是 Y"） | 新值 Active，旧值 Dormant，降低旧来源可信度 |
| **不确定**（Ambiguous） | 无法从上下文确定 | 两个都 Active，标记 conflict_group_id，等待用户确认 |

**ambiguous 用户确认流程（v3.7 细化）**：

- ambiguous 累计 3+ 个时，通过 `memory_store` 工具的 `hint` 字段引导 Agent 在对话中自然询问用户确认
- 不通过弹窗打扰，在对话中自然地询问
- 引导方式：在 Agent 的 System Prompt 中注入待确认的记忆摘要，Agent 自行决定何时自然地询问

**设计理由**：冲突分类是语义判断，遵循 LLM 优先原则。即时阶段仅做语义 + 时间的粗筛（两层信号），统一标记 Ambiguous。硬编码关键词匹配（否定词、变化词）已被证明覆盖面和可靠性不足，Phase 3 LLM 有完整上下文，判定质量远超启发式规则。

详见 docs/review/04-p2-s2-design-review.md §6.9

### 6.5 Abstention（拒答）机制（v3.7 新增）

**设计动机**：当检索结果的置信度不足时，Agent 应选择拒答而非生成可能不准确的回答。这是检索系统的最终质量门控——比检索降级策略（Level 0-3）更后置，是在检索结果已返回后的语义级别保障。

**min_score 阈值机制**：

`hybrid_search` 返回的结果中，所有分数低于 `min_score` 的结果被过滤。若过滤后结果为空，触发 Abstention（拒答）：

```
检索结果处理流程：
  ① hybrid_search 返回原始结果集
  ② 过滤：移除 score < min_score 的结果
  ③ 若过滤后结果为空：
     → 触发 Abstention
     → System Prompt 注入拒答指引：
       "当检索分数不足时回复'我不确定这个信息'，不要猜测"
     → Agent 基于自身通用知识回复，但明确标注不确定性
  ④ 若过滤后结果非空：
     → 正常注入检索结果到上下文
```

**默认阈值**：

| 阶段 | min_score 默认值 | 说明 |
|------|-----------------|------|
| Phase 2 | 0.6 | 保守值，精度优先。预期会过滤掉较多低质量结果 |
| Phase 3 | 基于实际数据校准 | 根据在线评估的 NRR 指标和 LongMemEval 成绩动态调整 |

**与检索降级策略的关系**：

Abstention 在 Level 0-3 降级策略之后生效，是最终的质量门控：

```
检索降级策略（§6.1）→ 返回原始结果集
  ↓
min_score 过滤 → 移除低质量结果
  ↓
Abstention 判断 → 结果为空则触发拒答
  ↓
Agent 回复（有依据 / 明确拒答）
```

- Level 0-3 解决的是"Grafeo 可用性"问题（硬件/软件故障）
- Abstention 解决的是"检索质量"问题（返回结果不可靠）
- 两者正交，互不替代

**LongMemEval Abs 维度目标**：

LongMemEval 的 Abs（Abstention）维度评估 Agent 在信息不足时是否选择拒答而非幻觉：

| 阶段 | Abs 目标 | 当前预期 | 说明 |
|------|---------|---------|------|
| Phase 2 | 60%+ | 40-50% | 通过 min_score 阈值 + System Prompt 注入实现 |
| Phase 3 | 75%+ | — | 结合在线评估反馈和轻量 LLM Judge 优化 |

**可配置性**：

`min_score` 通过 `MemoryQuery` 参数传入（§10.3 `MemoryQuery.min_score` 字段已预留），支持不同 Agent 不同阈值：

```rust
pub struct MemoryQuery {
    pub query_text: String,
    pub filters: MemoryFilters,
    pub limit: usize,
    pub expand_hops: u8,
    pub min_score: Option<f32>,   // Abstention threshold, None = use default (0.6)
}
```

- 工具型 Agent（如天气助手）：min_score = 0.5（容忍较低匹配，宁可多答）
- 学习型 Agent（如知识库助手）：min_score = 0.7（严格匹配，宁缺毋滥）
- 默认值从 manifest.toml `[memory.retrieval]` 节读取

### 6.6 检索权重（v3.10 简化）

**原设计（v3.7）**：通过 memory_hint.type 动态调整 RRF 权重和 graph_expand 参数。四种模式（s/f/r/i）各有不同权重配置。

**v3.10 简化**：移除 per-round memory_hint 提取和规则引擎类型检测。所有检索统一使用默认权重（vector: 0.7, text: 0.3, graph: 0.0），graph_expand 使用保守阈值 [0.15, 0.2, 0.25]。

**简化理由**：
- f/r 类型的权重微调收益未被 benchmark 验证，不值得引入规则复杂度
- i（Identity）类型虽然理论上有意义（仅搜 Autobiographical），但需要在检索质量损失（漏掉其他层相关知识）和性能优化之间权衡——当前默认搜全部 4 个 Label 是更安全的选择
- graph_expand 的激进阈值依赖图质量，当前图结构尚未经过充分验证

HintType 枚举和相关权重查找函数（`get_hint_weights`, `config_from_hint`）保留在代码中，供将来需要时使用。

## 7. 跨 Agent 知识共享

不同 Agent 之间不共享数据库，知识共享通过三种机制实现：

**路径 1：Intent 查询（推荐，主路径）**

Agent A 需要某项知识，直接向拥有该知识的 Agent B 发送 Intent 查询：

```json
{
  "type": "intent",
  "target": "com.example.weather",
  "action": "query_user_city",
  "params": {},
  "id": "msg-123"
}
```

天气 Agent 从自己的私有 Grafeo 查到结果并返回。这是最小权限方式——日历 Agent 只拿到了需要的那个事实。

**路径 2：Gateway UserProfile（身份与偏好）**

用户身份和偏好等系统级信息由 Gateway 的 UserProfile 模块统一管理，在 Agent 握手时注入，变更时热推送。详见 [18-user-identity-simplified.md](./18-user-identity-simplified.md)。

**身份查询的容错：**

- **本地缓存**：每个 Agent 缓存最近一次 identity 查询结果，TTL 5 分钟。缓存未过期时不发起查询（减少 Gateway 负载）
- **Gateway 不可用**：如果 Gateway 未响应（超时 2 秒），Agent 降级使用本地缓存（即使过期），或使用 manifest.toml 中的 identity_deps 默认值
- **Gateway 恢复**：Runtime 定期重连 Gateway，重连成功后重新获取完整 UserProfile
- **冷启动预加载**：新 Agent 首次启动时，Gateway 在握手阶段（AgentHello）将 UserProfile 注入启动参数（详见 [18-user-identity-simplified.md](./18-user-identity-simplified.md) §3），不依赖运行时缓存

**路径 3：云端 Memory Sync 同步**

云端作为知识同步层，Agent 写入的知识可按规则广播给订阅了该信息的其他 Agent，各 Agent 的本地 Grafeo 各自更新。

### 7.1 隐私与同步

记忆节点增加 PrivacyLevel 标记（Public / Personal / Sensitive），LLM 在即时提取时自动判断：

- **Public**：可跨 Agent 共享——"用户名叫张三"、"用户说中文"
- **Personal**：Agent 私有——"用户偏好简洁回复"
- **Sensitive**：Agent 私有，打包分享时剥离——"用户提到健康问题"

**PrivacyLevel 的实际作用域是打包边界控制**：当用户将 Agent 分享给他人时，Personal/Sensitive 节点被自动剥离，只保留 Agent 自身的能力（SkillIteration、ProceduralNode、AutobiographicalNode 中关于 Agent 自身的部分）。PrivacyLevel 不用于网络同步过滤或跨 Agent 隔离——LLM 上下文中的数据无技术访问控制手段，靠 prompt 约定约束。

云端同步按节点类型（Episodic / Knowledge / Procedural / Autobiographical）同步。全部数据明文同步，平台托管（与主流互联网平台一致，详见 00-prd.md ADR-002）。

## 8. 语义记忆节点类型汇总

### 8.1 节点类型（NodeType）— 认知功能分类

节点类型通过 **Grafeo LPG Label** 实现，区分记忆的**认知功能分层**：

| Label | 用途 | 遗忘 | 隐私 | 详见 |
|-------|------|------|------|------|
| `Episodic` | 经历层：交互片段、对话快照 | 激进清理（14天） | Personal | 本文档 §2 |
| `Knowledge` | 语义记忆：事实、偏好、关系 | 乘法衰减 + 节点类型规则（§5.2） | Public/Personal/Sensitive | 本文档 §3.1 |
| `Procedural` | 程序记忆：行为模式、操作规则 | 乘法衰减 + 节点类型规则（§5.2） | Personal | 本文档 §3.2 |
| `Autobiographical` | 自传体记忆：自我认知、能力边界 | 不遗忘 | Personal | 本文档 §3.3 |
| `SkillDraft` | 草稿 Skill（调试阶段） | 开发期保留 | Personal | [13-skill-system.md](./13-skill-system.md) §3.2 |
| `SkillIteration` | 迭代版本快照 | 开发期保留 | Personal | [13-skill-system.md](./13-skill-system.md) §3.3 |
| `SkillExecution` | 执行记录（含模型信息） | 开发期保留 | Personal | [13-skill-system.md](./13-skill-system.md) §3.4 |
| `SkillExperience` | 已发布 Skill 的运行经验 | 专用衰减 | Personal | [13-skill-system.md](./13-skill-system.md) §3.5 |

**NodeType 的设计原则：**
- 通过 **Grafeo Label** 实现（而非枚举字段），利用 Label 隔离实现类型区分
- 每种 Label 有独立的 properties schema 和检索索引
- 认知分层与 LPG Label 一一映射（见 §0 分层原则）

### 8.2 Zone 概念 — 业务场景分区（暂缓实现）

**⚠️ Zone 功能暂缓实现，本节仅做概念定义，避免与 NodeType 混淆。**

Zone 用于区分记忆的**业务场景分区**，与 NodeType 正交：
- **NodeType** 回答"这是什么类型的记忆？"（认知功能：经历/语义/程序/自传体）
- **Zone** 回答"这个记忆属于哪个业务场景？"（业务分区：work/personal/system）

**预定义的 Zone（Phase 4+）：**
- `work`：工作相关记忆（项目、任务、同事）
- `personal`：个人生活记忆（兴趣、家庭、日常）
- `system`：系统级记忆（配置、元数据）

**Zone 与 NodeType 的关系：**
```
一个 KnowledgeNode 可以属于：
  - NodeType: Knowledge（认知功能：语义记忆）
  - Zone: work（业务场景：工作相关）
  - 示例："用户的项目经理是王五" → Knowledge + work

一个 Episodic 可以属于：
  - NodeType: Episodic（认知功能：经历层）
  - Zone: personal（业务场景：个人生活）
  - 示例："用户提到周末去爬山" → Episodic + personal
```

**实现方式（Phase 4+）：**
- Zone 将作为 **Grafeo Node Property** 存储（而非独立 Label）
- 在 `KnowledgeNode`、`ProceduralNode` 等结构体中增加 `zone: String` 字段
- 检索时可通过 zone 过滤（如 `filters.zone = Some("work")`）

**⚠️ 当前状态（Phase 1-3）：**
- `MemoryNode.zone` 字段存在于 `rollball-core/src/memory/traits.rs` 中，但**暂未使用**
- `MemoryStore::list_by_zone()` 方法已定义，但 **GrafeoStore 未实现**
- Zone 功能推迟到 Phase 4+，当前所有节点默认属于 `default` zone

**设计理由：**
- Phase 1-3 聚焦认知分层架构（NodeType），业务分区需求尚未明确
- 避免过早引入 zone 导致架构复杂度增加
- Phase 4+ 根据实际使用场景再决定是否启用 zone 功能

## 9. 分阶段实现路线

### Phase 1：记忆基础（对应 Roadmap Phase 2）

**目标：** 让 Agent 能记住用户，能检索，能遗忘，但不做深度抽象和跨时段的巩固。

**交付内容：**

三层架构落地（瞬态层 / 经历层 / 沉淀层），Grafeo 支持 Episodic + Knowledge + Procedural + Autobiographical 四个 LPG Label 及对应 Edge Type，存储为 `.grafeo` 单文件格式。经历层存储对话原始记录（episode），沉淀层存储精炼知识（KnowledgeNode / AutobiographicalNode）。

即时提取通过 Tool Call 机制实现：`memory_store` 工具加入 Agent 内置工具列表，System Prompt 加入提取指引，LLM 在生成回复时自主判断是否调用。即时阶段仅做 embedding 相似度粗筛（相似度 > 0.85 → 标记候选冲突），不做三元组提取。三元组提取和精确去重发生在**离线巩固阶段**（Phase 3），详见 §4.1 和 §6.4。

基础遗忘机制落地：乘法衰减模型 decay_score = importance × activity_signal，activity_signal = clamp(recency_boost + access_boost, 0.05, 1.0)。Dormant 态区分：Fact/Relation 永不清除，Preference/ProceduralNode Dormant 超过 90 天可 Purge。dormant_since 字段计时，reactivate_node 时归零。

关联扩散检索落地：hybrid_search 基础上加 graph_expand，经历层 episode 通过 KnowledgeNode.source_episode 反向查询建立跨层关联，边权重 = min(0.8, confidence_avg × recency_factor)，扩散阈值 0.2（可配置），硬限制 **3 跳**（通过早期终止实际大多在 1-2 跳停止）。

AutobiographicalNode 从 manifest.toml 自动派生（Identity / Capability），History 节点超过 10 条时摘要压缩，注入上限 200 token。PrivacyLevel（Public / Personal / Sensitive）用于打包分享时的节点过滤。

Episode 内容分类压缩落地：信息性内容原样存储，工件性内容（代码/文件/命令输出）压缩为摘要 + ArtifactRef 引用。代码不住在 Grafeo 里——Grafeo 存"关于代码的描述"，需要实际代码时通过 artifact_refs 的 path + hash 在文件系统/版本控制中查找。

**Phase 1 不做的事：** 离线巩固、ProceduralNode 联动、分页换出、云端同步。这些是 Phase 2/3 的事。

---

### Phase 2：程序记忆与自我认知（对应 Roadmap Phase 3）

**目标：** 让 Agent 从自身经验中学习行为模式，并能认知自己的能力边界。

**核心组件一：ProceduralNode 完整生命周期**

ProceduralNode 的来源有三条路径：

路径 A — 用户反馈即时提取（Phase 1 已有，Tool Call）：用户明确纠正（"太长了" / "不要用表格"）→ ProceduralNode.trigger_condition = "用户要求简洁输出"，action_pattern = "优先给结论，控制长度"。

路径 B — 执行失败自动总结：SkillExecution 的 failure_case 触发。当 Skill 返回执行失败时，Runtime 自动将 failure_case 摘要写入 ProceduralNode，learned_from = "执行失败"，source_skill 指向对应 Skill。

路径 C — 离线巩固提炼（Phase 3 的离线巩固负责，Phase 2 先做单 Skill 内模式识别）。

ProceduralNode 的激活时机：每次 Agent 生成回复前，检索 relevant ProceduralNode（按 trigger_condition 匹配当前上下文），activation_count += 1，更新 last_accessed。被激活的 ProceduralNode 注入 System Prompt 行为准则区，格式："当 [trigger_condition] 时，优先 [action_pattern]"。

**核心组件二：Skill ↔ ProceduralNode 双向联动**

双向联动设计遵循"处方式知识"框架，但做了一定简化：

Skill → ProceduralNode：当 SkillExperience 的 failure_cases 积累超过 3 条同类失败（如都是"输出太长"），触发联动提取。LLM 阅读 failure_cases 摘要，生成跨 Skill 的通用 ProceduralNode。例如：weekly-report Skill 输出太长被纠正 5 次 + code-review Skill 输出太长被纠正 3 次 → 提炼出通用 ProceduralNode："此用户对输出长度敏感，所有 Skill 应先给结论再展开细节"。

ProceduralNode → Skill：ProceduralNode 的 activation_count 低于阈值（如 < 3）时，向对应 Skill 发送降级建议。例如某个 ProceduralNode 的 action_pattern 是"用表格"，但 activation_count 持续很低，Agent 应该反思：这个行为模式是否还适用？

**⚠️ 困境三的补设计 — 主动假设验证机制**

Phase 2 需要补上"困境三"（记忆泛化与抽象）的设计缺口。ProceduralNode 的提炼不能只是被动的"积累够了就合并"——需要有主动的假设验证：

当同一 trigger_condition 下有 >= 3 个 action_pattern 变体时（如"用户要求简洁"的表达方式有"太长了"、"少说废话"、"简短点"三种），Agent 应主动提出假设："这三种表达是否指向同一个偏好？" 并在后续交互中验证。验证通过后合并为单一 ProceduralNode，验证失败则保留多个变体。

具体实现：在 grafeo crate 新增 `procedural_abstract` 模块，提供 `detect_merge_candidates()` 方法，扫描同类 trigger_condition 下的多个 action_pattern，由 LLM 判断是否应合并。

**核心组件三：自我评估驱动的 AutobiographicalNode 更新**

自我评估的目标是让 Agent 认知自身的能力边界，主要体现在 Limitation 节点的自动更新。

触发时机：每次 SkillExecution 完成后，根据 success/failure 和模型信息，更新 SkillExperience.model_compatibility。当某模型上某类任务成功率低于 60% 时，生成或更新 AutobiographicalNode Limitation 节点："在 [模型] 上，[任务类型] 成功率约 [X]%，建议切换模型或简化任务"。

注入时机：Limitation 节点在每次对话的 System Prompt 注入时必须包含（与 Identity / Capability 同级），确保 Agent 始终知道自己的边界。

同时新增 Relationship 节点的自动维护：当用户与 Agent 合作超过一定时长（如 30 天），自动生成 Relationship 节点记录合作模式。

**Phase 2 验收标准：**

- ProceduralNode 有三条明确的来源路径，且 Skill ↔ ProceduralNode 联动可工作
- 同一 trigger_condition 出现 >= 3 个变体时，Agent 能提出合并假设（而非仅被动合并）
- AutobiographicalNode Limitation 节点能根据执行统计自动生成/更新
- 所有 Phase 1 功能在 Phase 2 改动后仍然正常运行

---

### Phase 3：离线巩固与持久化（对应 Roadmap Phase 6）

**目标：** 让 Agent 在空闲时主动整合经验、发现隐式关联，并具备跨设备持久化能力。

**核心组件一：离线巩固（"睡眠"模式）**

离线巩固是 Phase 1 即时提取的补充——即时提取处理"显式信息的即时存储"，离线巩固处理"隐式关联的发现与整合"。

触发条件（OR 关系，任一满足即触发）：Agent 空闲超过 30 分钟（可配置）、未巩固 episode 积攒超过 50 条、用户手动触发。多 Agent 场景下增加全局协调限制：同一时刻只有一个 Agent 执行离线巩固，避免 CPU/内存竞争。

离线巩固的 LLM prompt 设计（关键）：

```
你正在执行记忆巩固。输入是一组未巩固的情景记忆（episode），按时间排列。

你的任务：
1. 发现隐式关联：同一主体在多个 episode 中出现但未被显式存储（如用户多次提到"上海"但从未说"我住在上海"）
2. 检测知识冲突：新提取的知识是否与已有 KnowledgeNode 矛盾
3. 提炼跨 Skill 模式：多个 Skill 的 failure_cases 是否指向同一个根本原因
4. 评估现有 ProceduralNode：哪些已经被反复验证（activation_count 高）？哪些长期未激活（可能过时）？
5. 增强 Artifact 摘要：Phase 1 的模板摘要只是"读取了什么文件、多少行"，离线巩固时可以用 LLM 为带 artifact_refs 的 episode 生成语义摘要（如"这个文件实现了数据处理的管道模式"），替换原有的模板字符串

输出格式：
- 发现的新 KnowledgeNode（带 importance 和 privacy 评估）
- 需要合并或标记冲突的已有节点
- 需要降级或激活的 ProceduralNode
- 需要更新的 AutobiographicalNode（如 Limitation 节点）
- 需要增强摘要的 Episode（原 content 为模板字符串，替换为 LLM 生成的语义摘要）
```

知识冲突的处理策略：冲突时保留新旧两个节点，标记 confidence 更高的为 authoritative，另一条降级为 alternate。长期矛盾的节点（超过 3 次冲突记录）标记为"待用户确认"。

**⚠️ 离线巩固与困境三的关联**

困境三的核心是"缺乏主动假设验证机制"。Phase 3 的离线巩固 prompt 中加入主动假设步骤：LLM 在回放 episode 时，应主动提出"如果…会怎样"类型的假设。例如："用户三次提到加班但未提薪酬，可能反映工作满意度问题"——这类假设不写入 KnowledgeNode，而是生成一个 HypothesisNode（暂存，待后续验证）。 HypothesisNode 不参与常规检索，仅在后续离线巩固时被翻出来验证是否得到更多证据支持。

**Episode 摘要增强：从"读了什么"到"做了什么"**

Phase 1 的内容分类压缩用模板字符串和正则分离生成了确定性摘要（如"读取 src/main.rs，共 200 行，首行: fn main()"）。这类摘要能回答"发生了什么交互"，但无法回答"这个文件做了什么"或"这次改动的影响是什么"。

离线巩固时，LLM 对带 artifact_refs 的 episode 做摘要增强：

增强规则：
- 只增强 content_type = Artifact 的 episode（信息性内容不需要增强）
- 增强时 LLM 可以看到 episode 的自然语言上下文（用户说了什么、Agent 回复了什么），以及相邻 episode 的内容
- 增强后的摘要替换 content 字段，artifact_refs 保留不动
- 增强是有损的：如果 LLM 判断"此代码交互不值得增强"（如只是读取了一个配置文件），保持原摘要不变

增强示例：
```
Phase 1 摘要：
  "读取 src/processor.rs，共 800 行，首行: pub fn process_data(input: &str) -> Result<Data> {"

Phase 3 增强后：
  "读取 src/processor.rs — 数据处理管道的主模块，process_data 函数接受字符串输入，通过验证→清洗→转换三阶段处理，返回结构化 Data。用户要求增加输入验证的错误处理。"
```

为什么不在 Phase 1 就用 LLM 生成摘要？因为离线巩固是批量处理——50 个 episode 一起回放，LLM 有完整的上下文做更准确的摘要。Phase 1 的模板摘要保证零额外成本，Phase 3 的增强摘要保证质量，二者各司其职。

**核心组件二：分页换出（MemGPT 风格）**

分页换出的目标不是"无限扩展上下文"，而是"让 Agent 主动管理记忆的活跃度"。以下情况触发换出：

- Token 使用率 > 90% 且关键信息无法通过截断解决
- 某个 episode 超过 30 天未被任何检索命中
- 用户明确要求"专注新话题"（对应新 episode 写入时，旧 episode 换出）

换出单位是"消息块"（若干连续 episode 组成的事件片段），而非单条消息。换出时：episode 标记为 swapped_out = true，embedding 保留但降级（不参与向量检索，只在特定触发下换入）。换入时：沿时间线反向检索找到 swap_out 边界，重新激活相关 episode 的 embedding。

**⚠️ 分页换出的循环依赖风险（已知的实现风险）**

分页换出会引发一个矛盾：换出的判断本身需要消耗 Token（要分析哪些信息可以换出）。DeepSeek 的评审也指出了这一点。当前设计决策：换出判断使用专用的小模型（如 qwen3:1.7b）或简化的规则引擎，而非主模型。主模型只负责生成"我建议换出 X"的指令，由 Runtime 执行实际的换出操作。

云端同步按节点类型同步。全部数据明文同步，平台托管（与主流互联网平台一致，详见 00-prd.md ADR-002）：

冲突解决策略：Phase 6 阶段先实现单向同步（云端 → Agent），Agent 本地变更记录在本地不上报，避免双向写入导致的冲突。后续 Phase 7+ 若需双向同步，采用"最后写入者胜 + 用户确认"机制。

**Phase 3 验收标准：**

- 离线巩固能发现即时提取无法捕获的隐式关联（需人工评估验证）
- Hypothesis 机制能主动提出并验证假设（追踪假设生命周期）
- 分页换出/换入在极端上下文长度下（> 500 轮）正常工作
- 云端同步按节点类型正确同步，PrivacyLevel 控制打包分享时的过滤

---

### 三阶段总结对照

| 维度 | Phase 1 | Phase 2 | Phase 3 |
|------|---------|---------|---------|
| **核心问题** | 能不能记住 | 能不能学习 | 能不能整合 |
| **即时提取** | Tool Call（显式信息） | 扩展 Tool Call + failure_case 联动 | 离线 LLM 回放（隐式关联 + Artifact 摘要增强） |
| **遗忘机制** | 乘法衰减 + Dormant | ProceduralNode 90 天 Purge | HypothesisNode 过期清除 |
| **程序记忆** | ProceduralNode 结构体 | Skill ↔ ProceduralNode 双向联动 | 跨 Skill 通用模式提炼 |
| **自我认知** | Manifest 派生 Autobiographical | 自我评估更新 Limitation | HypothesisNode 主动假设验证 |
| **云端同步** | 无 | 无 | 按节点类型同步 |
| **困境覆盖** | 困境二、困境四（完整） | 困境三（部分补全） | 困境三（假设验证）、困境五（情感信号补充） |
| **可扩展性** | MemoryStore trait + 生命周期阶段定义 | 中间件管线 | 存储后端可替换 |

## 10. 记忆生命周期架构

### 10.1 设计动机

记忆系统是 Rollball Agent 的核心差异化能力——推理能力依赖 LLM，操作能力依赖 Tools，只有记忆系统是 Rollball 自主掌控的。随着平台演进，记忆系统必然经历大量迭代（新检索策略、新遗忘模型、新巩固方式、新存储引擎……）。如果记忆触发点硬编码在 Runtime 主循环里，每次记忆迭代都要改 Runtime 源码，这违反了"Runtime 是稳定执行引擎"的定位。

因此引入**记忆生命周期（Memory Lifecycle）**作为 Runtime 和 Memory 系统之间的标准化接口。Runtime 只负责在固定位置触发生命周期阶段，Memory 系统通过注册 handler 和中间件响应，两者解耦。

### 10.2 生命周期阶段定义

#### 10.2.1 主循环内阶段（同步，由 Runtime 在每轮迭代中触发）

| 阶段 | 触发点 | 输入 | 输出 | 说明 |
|------|--------|------|------|------|
| `Retrieve` | 步骤 ② 构建上下文 | 用户消息 + 当前上下文摘要 | `Vec<MemoryContext>` | 记忆检索：Grafeo 通道（hybrid_search + graph_expand，始终执行）；若 manifest 声明 RAG，并行查询 RAG 通道（RagClient.query，超时 5s 降级）。详见 00-prd.md §1.13.1 |
| `Inject` | 步骤 ② 构建上下文（Retrieve 之后） | `Vec<MemoryContext>` + Token 预算 | 格式化字符串 | 决定如何将记忆注入 LLM 上下文 |
| `Record` | 步骤 ⑥ 结果追加历史（异步） | 本轮 user_msg + assistant_reply + tool_results | `()` | 记录本轮交互到经历层 |

#### 10.2.2 后台阶段（异步，由 MemoryManager 独立调度）

| 阶段 | 触发条件 | 输入 | 输出 | 说明 |
|------|---------|------|------|------|
| `Consolidate` | 即时提取（每轮 Record 后检查）+ 离线巩固（空闲/阈值） | 未巩固 episode 列表 | 新建/更新的 KnowledgeNode | 巩固管道（§4） |
| `Decay` | 定时扫描（每小时，可配置） | 当前时间 | `DecayScanResult` | 遗忘衰减（§5） |
| `Compact` | 存储维护（启动时 + 空闲时） | 存储统计 | 清理数量 | 索引优化、旧 episode 清理、Grafeo WAL Checkpoint |

### 10.3 MemoryStore trait（存储后端抽象）

Runtime 和上层记忆逻辑不直接依赖任何具体存储引擎（grafeo-engine / Sled / LMDB / 远程服务），而是通过 `MemoryStore` trait 交互。这确保存储方案可替换——Phase 1 用 GrafeoStore（grafeo-engine），未来可无缝切换。

```rust
/// 记忆查询参数（替代裸 &str，支持扩展）
pub struct MemoryQuery {
    pub query_text: String,
    pub filters: MemoryFilters,
    pub limit: usize,
    pub expand_hops: u8,          // 关联扩散跳数（0 = 不扩散）
    pub min_score: Option<f32>,   // 最低分数阈值
}

pub struct MemoryFilters {
    pub node_types: Vec<NodeTypeFilter>,  // 按节点类型过滤
    pub privacy_levels: Vec<PrivacyLevel>,// 按隐私级别过滤
    pub time_range: Option<(DateTime, DateTime)>, // 时间范围
    pub session_id: Option<String>,       // 按会话过滤
}

/// 检索结果（统一经历层和沉淀层）
pub struct SearchResult {
    pub node: MemoryNode,         // Episode 或 KnowledgeNode
    pub score: f32,               // 相关性分数
    pub source: ResultSource,     // DirectMatch / GraphExpansion
    pub context_tokens: usize,    // 预估 token 数（用于裁剪预算计算）
}

/// 记忆上下文（Retrieve 阶段的输出、Inject 阶段的输入）
pub struct MemoryContext {
    pub content: String,          // 格式化后的记忆内容
    pub priority: u8,             // 注入优先级（0 = 最高，7 = 最低）
    pub source: ContextSource,    // 来自哪个认知层
    pub estimated_tokens: usize,  // 预估 token 数
}

pub enum ContextSource {
    Autobiographical,   // 自传体记忆（绝不裁剪）
    SemanticCore,       // 语义记忆核心事实
    Procedural,         // 程序记忆
    UserPreference,     // 用户偏好
    FailureLesson,      // 失败教训
    GraphExpansion,     // 关联扩散结果
    Episodic,           // 经历层情景
    RagChannel(String), // RAG 通道结果（参数为 RAG 工具名，如 "enterprise_knowledge"；仅 manifest 声明 RAG 时出现）
}

/// 记忆存储后端的标准化接口
/// 实现者可以是 grafeo-engine / Sled / LMDB / 远程服务 / 内存 mock
pub trait MemoryStore: Send + Sync {
    // ── 经历层 ──

    /// 写入交互片段（自动分类内容类型、工件性压缩）
    fn store_episode(&self, episode: &Episode) -> Result<()>;

    /// 检索情景记忆
    fn search_episodes(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>>;

    /// 标记情景已巩固
    fn mark_consolidated(&self, ids: &[String]) -> Result<()>;

    /// 清理已巩固且过期的情景
    fn cleanup_episodes(&self, older_than: Duration) -> Result<u64>;

    // ── 沉淀层 ──

    /// 写入/更新知识节点（Fact 自动语义去重）
    fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()>;

    /// 写入/更新程序记忆节点
    fn store_procedural(&self, node: &ProceduralNode) -> Result<()>;

    /// 写入/更新自传体记忆节点
    fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<()>;

    // ── 统一检索 ──

    /// 混合搜索：向量 + 全文 + RRF 融合
    fn hybrid_search(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>>;

    /// 关联扩散：从种子节点出发，沿图边扩展
    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> Result<Vec<SearchResult>>;

    // ── 遗忘 ──

    /// 衰减扫描（使用传入的配置，支持按 Agent 定制）
    fn run_decay_scan(&self, config: &DecayConfig) -> Result<DecayScanResult>;

    /// 恢复 Dormant 节点为 Active
    fn reactivate_node(&self, node_id: &str) -> Result<()>;

    /// 清理过期 Dormant 节点
    fn purge_expired(&self, max_dormant_age: Duration) -> Result<PurgeResult>;

    // ── 生命周期 ──

    /// 存储健康检查（用于监控和诊断）
    fn health_check(&self) -> Result<StoreHealth>;

    /// 存储统计信息（节点数、存储大小、索引状态等）
    fn stats(&self) -> Result<StoreStats>;

    /// 关闭存储（释放资源、Grafeo WAL 自动刷写）
    fn close(&self) -> Result<()>;
}

/// 存储健康状态
pub struct StoreHealth {
    pub is_healthy: bool,
    pub latency_ms: u64,          // 最近一次操作的延迟
    pub error_count: u32,         // 最近 N 分钟的错误数
    pub details: Option<String>,  // 错误详情（仅不健康时有值）
}

/// 存储统计
pub struct StoreStats {
    pub episode_count: u64,
    pub node_count: u64,          // Active + Dormant
    pub active_node_count: u64,
    pub dormant_node_count: u64,
    pub edge_count: u64,
    pub storage_size_bytes: u64,
    pub index_count: usize,       // 向量索引 + 全文索引数量
}

/// 遗忘配置（支持按 Agent 定制）
pub struct DecayConfig {
    pub lambda: f32,              // 衰减速率（默认 0.03）
    pub floor: f32,               // 最低活跃度（默认 0.05）
    pub access_per_hit: f32,      // 每次访问增量（默认 0.1）
    pub boost_cap: f32,           // 历史访问上限（默认 0.5）
    pub dormant_threshold: f32,   // Active → Dormant 阈值（默认 0.3）
    pub purge_after: Duration,    // Dormant → Purge 时长（默认 90 天）
    pub purge_importance_threshold: f32, // Purge 路径1的 importance 下限（默认 0.5），详见 §5.2
}
```

**设计要点：**

- `MemoryQuery` 替代裸 `&str`：未来扩展只需加字段（如 `temperature` 控制检索创造性、`recency_boost` 控制时间偏好），不破坏已有实现
- `MemoryContext` 带 `priority` 和 `estimated_tokens`：Inject 阶段可以直接按优先级和 token 预算裁剪，无需 Runtime 了解记忆内部结构
- `DecayConfig` 参数化：不同 Agent 可以有不同的遗忘策略（"学习型 Agent"遗忘慢，"工具型 Agent"遗忘快），通过 manifest 配置注入
- `health_check` + `stats`：为 Desktop App 的记忆管理面板和运维监控提供标准数据接口
- trait 中不包含任何 grafeo-engine 或其他存储后端的类型，实现完全隔离

### 10.4 MemoryManager（中间层）

Runtime 不直接调用 `MemoryStore`，而是通过 `MemoryManager` 这个中间层。`MemoryManager` 是记忆系统的"大脑"——它协调生命周期阶段、管理中间件链、注入配置。

```
┌──────────────────────────────────────────────────────────────┐
│  Agent Runtime 主循环                                         │
│                                                              │
│  ② 构建上下文                                                │
│     └─ memory_manager.retrieve(query) → Vec<MemoryContext>   │
│     └─ memory_manager.inject(contexts, budget) → String      │
│  ⑥ 结果追加历史                                              │
│     └─ memory_manager.record(episode) → ()  [异步]           │
│                                                              │
│  后台任务（MemoryManager 内部调度）：                          │
│     └─ memory_manager.consolidate() → ()                     │
│     └─ memory_manager.decay() → DecayScanResult              │
│     └─ memory_manager.compact() → ()                         │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────┐
│  MemoryManager                                               │
│                                                              │
│  ├── Lifecycle Handler Registry                              │
│  │   └─ 每个阶段可注册多个 handler，按优先级执行              │
│  │                                                           │
│  ├── MemoryMiddleware Chain                                  │
│  │   └─ Record 前后可插入中间件（情感标注/内容过滤/审计）     │
│  │                                                           │
│  ├── Config Provider                                         │
│  │   └─ 从 manifest + 系统默认读取配置，注入到各阶段           │
│  │                                                           │
│  └── Event Bus                                               │
│      └─ 发布 MemoryEvent（记忆写入/遗忘/巩固），供 Desktop App  │
│         订阅展示或日志系统记录                                 │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────┐
│  MemoryStore trait
│  └─ GrafeoStore (grafeo-engine v0.5.39，Phase 1 唯一实现)       │
│     └─ GrafeoDB → Episodic/Semantic/Forgetting/Retrieval modules   │
│     └─ 存储格式：.grafeo 单文件（LPG + HNSW + BM25 + WAL）        │
│  └─ (未来) RemoteMemoryStore (云端分布式存储)                │
│  └─ (未来) InMemoryStore (GrafeoDB::new_in_memory() 测试用 mock) │
└──────────────────────────────────────────────────────────────┘
```

**MemoryManager 核心方法：**

```rust
pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    rag_client: Option<Arc<RagClient>>,  // RAG 检索客户端（仅 manifest 声明 rag 时注入，None 则仅查 Grafeo）
    middlewares: Vec<Box<dyn MemoryMiddleware>>,
    config: MemoryConfig,
    event_bus: MemoryEventBus,
}

impl MemoryManager {
    /// 检索记忆（Retrieve 阶段）
    /// 内部调用 store.hybrid_search + store.graph_expand 检索 Grafeo 通道
    /// 若 rag_client 为 Some，并行查询 RAG 通道（RagClient.query，超时 5s 降级）
    /// 两条通道结果合并，按来源标注（Grafeo / RAG），转换为 MemoryContext
    pub async fn retrieve(&self, query: &str, context: &RetrieveContext) -> Result<Vec<MemoryContext>>;

    /// 注入记忆到上下文（Inject 阶段）
    /// 按 priority 排序，在 token 预算内从低到高裁剪
    /// 返回格式化字符串，供 Prompt Builder 插入上下文
    pub fn inject(&self, contexts: Vec<MemoryContext>, budget: &TokenBudget) -> String;

    /// 记录交互（Record 阶段，异步）
    /// 执行中间件链（pre_record → store_episode → post_record）
    /// 自动分类内容类型、工件性压缩
    pub async fn record(&self, episode: Episode) -> Result<()>;

    /// 即时巩固（Consolidate 阶段的即时部分）
    /// 检查本轮是否触发了 memory_store tool call
    /// 如果有，执行知识去重 + 写入沉淀层 + 标记 episode consolidated
    pub fn consolidate_immediate(&self, store_call: Option<&MemoryStoreCall>) -> Result<()>;

    /// 离线巩固（Consolidate 阶段的离线部分，Phase 3）
    pub async fn consolidate_offline(&self) -> Result<()>;

    /// 遗忘扫描（Decay 阶段）
    pub fn decay(&self) -> Result<DecayScanResult>;

    /// 存储维护（Compact 阶段）
    pub fn compact(&self) -> Result<()>;
}
```

### 10.5 MemoryMiddleware trait（中间件接口）

中间件可以在记忆管线的 Record/Retrieve 阶段前后插入自定义逻辑，无需修改 Runtime 或 Grafeo 代码。

```rust
pub trait MemoryMiddleware: Send + Sync {
    /// 中间件名称（用于日志和调试）
    fn name(&self) -> &str;

    /// 执行优先级（数字越小越先执行）
    fn priority(&self) -> i32;

    /// Record 阶段前处理（episode 写入前）
    /// 例如：情感标注、内容过滤、审计日志
    fn pre_record(&self, episode: &mut Episode, ctx: &MiddlewareContext) -> Result<()>;

    /// Record 阶段后处理（episode 写入后）
    /// 例如：触发关联索引更新、事件通知
    fn post_record(&self, episode: &Episode, ctx: &MiddlewareContext) -> Result<()>;

    /// Retrieve 阶段后处理（检索结果返回前）
    /// 例如：个性化重排序、合规过滤、结果增强
    fn post_retrieve(&self, results: &mut Vec<SearchResult>, ctx: &MiddlewareContext) -> Result<()>;
}
```

**中间件注册方式：**

```toml
# manifest.toml 中声明中间件（Phase 2+）
[memory.middlewares]
# 内置中间件（按名称引用）
emotion_tag = { priority = 10 }
audit_log = { priority = 100 }

# 自定义 WASM 中间件（Phase 3+）
custom_filter = { type = "wasm", path = "filters/content_filter.wasm", priority = 50 }
```

**中间件执行顺序：** 按 `priority` 从小到大排列，`pre_record` 正序执行，`post_record` 逆序执行（洋葱模型，类似 Tower middleware）。任一中间件返回 Err 时，该阶段中止并向上传播错误。

### 10.6 分阶段实现路线

| 阶段 | 内容 | 说明 |
|------|------|------|
| Phase 1 | `MemoryStore` trait 定义 + `MemoryQuery` / `SearchResult` / `MemoryContext` 等数据类型 + `GrafeoStore` 实现（基于 grafeo-engine） | trait 定义先行，GrafeoStore 作为唯一实现，Runtime 改为通过 trait 调用 |
| Phase 1 | `MemoryManager` 基础结构 + 生命周期阶段触发 | Manager 直接转发给 GrafeoStore，不引入中间件机制 |
| Phase 1 | `DecayConfig` 参数化 | 遗忘参数从硬编码改为可配置，通过 manifest 注入 |
| Phase 2 | `MemoryMiddleware` trait + 注册机制 + 内置中间件（emotion_tag / audit_log） | 打开中间件扩展能力 |
| Phase 3 | `InMemoryStore` mock 实现（基于 `GrafeoDB::new_in_memory()`，用于测试） | 替代当前的集成测试方案 |
| Phase 3 | `RemoteMemoryStore` 探索（云端分布式存储） | 如果跨设备实时同步需求明确 |

### 10.7 设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 存储抽象方式 | trait + impl | Rust 生态标准做法，零成本抽象（monomorphization），编译期检查 |
| 中间件模型 | 洋葱模型（Tower 风格） | 业界验证过的模式，支持前/后处理，错误传播自然 |
| 配置注入 | manifest.toml + 系统默认 | manifest 声明 Agent 级定制，系统默认兜底 |
| 事件通知 | Event Bus（发布/订阅） | Desktop App 和日志系统可订阅 MemoryEvent，不影响核心管线性能 |
| Retrieve/Inject 拆分为两个阶段 | 是 | Retrieve 关注"查什么"，Inject 关注"怎么放"，职责分离有利于未来 Inject 策略的独立演化（如 RAG 结果和本地记忆的混合排序） |
| RAG 双通道检索 | 配置驱动 Opt-In | RAG 通道仅当 manifest 声明 `type=rag` 时使能；MemoryManager 通过 `rag_client: Option<Arc<RagClient>>` 条件分支控制；无 RAG 声明的 Agent 行为零侵入（详见 00-prd.md §1.13.1） |
| 存储后端 | grafeo-engine v0.5.39 | 纯 Rust 图数据库，原生支持 LPG + GQL + HNSW + BM25 + WAL + MVCC |
| 数据模型 | LPG（Label + Property + Edge） | 替代关系型表结构，认知分层与 LPG Label 一一映射 |
| 存储格式 | `.grafeo` 单文件 | 替代 SQLite `.db`，内含 WAL + 向量/全文索引 |

## 11. 质量评估框架（v3.7 新增）

记忆系统的质量不能只靠设计直觉——需要系统化的评估体系持续验证和校准。质量评估框架分为在线评估（Runtime 阶段）和离线基准（Benchmark 阶段）两个互补维度，结合可观测指标提供持续反馈。

### 11.1 在线评估（Runtime 阶段）

在线评估在每次检索后异步执行，不增加用户感知的延迟：

**RetrievalMetrics（每次检索后异步计算）**：

```rust
/// Metrics collected after each retrieval operation
/// Computed asynchronously to avoid impacting retrieval latency
pub struct RetrievalMetrics {
    pub result_count: usize,          // Number of results returned
    pub avg_score: f32,               // Average relevance score of results
    pub max_score: f32,               // Highest relevance score
    pub abstention_triggered: bool,   // Whether Abstention was triggered (§6.5)
    pub retrieval_level: u8,          // Degradation level (0-3, §6.1)
    pub graph_expand_nodes: usize,    // Number of nodes expanded via graph_expand
    pub hint_type: HintType,          // memory_hint.type used (s/f/r/i)
}
```

- **result_count + avg_score + max_score**：基础检索质量指标，连续低 avg_score 暗示 min_score 阈值需调整
- **abstention_triggered**：拒答率过高（>30%）可能说明 min_score 过严；拒答率过低（<5%）可能说明 min_score 过松
- **retrieval_level**：降级频率反映 Grafeo 健康状况

**轻量 LLM Judge（Phase 3+，可选）**：

使用小型模型（如 qwen3:1.7b）评估检索结果与查询的相关性，作为在线评估的补充：

```
LLM Judge 流程（Phase 3+）：
  触发条件：采样率 10%（每 10 次检索评估 1 次）
  输入：查询文本 + Top-3 检索结果
  输出：每个结果的相关性评分（1-5 分）
  成本：qwen3:1.7b 约 50 tokens/次，可忽略
  用途：
    - 校准 RRF 分数与实际相关性的偏差
    - 识别 hybrid_search 系统性弱点（如特定 type 的检索质量差）
```

**用户隐式反馈**：

- Agent 后续输出是否引用了检索结果（引用率 = 引用了检索结果的回复数 / 触发了检索的回复数）
- 用户是否在同一话题追问（暗示检索结果不够完整）
- 用户是否直接否定 Agent 回复（暗示检索结果不准确）

### 11.2 离线基准（Benchmark 阶段）

**LongMemEval 5 维集成**：

LongMemEval 是当前 Agent 记忆系统最权威的评测基准，覆盖 5 个核心维度：

| 维度 | 代码 | 评估内容 | 与 RollBall 模块的对应 |
|------|------|---------|---------------------|
| 信息提取 | IE | 从对话中提取关键信息 | 即时提取（§4.1） |
| 多会话推理 | MR | 跨会话整合信息推理 | 关联扩散检索（§6） |
| 时序推理 | TR | 按时间顺序推理事件 | 经历层时间索引 + CDC history |
| 知识更新 | KU | 处理信息更新和冲突 | 冲突处理三层信号（§6.4） |
| 拒答 | Abs | 信息不足时选择拒答 | Abstention 机制（§6.5） |

**分阶段目标**：

| 阶段 | 综合目标 | IE | MR | TR | KU | Abs | 说明 |
|------|---------|-----|-----|-----|-----|------|------|
| Phase 2 | 65%+ | 70%+ | 60%+ | 55%+ | 60%+ | 60%+ | 精度优先，Abstention 是短板 |
| Phase 3 | 75%+ | 80%+ | 70%+ | 65%+ | 75%+ | 75%+ | 离线巩固 + LLM Judge 提升 |

**Phase 3 额外基准目标**：

- BEAM MDS < -0.12（多跳扩散检索的语义漂移控制）
- Accuracy@1M > 50%（大规模节点下的 Top-1 准确率）

### 11.3 可观测指标

**NRR（归一化检索相关性）**：

```
NRR = avg_score / max_possible_score

其中 max_possible_score 为完全匹配的理论最高分
NRR < 0.5 → 检索质量告警，需检查 embedding 模型或索引状态
NRR > 0.8 → 检索质量良好
```

**冲突处理准确率**：

```
冲突处理准确率 = 自动判定正确数 / 自动判定总数

"正确"定义：自动判定结果与后续 LLM 仲裁或用户确认一致
目标：Phase 2 > 85%，Phase 3 > 90%
告警：准确率 < 80% 时，回退所有启发式自动判定为 LLM 仲裁
```

**衰减参数校准指标**：

```
lambda 值 vs 用户反馈的"记忆过期率"：
  - 用户抱怨"记不住"→ lambda 可能过大（衰减太快）
  - 用户抱怨"老信息干扰"→ lambda 可能过小（衰减太慢）
  - 校准方式：收集用户反馈，与 Dormant 转化率交叉分析
  - 目标：Dormant 转化率与用户感知的"记忆过期率"偏差 < 15%
```

**指标聚合与告警**：

| 指标 | 采集频率 | 告警阈值 | 告警动作 |
|------|---------|---------|--------|
| NRR | 每次检索 | < 0.5 持续 10 次 | 检查 embedding 模型 + 索引状态 |
| Abstention 率 | 每次检索 | > 30% 或 < 5% | 调整 min_score 阈值 |
| 冲突自动判定准确率 | 每次离线巩固 | < 80% | 回退为 LLM 仲裁 |
| 降级频率 | 每次检索 | Level 2+ 占比 > 20% | 检查 Grafeo 健康状态 |

### 11.4 质量门禁

Phase 2 交付前必须通过以下验证：

**LongMemEval-S（~115K tokens）验证**：

- 使用 LongMemEval-S 标准子集（约 115K tokens 上下文长度）运行完整 5 维评测
- 综合分数 >= 65%，各维度不低于 50%
- Abs 维度 >= 60%（拒答是最关键的差异化能力）
- 测试环境：单 Agent，Grafeo 存储后端，embedding 模型 Ollama/Remote API（取决于 provider 配置）

**功能验证清单**：

| 功能 | 验证方法 | 通过标准 |
|------|---------|--------|
| Abstention 机制 | 人工构造低相关性查询 | 触发拒答且不产生幻觉 |
| 两层冲突检测 | 人工构造冲突场景 | 两层信号均能正确标记 |
| min_score 过滤 | 调整阈值观察检索结果 | 阈值与过滤效果符合预期 |
| 检索权重动态调整 | 不同 hint.type 查询 | 权重和扩散参数正确切换 |
| 即时/离线巩固边界 | 对比即时和离线产出 | PendingNode 正确升级/降级 |

**Phase 3 质量门禁（在 Phase 2 基础上追加）**：

- LongMemEval 完整集（非 S 子集）综合 >= 75%
- BEAM MDS < -0.12
- Accuracy@1M > 50%
- 离线巩固能发现即时提取无法捕获的隐式关联（人工评估）
