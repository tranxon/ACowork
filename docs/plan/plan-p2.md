# Rollball Phase 2 开发计划

> 版本：v1.2 | 更新日期：2026-04-21
>
> 本计划基于 `docs/09-roadmap-and-scenarios.md` v3.1 和 `docs/review/01-code-review.md` 遗留问题，涵盖 Phase 2 所有任务的分解、排期和进度追踪。v1.1 新增 S1.6 InboundQueue 和 S1.7 工具并行执行两个任务。v1.2 新增超时/取消语义设计文档（03-agent-runtime.md §3.5）和对应的 S1.7.5、S1.6.5 验收标准（qoder review 建议）。

---

## 1. 概述

### 1.1 Phase 2 目标

交付 **Grafeo 仿生记忆系统**、**System Agent**、**Intent 路由**、**多 Provider 支持**四大核心能力，完成从 MVP 到生产级 Agent 平台的跨越。

**核心交付物**：
- Grafeo 三层五类仿生记忆完整实现（经历层/沉淀层、情景/语义/程序/自传体记忆）
- System Agent 身份管理系统（冷启动注入、observe 通知）
- Intent 跨 Agent 通信与 Capability Registry
- Budget Tracker / Rate Limiter 完整实现
- Anthropic Provider + Provider 动态路由

### 1.2 Phase 1 交付总结

| 维度 | 成果 |
|------|------|
| **任务完成** | 18 个任务全部完成（S1.1~S1.4, S3.1~S3.7, S4.1~S4.5, S5.1~S5.2） |
| **测试通过** | 262+ 单元测试通过，7-crate workspace 结构稳定 |
| **Code Review** | 二轮系统性审查结案，12 项 P0/P1/P2 问题全部修复 |
| **里程碑** | M1~M4 全部达成，MVP 天气 Agent 端到端运行 |

### 1.3 Phase 1 遗留问题（需在 Phase 2 处理）

基于 `docs/review/01-code-review.md` 的审查结果，以下问题标记为 Phase 2 处理：

| 问题编号 | 严重度 | 问题描述 | 涉及文件 | 处理阶段 |
|----------|--------|----------|----------|----------|
| #1 | P1 | RollballError 过于宽泛，Provider 错误缺少 status_code | `rollball-core/src/error.rs` | S1 |
| #2 | P0 | 签名块存储改为二进制级嵌入（APK v2 思路） | `rollball-sign/src/sign.rs` | S1 |
| #3 | P1 | AgentLoop 不持有 GatewayClient，IPC 链路断裂 | `rollball-runtime/src/agent/loop_.rs` | S1 |
| #4 | P1 | 主循环缺少流式处理集成 | `rollball-runtime/src/agent/loop_.rs` | S1 |
| #5 | P1 | BudgetGuard session_tokens 代替 daily_tokens | `rollball-runtime/src/agent/budget_guard.rs` | S4 |
| #6 | P2 | Token 估算过于粗糙（4字符/token） | `rollball-runtime/src/agent/history.rs` | S5 |
| #7 | P2 | GatewayState 无并发保护 | `rollball-gateway/src/gateway/state.rs` | S1 |
| #8 | P2 | IPC Server 同步阻塞（单连接） | `rollball-gateway/src/ipc/server.rs` | S1 |
| #9 | P2 | Budget/Usage/Rate Handler 是占位符 | `rollball-gateway/src/ipc/server.rs` | S4 |
| #10 | P2 | Grafeo 全部 unimplemented | `rollball-grafeo/src/` | S2 |

---

## 2. 阶段划分

按分层递进原则拆分为 **S1~S5** 五个阶段，每阶段内含多个任务。

### 2.1 S1：架构改进与基础设施

**目标**：处理 Phase 1 遗留问题，为 Phase 2 功能奠定架构基础。

#### S1.1 任务：AgentLoop 注入 GatewayClient（遗留 P1）

| 任务 | 验收标准 |
|------|---------|
| S1.1.1 AgentLoop 结构体新增 `ipc_client: Option<GatewayClient>` 字段 | 编译通过 |
| S1.1.2 Runtime CLI 启动流程集成 GatewayClient 初始化 | `--gateway-socket` 参数生效 |
| S1.1.3 独立运行模式兼容（CLI 模式无 Gateway）| `ipc_client` 为 None 时优雅降级 |

