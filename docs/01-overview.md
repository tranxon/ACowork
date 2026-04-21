# Agent as APP：平台设计总纲

> 版本：v3.4 | 更新日期：2026-04-16

---

## 1. 背景与目标

设计一个去中心化、高安全、可扩展的 AI Agent 运行时平台。核心思想是将每个 Agent 视为一个独立的"应用包"（类似 Android APP），由统一的 Agent Runtime 进程加载执行，运行在客户端（用户电脑）并由轻量级 Gateway 管理生命周期。

**核心类比——Android 模型：**

| Android | Rollball | 作用 |
|---------|----------|------|
| zygote / ART | Agent Runtime 二进制 | 通用执行引擎，只有一个 |
| APK (DEX + resources) | .agent 包 (config + prompts + skills) | 声明式，无自定义代码 |
| APK Signature | .agent Signing Block | 包签名，验证完整性和来源 |
| ActivityManagerService | Gateway | 生命周期管理 |
| Binder IPC | Gateway Service API | 进程间通信（传输层由平台实现） |
| ContentProvider | 系统 Agent (com.rollball.system) | 系统级数据服务（身份、偏好等） |
| PackageManagerService | Package Manager | 安装/卸载 |
| AndroidManifest.xml | manifest.toml | 权限声明 |

## 2. 核心特性

- **标准化打包**：Agent 以压缩包（.agent）分发，内含配置、Prompt、Skill、工具声明，**不含可执行文件**。所有包必须经过签名，Gateway 安装时强制验证签名完整性和来源。
- **统一执行引擎**：Agent Runtime 是平台提供的唯一二进制，负责加载 .agent 包并执行 Agent 逻辑（LLM 交互、工具调度、记忆读写）。
- **进程级隔离**：每个 Agent 由 Gateway 启动为独立 Agent Runtime 进程，拥有独立工作区、私有 Grafeo 数据库、文件系统隔离、可选资源限制（cgroups/容器）。
- **Agent 自治**：Agent 进程内直连 LLM API、自主执行工具、自主管理权限校验，不依赖 Gateway 代理业务逻辑。
- **仿生 Memory 系统**：每个 Agent 内嵌私有 Grafeo，采用三层五类仿生分层（瞬态层/经历层/沉淀层），包含遗忘机制（三因子衰减）、隐私分级（PrivacyLevel）、关联扩散检索、记忆生命周期（Retrieve/Inject/Record/Consolidate/Decay/Compact）和内容分类压缩。系统 Agent 提供身份与偏好等系统级数据服务。云端同步全部 Zone 明文同步，平台托管（PrivacyLevel 仅控制打包分享时是否剥离，与同步策略解耦）。
- **权限声明与授权**：Agent 在清单中声明所需权限（网络、文件、调用其他 Agent 等），Gateway 在启动时配置沙箱，Agent 在运行时自主校验。
- **跨平台支持**：.agent 包格式和 Gateway Service API 合同跨平台统一，各平台运行时机制（进程模型、传输层、沙箱）可按平台特性适配。

## 3. 总体架构

