# Memory 仿生分层架构

> 版本：v3.5 | 更新日期：2026-04-21

> 本文档基于 docs/review/04-p2-s2-design-review.md S2 设计评审更新。主要变更：memory_store 接口从三元组改为自然语言（§4.1）、Purge 三条路径（§5.2）、检索降级策略（§6.1）、graph_expand 自适应深度（§6.3）、新增冲突处理章节（§6.4）、Token 分层计数与弹性预算分配（§1）。

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
│  HNSW 向量索引 + BM25 全文检索                           │
│  生命周期：天→周，巩固后晋升至沉淀层                      │
│  仿生对应：海马体临时编码                                 │
├─────────────────────────────────────────────────────────┤
│  沉淀层（Consolidated）                                  │
│  ───                                                    │
│  语义记忆 — 事实、偏好、关系（KnowledgeNode）             │
│  程序记忆 — 行为模式、操作规则（ProceduralNode）          │
│  自传体记忆 — 自我认知、能力边界（AutobiographicalNode）  │
│  LPG 知识图谱 + 关联扩散检索                             │
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
| 瞬态层 → 经历层 | 对话持久化 | Agent 发送回复后，异步写入 Grafeo（写队列 + 本地 WAL 保证不丢失，超时 500ms 降级为后台重试） |
| 经历层 → 沉淀层 | 巩固管道 | 即时提取（LLM 自主 tool call）+ 离线回放（空闲时专用调用） |
| 沉淀层 → 瞬态层 | 检索注入 | 用户输入到达时，检索相关记忆注入上下文 |
| 沉淀层/经历层内流动 | 关联扩散 | 检索时沿图边 1-2 跳扩展 |
| 沉淀层 → Dormant | 遗忘衰减 | 后台定期计算 decay_score |

**不可逆的单向门：** 经历层 → 沉淀层是信息精炼过程（原始片段 → 结构化知识），天然单向。但沉淀层 → 经历层可以通过"回忆"机制实现——用户或 Agent 主动触发时，从沉淀层提取关联知识，作为新的情景上下文注入瞬态层。

**分层与 Grafeo 存储的映射：**

| 认知层 | 内容 | Grafeo 存储 | 说明 |
|--------|------|-------------|------|
| 瞬态层 | 工作记忆 | 不在 Grafeo 中 | LLM 上下文窗口，纯进程内存 |
| 经历层 | 情景记忆 | `episodes` 表 | 向量 + 全文 + metadata |
| 沉淀层 | 语义/程序/自传体/Skill 经验 | `memory_nodes` + `memory_edges` 表 | LPG 知识图谱 |

不存在"经历层节点存在 Grafeo semantic 中"的歧义——认知分层和存储表是一一映射的。

## 0.1 LLM 优先原则

**信任 LLM 超过信任规则——除非规则能解决 LLM 不能解决的问题。**

记忆系统中涉及大量语义判断（什么值得记、confidence 多高、是否冲突、如何分类），这些判断由 LLM 完成而非规则引擎。具体应用：

- **即时提取**：LLM 自主判断是否调用 memory_store、评估 confidence（high/medium/low），Runtime 不做语义层面的二次检查
- **离线巩固**：三元组提取、冲突分类、证据验证由 LLM 在有完整上下文时执行，而非实时阶段的规则近似
- **Runtime 仅做机械护栏**：内容长度限制、调用频率限制、安全过滤——这些是 LLM 无法自我约束的机械性限制
- **检索辅助**：memory_hint 结构化输出（实体提取 + 查询分类）由 LLM 顺手完成，Runtime 规则仅在 LLM 未输出时提供冷启动降级

## 1. 瞬态层：工作记忆

工作记忆是 Agent 当前正在"思考"的内容，直接映射到 LLM 的上下文窗口。

```
┌─ System Prompt ──────────────────────────┐
│  Agent 身份定义                            │
│  自传体记忆摘要（来自沉淀层注入）           │
│  Skill Instructions                       │
│  工具定义                                  │
│  Memory Hint 指令（结构化实体输出约束）     │
│  内容折叠提示（📎 标记内容的召回指引）       │
├─ Retrieved Memory ───────────────────────┤
│  巩固层检索结果（语义/程序/自传体）         │
│  经历层检索结果（相似情景）                 │
│  关联扩散结果                              │
├─ Conversation ──────────────────────────┤
│  用户消息 + Agent 回复                     │
│  工具调用与结果                            │
│  （大段内容可能被折叠为 📎 引用）           │
├─ Scratchpad ────────────────────────────┤
│  Agent 内部推理链                          │
└──────────────────────────────────────────┘
```