#### S1.2 任务：IPC Server 改异步多连接（遗留 P1）

| 任务 | 验收标准 |
|------|---------|
| S1.2.1 GatewayState 改为 `Arc<Mutex<GatewayState>>` | 并发安全 |
| S1.2.2 IPC Server 使用 `tokio::spawn` 处理多连接 | 多 Agent 并发连接测试通过 |
| S1.2.3 Session 管理支持并发读写 | 10 并发连接压力测试通过 |

#### S1.3 任务：签名块存储改为二进制级嵌入（遗留 P0）

| 任务 | 验收标准 |
|------|---------|
| S1.3.1 修改 SigningBlock 序列化格式（magic + size prefix + block + size suffix）| 与 APK v2 兼容思路 |
| S1.3.2 sign.rs 在 ZIP Central Directory 前插入 Signing Block | 签名后 ZIP 结构合法 |
| S1.3.3 verify.rs 从二进制偏移读取 Signing Block | 验证通过 |
| S1.3.4 向后兼容：支持读取旧版 ZIP entry 格式 | 迁移测试通过 |

#### S1.4 任务：结构化错误类型改进（遗留 P1）

| 任务 | 验收标准 |
|------|---------|
| S1.4.1 ProviderError 新增 `status_code: Option<u16>` 字段 | 可区分 429/401/500 |
| S1.4.2 ReliableProvider 改用 status_code 判断可重试 | 不再字符串匹配 |
| S1.4.3 错误码标准化（RateLimited/Unauthorized/ServerError）| 分类正确 |

#### S1.5 任务：流式处理集成到主循环（遗留 P1）

| 任务 | 验收标准 |
|------|---------|
| S1.5.1 AgentLoop 步骤③调用 `provider.chat_stream()` 替代 `chat()` | 流式接口接入 |
| S1.5.2 实现 streaming + tool_calls 状态机 | 检测到 tool_calls 立即中断 |
| S1.5.3 已输出 text 暂存到历史 | 上下文完整 |
| S1.5.4 Desktop App WebSocket 流式推送 | 用户可见逐字输出 |

#### S1.6 任务：AgentLoop InboundQueue（消息注入队列）

| 任务 | 验收标准 |
|------|---------|
| S1.6.1 AgentLoop 引入 `mpsc::channel<InboundMessage>`，定义三类消息 | `InboundMessage` enum 编译通过 |
| S1.6.2 主循环步骤⓪实现 drain 逻辑（非阻塞 `try_recv`）| 无消息时零延迟跳过 |
| S1.6.3 `inbound_tx` 通过 Runtime IPC 层公开，Gateway push 可注入 `SystemNotification` | Gateway capability_update 可到达正在运行的 Agent |
| S1.6.4 单元测试：并发 send + 循环 drain 不丢消息 | 100 条并发注入全部命中 |
| S1.6.5 队列容量/背压验收：满队列（64条）时 sender 阻塞不 panic，drain 后恢复正常 | 测试脚本验证满队列背压 + 恢复后仍可正常收发 |

#### S1.7 任务：工具调度改并行执行

| 任务 | 验收标准 |
|------|---------|
| S1.7.1 步骤⑤使用 `futures::future::join_all` 并行执行所有 tool_calls | 编译通过，替换串行 for 循环 |
| S1.7.2 permission check 和 approval gate 保持串行（并行执行前执行）| 权限校验逻辑不变 |
| S1.7.3 单个工具失败不短路其他工具 | join_all 收集全部结果（含错误） |
| S1.7.4 并行执行性能测试：3 个独立工具并行执行 vs 串行，耗时降低 50%+ | 有效并行 |
| S1.7.5 超时/取消语义：每个工具调用内部包 `tokio::time::timeout`；迭代整体由 `iteration_timeout_ms` 控制；超时工具返回明确错误 `ToolResult`；迭代超时时 History/日志中有可追踪信息 | 集成测试：设置短超时，验证超时后 History 中有 `"[iteration timed out after N ms, N tool(s) not completed]"` 系统消息 |

---

### 2.2 S2：Grafeo 仿生记忆

**目标**：实现完整的 Grafeo 三层五类仿生记忆系统。

#### S2.0 任务：MemoryStore Trait 重设计与标准化

