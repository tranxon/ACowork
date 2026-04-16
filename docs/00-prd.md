# Rollball 平台需求定义

> 版本：v1.2 | 更新日期：2026-04-16
>
> 本文档从设计文档（01~14）和设计对话中反向提取需求，作为平台功能的权威需求来源。设计文档描述"怎么做"，本文档描述"做什么"和"为什么"。

---

## 0. 项目定位

Rollball 是一个"Agent as APP"平台。核心隐喻借鉴 Android：Agent 如 APK 是声明式包，Agent Runtime 如 ART 是统一执行引擎，Gateway 如 AMS 管理生命周期。

**目标用户**：个人用户和小团队，以及企业用户。核心差异在于企业用户可以在 Agent 中接入自己部署的 RAG 知识库，实现企业级知识增强。

**核心价值主张**：

- 声明式 Agent 包——零代码、可分发、可签名验证
- 进程级隔离——每个 Agent 独立运行、互不干扰
- 仿生记忆——Agent 拥有分层记忆系统，能记住、能遗忘、能学习
- 跨 Agent 协作——通过 Intent 机制实现 Agent 间通信
- 隐私安全分享——Agent 可自由分享给他人，Personal/Sensitive 数据自动剥离，只带走"Agent 能力"而非"用户记忆"
- 跨平台——同一 .agent 包在桌面和移动端运行
- 企业级扩展——通过标准 RAG 接口接入企业知识库，无需平台托管数据

---

## 1. 功能需求

### 1.1 Agent 打包与分发

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| PKG-01 | Agent 以 `.agent` 压缩包分发，内含配置、Prompt、Skill、工具声明，**不含可执行文件** | P0 | 声明式打包是平台核心前提 |
| PKG-02 | .agent 包必须签名，Gateway 安装时强制验证完整性和来源 | P0 | 安全底线 |
| PKG-03 | 支持两类签名身份：Developer（自签名）和 Platform（平台签发） | P0 | Phase 1 最小签名模型 |
| PKG-04 | 系统 Agent 必须由 Platform Key 签名 | P0 | 防止伪装系统 Agent |
| PKG-05 | Agent 升级时签名证书指纹必须与已安装版本一致 | P0 | 防止恶意包覆盖 |
| PKG-06 | 提供 `rollball-keygen` / `rollball-sign` / `rollball-verify` 签名工具链 | P1 | 开发者自签流程 |
| PKG-07 | 提供 Debug 签名模式（本地开发自动签名） | P1 | 降低开发门槛 |
| PKG-08 | 支持远程仓库（多 HTTP 源、定期检查更新） | P2 | 生态分发 |
| PKG-09 | 支持双密钥模型（Upload Key + Distribution Key） | P3 | 商店分发阶段 |
| PKG-10 | 支持密钥轮换（Proof-of-Rotation） | P3 | 长期运维 |
| PKG-11 | 支持证书吊销列表（CRL） | P3 | 安全事件响应 |

### 1.2 Agent 包格式

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| FMT-01 | manifest.toml 用纯 TOML 格式（机器配置文件） | P0 | Rust 生态友好 |
| FMT-02 | SKILL.md 用 YAML frontmatter + Markdown body，兼容 agentskills.io 标准 | P0 | 复用社区技能生态 |
| FMT-03 | manifest 中声明权限、LLM 配置、工具、能力、触发器 | P0 | 声明式包的核心 |
| FMT-04 | manifest 中声明平台兼容性（target_platforms），支持 required/optional 模式 | P1 | 跨平台降级 |
| FMT-05 | manifest 中声明 identity_deps，启动时由 Gateway 注入身份信息 | P1 | 跨 Agent 身份一致 |
| FMT-06 | 包大小上限 50 MB | P1 | 防止安装问题 |
| FMT-07 | skills/references/ 仅允许不可执行的数据文件 | P1 | 安全约束 |