**Memory Hint 指令（Phase 2 新增）：**

System Prompt 末尾注入约 40 tokens 的结构化输出约束，要求 LLM 每轮回复末尾附加极简的记忆提示元数据：

```
每次回复末尾附加记忆提示，格式：<mh>{"e":[实体],"t":类型}</mh>
- e: 本轮对话涉及的核心实体，最多3个
- t: s=语义联想 f=精确事实 t=时间相关 i=身份偏好
示例：<mh>{"e":["React","性能优化"],"t":"s"}</mh>
```

该指令仅包含 2 个字段（`e` 实体列表 + `t` 查询类型单字符），设计原则是"LLM 顺手就能做的事"，不增加额外认知负担。Runtime 解析 `<mh>` 块后用于优化下一轮的检索策略（调整 RRF 权重、增强 BM25 关键词）。对话历史存储时剥离 `<mh>` 块，不占后续上下文空间。解析失败时静默降级为默认检索策略。详见 review/04-p2-s2-design-review.md §6.1。

**内容折叠提示（Phase 2 新增）：**

当对话历史中有内容被折叠为 📎 引用时（见下方"对话历史三阶段裁剪"），System Prompt 中注入折叠召回指引（约 30 tokens）：

```
对话中带 📎 标记的内容已被折叠以节省空间。如需查看原文，请使用对应工具重新读取。
```

该提示仅在对话历史中实际存在折叠内容时才注入，无折叠内容时不占用 token。LLM 看到 📎 引用后，自然通过已有的 tool_call 机制（如 file_read）召回原始内容，无需额外工具或机制。

**瞬态层的管理策略（Phase 2 更新）**：

**Token 计数——分层实现 + 增量缓存**：

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

**预算分配——弹性分区 + 保底线**：

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

详见 docs/review/04-p2-s2-design-review.md §6.5

- **两条独立裁剪流水线**：对话历史和检索结果分别管理，不互相挤占。

  **流水线 A — 对话历史（Conversation）：**
  采用三阶段渐进裁剪（Phase 2 改进），裁剪由 Runtime 强制执行：

  - **第一阶段：内容折叠（无损压缩）**：优先将历史中的大段内容（单块 > 200 tokens）折叠为紧凑引用。文件内容折叠为 `📎 path@L1..L150`，工具长输出折叠为 `📎 tool:name(args) → summary`，用户粘贴的大文本折叠为 `📎 inline#hash (type, N tokens)`。最近 3 轮不折叠。折叠后的 FoldedRef 元数据保留 content_hash，支持按需召回和变更检测。典型代码开发场景可节省 50-60% 的 history tokens。
  - **第二阶段：FIFO 淘汰**：折叠后仍超预算时，从最早的消息对（user+assistant）开始移除，保留最近 3 轮和包含 tool_call 结果的消息对。
  - **第三阶段：摘要替代（Phase 3）**：对被 FIFO 裁剪的消息生成摘要（~100 tokens），保留核心决策信息。

  裁剪后的消息对写入经历层（episodic），保证信息不丢失。折叠引用（FoldedRef）在会话结束后可转为 ArtifactRef 存入经历层。

  **流水线 B — 检索结果（Retrieved Memory）：**
  按优先级从低到高砍，由 Runtime 强制执行。每条检索结果预计算 token 数，从优先级最低的开始移除直到剩余预算够用。

- **检索结果裁剪优先级**（从最先裁剪到最后裁剪）：
  1. 经历层检索结果（相似情景）
  2. 关联扩散结果
  3. 失败教训 / 低优先级经验
  4. 用户偏好
  5. 成功模式 / 程序记忆规则
  6. 语义记忆核心事实
  7. 自传体记忆摘要（绝不裁剪）
  8. Agent 身份定义（绝不裁剪）

- **分页换出**（Phase 3，受 MemGPT 启发）：当 Token 预算不足时，Agent 可主动决定将部分信息"换出"到经历层（写入 episodic），下次需要时通过检索"换入"。这是从操作系统虚拟内存借鉴的机制。

## 2. 经历层：情景记忆

情景记忆存储 Agent 与用户的交互片段，是记忆的"原始素材"。

```
Grafeo Episodic Store
├── episode_id: String              // 唯一 ID
├── timestamp: DateTime             // 发生时间
├── role: Role                      // user / agent / tool
├── content: String                 // 内容（信息性内容原样存储；工件性内容仅存摘要）
├── content_type: ContentType       // Informational / Artifact / Structural
├── artifact_refs: Vec<ArtifactRef> // 工件引用（仅 Artifact 类型有值）
├── embedding: Vec<f32>             // 语义向量（基于 content 而非原始代码生成）
├── metadata: HashMap<String, Value>  // 上下文元数据（话题、情感倾向等）；不预存关联 node_id，跨层扩散通过 source_episode 反向查询实现
├── session_id: String              // 所属会话
├── consolidated: bool              // 是否已巩固到沉淀层
└── importance: f32                 // 重要性评分（写入时 LLM 打分 0.0-1.0）
```