| 任务 | 涉及 crate | 验收标准 |
|------|-----------|---------|
| S2.0.1 重构 MemoryStore trait — 经历层 API | rollball-core | 增加 store_episode, search_episodes, mark_consolidated 方法 |
| S2.0.2 重构 MemoryStore trait — 沉淀层 API | rollball-core | 增加 store_knowledge, store_procedural, store_autobiographical 方法 |
| S2.0.3 重构 MemoryStore trait — 遗忘/检索 API | rollball-core | 增加 hybrid_search, graph_expand, run_decay_scan, reactivate_node, purge_expired 方法 |
| S2.0.4 可选子 trait 拆分 | rollball-core | EpisodicStore, SemanticStore, ForgettingStore 等模块化 trait |
| S2.0.5 更新 GrafeoStore 框架 | rollball-grafeo | 实现新 trait 方法签名（stub 实现） |
| S2.0.6 单元测试框架 | rollball-grafeo | mock 测试覆盖各 trait 方法 |

预期测试数：8
依赖：S1 完成

#### S2.1 任务：Grafeo 数据模型实现

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.1.1 Episode 结构体完整实现 | `types.rs` | 所有字段可序列化 |
| S2.1.2 KnowledgeNode 结构体完整实现 | `types.rs` | Fact/Preference/Relation 子类型 |
| S2.1.3 ProceduralNode 结构体完整实现 | `types.rs` | trigger_condition/action_pattern |
| S2.1.4 AutobiographicalNode 结构体完整实现 | `types.rs` | Identity/Capability/Limitation/Preference/History/Relationship |
| S2.1.5 ArtifactRef 结构体实现 | `types.rs` | path/hash/description/line_range |
| S2.1.6 数据库 Schema 完整迁移 | `schema.rs` | episodes/memory_nodes/memory_edges 三张表 |

#### S2.2 任务：经历层（Episodic）实现

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.2.1 Episode 写入（自动内容分类）| `episodic/store.rs` | Informational/Artifact/Structural 分类正确；纯 Runtime 模板逻辑，无 LLM 调用 |
| S2.2.2 工件性内容压缩（摘要 + ArtifactRef）| `episodic/store.rs` | 代码/文件内容不膨胀 Grafeo；纯 Runtime 模板逻辑，无 LLM 调用 |
| S2.2.3 情景记忆检索接口 | `episodic/search.rs` | 按时间/会话/关键词过滤 |
| S2.2.4 巩固标记与清理 | `episodic/consolidate.rs` | consolidated 标记 + 过期清理 |

#### S2.3 任务：沉淀层（Semantic）实现

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.3.1 KnowledgeNode 存储（Fact 语义去重）| `semantic/knowledge.rs` | (subject, predicate) 去重 |
| S2.3.2 ProceduralNode 存储 | `semantic/procedural.rs` | trigger_condition 索引 |
| S2.3.3 AutobiographicalNode 存储 | `semantic/autobiographical.rs` | 强制 status=Active |
| S2.3.4 LPG 图操作（节点/边/属性）| `semantic/graph.rs` | CRUD 完整 |
| S2.3.5 边权重计算 | `semantic/graph.rs` | confidence_avg × recency_factor |

#### S2.4 任务：向量索引（HNSW）集成

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.4.1 EmbeddingProvider trait 实现 | `embedding/mod.rs` | 抽象接口定义 |
| S2.4.2 ONNX Runtime 本地生成（all-MiniLM-L6-v2）| `embedding/local.rs` | CPU 10-50ms 生成 |
| S2.4.3 HNSW 索引实现 | `vector/hnsw.rs` | 向量相似度检索 |
| S2.4.4 Embedding 超时降级 | `embedding/local.rs` | 200ms 超时后台补生成 |
| S2.4.5 向量 Embedding 持久化 | rollball-grafeo | episode 写入时将向量 blob 存入数据库；批量查询恢复 |
| S2.4.6 HNSW 索引从磁盘加载 | rollball-grafeo | Agent 重启后恢复索引；5000 条 episode 索引加载 < 2s |

#### S2.5 任务：全文索引（BM25）集成

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.5.1 rusqlite FTS5 配置 | `fulltext/bm25.rs` | 倒排索引建立 |
| S2.5.2 BM25 评分实现 | `fulltext/bm25.rs` | 关键词匹配排序 |
| S2.5.3 混合检索（向量 + 全文）| `retrieval/hybrid_search.rs` | RRF 融合排序 |

