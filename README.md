<h1 align="center">RollBall.AI — Agent as APP</h1>

<p align="center">
  ⚡️ <strong>Easy to develop an agent for everyone.</strong><br>
  ⚡️ <strong>Easy to deliver an agent to everyone.</strong><br>
  ⚡️ <strong>Easy to deploy an agent everywhere.</strong>
</p>

---

RollBall.AI 是一个去中心化、高安全、可扩展的 AI Agent 运行时平台。核心思想借鉴 Android 模型——每个 Agent 是一个独立的声明式应用包（`.agent`），由统一的 Agent Runtime 加载执行，运行在客户端并由轻量级 Gateway 管理生命周期。

每个 Agent 都是独立的"数字人"：拥有自己的运行时进程、私有记忆、工作区和配置，对用户保有完全独立的个性化认知。

## 核心架构

| Android | RollBall | 作用 |
|---------|----------|------|
| ART | Agent Runtime | 通用执行引擎（平台唯一二进制） |
| APK | `.agent` 包 | 声明式打包（config + prompts + skills，无可执行代码） |
| APK Signature | Signing Block | 包签名，验证完整性和来源 |
| AMS | Gateway | 生命周期管理（安装、启停、预算、速率） |
| Binder IPC | Gateway Service API | 进程间通信 |
| ContentProvider | 系统 Agent | 系统级数据服务（身份、偏好） |
| PMS | Package Manager | 安装/卸载/升级 |

## 核心特性

- **标准化打包** — Agent 以 `.agent` 压缩包分发，内含 manifest.toml、Prompts、Skills、工具声明，不含可执行文件。所有包必须签名，Gateway 安装时强制验证。
- **统一执行引擎** — Agent Runtime 是平台提供的唯一二进制，负责加载 `.agent` 包并执行 LLM 交互、工具调度、记忆读写。Agent 直连 LLM API，不经 Gateway 代理。
- **进程级隔离** — 每个 Agent 由 Gateway 启动为独立进程，拥有独立工作区、私有 Grafeo 数据库、文件系统隔离、可选资源限制。
- **Agent 自治** — Agent 进程内直连 LLM、自主执行工具、自主管理权限校验。Gateway 只管必须集中化的事（Key 分发、Intent 路由、预算协调）。
- **仿生 Memory** — 每个 Agent 内嵌私有 Grafeo，三层五类仿生分层（瞬态/经历/沉淀 + 工作记忆/情景/语义/程序/自传体），系统 Agent 提供身份与偏好等系统级数据服务，Zone-Based 差异化云端同步。
- **Intent 通信** — 跨 Agent 通信通过 Gateway 的 Intent Router，支持 Capability Registry、同步/异步模式、变更订阅（observe）。
- **权限声明与授权** — Agent 在 manifest 中声明所需权限，Gateway 在启动时配置沙箱，Agent 运行时自主校验。
- **跨平台支持** — `.agent` 包格式和 Gateway Service API 合同跨平台统一，各平台运行时机制按特性适配。
- **全链路开发框架** — Desktop App（Tauri）提供对话调试、Skill 热加载、Provider 动态切换、录制回放、Agent 克隆与发布向导。

## 总体架构

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
                           │ (Unix Socket / Named Pipe / Local TCP)
       ┌───────────────────┼───────────────────┐
       │                   │                   │
       ▼                   ▼                   ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│ Agent Runtime   │ │ Agent Runtime   │ │ Agent Runtime   │