### 1.3 Agent Runtime（统一执行引擎）

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| RUN-01 | Agent Runtime 是平台唯一二进制，加载 .agent 包并执行 | P0 | 统一引擎，零自定义代码 |
| RUN-02 | Agent Runtime 直连 LLM API，不经 Gateway 代理 | P0 | 低延迟、流式、自治 |
| RUN-03 | Agent Runtime 自主执行工具调用，自主校验权限 | P0 | Agent 自治原则 |
| RUN-04 | 支持多 LLM Provider 配置和路由策略（cost/quality/latency priority） | P1 | 成本和场景灵活性 |
| RUN-05 | 支持预算管理（Token 限额、费用限额、超限动作） | P1 | 防超支 |
| RUN-06 | 支持 LLM fallback（主 Provider 失败时自动切换备用） | P1 | 可靠性 |
| RUN-07 | 支持流式输出 + tool_calls 并发处理（检测到 tool_calls 立即中断 streaming） | P0 | 用户体验和正确性 |
| RUN-08 | 循环检测（Exact Repeat / Ping-Pong / No Progress）+ 三级渐进响应 | P0 | 防死循环 |
| RUN-09 | 上下文溢出恢复（Preemptive Trim + Reactive Recovery） | P0 | 大上下文场景必需 |
| RUN-10 | Tool Call 单轮去重（防止单次响应内重复调用同一工具） | P1 | 常见 LLM 行为修正 |
| RUN-11 | Tool Result 折叠（保留最近 4 轮完整结果，更早的折叠为摘要） | P1 | 上下文空间优化 |
| RUN-12 | Rate Limit 分层处理（可重试限流 vs 不可重试余额不足） | P1 | API 调用健壮性 |
| RUN-13 | 高风险工具执行前的用户确认（Approval Gate） | P1 | 安全保障 |
| RUN-14 | 支持 API Key 轮换（多 Key 集中管理，Vault 分发） | P2 | 企业场景 |

### 1.4 Memory 系统

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| MEM-01 | 每个 Agent 拥有完全独立的私有 Grafeo，不存在公共数据库 | P0 | 数据隔离底线 |
| MEM-02 | 三层五类仿生分层：瞬态层（工作记忆）→ 经历层（情景记忆）→ 沉淀层（语义+程序+自传体） | P0 | 仿生记忆架构 |
| MEM-03 | 即时提取：LLM 通过 memory_store 工具自主判断是否存储，零额外 API 成本 | P0 | 记忆积累的核心机制 |
| MEM-04 | 遗忘机制：乘法衰减模型（decay_score = importance × activity_signal），半衰期约 23 天 | P1 | 防止记忆膨胀 |
| MEM-05 | 关联扩散检索：1-2 跳图扩展，支持跨层（经历层↔沉淀层） | P1 | 检索质量 |
| MEM-06 | 自传体记忆：六维度自我认知，从 manifest 自动派生，注入 System Prompt | P1 | Agent 自我认知 |
| MEM-07 | 程序记忆：跨 Skill 的通用行为模式 | P2 | 自学习能力 |
| MEM-08 | 隐私分级：PrivacyLevel（Public/Personal/Sensitive），LLM 自动判断。控制的是"数据打包分享时是否包含该节点"——Personal/Sensitive 节点在 Agent 分享导出时剥离，Public 节点保留。LLM 上下文中的数据无法从技术上访问控制，只能通过 prompt 约定约束 | P1 | 打包边界隐私保护 |
| MEM-09 | 离线巩固：空闲时触发专用 LLM 调用，将经历层提炼到沉淀层 | P2 | 记忆质量提升 |
| MEM-10 | Grafeo Zone-Based Cloud Sync：identity / preferences / knowledge / work 四区均可同步（平台明文托管，多设备体验一致）。enterprise Zone 改名为 work Zone（个人工作记忆，与企业 RAG 无关）。隐私分级与同步策略解耦——PrivacyLevel 控制打包边界，Zone 控制同步分区 | P1 | 多设备同步 |
| MEM-11 | 内容分类压缩：工件性内容（代码/文件/命令输出）仅存摘要 + ArtifactRef 引用 | P1 | 防 Grafeo 膨胀 |
| MEM-12 | Embedding 本地生成（ONNX Runtime），离线可用 | P1 | 向量检索前提 |