#### S2.6 任务：巩固管道（Consolidation Pipeline）

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.6.1 即时提取执行层 | `consolidation/instant.rs` | memory_store 工具调用处理 |
| S2.6.2 Fact 语义去重逻辑 | `consolidation/instant.rs` | subject+predicate 匹配更新 |
| S2.6.3 标记 episode consolidated | `consolidation/instant.rs` | source_episode 关联 |
| S2.6.4 离线巩固占位（Phase 3）| `consolidation/offline.rs` | 结构预留 |

#### S2.7 任务：遗忘衰减机制（Decay）

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.7.1 乘法衰减公式实现 | `forgetting/decay.rs` | decay_score = importance × activity_signal |
| S2.7.2 后台衰减扫描 | `forgetting/scan.rs` | 每小时扫描（可配置）|
| S2.7.3 Active → Dormant 状态转换 | `forgetting/scan.rs` | 参数化实现，从 DecayConfig 读取（默认 0.3） |
| S2.7.4 Dormant → Purge 清理 | `forgetting/scan.rs` | 按 DecayConfig.purge_days 清理（默认 90 天） |
| S2.7.5 节点恢复激活 | `forgetting/scan.rs` | reactivate_node 接口 |

**DecayConfig 参数**：
- dormant_threshold: f32（默认 0.3）— decay_score 低于此值进入 Dormant
- purge_days: u32（默认 90）— Dormant 节点保留天数
- decay_lambda: f32（默认 0.03）— 衰减速率
- 参数可通过 manifest.toml [memory] 表配置，支持按 Agent 定制

#### S2.8 任务：关联扩散检索（Associative Spreading）

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.8.1 Graph Expand 实现 | `retrieval/graph_expand.rs` | 1-2 跳扩展 |
| S2.8.2 跨层关联（episode ↔ memory_nodes）| `retrieval/graph_expand.rs` | source_episode 反向查询 |
| S2.8.3 扩展节点评分 | `retrieval/graph_expand.rs` | 路径权重 × 源节点分数 |
| S2.8.4 扩展限制（2跳/每跳5边/总数20）| `retrieval/graph_expand.rs` | 性能保障 |

#### S2.9 任务：MemoryManager 集成

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S2.9.1 MemoryManager 结构实现 | `rollball-runtime/src/memory/manager.rs` | 生命周期阶段协调 |
| S2.9.2 Retrieve 阶段（检索）| `manager.rs` | hybrid_search + graph_expand |
| S2.9.3 Inject 阶段（注入）| `manager.rs` | 按 token 预算裁剪 |
| S2.9.4 Record 阶段（记录）| `manager.rs` | 异步写入 episode |
| S2.9.5 Runtime 集成 MemoryManager | `rollball-runtime` | 主循环调用 |

**架构说明**：MemoryManager 是 Runtime 主循环与存储后端之间的编排层，负责：
1. Retrieve 阶段：调用 MemoryStore.hybrid_search() + graph_expand() 检索相关记忆
2. Inject 阶段：按 Token 预算裁剪检索结果，注入 LLM 上下文
3. Record 阶段：异步调用 MemoryStore.store_episode() 记录对话

MemoryManager 包含业务逻辑（Token 裁剪策略、优先级排序等），归属 rollball-runtime。
rollball-memory 保持为瘦 wrapper，仅导出 MemoryStore trait 定义。

---

### 2.3 S3：System Agent

**目标**：实现系统 Agent 身份管理和冷启动注入。

#### S3.1 任务：System Agent 包和清单

| 任务 | 验收标准 |
|------|---------|
| S3.1.1 创建 `system-agent/` 目录结构 | manifest.toml + prompts/ |
| S3.1.2 编写 system-agent manifest | `platform.system = true` 标记 |
| S3.1.3 编写 System Agent prompts | system.md + default.md |
| S3.1.4 签名并安装到 Gateway | `rollball-sign` 签名 |

#### S3.2 任务：身份信息系统

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S3.2.1 Identity Zone 定义 | `rollball-core/src/identity.rs` | Identity/Preferences/Knowledge/Work |
| S3.2.2 PrivacyLevel 枚举 | `rollball-core/src/identity.rs` | Public/Personal/Sensitive |
| S3.2.3 IdentityStore 接口 | `rollball-core/src/identity.rs` | store/query/observe |
| S3.2.4 System Agent Grafeo 存储身份 | `rollball-grafeo` | 私有存储 |