**内容分类与压缩：**

Episode 不是原样存储对话全文。写入前按内容类型分类处理，避免代码/文件等"工件性内容"膨胀 Grafeo 体积。

| 内容类型 | 判断规则 | 存储策略 | 示例 |
|---------|---------|---------|------|
| Informational（信息性） | 自然语言对话、Agent 决策解释 | 原样存储 | "我喜欢简洁的回复" |
| Artifact（工件性） | Tool Call 中 file_read/shell_exec/code_write 的输出；单条消息中超过 2000 字符的连续代码块 | 仅存 LLM 生成的摘要 + artifact_refs | 800 行 Rust 代码 → "修改了 process_data 函数，增加输入验证" |
| Structural（结构性） | Tool Call 参数/元数据、JSON 结构 | 精简摘要 | file_read → "读取 src/main.rs，共 200 行" |

**ArtifactRef 结构：**

```rust
struct ArtifactRef {
    path: String,           // 文件路径（如 "src/processor.rs"）
    hash: String,           // 内容哈希（sha256，用于判断是否已变更）
    description: String,    // LLM 生成的 1-3 句内容摘要
    line_range: Option<(u32, u32)>,  // 涉及的行范围
    modified_at: DateTime,  // 文件最后修改时间
}
```

**为什么代码不属于 Grafeo：** 代码住在文件系统里，不住在记忆里。人类不会把代码全文记在脑子里——你记住的是"上次用了 React Hooks 模式"，不是具体的 800 行代码。Grafeo 存"关于代码的描述"，需要实际代码时通过 artifact_refs 的 path + hash 在文件系统/版本控制中查找。

**Tool Result 摘要规则：**

| 工具 | 原始结果 | 摘要存储 | 摘要生成方式 |
|------|---------|---------|-------------|
| file_read | 文件全文 | "读取 [path]，共 [N] 行，首行: [首行前100字符]" + ArtifactRef | Runtime 模板提取 |
| shell_exec | 命令输出 | "执行 [cmd]，退出码 [N]，输出前200字符: [...]" | Runtime 模板提取 |
| web_fetch | 网页全文 | "获取 [url]，标题 [title]，内容前200字符: [...]" | Runtime 模板提取 |
| code_write | 写入的代码 | "写入 [path]，共 [N] 行" + ArtifactRef | Runtime 模板提取 |

**摘要生成的实现方式——零 LLM 调用，纯 Runtime 逻辑：**

内容分类压缩不需要额外 LLM 调用，全部由 Runtime 的确定性逻辑完成。具体分两个管道：

**管道 A：Tool Call 结果的摘要（结构化模板提取）**

Tool Call 的返回结果本身就是结构化 JSON，Runtime 按工具类型做模板化字符串拼接，零推理成本：

```
file_read 返回 { path: "src/main.rs", content: "..." }
  → content_type = Artifact
  → content = "读取 src/main.rs，共 200 行，首行: fn main() {"
  → artifact_refs = [{ path: "src/main.rs", hash: "sha256:abc...", description: "读取 src/main.rs，共 200 行", line_range: "1-200", modified_at: <now> }]

shell_exec 返回 { cmd: "cargo test", exit_code: 0, output: "..." }
  → content_type = Structural
  → content = "执行 cargo test，退出码 0，输出前200字符: running 42 tests..."
```

**管道 B：Agent 回复中代码块的分离（Markdown 正则提取）**

Agent 回复是 Markdown 格式，代码块有明确的 ``` 围栏标记。Runtime 用正则分离代码和自然语言：

```
Agent 回复原文：
  "这是重构后的文件：\n```rust\nfn process_data(input: &str) -> Result<Data> {\n  // 800行...\n}\n```\n主要改动是增加了输入验证。"

分离过程：
  1. 正则提取 ```rust\n...\n``` 代码块
  2. 代码块替换为占位符：[代码输出: 写入 src/processor.rs，800 行]
  3. 自然语言保留："这是重构后的文件：[代码输出: ...] 主要改动是增加了输入验证。"
  4. content_type = Artifact
  5. artifact_refs = [{ path: "src/processor.rs", hash: "sha256:def...", ... }]