### 1.5 工具系统

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| TOL-01 | 内置 13 个工具：memory×2 / network×2 / web×2 / shell / file×4 / intent×1 / search×1 | P0 | Agent 感知和操作世界的基本能力 |
| TOL-02 | 支持 WASM 自定义工具（Wasmtime 沙箱执行） | P1 | 可扩展性 |
| TOL-03 | WASM 工具资源限制（max_memory_mb, max_execution_time_ms, Fuel metering） | P1 | 安全隔离 |
| TOL-04 | API Key 对 WASM 工具不可见（secrecy::SecretString） | P1 | 安全底线 |
| TOL-05 | 工具权限校验：所有工具调用需匹配 manifest 声明的权限 | P0 | 安全底线 |
| TOL-06 | 平台支持矩阵：shell 仅桌面端，文件操作移动端受限 | P1 | 跨平台适配 |
| TOL-07 | Skill 级联降级：依赖的 tool 不可用时 skill 自动降级 | P2 | 优雅降级 |
| TOL-08 | WASM 运行时选型：Wasmtime（桌面端），Wasmi（移动端/iOS 禁 JIT） | P1 | 跨平台 |
| TOL-09 | WASI Preview 2（目录级沙箱 + 能力安全） | P1 | 安全沙箱 |
| TOL-10 | 内置工具范围仅限平台基础设施级，SaaS 集成由独立 Agent 提供 | P1 | 架构边界 |

### 1.6 Skill 系统

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| SKL-01 | 双层模型：SKILL.md（静态定义层）+ Grafeo（动态经验层） | P0 | Skill 架构基础 |
| SKL-02 | SKILL.md 兼容 agentskills.io 开放标准 | P0 | 复用社区技能 |
| SKL-03 | 调试流程：Agent 在 Grafeo 中创建草稿 → Debug 模式试运行 → 用户确认 → 提交到 SKILL.md | P1 | Skill 开发闭环 |
| SKL-04 | 自学习闭环：发布后积累经验，经验达到阈值时提示用户更新 SKILL.md | P2 | 持续改进 |
| SKL-05 | 模型兼容性：SkillExecution 记录模型信息，SkillExperience 按模型聚合，运行时自动注入适配指令 | P2 | 跨模型可移植 |

### 1.7 Gateway

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| GTW-01 | Gateway 纯基础设施定位，零业务逻辑，不维护任何业务数据库 | P0 | 架构原则 |
| GTW-02 | 包管理：安装（含签名验证）、卸载、升级、版本管理 | P0 | Agent 生命周期起点 |
| GTW-03 | 生命周期管理：启动/停止/重启 Agent 进程，健康检查 | P0 | Agent 运行保障 |
| GTW-04 | Intent 路由：跨 Agent 消息转发 + Capability Registry | P1 | Agent 协作基础 |
| GTW-05 | Key Vault：加密存储 API Key，一次性分发，不通过环境变量 | P0 | 安全底线 |
| GTW-06 | 预算追踪：接收 Agent 上报，超限信号 | P1 | 成本控制 |
| GTW-07 | 速率限制：令牌分配，跨 Agent 共享资源协调 | P1 | API 调用公平性 |
| GTW-08 | HTTP API（Axum，端口 19876）：供 Desktop App / CLI 使用 | P1 | 管理面接口 |
| GTW-10 | 定时触发器（cron 解析） | P2 | 定时任务 |
| GTW-11 | Gateway CLI 二进制：命令行管理 Agent | P1 | 无 GUI 场景 |
| GTW-12 | 冷启动身份注入：启动 Agent 前向系统 Agent 查询 identity_deps 并注入 | P1 | 身份一致性 |

### 1.8 系统 Agent

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| SYS-01 | 系统 Agent 随 Gateway 分发，不可卸载，自动启动 | P0 | 系统级服务 |
| SYS-02 | 承担身份管理 ContentProvider 角色，其他 Agent 通过 Intent 查询 | P0 | 跨 Agent 身份一致 |
| SYS-03 | 接收身份提报，用 LLM 做二次判断（替代用户确认弹窗） | P1 | 自动化决策 |
| SYS-04 | 默认交互入口——无第三方 Agent 时的唯一界面 | P1 | 首次使用体验 |
| SYS-05 | observe 通知机制——身份变更时通知订阅 Agent | P2 | 实时一致性 |
| SYS-06 | 必须 Platform 签名，享有系统特权 | P0 | 安全底线 |