#### S3.3 任务：冷启动身份注入

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S3.3.1 Gateway 启动时拉起 System Agent | `lifecycle/manager.rs` | auto_start 特权 |
| S3.3.2 其他 Agent 启动前查询 identity_deps | `lifecycle/manager.rs` | 向 System Agent 发 Intent |
| S3.3.3 identity_delivery 消息推送 | `ipc/handlers.rs` | 握手第④步 |
| S3.3.4 Runtime 接收并注入身份 | `runtime/main.rs` | 存入内存 |

#### S3.4 任务：Identity 工具完整化

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S3.4.1 identity_query 工具实现 | `tools/builtin/identity_query.rs` | 查询系统 Agent |
| S3.4.2 identity_store 工具实现 | `tools/builtin/identity_store.rs` | 向系统 Agent 提报 |
| S3.4.3 identity_observe 工具实现 | `tools/builtin/identity_observe.rs` | 订阅变更通知 |
| S3.4.4 System Agent LLM 二次判断 | `system-agent/prompts/` | 语义有效性判断 |

---

### 2.4 S4：Intent 路由与 Budget/Rate

**目标**：实现跨 Agent 通信和用量管控。

#### S4.1 任务：Intent 跨 Agent 转发

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S4.1.1 IntentSend Handler 完整实现 | `ipc/handlers.rs` | 解析并路由 |
| S4.1.2 目标 Agent 未运行时拉起 | `lifecycle/manager.rs` | spawn + 等待就绪 |
| S4.1.2a Agent 启动就绪判断 | rollball-gateway | IPC 握手完成（identity_delivery 回复）视为就绪；复用 S3.3 冷启动逻辑 |
| S4.1.3 同步 Intent 超时处理 | `intent/router.rs` | 默认 30s 超时 |
| S4.1.4 异步 Intent 缓存结果 | `intent/router.rs` | callback 机制 |
| S4.1.5 错误处理（AgentNotFound/CapabilityNotFound）| `intent/router.rs` | 正确返回错误码 |

#### S4.2 任务：Capability Registry

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S4.2.1 Registry 数据结构 | `capability/registry.rs` | HashMap<agent:action, CapabilityDef> |
| S4.2.2 安装时注册 Capability | `package_manager/install.rs` | 解析 manifest.capabilities |
| S4.2.3 卸载时移除 Capability | `package_manager/uninstall.rs` | 清理注册表 |
| S4.2.4 CapabilityQuery Handler | `ipc/handlers.rs` | 返回完整 schema |
| S4.2.5 capability_overview 推送 | `ipc/handlers.rs` | 握手第⑤步 |
| S4.2.6 capability_update 增量推送 | `ipc/handlers.rs` | 安装/卸载/更新时广播 |

#### S4.3 任务：Budget Tracker 完整实现

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S4.3.1 Budget 持久化存储 | `budget/store.rs` | 日/月累计用量 |
| S4.3.2 UsageReport Handler 实际处理 | `ipc/handlers.rs` | 更新累计用量 |
| S4.3.3 BudgetQuery Handler 实际查询 | `ipc/handlers.rs` | 返回真实剩余额度 |
| S4.3.4 超限信号发送 | `budget/tracker.rs` | stop/fallback/warn |
| S4.3.5 Gateway HTTP API 预算查询 | `http/routes.rs` | GET /api/budget |

#### S4.4 任务：Rate Limiter 完整实现

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S4.4.1 Token Bucket 实现 | `rate_limiter/bucket.rs` | per Provider |
| S4.4.2 RateAcquire Handler 实际处理 | `ipc/handlers.rs` | 令牌分配 |
| S4.4.3 可重试限流（429 + retry_after）| `rate_limiter/bucket.rs` | 等待后重试 |
| S4.4.4 不可重试限流（余额不足）| `rate_limiter/bucket.rs` | 立即拒绝 |
| S4.4.5 多 Agent 公平调度 | `rate_limiter/bucket.rs` | 避免饥饿 |

#### S4.5 任务：UsageReport 上报链路贯通

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S4.5.1 AgentLoop 步骤⑦实际发送 UsageReport | `agent/loop_.rs` | 通过 IPC 异步发送 |
| S4.5.2 断连时缓存待上报数据 | `ipc/client.rs` | 重连后补发 |
| S4.5.3 用量统计准确性 | `tests/` | 误差 < 5% |