描述来源的优先级：
  1. 代码块前的自然语言上下文（Agent 通常会说"这是重构后的文件"之类的话）
  2. 代码块语言标记 + 行数（fallback："rust 代码，800 行"）
  3. 无描述时仅存 "[代码输出: {行数} 行]"
```

**关键设计决策：为什么不用 LLM 生成摘要？**

- 确定性：模板提取和正则分离的结果是确定性的，不会因 LLM 幻觉导致摘要失真
- 零成本：不增加任何 API 调用或推理延迟
- 足够用：episode 的目的是让 Agent "记住有过这次交互"，而非替代文件系统存储精确代码。下次需要精确代码时，走 artifact_refs 的 path + hash 查文件系统
- 如果未来需要更精细的语义摘要（如"这个函数做了 X"），可在 Phase 3 离线巩固时用 LLM 补充，不阻塞 Phase 1

**检索能力：**

- **语义检索**：HNSW 向量索引，按 embedding 余弦相似度
- **全文检索**：BM25 倒排索引，按关键词精确匹配
- **混合检索**：向量 + 全文，通过 Reciprocal Rank Fusion (RRF) 融合排序
- **时间过滤**：按时间范围缩小检索空间
- **跨层关联扩散**（§6）：检索到的 episode 通过沉淀层 KnowledgeNode 的 `source_episode` 字段反向查询关联节点，沿图边扩展到沉淀层知识和其他经历层 episode。例如：用户问"上次去上海住的酒店"，episodic 检索到出差记录 → 反向查到沉淀层"用户常住锦江之星" → 通过 memory_edges 扩展到同一酒店的另一次出差 episode。

**Embedding 生成时机：**

episode 写入时同步生成 embedding（all-MiniLM-L6-v2 在 CPU 上约 10-50ms）。如果生成超时（200ms），embedding 置空，后台任务补生成。检索时如果 episode 的 embedding 为空，退化为仅 BM25 全文检索。

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
           ├─ 写入 PendingKnowledgeNode（含 embedding）
           ├─ 冲突候选检测（embedding 相似度 > 0.85）
           └─ 标记相关 episode

离线巩固时（Phase 3）：
  → LLM 批量提取 PendingKnowledgeNode 的三元组
  → 构建图边、执行冲突分类
  → 升级为正式 KnowledgeNode
```

**关键行为保证：**

- **不强制提取**：LLM 有权不在每轮调用 memory_store。简单问候、天气查询等不存储，这比预过滤规则更智能
- **Fact 自动去重**：Runtime 在执行 memory_store 时检查 (subject, predicate) 是否已存在，避免重复
- **对话始终记录**：无论是否触发 memory_store，每轮对话内容都写入经历层（episode），供离线巩固回溯
- **工具调用可见**：memory_store 的调用记录在对话历史中，用户知道 Agent 记住了什么

### 4.2 离线巩固（Phase 3）

即时提取（Tool Call）覆盖了"显式信息的即时记忆"，但有两类信息它处理不了：

1. **隐式关联**：用户三次提到上海，但每次都没说"我住在上海"——Tool Call 不会触发，但离线回放可以发现"用户可能常住上海"
2. **跨片段模式**：多个 Skill 都因"输出太长"被纠正——单个 Tool Call 只记录 ProceduralNode，但离线巩固能发现跨 Skill 的通用模式
3. **主动假设验证**（Phase 3 补充）：LLM 在回放时主动提出"如果…会怎样"类型的假设，生成 HypothesisNode 暂存供后续验证——这是困境三（记忆泛化与抽象）在 Phase 3 的具体补全

离线巩固用**专用 LLM 调用**（非 Tool Call），因为它的输入是批量情景记忆而非实时对话，需要独立的 prompt 和推理空间。

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
| 能力 | 提取用户明确表达的信息 | 发现隐式模式、合并冲突知识、提炼跨 Skill 模式 |
| 成本 | 零额外 API 调用（每轮 ~150 token 工具定义开销） | 批量 LLM 调用（空闲时进行） |
| 可靠性 | 中（依赖 LLM 自主判断，可能遗漏） | 高（专用 prompt，深度推理） |

## 5. 遗忘机制

遗忘不是记忆的失败，是记忆的优化。没有遗忘的记忆系统会退化——检索效率下降、无关信息干扰决策、存储资源无限增长。

### 5.1 衰减公式

沉淀层每个节点（KnowledgeNode / ProceduralNode）的衰减分数由两个维度决定：

- **importance（固有价值）**：写入时 LLM 打分（0.0-1.0），静态不变，代表这条知识的内在重要性
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

**Fact 语义去重：**

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
② 同时删除相关 memory_edges
   │
   ▼