### 1.9 通信协议

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| COM-01 | Gateway Service API 合同层平台无关，实现层各平台自选 | P0 | 跨平台兼容 |
| COM-02 | Socket API（Unix Socket / Named Pipe）给 Agent Runtime 用 | P0 | IPC 通道 |
| COM-03 | HTTP API（REST + WebSocket）给 Desktop App / CLI 用 | P1 | 管理面通道 |
| COM-04 | Debug Protocol（JSON-RPC 2.0 over WebSocket）给 DevMode 用 | P2 | 调试通道 |
| COM-05 | 敏感数据（API Key、身份信息）走 Socket，不暴露在进程命令行 | P0 | 安全底线 |

### 1.10 安全

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| SEC-01 | 进程级隔离——每个 Agent 独立进程，一个崩溃不影响其他 | P0 | 稳定性底线 |
| SEC-02 | 文件系统隔离——Agent 只能写入自己的工作区和授权目录 | P0 | 数据安全 |
| SEC-03 | 网络隔离——默认禁止网络，仅按 manifest 授权白名单 | P1 | 最小权限 |
| SEC-04 | 权限声明——manifest 必须声明所有权限，未声明不可用 | P0 | 最小权限原则 |
| SEC-05 | WASM 工具沙箱——无法访问宿主内存、文件系统、网络 | P0 | 自定义代码隔离 |
| SEC-06 | 沙箱强化——Linux 使用 bubblewrap + seccomp-bpf | P2 | 深度隔离 |
| SEC-07 | API Key 不通过环境变量分发，通过 Socket 一次性传输 | P0 | 防 ps/procfs 泄露 |

### 1.11 Desktop App

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| DSK-01 | Desktop App 与 Gateway 独立进程，通过 Gateway HTTP API 通信 | P1 | 架构一致性 |
| DSK-02 | 对话界面：消息收发、流式输出、工具调用展示 | P1 | 核心交互 |
| DSK-03 | Agent 管理界面：安装、卸载、启停、列表 | P1 | Agent 生命周期管理 |
| DSK-04 | 设置页面：Gateway 连接配置、Provider 管理、Vault API Key 管理（脱敏预览） | P1 | 配置管理 |
| DSK-05 | 系统托盘：关闭窗口隐藏到托盘不退出，显示 Gateway 连接状态 | P2 | 桌面体验 |
| DSK-06 | 开发者模式：通过 Developer Mode toggle 切换，提供调试面板、断点、录制回放 | P2 | 开发调试 |
| DSK-07 | Skill 编辑器、Manifest 编辑器、发布向导 | P3 | 开发工具链 |
| DSK-08 | 首次启动引导流程 | P2 | 用户引导 |

### 1.12 跨平台

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| PLT-01 | .agent 包格式和 Gateway Service API 合同跨平台统一 | P0 | 平台无关性 |
| PLT-02 | 桌面端（Windows/Linux/macOS）完整支持 | P1 | Phase 1 目标 |
| PLT-03 | 移动端（Android/iOS）降级运行（SingleProcess 模式、Local TCP、wasmi） | P3 | 远期目标 |
| PLT-04 | 各平台传输层实现不同（Unix Socket / Named Pipe / Local TCP），但不影响包兼容性 | P1 | 实现层差异 |
| PLT-05 | 移动端能力降级：shell 不可用、文件操作路径收窄、Skill 级联降级 | P2 | 优雅降级 |

### 1.13 企业 RAG 集成

> 企业级 Agent 不是 Rollball 平台内置的能力，而是 Agent 开发的一种范式——Agent 开发者通过标准 RAG 接口对接企业知识库，用户感知到的只是一个普通的 Agent。

**设计原则**：

- **纯对接，不托管**：Rollball 不运营 RAG 服务，知识属于企业自己
- **隔离优先**：本地 Grafeo（个人记忆）和企业 RAG（集体知识）是两条独立的检索通道，互不干扰
- **通用接入**：企业可对接任意符合标准接口的 RAG 系统（Milvus / Qdrant / Weaviate / Elasticsearch + Embedding / 商业 RAG SaaS）