---

### 2.5 S5：多 Provider 与集成验证

**目标**：支持多 Provider 和端到端集成测试。

#### S5.1 任务：Anthropic Provider

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S5.1.1 Anthropic Client 实现 | `providers/anthropic.rs` | Messages API |
| S5.1.2 Anthropic 流式处理 | `providers/anthropic.rs` | streaming + tool_calls |
| S5.1.3 Anthropic 错误处理 | `providers/anthropic.rs` | 429/401/500 分类 |
| S5.1.4 Anthropic Token 计数 | `providers/anthropic.rs` | 精确计数 |

#### S5.2 任务：Provider 动态路由切换

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S5.2.1 ProviderRegistry 动态注册 | `providers/registry.rs` | 运行时切换 |
| S5.2.2 Fallback 链实现 | `providers/reliable.rs` | 主 Provider 失败时切换 |
| S5.2.3 模型能力查询 | `providers/registry.rs` | 查询支持的功能 |
| S5.2.4 manifest 配置覆盖 | `manifest.rs` | llm.routing 配置 |

#### S5.3 任务：Embedding 生成（ONNX Runtime）

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S5.3.1 ort 集成（feature-gated）| `Cargo.toml` | local-embeddings feature |
| S5.3.2 all-MiniLM-L6-v2 模型加载 | `embedding/local.rs` | 本地推理 |
| S5.3.3 Embedding 批量生成 | `embedding/local.rs` | 性能优化 |
| S5.3.4 远程 Embedding API 备用 | `embedding/remote.rs` | OpenAI text-embedding-3-small 作为降级方案；超时阈值 200ms；连续失败 2 次后自动切换 |

#### S5.4 任务：Token 计数精度改进

| 任务 | 文件 | 验收标准 |
|------|------|---------|
| S5.4.1 tiktoken-rs 集成 | `Cargo.toml` | cl100k_base |
| S5.4.2 精确 Token 计数 | `history.rs` | tiktoken 精确计数（对话+System Prompt）；误差 < 5% |
| S5.4.3 检索结果 Token 估算（混合策略）| `history.rs` | 字符数/3 近似估算；边界位置精确计数；CJK 比例通过采样校准 |
| S5.4.4 ChatMessage 全字段计数 | `history.rs` | role/name/tool_calls 计入 |

#### S5.5 任务：端到端集成测试

| 任务 | 验收标准 |
|------|---------|
| S5.5.1 多 Agent 协作测试 | 天气 Agent → 日历 Agent Intent 调用 |
| S5.5.2 记忆持久化测试 | 重启后记忆不丢失 |
| S5.5.3 身份注入测试 | 冷启动身份正确注入 |
| S5.5.4 预算管控测试 | 超限正确拦截 |
| S5.5.5 流式输出测试 | WebSocket 逐字推送 |

#### S5.6 任务：多 Agent 协作示例

| 任务 | 验收标准 |
|------|---------|
| S5.6.1 日历 Agent 示例 | 创建/查询/删除事件 |
| S5.6.2 天气+日历协作场景 | 天气查询后自动创建提醒 |
| S5.6.3 文档编写 Agent 示例 | 多步骤任务分解 |

---

## 3. 任务总表