③ 记录 purge_log（节点 ID、类型、内容摘要、purge 原因、关联节点 ID）
   - purge_log 保留 30 天，用于调试和"找回被遗忘的记忆"
   - purge_log 支持手动回滚：用户可从 purge_log 恢复任意已删除节点
```

**用户手动操作：**

- 用户可随时手动 purge 指定节点或所有 Dormant 节点（Desktop App → Memory 管理面板）
- 用户可手动恢复任意 Dormant 节点为 Active（等同于"我想起来了"）

### 5.3 不参与遗忘的节点

| 节点类型 | 是否遗忘 | 原因 |
|---------|---------|------|
| AutobiographicalNode | 否 | 核心身份，遗忘 = 人格断裂 |
| KnowledgeNode（identity 类） | 否 | 用户姓名、语言等基础身份 |
| SkillExperience | 专用衰减 | 按 Skill 系统规则管理 |
| SkillDraft / Iteration / Execution | 开发期保留 | 调试完成后归档 |

## 6. 关联扩散检索

传统检索是"查到什么就是什么"，关联扩散是"查到一个，带出一串"——模拟海马体的模式完成和激活扩散。

### 6.1 检索流程（Phase 2 更新）

检索同时查询经历层和沉淀层，并支持跨层关联扩散。MemoryManager 根据 Grafeo 的就绪状态和检索耗时决定降级：

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
  SLA：P99 < 10ms

  ↓ Grafeo 完全不可用

Level 3（内存模式）
  策略：仅返回当前会话工作记忆，无持久化检索
  SLA：P99 < 1ms
```

**单次检索预算分配（500ms 硬超时）**：

```
① embedding 生成：≤200ms（超时→跳过向量，BM25 only）
② hybrid_search：≤150ms（向量和 BM25 并行，先返回的直接进融合）
③ graph_expand：≤100ms（早期终止，超时返回已扩展节点）
④ 排序+格式化：≤50ms
任一环节超时，使用已有部分结果继续后续步骤。
```

**检索流程**：

```
用户输入 / Agent 内部查询
   │
   ▼
① 并行检索两层数据
   ├─ 经历层 hybrid_search（episodes 表）：向量 + 全文 + RRF
   │   返回 Top-K 相似情景
   └─ 沉淀层 hybrid_search（memory_nodes 表）：向量 + 全文 + RRF
       返回 Top-K 匹配知识（Knowledge / Procedural / Autobiographical）
   │
   ▼
② graph_expand：从所有匹配节点出发，沿 memory_edges 做跨层扩展
   - 经历层 episode → 通过 KnowledgeNode.source_episode 反向查询关联的沉淀层节点
   - 沉淀层 node → 通过 memory_edges 扩展到其他沉淀层节点
   - 1 跳：直接关联（边权重 > 0.3）
   - 2 跳：间接关联（累积路径权重 > 0.1）
   - 3 跳：复杂推理（累积路径权重 > 0.05），通过早期终止实际大多在 1-2 跳停止
   │
   ▼
③ 去重 + 评分
   - 直接匹配节点分数 = RRF 分数
   - 扩展节点分数 = 路径权重 × 源节点 RRF 分数
   - 多路径节点获得 PageRank 式额外权重加成
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

### 6.3 性能保障（Phase 2 更新）

- 扩展深度硬限制 **3 跳**（通过早期终止实际大多在 1-2 跳停止）
- early_stop_threshold 随跳数递增（1跳: 0.1, 2跳: 0.15, 3跳: 0.2），越远越严格
- 每跳最多扩展 5 条边（按权重 Top-5）
- 扩展节点总数上限 20（防止 Token 膨胀）
- 只对 Active 节点做扩展，Dormant 节点不参与
- 经历层和沉淀层并行检索，扩展阶段串行（避免并发复杂度）
- 扩散阈值可配置（默认 0.2），在首次运行时可根据实际效果调整
- 多路径节点获得 PageRank 式额外权重加成

详见 docs/review/04-p2-s2-design-review.md §6.2、§6.3

### 6.4 冲突处理（Phase 2 新增）

冲突处理采用**两阶段方案**：即时阶段做粗筛（候选冲突标记），离线阶段做精确分类与解决。

**即时阶段（每轮 memory_store 时）**：

```
PendingKnowledgeNode 写入时：
  1. 计算新节点 embedding 与已有 Active 节点的余弦相似度
  2. 相似度 > 0.85 → 标记为候选冲突（conflict_candidate = true）
  3. 记录 conflict_pair: (new_node_id, old_node_id, similarity)
  4. 新节点立即可被检索，但暂不参与 graph_expand
