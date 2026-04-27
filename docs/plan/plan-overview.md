# 实现路线图与使用场景

> 版本：v3.2 | 更新日期：2026-04-27

---

## 1. 实现路线图

### Phase 1: 基础框架 + LLM 交互（MVP）

- 定义 manifest v1 规范，实现 ZIP 解析。
- 实现 .agent 包签名机制：密钥对生成（`rollball-keygen`）、签名（`rollball-sign`）、验证（`rollball-verify`）。
- 实现 Agent Runtime 核心：加载 .agent 包、组装 prompt、LLM 主循环（含 Preemptive Trim、Streaming + tool_calls 状态机）、内置工具（memory, http, shell）。
- History Manager：对话历史 FIFO 裁剪、动态 N 计算、Tool Result 折叠（规则引擎版 HistoryPruner，保留最近 4 轮完整 tool result）。
- Loop Controller：循环检测三种模式（Exact Repeat / Ping-Pong / No Progress）+ 三级渐进响应（Warning → Block → Break）。
- Tool Call 单轮去重（HashSet）。
- Budget Manager：本地预算预检 + 上下文 token 预估校验。
- Rate Limit 分层：区分可重试限流 / 不可重试余额不足，解析 retry_after，不计入 LLM 重试次数。
- Context Exceeded Reactive Recovery：Tool Result 折叠 → Emergency History Trim 渐进式恢复。
- Gateway 基础功能：安装（含签名验证）、卸载、启动/停止进程、Socket 通信。
- Key Vault 基础功能：加密存储、一次性分发。
- Gateway CLI 二进制：命令行管理 Agent（install/uninstall/start/stop/list）。
- 实现一个示例 Agent（天气查询，能调 LLM + 用工具）。
- 本地目录隔离（不使用命名空间，仅 `--work-dir`）。

### Phase 2: Memory 分层 + 系统 Agent

- Agent Runtime 内嵌 Grafeo：三层五类仿生分层（瞬态/经历/沉淀）、情景记忆写入/检索、语义记忆图操作、乘法衰减遗忘机制（decay_score = importance × activity_signal）、Dormant/Purge 差异化策略。
- 即时提取 Tool Call 机制：memory_store 工具加入内置列表，LLM 自主判断是否调用，支持 Fact/Preference/Relation/Procedural/Autobiographical 五种类型，带 importance 和 privacy 参数。
- 关联扩散检索：hybrid_search（HNSW + BM25）+ graph_expand（1-3 跳，含 PageRank + topology_boost + 社区检测），经历层↔沉淀层通过 source_episode 反向查询建立跨层关联。
- AutobiographicalNode：从 manifest.toml 自动派生 Identity/Capability，注入 System Prompt 头部（上限 200 token），History 超过 10 条自动摘要压缩。
- Episode 内容分类压缩：信息性内容原样存储，工件性内容（代码/文件/命令输出）压缩为摘要 + ArtifactRef 引用，零 LLM 调用，纯 Runtime 确定性逻辑。
- PrivacyLevel（Public/Personal/Sensitive）按 Zone 强制执行。
- 系统 Agent 实现：身份管理 ContentProvider、默认交互入口、身份提报接收与 LLM 判断、observe 通知机制。
- 冷启动身份注入：Gateway 启动 Agent 前向系统 Agent 查询 identity_deps 并注入。
- Embedding 生成：集成 ONNX Runtime，本地向量生成（all-MiniLM-L6-v2）。
- 离线工作：所有 Memory 操作本地完成，不依赖网络。

> **2026-04-27 更新**：Phase 2 正在开发中。S1（架构改进）全部完成，S3.1~S3.3（System Agent 基础）完成，S3.4 工具骨架完成但 IPC 链路未贯通。S2（Grafeo 核心）进行中，S2.0（grafeo-engine 依赖集成）已完成。详细实施计划见 `docs/plan/plan-p2.md`。