| ID | 任务 | 模块 | 阶段 | 依赖 | 预期测试数 | 状态 |
|----|------|------|------|------|-----------|------|
| S1.1 | AgentLoop 注入 GatewayClient | rollball-runtime | S1 | - | 5 | ⬚ |
| S1.2 | IPC Server 改异步多连接 | rollball-gateway | S1 | S1.1 | 5 | ⬚ |
| S1.3 | 签名块二进制级嵌入 | rollball-sign | S1 | - | 8 | ⬚ |
| S1.4 | 结构化错误类型改进 | rollball-core | S1 | - | 6 | ⬚ |
| S1.5 | 流式处理集成到主循环 | rollball-runtime | S1 | S1.1 | 8 | ⬚ |
| S1.6 | AgentLoop InboundQueue | rollball-runtime | S1 | S1.1 | 6 | ⬚ |
| S1.7 | 工具调度改并行执行 | rollball-runtime | S1 | S1.1 | 5 | ⬚ |
| S2.0 | MemoryStore Trait 重设计与标准化 | rollball-core | S2 | S1 | 8 | ⬚ |
| S2.1 | Grafeo 数据模型实现 | rollball-grafeo | S2 | S2.0 | 15 | ⬚ |
| S2.2 | 经历层（Episodic）实现 | rollball-grafeo | S2 | S2.1 | 12 | ⬚ |
| S2.3 | 沉淀层（Semantic）实现 | rollball-grafeo | S2 | S2.1 | 15 | ⬚ |
| S2.4 | 向量索引（HNSW）集成 | rollball-grafeo | S2 | S2.1 | 12 | ⬚ |
| S2.5 | 全文索引（BM25）集成 | rollball-grafeo | S2 | S2.1 | 8 | ⬚ |
| S2.6 | 巩固管道（Consolidation）| rollball-grafeo | S2 | S2.2,S2.3 | 10 | ⬚ |
| S2.7 | 遗忘衰减机制（Decay）| rollball-grafeo | S2 | S2.3 | 8 | ⬚ |
| S2.8 | 关联扩散检索（Graph Expand）| rollball-grafeo | S2 | S2.3,S2.4,S2.5 | 10 | ⬚ |
| S2.9 | MemoryManager 集成 | rollball-runtime | S2 | S2.0~S2.8 | 12 | ⬚ |
| S3.1 | System Agent 包和清单 | examples/system-agent | S3 | - | 3 | ⬚ |
| S3.2 | 身份信息系统 | rollball-core | S3 | - | 6 | ⬚ |
| S3.3 | 冷启动身份注入 | rollball-gateway | S3 | S3.1,S3.2,S1.2 | 5 | ⬚ |
| S3.4 | Identity 工具完整化 | rollball-runtime | S3 | S3.2 | 8 | ⬚ |
| S4.1 | Intent 跨 Agent 转发 | rollball-gateway | S4 | S1.2 | 10 | ⬚ |
| S4.2 | Capability Registry | rollball-gateway | S4 | S4.1 | 8 | ⬚ |
| S4.3 | Budget Tracker 完整实现 | rollball-gateway | S4 | S1.2 | 10 | ⬚ |
| S4.4 | Rate Limiter 完整实现 | rollball-gateway | S4 | S1.2 | 8 | ⬚ |
| S4.5 | UsageReport 上报链路贯通 | rollball-runtime | S4 | S1.1,S4.3 | 6 | ⬚ |
| S5.1 | Anthropic Provider | rollball-runtime | S5 | - | 10 | ⬚ |
| S5.2 | Provider 动态路由切换 | rollball-runtime | S5 | S5.1 | 6 | ⬚ |
| S5.3 | Embedding 生成（ONNX）| rollball-grafeo | S5 | S2.4 | 8 | ⬚ |
| S5.4 | Token 计数精度改进 | rollball-runtime | S5 | - | 6 | ⬚ |
| S5.5 | 端到端集成测试 | tests/ | S5 | S1~S4 | 10 | ⬚ |
| S5.6 | 多 Agent 协作示例 | examples/ | S5 | S3,S4 | 4 | ⬚ |

**总计：33 个任务，预期 270+ 测试**

---

## 4. 里程碑

| 里程碑 | 完成标志 | 预计日期 |
|--------|---------|----------|
| **M5: Grafeo 可用** | S2 全部完成（含 S2.0 Trait 重设计）；MemoryManager 可检索/存储/遗忘 | Week 4 |
| **M6: System Agent 可用** | S3 全部完成；身份注入端到端测试通过 | Week 6 |
| **M7: Intent 路由可用** | S4 全部完成；多 Agent Intent 转发测试通过 | Week 8 |
| **M8: 多 Provider 支持** | S5.1~S5.4 完成；Anthropic + 动态路由可用 | Week 10 |
| **M9: Phase 2 MVP 交付** | S5 全部完成；天气+日历协作示例运行 | Week 12 |

---

## 5. 技术选型

### 5.1 向量索引：HNSW 库评估

| 方案 | 优点 | 缺点 | 选择 |
|------|------|------|------|
| instant-distance | 纯 Rust，无依赖 | 功能较简单 | ✅ 选用 |
| hnsw-rs | 功能完整 | 依赖较多 | 备选 |
| 自定义实现 | 可控性强 | 开发成本高 | Phase 3 考虑 |