#### 1.13.1 双通道检索模型

| 通道 | 存储 | 内容 | 所有权 |
|------|------|------|--------|
| 本地记忆通道 | Grafeo（图数据库） | 个人偏好、交互历史、自传体、经历、语义沉淀 | 用户本地 |
| 企业知识通道 | 企业自建 RAG | 产品文档、业务流程、行业知识、内部规范 | 企业所有 |

Agent 检索记忆时并行执行两条通道，检索结果按来源标记后拼接送入 LLM 上下文。LLM 能够同时引用个人经验和企业知识，但两者的隐私边界和所有权清晰：个人的不上去，企业的不下来。

#### 1.13.2 RAG 工具定义

| 编号 | 需求 | 优先级 | 说明 |
|------|------|--------|------|
| RAG-01 | manifest 中声明 `[[tools]]` 类型为 `rag`，提供企业 RAG 服务地址（URL）和认证信息 | P1 | 企业 RAG 接入标准方式 |
| RAG-02 | RAG 工具支持标准查询接口（向量检索 + 可选混合关键词检索 + 元数据过滤） | P1 | 兼容主流 RAG 系统 |
| RAG-03 | RAG 工具支持企业认证（API Key / OAuth 2.0 / Bearer Token） | P1 | 企业安全要求 |
| RAG-04 | RAG 认证信息走 Vault 管理，不明文暴露在 manifest 或进程环境 | P1 | 安全底线 |
| RAG-05 | RAG 查询结果标注来源（source_url / chunk_id），供 LLM 和用户追溯 | P1 | 可解释性 |
| RAG-06 | manifest 中声明 RAG 知识库的查询范围（namespace / collection / index），运行时按此约束查询 | P2 | 多租户隔离 |
| RAG-07 | RAG 工具离线降级：RAG 服务不可达时跳过该通道，不阻塞 Agent 运行 | P1 | 离线鲁棒性 |

#### 1.13.3 架构边界

企业 RAG 集成严格限定为检索通道，不向上整合进 Memory 系统抽象层。原因：Grafeo 是图数据库（支持关联扩散、遗忘衰减），RAG 是向量检索（批量查询、无状态），两者查询范式和存储模型完全不同。强行统一抽象会引入不必要的复杂度，且企业 RAG 的多租户隔离、数据写入权限与 Grafeo 的模型不兼容。

企业 RAG 集成属于"企业级 Agent 开发范式"，不要求所有 Agent 都支持 RAG，也不出现在 Rollball 核心平台的功能承诺中。

---

## 2. 非功能需求

### 2.1 性能

| 编号 | 需求 | 目标 |
|------|------|------|
| PERF-01 | Agent Runtime 空闲内存占用 | 目标与 ZeroClaw 相当（~5-10 MB） |
| PERF-02 | Agent 启动时间（从 spawn 到 LLM 首次请求发出） | < 2 秒 |
| PERF-03 | Gateway 内存占用 | < 50 MB（不含 Agent 进程） |
| PERF-04 | Memory 检索延迟 | < 100 ms（单次 hybrid_search） |
| PERF-05 | WASM 工具调用开销 | < 5 ms（Host-WASM 通信） |

### 2.2 可靠性

| 编号 | 需求 | 目标 |
|------|------|------|
| REL-01 | Agent 进程崩溃不影响其他 Agent | 进程级隔离保障 |
| REL-02 | Agent 崩溃后状态不丢失 | 私有 Grafeo 持久化 |
| REL-03 | LLM Provider 失败自动 fallback | 多 Provider + 重试机制 |
| REL-04 | 对话写入不丢失 | WAL + 写队列 + 超时降级重试 |

### 2.3 安全

| 编号 | 需求 | 目标 |
|------|------|------|
| SECR-01 | .agent 包未签名或签名无效，拒绝安装 | 安装时强制校验 |
| SECR-02 | API Key 不泄露到进程参数或环境变量 | Socket 一次性分发 |
| SECR-03 | WASM 工具无法越权访问 | Wasmtime + WASI Preview 2 |
| SECR-04 | Agent 间数据默认不可见 | 私有 Grafeo + 进程隔离 |