### Phase 3: 权限与工具安全

- 实现权限声明和用户授权对话框（CLI 或简单 GUI）。
- 运行时权限请求机制（含 PermissionRequest/Response IPC 协议）。
- WASM 工具沙箱（Wasmtime 集成 + WIT 组件模型）。
- Prompt Guard 和 approval 机制（Approval Gate：高风险工具需用户确认，Gateway → Desktop App 确认流程）。
- Shell 命令风险分级（Low/Medium/High/Blocked）+ FileProvenance 文件来源追踪 + 审计日志。
- 离线巩固（三元组提取 + 经验泛化 + 记忆质量评估，LongMemEval 5 维评估框架）。
- Intent 权限校验：发送方持有 intent:send 权限 + capability 匹配校验。

> **2026-04-25 更新（ADR-007）**：进程级沙箱（bubblewrap / AppContainer / Seatbelt）延后至 Phase 7。Phase 3 聚焦应用层安全防线。
> 
> **2026-04-27 更新**：Phase 3 已完成（94% 一致性，179/210 测试通过）。遗留 6 项 P2 问题延后至 Phase 5（API 错误格式统一、HTTP 请求限流、API 版本控制等）。详细实施计划见 `docs/plan/plan-p3.md`。

### Phase 4: HTTP API 与通信完善

- Gateway HTTP API（Axum）：Agent 管理 REST API、Chat API（REST + WebSocket 流式）、Config API、Vault API、Permission API、Cron API。
- Permission IPC 协议：Runtime → Gateway 权限请求链路、Permission HTTP API、权限缓存 + 分类策略。
- Cron 触发器集成：CronStore 持久化、Cron HTTP API、Cron 调度器与 Gateway 事件循环集成、Agent 未运行时自动拉起。
- RAG 工具集成：manifest 配置驱动 opt-in、标准查询协议（企业 RAG 自适配）、双通道检索（自动 + 显式触发）、Vault 管理 RAG 认证凭据、RAG 工具权限双校验（rag:query + network 白名单）。

> **2026-04-27 更新**：
> - 原 Roadmap Phase 4 定义的 Intent/Budget/Rate Limiter 已在 Phase 2 提前交付（见 plan-p2.md §2.4 S4）。
> - Phase 4 实际聚焦 HTTP API 基础设施、Permission IPC、Cron 集成和 RAG 工具。
> - Intent 跨 Agent 消息转发 + Capability Registry：✅ Phase 2 已交付
> - Budget Tracker（用量上报 + 超限信号）：✅ Phase 2 已交付
> - Rate Limiter（速率令牌分配）：✅ Phase 2 已交付
> - Cron 触发器模块已有（16.6KB），需与 Gateway 事件循环集成
> - 详细实施计划见 `docs/plan/plan-p4.md`。

### Phase 5: Desktop App + 开发框架

Desktop App 和开发调试能力在同一阶段交付，因为它们共享 Debug Protocol 基础设施。

> **2026-04-27 更新**：Phase 5 实施计划已编制完成，详见 `docs/plan/plan-p5.md`。
> - S1：Desktop App 骨架 + 系统托盘（Tauri v2 + React 19）
> - S2：Debug Protocol 实现（WebSocket + JSON-RPC 2.0 + 执行控制）
> - S3：开发框架高级能力（Skill 热加载 + Provider 切换 + 录制回放）
> - S4：发布工具链（Agent 克隆 + 发布检查 + 打包签名）
> - S5：Phase 4 遗留技术债务清偿 + 全链路集成验证
> - 预计 16~18 周，40 项任务，217 项测试

#### Phase 4 遗留技术债务

> 以下 P2 问题从 Phase 4 Code Review 和 P2 Grafeo/Memory Review 中延后，纳入 Phase 5 S5 处理。
> 来源：`docs/review/05-p4-code-review.md`（P2-3~P2-14）、`docs/review/09-p2-grafeo-memory-code-review.md`（P2-3g~P2-4g）。