### 5.2 全文索引：tantivy 评估

| 方案 | 优点 | 缺点 | 选择 |
|------|------|------|------|
| rusqlite FTS5 | 无额外依赖，与 Grafeo 一致 | 功能较基础 | ✅ 选用 |
| tantivy | 功能强大，BM25 标准 | 增加 ~10MB 体积 | Phase 3 考虑 |

### 5.3 Embedding：ONNX Runtime + 模型选择

| 模型 | 维度 | 大小 | 延迟（CPU）| 选择 |
|------|------|------|-----------|------|
| all-MiniLM-L6-v2 | 384 | 23MB | 10-50ms | ✅ 默认 |
| all-MiniLM-L12-v2 | 384 | 33MB | 20-80ms | 高精度备选 |
| paraphrase-multilingual | 384 | 50MB | 30-100ms | 多语言备选 |

### 5.4 Token 计数：tiktoken-rs

| 方案 | 优点 | 缺点 | 选择 |
|------|------|------|------|
| tiktoken-rs | OpenAI 官方算法，精确 | 仅支持 OpenAI 模型 | ✅ 选用 |
| tokenizers | 支持多模型 | 需要模型配置 | Anthropic 备用 |
| 字符数/4 | 零依赖 | 误差大 | Phase 1 临时 |

---

## 6. 进度追踪

### 6.1 状态定义

| 状态 | 含义 |
|------|------|
| **⬚ 待开始** | 尚未开始开发 |
| **🚧 进行中** | 正在开发 |
| **🧪 待测试** | 代码完成，等待单元测试 |
| **✅ 完成** | 代码 + 测试通过 |
| **⏸️ 阻塞** | 等待其他任务完成 |

### 6.2 当前状态（初始）

所有任务初始状态为 **⬚ 待开始**。

---

## 7. 风险与缓解

| 风险 | 严重度 | 影响 | 缓解策略 |
|------|--------|------|----------|
| **ONNX Runtime 编译问题** | 中 | S2.4, S5.3 延迟 | 准备远程 Embedding API 备用方案；feature-gate 本地嵌入 |
| **HNSW 性能不达标** | 中 | 检索延迟高 | 预留 tantivy 全文索引作为主要检索方式；向量检索降级 |
| **System Agent LLM 判断准确性** | 中 | 身份更新误判 | 设计保守策略（低 confidence 不更新）；用户可手动修正 |
| **多 Agent 并发资源竞争** | 中 | Gateway 性能下降 | 限制并发连接数；Rate Limiter 公平调度 |
| **Intent 路由超时处理** | 低 | 用户体验差 | 合理设置超时（默认 30s）；异步 Intent 支持 |
| **Phase 1 代码债务** | 低 | S1 任务延迟 | 预留 2 周缓冲时间；优先处理阻塞性遗留问题 |

---

## 8. 附录

### 8.1 Phase 2 不包含的内容

以下内容在 Phase 3+ 实现，Phase 2 刻意不做：

| 内容 | 原因 | 目标 Phase |
|------|------|-----------|
| 离线巩固（批量 LLM 回放）| Phase 2 即时提取足够 | Phase 3 |
| ProceduralNode ↔ Skill 双向联动 | 依赖 Skill 系统完善 | Phase 3 |
| 分页换出（MemGPT 风格）| 上下文长度管理复杂 | Phase 3 |
| WASM 工具沙箱 | Phase 2 内置工具足够 | Phase 3 |
| bubblewrap 文件系统隔离 | Phase 2 用目录隔离 | Phase 3 |
| Desktop App | Phase 5 | Phase 5 |
| DevMode Debug Protocol | Phase 5 | Phase 5 |
| 云端 Memory Sync | Phase 6 | Phase 6 |
| macOS / Windows 适配 | Linux 优先 | Phase 7 |

### 8.2 参考文档

- `docs/05-memory.md` — Grafeo 仿生记忆架构设计
- `docs/module-design/04-grafeo.md` — Grafeo 模块详细设计
- `docs/07-system-agent.md` — System Agent 设计
- `docs/06-communication.md` — Intent 路由和通信协议
- `docs/04-gateway.md` — Gateway 设计
- `docs/03-agent-runtime.md` — Runtime 设计
- `docs/review/01-code-review.md` — Phase 1 代码审查报告