### 2.4 可维护性

| 编号 | 需求 | 目标 |
|------|------|------|
| MNT-01 | Rust workspace 模块化（7 crate 结构） | rollball-core / memory / runtime / gateway / grafeo / vault / sign |
| MNT-02 | 配置驱动——Agent 行为由 manifest + prompt 定义，无需改代码 | 声明式架构保障 |
| MNT-03 | ADR 记录所有重大技术决策 | 每个设计文档内含决策记录表 |

### 2.5 可扩展性

| 编号 | 需求 | 目标 |
|------|------|------|
| EXT-01 | Runtime 依赖 trait/接口，不依赖具体实现 | RXT-01 依赖倒置准则 |
| EXT-02 | 核心模块通过标准化生命周期阶段接入 Runtime | RXT-02 生命周期钩子准则 |
| EXT-03 | 所有可调参数通过 manifest + 系统默认注入 | RXT-03 配置外置准则 |
| EXT-04 | 功能管线支持中间件插入 | RXT-04 中间件管线准则 |
| EXT-05 | 存储后端可替换（MemoryStore trait） | RXT-05 存储可替换准则 |
| EXT-06 | 关键操作发布事件供外部订阅 | RXT-06 事件可观测准则 |

---

## 3. 约束与假设

### 3.1 约束

- Agent 包不含可执行文件——所有逻辑由 LLM + Tool 实现，WASM 是唯一自定义代码入口
- Gateway 不代理业务逻辑——LLM 调用、工具执行、记忆读写均在 Agent 进程内
- 系统 Agent 用 LLM 推理替代用户确认弹窗——避免复杂的用户仲裁流程
- Phase 1 仅桌面端（Linux 优先）——移动端适配延后

### 3.2 假设

- 用户本地有可用的 LLM API（OpenAI / Claude / Ollama 等），平台不内置 LLM
- 用户信任本地 Agent Runtime 二进制（平台信任链起点）
- 网络非必需——除 LLM 调用外，所有功能离线可用

---

## 4. 需求优先级与阶段映射

| 优先级 | 含义 | 阶段 |
|--------|------|------|
| P0 | 平台核心——没有就不叫 Rollball | Phase 1 |
| P1 | 平台必需——缺少会显著影响可用性 | Phase 1~2 |
| P2 | 平台增强——提升体验和安全 | Phase 3~4 |
| P3 | 生态扩展——面向未来的能力 | Phase 5~7 |

**P0 需求汇总**（Phase 1 必须交付）：

PKG-01~05, FMT-01~03, RUN-01~03, RUN-07~09, MEM-01~03, TOL-01, TOL-05, SKL-01~02, GTW-01~03, GTW-05, SYS-01~02, SYS-06, COM-01~02, COM-05, SEC-01~02, SEC-04~05, SEC-07, PLT-01

---

## 5. 核心用户场景

### 5.1 个人用户日常场景

用户安装天气 Agent 和日历 Agent。每天早上 7 点，天气 Agent 自动获取天气，通过 Intent 让日历 Agent 创建提醒（如"带伞"）。天气 Agent 从私有 Grafeo 记住用户城市，无需每次询问。

### 5.2 开发者创建 Agent 场景

开发者编写 manifest.toml + system prompt + SKILL.md，使用 `rollball-sign` 签名，通过 Gateway CLI 安装到本地。在 Desktop App 开发者模式下单步调试、试运行 Skill，确认无误后发布到仓库。

### 5.3 跨 Agent 协作场景

用户对天气 Agent 说"我搬到上海了"，天气 Agent 向系统 Agent 提报身份变更，系统 Agent 用 LLM 确认语义后更新用户城市，通知所有订阅了 city 变更的 Agent。

### 5.4 移动端降级场景

用户在手机上使用同一套 .agent 包。shell 工具不可用、文件操作受限，但 Agent 仍可通过 HTTP 工具和 Memory 工具正常工作，Skill 自动降级跳过不可用工具依赖的步骤。

### 5.5 企业 Agent 场景