```
┌──────────────────────────────────────────────────────────────┐
│                        Gateway（常驻）                        │
│                                                              │
│  ┌────────────┐ ┌────────────┐ ┌───────────┐ ┌───────────┐ │
│  │ Package    │ │ Lifecycle  │ │ Intent    │ │ Rate      │ │
│  │ Manager    │ │ Manager    │ │ Router    │ │ Limiter   │ │
│  └────────────┘ └────────────┘ └───────────┘ └───────────┘ │
│                                                              │
│  ┌────────────┐ ┌────────────┐             ┌───────────┐   │
│  │ Budget    │ │ Key       │             │ Config    │   │
│  │ Tracker   │ │ Vault     │             │ Manager   │   │
│  └────────────┘ └───────────┘             └───────────┘   │
│                                                              │
└──────────────────────────┬───────────────────────────────────┘
                           │ Gateway Service API
                           │ (传输层：Unix Socket / Named Pipe / Local TCP)
       ┌───────────────────┼───────────────────┐
       │                   │                   │
       ▼                   ▼                   ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│ Agent Runtime   │ │ Agent Runtime   │ │ Agent Runtime   │
│ (统一二进制)     │ │ (统一二进制)     │ │ (统一二进制)     │
│                 │ │                 │ │                 │
│ ┌─────────────┐│ │ ┌─────────────┐│ │ ┌─────────────┐│
│ │ 系统 Agent  ││ │ │ 天气 Agent  ││ │ │ 日历 Agent  ││
│ │ (com.roll-  ││ │ │ (config +   ││ │ │ (config +   ││
│ │  ball.sys-  ││ │ │  prompt +   ││ │ │  prompt +   ││
│ │  tem)       ││ │ │  skills)    ││ │ │  skills)    ││
│ └─────────────┘│ │ └─────────────┘│ │ └─────────────┘│
│                 │ │                 │ │                 │
│ ✅ 私有 Grafeo │ │ ✅ 私有 Grafeo │ │ ✅ 私有 Grafeo │
│ ✅ LLM 直连    │ │ ✅ LLM 直连    │ │ ✅ LLM 直连    │
│ ✅ Tools 执行  │ │ ✅ Tools 执行  │ │ ✅ Tools 执行  │
│ ✅ 本地预算    │ │ ✅ 本地预算    │ │ ✅ 本地预算    │
│ ⭐ 系统特权   │ │                 │ │                 │
│                 │ │                 │ │                 │
│ ↗ 用量上报     │ │ ↗ 用量上报     │ │ ↗ 用量上报     │
│ ↗ 速率申请     │ │ ↗ 速率申请     │ │ ↗ 速率申请     │
│ ↗ Intent 收发  │ │ ↗ Intent 收发  │ │ ↗ Intent 收发  │
│ ↗ 身份提报接收 │ │ ↗ 身份查询/提报│ │ ↗ 身份查询/提报│
└─────────────────┘ └─────────────────┘ └─────────────────┘

                    ┌─────────────────────────┐
                    │  Memory Sync Service     │
                    │  (云端同步/跨设备)        │
                    │  - 全部 Zone 明文同步     │
                    │  - PrivacyLevel 控制打包  │
                    └─────────────────────────┘
```

## 4. 职责划分原则

**Agent 尽可能自治，Gateway 只管必须集中化的事。**

| 职责 | 执行位置 | 原因 |
|------|---------|------|
| LLM 调用 | Agent 进程 | 直连无 RPC 开销，流式自然，Agent 自治 |
| Tool 执行 | Agent 进程 | 自治权限校验，低延迟 |
| 私有 Memory 读写 | Agent 进程（内嵌 Grafeo） | 零延迟，数据隔离 |
| API Key 存储 | Gateway Vault | 安全集中管理 |
| API Key 分发 | 启动时一次性给 Agent | Agent 直连 LLM 需要 |
| 预算追踪 | Gateway（接收上报） | 跨 Agent 统计 |
| 预算执行 | Agent（本地预检） | 低延迟，自治 |
| 预算硬限 | Gateway（超限信号） | 兜底保障 |
| 速率限制 | Gateway（令牌分配） | 共享资源协调 |
| 用户身份与偏好 | 系统 Agent（私有 Grafeo） | 系统级数据服务，LLM 推理能力，自治管理 |
| Intent 路由 | Gateway | 跨进程调度 |
| 沙箱配置 | Gateway（启动时） | 系统级权限 |

## 4.1 LLM 优先原则

**信任 LLM 超过信任规则——除非规则能解决 LLM 不能解决的问题。**

长期来看 LLM 的能力在持续提升（幻觉率下降、推理能力增强），而基于规则的方案缺乏泛化性，不是长期方案。在能力边界的权衡中，RollBall 遵循以下准则：