│ (统一二进制)     │ │ (统一二进制)     │ │ (统一二进制)     │
│                 │ │                 │ │                 │
│ ┌─────────────┐│ │ ┌─────────────┐│ │ ┌─────────────┐│
│ │ 系统 Agent  ││ │ │ 天气 Agent  ││ │ │ 日历 Agent  ││
│ │(com.rollball ││ │ │ (config +   ││ │ │ (config +   ││
│ │  .system)   ││ │ │  prompt +   ││ │ │  prompt +   ││
│ │             ││ │ │  skills)    ││ │ │  skills)    ││
│ └─────────────┘│ │ └─────────────┘│ │ └─────────────┘│
│                 │ │                 │ │                 │
│ ✅ 私有 Grafeo │ │ ✅ 私有 Grafeo │ │ ✅ 私有 Grafeo │
│ ✅ LLM 直连    │ │ ✅ LLM 直连    │ │ ✅ LLM 直连    │
│ ✅ Tools 执行  │ │ ✅ Tools 执行  │ │ ✅ Tools 执行  │
│ ⭐ 系统特权   │ │                 │ │                 │
└─────────────────┘ └─────────────────┘ └─────────────────┘
```

## `.agent` 包结构

```
<agent_id>.agent
├── manifest.toml          # 元数据 + LLM 配置 + 权限 + 工具声明
├── prompts/               # System prompt 模板
├── config/                # 默认配置文件
├── data/                  # 初始数据
├── skills/                # Skill 定义（YAML frontmatter + Markdown）
├── tools/                 # 自定义工具（WASM，可选）
└── resources/             # 图标、本地化等
```

包必须签名（APK Signature Scheme v2 思路），Phase 1 支持两类签名身份：Developer（自签名）和 Platform（系统 Agent 专用）。

## Memory 仿生分层

| 层级 | 内容 | 生命周期 | 说明 |
|------|------|---------|------|
| 瞬态层 | 工作记忆 | 单次会话 | 对话历史、LLM 上下文窗口 |
| 经历层 | 情景记忆 | 持久化 | Episode 节点、关联扩散检索、内容分类压缩 |
| 沉淀层 | 语义记忆 + 程序记忆 + 自传体记忆 | 长期 | 知识图谱、跨 Skill 通用行为、六维度自我认知 |

每个 Agent 拥有完全独立的私有 Grafeo，不存在公共数据库。跨 Agent 数据共享通过 Intent 查询和系统 Agent 服务实现。云端同步采用 Zone-Based 策略：identity/preferences Zone 强制本地，knowledge/enterprise Zone 允许云端。

## 设计文档

| 文档 | 内容 |
|------|------|
| [01-overview.md](./docs/01-overview.md) | 平台总纲：背景目标、核心类比、架构总览、与现有方案对比 |
| [02-agent-package.md](./docs/02-agent-package.md) | `.agent` 包格式、签名机制、manifest.toml 架构 |
| [03-agent-runtime.md](./docs/03-agent-runtime.md) | Agent Runtime 主循环、上下文构建、循环检测、Approval Gate |
| [04-gateway.md](./docs/04-gateway.md) | Gateway 组件：PackageManager、Lifecycle、IntentRouter、Vault、Budget、Rate、沙箱 |
| [05-memory.md](./docs/05-memory.md) | Memory 仿生分层：三层五类、Grafeo 知识图谱、遗忘机制、关联扩散检索 |
| [06-communication.md](./docs/06-communication.md) | Gateway Service API + Intent 通信协议 + Capability Registry |
| [07-system-agent.md](./docs/07-system-agent.md) | 系统 Agent：ContentProvider、冷启动身份注入、LLM 二次判断 |
| [08-security.md](./docs/08-security.md) | 安全设计：进程隔离、文件隔离、签名验证、网络隔离、WASM 沙箱 |
| [09-roadmap-and-scenarios.md](./docs/09-roadmap-and-scenarios.md) | 七阶段实现路线图与使用场景示例 |
| [10-debug-protocol.md](./docs/10-debug-protocol.md) | Debug Protocol：DevMode、执行控制、断点系统、消息快照与回滚 |
| [11-module-design.md](./docs/11-module-design.md) | 模块设计索引（rollball-core / runtime / gateway / grafeo / vault / sign） |
| [12-tool-system.md](./docs/12-tool-system.md) | 工具系统：Built-in Tools、WASM 沙箱（Wasmtime）、Gateway Tools、平台兼容性 |
| [13-skill-system.md](./docs/13-skill-system.md) | 技能系统：SKILL.md 格式、Grafeo 经验层、自学习闭环、模型兼容性 |
| [14-desktop-app.md](./docs/14-desktop-app.md) | Desktop App：Tauri v2 四栏布局、系统托盘、Gateway HTTP API、开发者模式 |

## 实现路线图

| 阶段 | 内容 |
|------|------|
| Phase 1 | 基础框架 + LLM 交互（MVP）：包解析、签名验证、Runtime 主循环、循环检测、Tool 去重、Rate 分层、Gateway 基础 |
| Phase 2 | Memory 分层 + 系统 Agent：Grafeo 仿生分层、即时提取、关联扩散、AutobiographicalNode、系统 Agent |
| Phase 3 | 权限与沙箱：文件系统隔离、WASM 沙箱（Wasmtime）、Approval Gate |
| Phase 4 | 通信与协调：Intent、Budget Tracker、Rate Limiter、Cron |
| Phase 5 | Desktop App + 开发框架：Tauri 应用、Debug Protocol、Skill 热加载、录制回放、发布向导 |
| Phase 6 | 云端与生态：Memory Sync、远程仓库、Agent 商店 |
| Phase 7 | 跨平台适配：Windows / macOS / Android / iOS |

## 仓库结构

```
agent-study/
├── docs/                    # 架构设计文档（v3.x）
├── docs/module-design/      # 模块设计子文档
├── ref-doc/                 # 参考文档（ZeroClaw 学习材料等）
├── AGENTS.md                # 项目指引
└── README.md                # 本文件
```

> 本仓库为设计研究阶段，实现尚未启动。`zeroclaw/` 目录为参考实现，非 RollBall.AI 设计的 Source of Truth。

## License

Apache-2.0