```

**离线阶段（巩固管道，Phase 3）**：

```
离线巩固时，LLM 批量处理候选冲突：

输入：冲突的两条记忆 + 各自的 source_episode 上下文
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

**不确定类冲突的处理**：
- ambiguous 累计 3+ 个时，Agent 在下次对话中自然地询问用户确认
- 不通过弹窗打扰，在对话中自然地询问

**设计理由**：冲突分类是语义判断，遵循 LLM 优先原则，由离线巩固阶段的 LLM 完成（此时 LLM 已在场做三元组提取）。规则判断（"时间差>30天→演进"）是语义判断伪装成规则，泛化性差。

详见 docs/review/04-p2-s2-design-review.md §6.9

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

**路径 2：系统 Agent ContentProvider（身份与偏好）**

用户身份和偏好等系统级信息由系统 Agent（`com.rollball.system`）统一管理，其他 Agent 通过 Intent 查询。详见 [07-system-agent.md](./07-system-agent.md)。

**系统 Agent 查询的容错：**

- **本地缓存**：每个 Agent 缓存最近一次 identity 查询结果，TTL 5 分钟。缓存未过期时不发起 Intent 查询（减少系统 Agent 负载）
- **系统 Agent 不可用**：如果系统 Agent 未响应（超时 2 秒），Agent 降级使用本地缓存（即使过期），或使用 manifest.toml 中的 identity_deps 默认值
- **系统 Agent 崩溃恢复**：Gateway 检测到系统 Agent 进程退出后自动重启（auto_start 特权），重启期间其他 Agent 的 identity 查询走缓存降级
- **冷启动预加载**：新 Agent 首次启动时，Gateway 在拉起进程前先向系统 Agent 查询 identity_deps 并注入启动参数（详见 [07-system-agent.md](./07-system-agent.md) §3），不依赖运行时缓存

**路径 3：云端 Memory Sync 同步**

云端作为知识同步层，Agent 写入的知识可按规则广播给订阅了该信息的其他 Agent，各 Agent 的本地 Grafeo 各自更新。

### 7.1 隐私与同步

记忆节点增加 PrivacyLevel 标记（Public / Personal / Sensitive），LLM 在即时提取时自动判断：

- **Public**：可跨 Agent 共享——"用户名叫张三"、"用户说中文"
- **Personal**：Agent 私有——"用户偏好简洁回复"
- **Sensitive**：Agent 私有，打包分享时剥离——"用户提到健康问题"

**PrivacyLevel 的实际作用域是打包边界控制**：当用户将 Agent 分享给他人时，Personal/Sensitive 节点被自动剥离，只保留 Agent 自身的能力（SkillIteration、ProceduralNode、AutobiographicalNode 中关于 Agent 自身的部分）。PrivacyLevel 不用于网络同步过滤或跨 Agent 隔离——LLM 上下文中的数据无技术访问控制手段，靠 prompt 约定约束。

云端同步采用 Zone-Based 分区，全部 Zone 明文同步（平台托管，与主流互联网平台一致）：

| Zone | 内容 | 同步策略 |
|------|------|---------|
| Identity | 姓名、语言、时区 | CloudSync（通过系统 Agent 查询） |
| Preferences | 回复风格、默认模型 | CloudSync（各 Agent 独立偏好） |
| Knowledge | 事实、关系 | CloudSync |
| Work | 工作相关的个人记忆 | CloudSync |

打包分享时，PrivacyLevel 过滤独立于 Zone：无论哪个 Zone 的节点，只要标记为 Personal/Sensitive，打包时一律剥离。

## 8. 语义记忆节点类型汇总

| 节点类型 | 用途 | 遗忘 | 隐私 | 详见 |
|---------|------|------|------|------|
| KnowledgeNode | 事实、偏好、关系 | 乘法衰减 + 节点类型规则（§5.2） | Public/Personal/Sensitive | 本文档 §3.1 |
| ProceduralNode | 行为模式、操作规则 | 乘法衰减 + 节点类型规则（§5.2） | Personal | 本文档 §3.2 |
| AutobiographicalNode | 自我认知、能力边界 | 不遗忘 | Personal | 本文档 §3.3 |
| SkillDraft | 草稿 Skill（调试阶段） | 开发期保留 | Personal | [13-skill-system.md](./13-skill-system.md) §3.2 |
| SkillIteration | 迭代版本快照 | 开发期保留 | Personal | [13-skill-system.md](./13-skill-system.md) §3.3 |
| SkillExecution | 执行记录（含模型信息） | 开发期保留 | Personal | [13-skill-system.md](./13-skill-system.md) §3.4 |
| SkillExperience | 已发布 Skill 的运行经验 | 专用衰减 | Personal | [13-skill-system.md](./13-skill-system.md) §3.5 |