某企业开发"销售助手 Agent"，manifest 声明 `[[tools]] type = "rag"`，指向企业内部的 Qdrant RAG 服务（含产品知识库、销售话术库、合规文档）。用户安装后在 Desktop App 与 Agent 对话，Agent 同时查询本地 Grafeo（记住该用户的偏好、历史提问）和企业 RAG（检索产品参数、竞品对比、合规要点），拼接后给出回答。RAG 服务由企业自己运维，Rollball 平台不接触任何企业数据，用户量增长对 Rollball 云端压力为零。

### 5.6 Agent 打包分享场景

用户将自己调教好的"私人助手 Agent"分享给朋友。打包时，PrivacyLevel 过滤自动剥离 Personal/Sensitive 节点（朋友无法看到原用户的偏好、历史对话、私密信息）。打包后的 Agent 保留了：Agent 自学的 SkillIteration 和调教经验（Agent 能力）、ProceduralNode（通用行为模式）、行事风格和擅长领域（AutobiographicalNode 中关于 Agent 自身的部分）。朋友安装后，Agent 在新的 Grafeo 上运行，记忆为空，从头开始积累。

---

## 6. 术语表

| 术语 | 定义 |
|------|------|
| Agent | Rollball 平台上的独立 AI 应用，以 .agent 包分发 |
| .agent 包 | 声明式压缩包，含配置、Prompt、Skill、工具声明，不含可执行文件 |
| Agent Runtime | 平台唯一二进制，加载并执行 .agent 包 |
| Gateway | 常驻系统进程，管理 Agent 生命周期和跨 Agent 协调 |
| Grafeo | Agent 私有的图数据库，存储分层记忆 |
| Intent | 跨 Agent 消息，类似 Android Intent |
| Skill | Agent 行为模式的扩展，分静态定义层（SKILL.md）和动态经验层（Grafeo） |
| 系统 Agent | com.rollball.system，平台内置 Agent，提供身份管理等系统级服务 |
| Vault | Gateway 内的加密 API Key 存储服务 |
| ContentProvider | 系统 Agent 提供的只读数据服务，其他 Agent 通过 Intent 查询 |
| identity_deps | Agent 声明的身份依赖字段，启动时由 Gateway 注入 |
| Platform Key | 平台签发密钥，用于系统 Agent 签名 |
| 企业 RAG | 企业自建的 RAG 知识库服务，Agent 通过标准 rag 工具接入，不经 Rollball 云端中转 |
| 双通道检索 | Agent 同时查询本地 Grafeo 和企业 RAG 两条通道的检索模式 |
| work Zone | Grafeo 沉淀层中与个人工作相关的记忆分区（原 enterprise Zone），与 Rollball 企业 RAG 无关 |
| PrivacyLevel | 节点级隐私标记（Public/Personal/Sensitive），控制 Agent 打包分享时是否包含该节点，与云端同步策略解耦 |

---

## 7. 架构决策记录（ADR）

### ADR-001：企业 RAG 定位

**状态**：已接受

**上下文**：
从个人和小团队扩展到企业用户场景时，需要回答两个问题：（1）企业 RAG 与本地 Grafeo 是同一层还是不同通道？（2）Rollball 是否要自己托管 RAG 服务？

**决策**：

1. 采用双通道检索模型（Mode A）：本地 Grafeo 和企业 RAG 是两条独立的检索通道，结果拼接后送入 LLM 上下文。两者不整合进统一的记忆抽象层。
2. Rollball 不托管 RAG 服务（Option 1）：企业自建或采购 RAG 系统，Rollball 在 manifest 中声明 RAG 接入点（URL + 认证），运行时作为标准工具调用。Rollball 云端零状态。
3. 企业 RAG 定位为"企业级 Agent 开发范式"，不属于 Rollball 核心平台功能承诺。

**权衡**：

- 得：架构侵入最小，隐私边界清晰，企业自主可控，云端压力仅来自接入管理（轻量）
- 失：RAG 查询结果与本地记忆检索质量依赖 Agent 自己拼接，无统一排序；企业需自建/采购 RAG，有一定接入门槛

**前提**：若未来出现大量中小企业无法自建 RAG 的场景，可叠加"托管 RAG 服务"作为增值选项，与纯对接模式并存。