| 编号 | 模块 | 内容 | 来源 | Phase 5 任务 |
|------|------|------|------|-------------|
| P2-3 | Gateway | API 错误响应格式统一 | P4 S1 review | S5.1 |
| P2-4 | Gateway | HTTP API 请求限流（rate limiting） | P4 S1 review | S5.2 |
| P2-5 | Gateway | API 版本控制 `/api/v1/` | P4 S1 review | S5.3 |
| P2-8 | Gateway | PermissionGrant 序列化压缩 | P4 S2 review | S5.5 |
| P2-9 | Gateway | PermissionPolicy 运行时可配置 | P4 S2 review | S5.6 |
| P2-10 | Runtime | PermissionChecker 监控指标（缓存命中率、请求延迟） | P4 S2 review | S5.7 |
| P2-11 | Gateway | Cron 时区支持 | P4 S3 review | S5.8 |
| P2-12 | Gateway | Cron 重试机制 | P4 S3 review | S5.8 |
| P2-13 | Gateway | Cron 批量操作 | P4 S3 review | S5.8 |
| P2-14 | Gateway | Cron 最大执行次数（max_runs / expires_at） | P4 S3 review | S5.8 |
| P2-3g | Grafeo | 冲突检测 Negation/Evolution keywords 可配置（当前硬编码中英双语） | P2 Grafeo review | S5.9 |
| P2-4g | Grafeo | PageRank O(V²) 增量优化或采样策略 | P2 Grafeo review | S5.9 |
| P2-5g | Memory | MEM-04 遗忘机制实现方式冲突（PRD: 按需计算 vs 代码: 后台扫描） | PRD §1.4 vs plan-p2.md S2.7 | S5.4 |

> **待讨论议题（P2-5g，S5.4）**：遗忘机制实现方式——PRD MEM-04 描述为"按需计算模型（查询时实时计算），无后台定时扫描"，但 plan-p2.md S2.7 和代码实现为后台扫描。需要讨论：更新 PRD 描述（承认后台扫描）或回改代码实现为按需计算。

#### 5.1: Desktop App 基础（用户模式）

- Tauri v2 Desktop App 骨架：Rust backend + React 19 frontend + Vite + Tailwind CSS + shadcn/ui + Zustand。
- Gateway HTTP Client：Rust 后端封装 Gateway HTTP API，Tauri Commands 暴露给前端。
- 四栏布局：导航栏 + Agent 列表 + 聊天面板 + 执行结果区。
- 系统托盘：Gateway 状态指示、快捷操作、关闭隐藏不退出。
- Agent 管理界面：安装（文件选择+拖放）、卸载、启停、Agent 列表。
- 对话界面：消息收发、流式输出（WebSocket）、工具调用展示（可展开/折叠）。
- 设置页面：Gateway 连接配置、Provider 管理、Vault API Key 管理、外观设置。
- 首次启动引导：5 步流程（欢迎 → Gateway 连接 → API Key → 身份信息 → 安装 Agent）。

#### 5.2: 开发框架（Debug Protocol + 开发者模式）

- Agent Runtime DevMode：`--dev-mode` 启动参数、Debug Protocol Server（WebSocket `ws://127.0.0.1:19877`）。
- Debug Protocol 实现：JSON-RPC 2.0、执行控制（Pause/Step/Resume/Stop）、状态查询、断点系统（4 类条件）。
- 消息快照与回滚机制：`ConversationSnapshot`（轻量，记录 message_count）、`debugger.rollback(target_index)`。
- Gateway DevMode 集成：Agent 标记 `dev: true` 时追加 `--dev-mode` 启动参数。
- Desktop App 开发者模式：调试面板、单步执行详情、断点管理、消息编辑/回滚。

#### 5.3: 开发框架高级