## 9. 分阶段实现路线

### Phase 1：记忆基础（对应 Roadmap Phase 2）

**目标：** 让 Agent 能记住用户，能检索，能遗忘，但不做深度抽象和跨时段的巩固。

**交付内容：**

三层架构落地（瞬态层 / 经历层 / 沉淀层），Grafeo 支持 episodes + memory_nodes + memory_edges 三张表。经历层存储对话原始记录（episode），沉淀层存储精炼知识（KnowledgeNode / AutobiographicalNode）。

即时提取通过 Tool Call 机制实现：`memory_store` 工具加入 Agent 内置工具列表，System Prompt 加入提取指引，LLM 在生成回复时自主判断是否调用。Fact 自动按 (subject, predicate) 语义去重。

基础遗忘机制落地：乘法衰减模型 decay_score = importance × activity_signal，activity_signal = clamp(recency_boost + access_boost, 0.05, 1.0)。Dormant 态区分：Fact/Relation 永不清除，Preference/ProceduralNode Dormant 超过 90 天可 Purge。dormant_since 字段计时，reactivate_node 时归零。

关联扩散检索落地：hybrid_search 基础上加 graph_expand，经历层 episode 通过 KnowledgeNode.source_episode 反向查询建立跨层关联，边权重 = min(0.8, confidence_avg × recency_factor)，扩散阈值 0.2（可配置），硬限制 2 跳。

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

双向联动的设计参考了 PlugMem 的"处方式知识"框架，但做了一定简化：

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

**核心组件三：云端同步（Zone-Based，平台托管）**

云端同步不是"全量备份"，而是按 Zone 的差异化同步。全部 Zone 明文同步，平台托管（与主流互联网平台一致，详见 00-prd.md ADR-002）：

| Zone | 内容 | 同步策略 |
|------|------|---------|
| identity Zone | AutobiographicalNode Identity / Capability | CloudSync |
| preferences Zone | Preference / Relationship | CloudSync |
| knowledge Zone | Fact / Relation | CloudSync |
| work Zone | SkillExperience / ProceduralNode | CloudSync |

打包分享时，PrivacyLevel 过滤独立于 Cloud Sync：无论哪个 Zone 的节点，标记为 Personal/Sensitive 的一律剥离，只保留 Agent 自身能力（详见 00-prd.md ADR-002 打包边界语义）。

冲突解决策略：Phase 6 阶段先实现单向同步（云端 → Agent），Agent 本地变更记录在本地不上报，避免双向写入导致的冲突。后续 Phase 7+ 若需双向同步，采用"最后写入者胜 + 用户确认"机制。

**Phase 3 验收标准：**

- 离线巩固能发现即时提取无法捕获的隐式关联（需人工评估验证）
- Hypothesis 机制能主动提出并验证假设（追踪假设生命周期）
- 分页换出/换入在极端上下文长度下（> 500 轮）正常工作
- 云端同步按 Zone 正确同步，PrivacyLevel 控制打包分享时的过滤

---

### 三阶段总结对照

| 维度 | Phase 1 | Phase 2 | Phase 3 |
|------|---------|---------|---------|
| **核心问题** | 能不能记住 | 能不能学习 | 能不能整合 |
| **即时提取** | Tool Call（显式信息） | 扩展 Tool Call + failure_case 联动 | 离线 LLM 回放（隐式关联 + Artifact 摘要增强） |
| **遗忘机制** | 乘法衰减 + Dormant | ProceduralNode 90 天 Purge | HypothesisNode 过期清除 |
| **程序记忆** | ProceduralNode 结构体 | Skill ↔ ProceduralNode 双向联动 | 跨 Skill 通用模式提炼 |
| **自我认知** | Manifest 派生 Autobiographical | 自我评估更新 Limitation | HypothesisNode 主动假设验证 |
| **云端同步** | 无 | 无 | Zone-Based 差异化同步 |
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
| `Retrieve` | 步骤 ② 构建上下文 | 用户消息 + 当前上下文摘要 | `Vec<MemoryContext>` | 记忆检索：hybrid_search + graph_expand |
| `Inject` | 步骤 ② 构建上下文（Retrieve 之后） | `Vec<MemoryContext>` + Token 预算 | 格式化字符串 | 决定如何将记忆注入 LLM 上下文 |
| `Record` | 步骤 ⑥ 结果追加历史（异步） | 本轮 user_msg + assistant_reply + tool_results | `()` | 记录本轮交互到经历层 |

