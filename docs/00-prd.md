# Rollball 平台需求定义

> 版本：v1.0 | 更新日期：2026-04-15
>
> 本文档从设计文档（01~14）和设计对话中反向提取需求，作为平台功能的权威需求来源。设计文档描述"怎么做"，本文档描述"做什么"和"为什么"。

---

## 0. 项目定位

Rollball 是一个"Agent as APP"平台。核心隐喻借鉴 Android：Agent 如 APK 是声明式包，Agent Runtime 如 ART 是统一执行引擎，Gateway 如 AMS 管理生命周期。

**目标用户**：个人用户和小团队，需要在本地运行多个 AI Agent 并让它们协作完成任务的场景。

**核心价值主张**：

- 声明式 Agent 包——零代码、可分发、可签名验证
- 进程级隔离——每个 Agent 独立运行、互不干扰
- 仿生记忆——Agent 拥有分层记忆系统，能记住、能遗忘、能学习
- 跨 Agent 协作——通过 Intent 机制实现 Agent 间通信
- 跨平台——同一 .agent 包在桌面和移动端运行

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
| MEM-08 | 隐私分级：PrivacyLevel（Public/Personal/Sensitive），LLM 自动判断 | P1 | 隐私保护 |
| MEM-09 | 离线巩固：空闲时触发专用 LLM 调用，将经历层提炼到沉淀层 | P2 | 记忆质量提升 |
| MEM-10 | 云端同步：Zone-Based 策略，identity/preferences 强制本地，knowledge/enterprise 允许同步 | P2 | 多设备同步 |
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
| GTW-09 | Socket API：供 Agent Runtime IPC 使用 | P0 | 数据面接口 |
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
| MNT-01 | Rust workspace 模块化（7 crate 结构） | rollball-core / runtime / gateway / grafeo / vault / sign / cli |
| MNT-02 | 配置驱动——Agent 行为由 manifest + prompt 定义，无需改代码 | 声明式架构保障 |
| MNT-03 | ADR 记录所有重大技术决策 | 每个设计文档内含决策记录表 |

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

PKG-01~05, FMT-01~03, RUN-01~03, RUN-07~09, MEM-01~03, TOL-01, TOL-05, SKL-01~02, GTW-01~03, GTW-05, GTW-09, SYS-01~02, SYS-06, COM-01~02, COM-05, SEC-01~02, SEC-04~05, SEC-07, PLT-01

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