- Skill 热加载：`debugger.reloadSkills` 命令、Desktop Skill 编辑器。
- Provider 动态切换：`debugger.switchProvider` 命令、Desktop Provider 切换器。
- Grafeo Skill 经验层：`SkillDraft` / `SkillIteration` / `SkillExecution` / `SkillExperience` 节点类型。
- 录制回放引擎：JSONL 格式录制、自动/手动回放、回放中编辑/切换 Provider。
- Desktop 录制回放 UI：录制控制栏、回放进度条、步骤详情。

#### 5.4: 发布工具链

- Agent 克隆 API（Gateway `POST /api/agents/:id/clone`）：骨架克隆 + 完整克隆。
- 发布检查与清理 API（`POST /api/agents/:id/publish/prepare`）：manifest/SKILL.md 校验、清理。
- 打包与签名 API（`POST /api/agents/:id/publish/build`）：ZIP 打包 + rollball-sign 签名。
- 分发 API：本地安装 + 导出文件。
- Desktop 发布向导 + 克隆对话框 + 创建向导。

### Phase 6: 云端与生态

- Memory Sync Service（云端增量同步、冲突解决）。
- 远程仓库支持（添加仓库、更新、自动下载）。
- Agent 商店原型。
- 付费 Agent 许可证验证。

> **备忘：企业级记忆（Enterprise Memory）**
>
> Phase 6 需重点讨论的话题：企业级 RAG 升级为企业级记忆，与本地 Grafeo MemoryStore 接口兼容。
> 核心思路：Grafeo 本身已支持 RAG 的向量检索能力（hybrid_search），企业级 RAG 通道可演化为 RemoteMemoryStore（实现 MemoryStore trait），
> 使企业数据不再局限于向量检索，获得完整图检索能力（hybrid_search + graph_expand）。
> 技术演进：Phase 4 的 RagClient → RemoteMemoryStore；`rag_client: Option<Arc<RagClient>>` → `enterprise_store: Option<Arc<dyn MemoryStore>>`；
> 现有 RAG 服务可降级适配（仅实现 MemoryStore 子集）。
> 当前思考尚不完善，需在 Phase 6 启动前深入讨论：协议细节、隐私边界、延迟优化、部署模型等。

### Phase 7: 跨平台适配 + 进程级沙箱

- 进程级沙箱：Linux（bubblewrap + seccomp-bpf）、Windows（AppContainer + Job Objects）、macOS（Seatbelt）。
- Windows 适配：Named Pipe 传输、Windows 路径规范。
- macOS 适配：App Sandbox 隔离、macOS 路径规范。
- 移动端适配（Android/iOS）：SingleProcess 运行模式、Local TCP 传输、wasmi WASM 引擎、移动端路径规范。
- 注意：.agent 包格式和 Gateway Service API 合同无需修改，适配仅在实现层。

### Phase 8: 企业知识平台

> **备忘：企业知识生命周期平台**
>
> 基于 Phase 6 的企业级记忆基础设施，构建企业知识的本地训练调优→云端发布平台。
> 核心价值：RollBall 不仅是 Agent 运行时，更是企业知识的本地化训练调优发布平台。
> 功能维度：
> 1. 本地训练调优：Agent 开发者/训练师在工作区中对话式写入事实、关系、程序知识，离线巩固提炼语义沉淀
> 2. 知识打包发布：选定知识子图（按 Zone/NodeType/PrivacyLevel 过滤），个人/敏感数据自动剥离，质量门禁（importance > 0.7, 无冲突标记），导出为 .grafeo 快照
> 3. 企业部署：企业管理员审批导入企业 Grafeo 服务 → 全企业 Agent 可检索，版本管理 + 回滚
> 4. 持续更新：增量发布、知识衰期管理、使用反馈闭环
>
> 依赖关系：Phase 6 RemoteMemoryStore 基础设施 + Phase 5 Desktop App 发布向导 + Phase 3 质量评估框架和巩固管道
> 当前为初步构想，需在 Phase 6 实施过程中进一步细化。