#### 10.2.2 后台阶段（异步，由 MemoryManager 独立调度）

| 阶段 | 触发条件 | 输入 | 输出 | 说明 |
|------|---------|------|------|------|
| `Consolidate` | 即时提取（每轮 Record 后检查）+ 离线巩固（空闲/阈值） | 未巩固 episode 列表 | 新建/更新的 KnowledgeNode | 巩固管道（§4） |
| `Decay` | 定时扫描（每小时，可配置） | 当前时间 | `DecayScanResult` | 遗忘衰减（§5） |
| `Compact` | 存储维护（启动时 + 空闲时） | 存储统计 | 清理数量 | 索引优化、旧 episode 清理、VACUUM |

### 10.3 MemoryStore trait（存储后端抽象）

Runtime 和上层记忆逻辑不直接依赖任何具体存储引擎（rusqlite / Sled / LMDB / 远程服务），而是通过 `MemoryStore` trait 交互。这确保存储方案可替换——Phase 1 用 GrafeoStore（rusqlite），未来可无缝切换。

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
}

/// 记忆存储后端的标准化接口
/// 实现者可以是 rusqlite / Sled / LMDB / 远程服务 / 内存 mock
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

    /// 关闭存储（释放资源、刷写 WAL）
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
}
```

**设计要点：**

- `MemoryQuery` 替代裸 `&str`：未来扩展只需加字段（如 `temperature` 控制检索创造性、`recency_boost` 控制时间偏好），不破坏已有实现
- `MemoryContext` 带 `priority` 和 `estimated_tokens`：Inject 阶段可以直接按优先级和 token 预算裁剪，无需 Runtime 了解记忆内部结构
- `DecayConfig` 参数化：不同 Agent 可以有不同的遗忘策略（"学习型 Agent"遗忘慢，"工具型 Agent"遗忘快），通过 manifest 配置注入
- `health_check` + `stats`：为 Desktop App 的记忆管理面板和运维监控提供标准数据接口
- trait 中不包含任何 rusqlite 类型，实现完全隔离

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
│  MemoryStore trait                                           │
│  └─ GrafeoStore (rusqlite，Phase 1 唯一实现)                │
│     └─ episodic/ + semantic/ + forgetting/ + retrieval/      │
│  └─ (未来) RemoteMemoryStore (云端分布式存储)                │
│  └─ (未来) InMemoryStore (测试用 mock)                       │
└──────────────────────────────────────────────────────────────┘
```

**MemoryManager 核心方法：**

```rust
pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    middlewares: Vec<Box<dyn MemoryMiddleware>>,
    config: MemoryConfig,
    event_bus: MemoryEventBus,
}

impl MemoryManager {
    /// 检索记忆（Retrieve 阶段）
    /// 内部调用 store.hybrid_search + store.graph_expand
    /// 将 SearchResult 转换为 MemoryContext（带优先级和 token 预估）
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
| Phase 1 | `MemoryStore` trait 定义 + `MemoryQuery` / `SearchResult` / `MemoryContext` 等数据类型 + `GrafeoStore` 实现 | trait 定义先行，GrafeoStore 作为唯一实现，Runtime 改为通过 trait 调用 |
| Phase 1 | `MemoryManager` 基础结构 + 生命周期阶段触发 | Manager 直接转发给 GrafeoStore，不引入中间件机制 |
| Phase 1 | `DecayConfig` 参数化 | 遗忘参数从硬编码改为可配置，通过 manifest 注入 |
| Phase 2 | `MemoryMiddleware` trait + 注册机制 + 内置中间件（emotion_tag / audit_log） | 打开中间件扩展能力 |
| Phase 3 | `InMemoryStore` mock 实现（用于测试） | 替代当前的集成测试方案 |
| Phase 3 | `RemoteMemoryStore` 探索（云端分布式存储） | 如果跨设备实时同步需求明确 |

### 10.7 设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 存储抽象方式 | trait + impl | Rust 生态标准做法，零成本抽象（monomorphization），编译期检查 |
| 中间件模型 | 洋葱模型（Tower 风格） | 业界验证过的模式，支持前/后处理，错误传播自然 |
| 配置注入 | manifest.toml + 系统默认 | manifest 声明 Agent 级定制，系统默认兜底 |
| 事件通知 | Event Bus（发布/订阅） | Desktop App 和日志系统可订阅 MemoryEvent，不影响核心管线性能 |
| Retrieve/Inject 拆分为两个阶段 | 是 | Retrieve 关注"查什么"，Inject 关注"怎么放"，职责分离有利于未来 Inject 策略的独立演化（如 RAG 结果和本地记忆的混合排序） |