- **语义判断交给 LLM**：分类、评分、质量检查等涉及理解语义的任务，由 LLM 完成，不用规则模拟
- **机械性限制交给规则**：长度校验、频率限制、安全过滤等 LLM 做不了的自我约束，由 Runtime 规则执行
- **规则作为补充而非替代**：当规则相比 LLM 不能带来显著提升时，不应用规则替代 LLM，这是一种能力倒退
- **离线巩固是质量防线**：实时阶段信任 LLM 的判断，离线阶段用 LLM（而非规则）在有完整上下文的条件下复核和校准

## 5. 与现有方案对比

| 特性 | Agent as APP (Rollball) | ZeroClaw | OpenClaw | Docker + 微服务 |
|------|------------------------|----------|----------|----------------|
| 隔离级别 | 进程 + 沙箱 + WASM | 单进程内逻辑隔离 | 单进程内逻辑隔离 | 操作系统容器 |
| 执行模型 | 统一 Runtime + 声明式包 | 单体二进制 | Node.js 进程 | 容器镜像 |
| 资源开销 | 极低（空闲可杀死） | 低（常驻 ~5MB） | 中（Node.js 常驻） | 较高 |
| Memory | 三层五类仿生 Grafeo（遗忘+隐私+生命周期+同步） | SQLite/PG/Markdown | ContextEngine | 外部数据库 |
| LLM 集成 | Agent 直连 + Gateway 协调 | 内置 Provider | 内置 Provider | 各服务自连 |
| 分发模型 | 应用商店式 .agent 包 | 代码库/配置 | 代码库/配置 | 镜像仓库 |
| 跨Agent通信 | Intent + Capability Registry | 无（单 Agent） | 无（单 Agent） | HTTP/gRPC |
| 适用规模 | 个人/小团队 | 单 Agent 场景 | 单 Agent 场景 | 任意（较重） |

## 6. 未来扩展

- **Agent 商店**：建立公开仓库，用户可一键安装，含评分和评论。
- **付费 Agent**：支持许可证验证（Gateway 内集成）。
- **联邦 Memory**：多个用户之间的 Memory 共享（需授权）。
- **Agent 组合**：多个 Agent 编排为工作流（DAG 调度）。
- **多模态 Agent**：支持图像、音频、视频输入输出的 Agent。
- **移动端深度适配**：Android 多进程 Service 架构、iOS App Extension 集成、移动端 UI 交互优化。

---

> 详细设计见各子文档：
> - [02-agent-package.md](./02-agent-package.md) — Agent 打包格式、签名机制、manifest.toml
> - [03-agent-runtime.md](./03-agent-runtime.md) — Agent Runtime 内部结构与主循环
> - [04-gateway.md](./04-gateway.md) — Gateway 组件详细设计
> - [05-memory.md](./05-memory.md) — Memory 分层架构
> - [06-communication.md](./06-communication.md) — 通信协议（Gateway Service API + Intent 机制）
> - [07-system-agent.md](./07-system-agent.md) — 系统 Agent 设计
> - [08-security.md](./08-security.md) — 安全设计
> - [09-roadmap-and-scenarios.md](./09-roadmap-and-scenarios.md) — 实现路线图与使用场景
> - [10-debug-protocol.md](./10-debug-protocol.md) — 调试协议（DevMode、断点、录制回放）
> - [12-tool-system.md](./12-tool-system.md) — 工具系统（Built-in / WASM / Gateway）
> - [13-skill-system.md](./13-skill-system.md) — 技能系统（SKILL.md + Grafeo 经验层）
> - [14-desktop-app.md](./14-desktop-app.md) — 桌面应用（Tauri、布局、系统托盘）

---

## 语言规则

- **文档用中文**：所有设计文档（`docs/` 目录下的 `.md` 文件）使用中文撰写
- **代码注释用英文**：所有 Rust 源码中的注释（`//`、`//!`、`///`）必须使用英文
- **多语言文档**：等项目完全开发完毕后，再根据中文文档翻译其他多语言文档