### ADR-002：PrivacyLevel 边界与 Cloud Sync 模型

**状态**：已接受

**上下文**：
讨论 Grafeo 云端同步时出现两个问题：（1）Sensitive 数据不上云导致跨设备不一致，是否影响功能？（2）"enterprise Zone"与"企业 RAG"命名重叠，是否造成混淆？

**决策**：

1. Grafeo Cloud Sync 全部 Zone 明文同步，平台托管。遵循主流互联网平台实践（Google/iCloud/Notion 均明文存储），同设备体验一致，风险与主流平台同量级。
2. PrivacyLevel 作用域限定为**打包边界控制**：Personal/Sensitive 节点在 Agent 分享导出时剥离，Public 节点保留。网络边界和跨 Agent 边界暂不考虑。
3. "enterprise Zone"改名为"work Zone"：原指个人工作相关记忆，与企业 RAG 完全无关。改名消除歧义。
4. PrivacyLevel 与 Cloud Sync 策略完全解耦：PrivacyLevel 控制打包时"带不带"，Cloud Sync 控制"同步到哪里"。LLM 上下文中的数据无技术访问控制手段，靠 prompt 约定约束。

**打包边界语义**（PrivacyLevel 的实际意义）：

| 节点类型 | Personal/Sensitive 剥离后剩余 |
|---------|-----------------------------|
| Personal 节点（用户偏好、历史） | 不打包 |
| Sensitive 节点（私密信息） | 不打包 |
| Agent 自学的 SkillIteration / SkillExperience | 保留——这是 Agent 能力的体现 |
| Agent 的 ProceduralNode | 保留——跨次学会的通用行为 |
| AutobiographicalNode（关于 Agent 自身） | 保留行事风格、擅长领域；剥离关于用户的认知 |
| ArtifactRef | 保留引用，剥离原始内容（工件性内容已在写入时分类压缩） |

**权衡**：

- 得：边界清晰、实现简单、多设备体验完整、打包分享隐私安全
- 失：平台明文存储用户全部记忆，依赖平台信任和隐私承诺（与主流平台同等风险）

### ADR-003：记忆生命周期架构与 Runtime 可扩展性准则

**状态**：已接受

**上下文**：

记忆系统是 Rollball 的核心差异化能力（推理靠 LLM、操作靠 Tools，只有记忆是自主掌控的），必然经历大量迭代。当时存在三个紧耦合风险：
1. 记忆触发点硬编码在 Runtime 主循环，每次记忆迭代都要改 Runtime
2. Grafeo 与 rusqlite 紧耦合，无法替换存储后端
3. 记忆管线缺乏中间件机制，无法在不改源码的情况下扩展记忆行为

**决策**：

1. 引入 **Memory Lifecycle** 标准化接口，定义 6 个生命周期阶段（Retrieve/Inject/Record/Consolidate/Decay/Compact），Runtime 在固定位置触发阶段，Memory 系统通过 handler 响应
2. 定义 **MemoryStore trait** 作为存储后端抽象（独立 `rollball-memory` crate），Grafeo 作为 trait 的第一个实现（GrafeoStore），未来可替换
3. 引入 **MemoryManager** 中间层，管理生命周期阶段调度、中间件链、配置注入
4. 定义 **MemoryMiddleware trait**，支持在记忆管线中插入自定义逻辑（Phase 2），采用洋葱模型（Tower 风格）
5. 确立 **Runtime 可扩展性设计准则**（RXT-01~06），作为所有 Runtime 模块的架构约束
6. 对 Runtime 全模块执行**紧耦合审计**，高风险项立即修复，中低风险项纳入 Phase 路线图

**权衡**：

- 得：Runtime 稳定性大幅提升（记忆迭代不影响 Runtime）、存储可替换、可测试性增强（InMemoryStore mock）、扩展通过中间件而非改源码
- 失：引入一层间接调用（trait dispatch，但 Rust monomorphization 零成本）、概念增加（MemoryManager / MemoryStore / MemoryMiddleware）
- Phase 1 额外成本：低——trait 定义 + GrafeoStore 实现即可，中间件管线推迟到 Phase 2
