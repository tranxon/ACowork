# ZeroClaw 项目分析文档

## 1. 项目概述

**ZeroClaw** 是一个用 Rust 编写的轻量级自主 Agent 运行时，专为高性能、高效率、高稳定性而设计。该项目也被称为 "Rust-first autonomous agent runtime"。

### 1.1 核心特性

| 特性 | 描述 |
|------|------|
| **极简内存占用** | 运行时仅需 < 5MB RAM，比 OpenClaw 少 99% |
| **快速启动** | 在 0.8GHz 边缘硬件上启动时间 < 10ms |
| **小型二进制** | 约 8.8MB 的发布版本二进制文件 |
| **低成本部署** | 可在 10 美元硬件上运行 |
| **安全优先** | 配对机制、严格沙箱、显式白名单、工作区作用域 |
| **全可插拔** | 核心系统均为 Trait（providers、channels、tools、memory、tunnels） |

### 1.2 设计原则

项目遵循以下核心工程原则：

- **KISS** (Keep It Simple, Stupid) - 保持简单，控制流直接，减少元编程
- **YAGNI** (You Aren't Gonna Need It) - 不添加未经验证的功能
- **DRY + Rule of Three** - 提取共享工具需经过验证的稳定模式
- **SRP + ISP** - 单一职责 + 接口隔离
- **Fail Fast + Explicit Errors** - 快速失败，明确错误
- **Secure by Default** - 默认拒绝，敏感信息零日志
- **Determinism + Reproducibility** - 确定性和可重现性

---

## 2. 技术栈分析

### 2.1 编程语言与工具链

| 技术 | 版本/详情 | 用途 |
|------|-----------|------|
| **Rust** | 2021 edition, 最低 1.87 | 核心语言 |
| **Cargo** | - | 包管理器与构建工具 |
| **tokio** | 1.42 | 异步运行时 |
| **reqwest** | 0.12 | HTTP 客户端 |

### 2.2 核心依赖

#### CLI 与配置
```toml
clap = { version = "4.5", features = ["derive"] }
clap_complete = "4.5"
directories = "6.0"
toml = "1.0"
schemars = "1.2"
```

#### 异步运行时
```toml
tokio = { version = "1.42", default-features = false, 
          features = ["rt-multi-thread", "macros", "time", "net", "io-util", "sync", "process", "io-std", "fs", "signal"] }
tokio-util = "0.7"
tokio-stream = "0.1.18"
```

#### HTTP 与网络
```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "blocking", "multipart", "stream", "socks"] }
axum = "0.8"
tokio-tungstenite = "0.28"
```

#### 数据序列化
```toml
serde = "1.0"
serde_json = "1.0"
chrono = "0.4"
```

#### 存储与数据库
```toml
rusqlite = "0.37"
postgres = "0.19"  # 可选
```

#### 安全与加密
```toml
chacha20poly1305 = "0.10"  # AEAD
hmac = "0.12"
sha2 = "0.10"
ring = "0.17"
```

#### 日志与可观测性
```toml
tracing = "0.1"
tracing-subscriber = "0.3"
prometheus = "0.14"
opentelemetry = "0.31"  # 可选
```

### 2.3 可选特性 (Feature Flags)

```toml
[features]
default = []
hardware = ["nusb", "tokio-serial"]
channel-matrix = ["dep:matrix-sdk"]
channel-lark = ["dep:prost"]
memory-postgres = ["dep:postgres"]
observability-otel = ["dep:opentelemetry", ...]
peripheral-rpi = ["rppal"]
browser-native = ["dep:fantoccini"]
whatsapp-web = ["dep:wa-rs", ...]
```

### 2.4 发布配置优化

```toml
[profile.release]
opt-level = "z"      # 优化体积
lto = "fat"          # 最大跨 crate 优化
codegen-units = 1   # 串行代码生成
strip = true         # 移除调试符号
panic = "abort"      # 减少二进制体积
```

---

## 3. 架构设计

### 3.1 整体架构图

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              CLI / Gateway                              │
│                    (main.rs, gateway/, config/)                         │
└─────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                              Agent Core                                 │
│                          (agent/, runtime/)                              │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐        │
│  │    Classifier   │  │    Dispatcher   │  │    Prompt       │        │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘        │
└─────────────────────────────────────────────────────────────────────────┘
           │                    │                    │
           ▼                    ▼                    ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│     Providers    │  │     Channels     │  │      Tools       │
│  (LLM Adapters)  │  │ (Messaging I/F)  │  │   (Capabilities) │
└──────────────────┘  └──────────────────┘  └──────────────────┘
           │                    │                    │
           ▼                    ▼                    ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│  OpenAI / Claude │  │  Telegram/Discord│  │  Shell/File/Mem  │
│  Gemini / Ollama │  │  Slack/Matrix   │  │  Browser/HTTP    │
│  Bedrock / GLM   │  │  WhatsApp/Email  │  │  Cron/Delegate   │
└──────────────────┘  └──────────────────┘  └──────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                           Memory & Storage                              │
│                     (memory/, observability/)                           │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐               │
│  │ Markdown │  │  SQLite  │  │ Postgres │  │ Vector   │               │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘               │
└─────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                        Security & Sandboxing                            │
│                    (security/, approval/)                               │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐               │
│  │ Pairing  │  │ Policy   │  │ Secrets  │  │ Landlock │               │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘               │
└─────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                           Peripherals (Optional)                        │
│                        (peripherals/, hardware/)                        │
│         ┌──────────┐  ┌──────────┐  ┌──────────┐                       │
│         │   STM32  │  │   RPi    │  │ Hardware │                       │
│         │   GPIO   │  │   GPIO   │  │   Tools  │                       │
│         └──────────┘  └──────────┘  └──────────┘                       │
└─────────────────────────────────────────────────────────────────────────┘
```

### 3.2 Trait 驱动架构

ZeroClaw 的核心设计哲学是 **Trait + Factory 架构**，所有扩展点都通过 Trait 定义并通过 Factory 注册。

#### 核心 Trait 接口

| Trait | 文件位置 | 职责 |
|-------|----------|------|
| `Provider` | `src/providers/traits.rs` | LLM 模型适配 |
| `Channel` | `src/channels/traits.rs` | 消息通道接入 |
| `Tool` | `src/tools/traits.rs` | 工具能力扩展 |
| `Memory` | `src/memory/traits.rs` | 记忆存储后端 |
| `Observer` | `src/observability/traits.rs` | 可观测性收集 |
| `RuntimeAdapter` | `src/runtime/traits.rs` | 运行时适配 |
| `Peripheral` | `src/peripherals/traits.rs` | 硬件外设 |

---

## 4. 模块列表与详解

### 4.1 核心模块

#### `src/agent/` - Agent 核心编排

负责 Agent 的主循环和消息处理。

| 文件 | 功能 |
|------|------|
| `agent.rs` | Agent 构建器与主接口 |
| `loop_.rs` | Agent 主循环逻辑 |
| `classifier.rs` | 消息分类器 |
| `dispatcher.rs` | 工具调用分发器 |
| `prompt.rs` | 提示词管理 |
| `memory_loader.rs` | 记忆加载器 |

#### `src/providers/` - LLM 提供商

支持多种 LLM 提供商的适配器。

| 文件 | 支持的提供商 |
|------|--------------|
| `openai.rs` | OpenAI (GPT-4, GPT-4o) |
| `anthropic.rs` | Anthropic (Claude) |
| `gemini.rs` | Google Gemini |
| `ollama.rs` | Ollama (本地模型) |
| `bedrock.rs` | AWS Bedrock |
| `glm.rs` | 智谱 GLM |
| `copilot.rs` | GitHub Copilot |
| `openai_codex.rs` | OpenAI Codex |
| `openrouter.rs` | OpenRouter 聚合 |
| `telnyx.rs` | Telnyx |
| `compatible.rs` | OpenAI 兼容接口 |
| `reliable.rs` | 可靠包装器 (重试/降级) |
| `router.rs` | 多模型路由 |

#### `src/channels/` - 消息通道

支持多种消息平台的接入。

| 文件 | 平台 |
|------|------|
| `telegram.rs` | Telegram |
| `discord.rs` | Discord |
| `slack.rs` | Slack |
| `whatsapp.rs` / `whatsapp_web.rs` | WhatsApp |
| `matrix.rs` | Matrix (去中心化) |
| `email_channel.rs` | Email (SMTP/IMAP) |
| `lark.rs` / `dingtalk.rs` | 飞书/钉钉 |
| `signal.rs` | Signal |
| `nostr.rs` | Nostr |
| `irc.rs` | IRC |
| `mattermost.rs` | Mattermost |
| `imessage.rs` | iMessage |
| `cli.rs` | 命令行 |
| `qq.rs` | QQ |
| `linq.rs` / `nextcloud_talk.rs` | Linq/Nextcloud Talk |

#### `src/tools/` - 工具集

Agent 可调用的工具能力。

| 类别 | 工具文件 |
|------|----------|
| **文件系统** | `file_read.rs`, `file_write.rs`, `file_edit.rs`, `glob_search.rs` |
| **Shell** | `shell.rs`, `cli_discovery.rs` |
| **浏览器** | `browser.rs`, `browser_open.rs`, `screenshot.rs` |
| **HTTP** | `http_request.rs`, `web_search_tool.rs`, `content_search.rs` |
| **记忆** | `memory_recall.rs`, `memory_store.rs`, `memory_forget.rs` |
| **Cron 调度** | `cron_add.rs`, `cron_list.rs`, `cron_remove.rs`, `cron_run.rs`, `cron_update.rs` |
| **代码** | `git_operations.rs`, `pdf_read.rs`, `image_info.rs` |
| **硬件** | `hardware_board_info.rs`, `hardware_memory_map.rs`, `hardware_memory_read.rs` |
| **其他** | `delegate.rs`, `schedule.rs`, `pushover.rs`, `proxy_config.rs` |

#### `src/memory/` - 记忆后端

| 文件 | 功能 |
|------|------|
| `markdown.rs` | Markdown 文件存储 |
| `sqlite.rs` | SQLite 后端 |
| `postgres.rs` | PostgreSQL 后端 |
| `vector.rs` | 向量存储 |
| `embeddings.rs` | Embedding 生成 |
| `chunker.rs` | 文本分块 |
| `response_cache.rs` | 响应缓存 |
| `hygiene.rs` | 记忆清理 |

#### `src/security/` - 安全模块

| 文件 | 功能 |
|------|------|
| `pairing.rs` | 设备配对机制 |
| `policy.rs` | 安全策略 |
| `secrets.rs` | 密钥存储 (加密) |
| `landlock.rs` | Linux Landlock 沙箱 |
| `bubblewrap.rs` | Bubblewrap 沙箱 |
| `firejail.rs` | Firejail 沙箱 |
| `detect.rs` | 威胁检测 |
| `prompt_guard.rs` | 提示词注入防护 |
| `otp.rs` | 一次性密码 |
| `audit.rs` | 审计日志 |

#### `src/runtime/` - 运行时适配

| 文件 | 功能 |
|------|------|
| `native.rs` | 本地原生运行时 |
| `docker.rs` | Docker 容器运行时 |
| `traits.rs` | RuntimeAdapter Trait |

#### `src/gateway/` - HTTP 网关

基于 Axum 的 HTTP 服务器，提供 webhook 和 API 接口。

| 文件 | 功能 |
|------|------|
| `api/` | REST API 端点 |
| `sse.rs` | Server-Sent Events |
| `ws.rs` | WebSocket 支持 |
| `static_files.rs` | 静态文件服务 |

#### `src/config/` - 配置管理

| 文件 | 功能 |
|------|------|
| `schema.rs` | 配置结构定义 |
| `traits.rs` | 配置 Trait |

#### `src/peripherals/` - 硬件外设

支持硬件板卡的扩展。

| 文件 | 功能 |
|------|------|
| `stm32.rs` | STM32 微控制器 |
| `rpi.rs` | Raspberry Pi GPIO |
| `traits.rs` | Peripheral Trait |

#### `src/observability/` - 可观测性

| 文件 | 功能 |
|------|------|
| `metrics.rs` | Prometheus 指标 |
| `tracing.rs` | 分布式追踪 |
| `health.rs` | 健康检查 |

### 4.2 支持模块

| 模块 | 功能 |
|------|------|
| `src/approval/` | 工具执行审批 |
| `src/auth/` | 认证机制 |
| `src/cost/` | 成本追踪 |
| `src/cron/` | 定时任务调度 |
| `src/daemon/` | 守护进程管理 |
| `src/doctor/` | 健康诊断 |
| `src/health/` | 健康检查 |
| `src/heartbeat/` | 心跳机制 |
| `src/hooks/` | 钩子系统 |
| `src/identity/` | 身份管理 |
| `src/integrations/` | 第三方集成 |
| `src/migration/` | 数据迁移 |
| `src/multimodal.rs` | 多模态支持 |
| `src/rag/` | RAG (检索增强生成) |
| `src/service/` | 服务管理 |
| `src/skills/` | 技能系统 |
| `src/tunnel/` | 隧道连接 |
| `src/util/` | 工具函数 |

### 4.3 工作区成员

| 成员 | 路径 | 描述 |
|------|------|------|
| **zeroclaw** | `.` | 主二进制 crate |
| **robot-kit** | `crates/robot-kit` | 机器人套件库 |

---

## 5. 模块工作流程

### 5.1 消息处理流程

```
用户消息 (Channel)
      │
      ▼
┌─────────────────┐
│  Channel.listen │  监听消息
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Agent.process   │  分类消息
│ (Classifier)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Provider.chat   │  调用 LLM
│                 │  (可含 Tools)
└────────┬────────┘
         │
         ▼
    ┌────┴────┐
    │         │
有工具调用    纯文本
    │         │
    ▼         ▼
┌─────────┐  ┌─────────┐
│Dispatcher│  │Channel  │
│execute  │  │send     │
│Tools    │  │reply    │
└────┬────┘  └─────────┘
     │
     ▼
┌─────────────┐
│ Tool Result │
│ (返回给 LLM)│
└──────┬──────┘
       │
       ▼
┌─────────────────┐
│ Provider.chat   │  继续对话
│ (with results) │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Channel.send    │  返回结果
└─────────────────┘
```

### 5.2 配置加载流程

```
CLI / Gateway 启动
       │
       ▼
┌─────────────────┐
│ config/schema   │  加载配置
│ Config::load()  │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Factory 创建    │
│ providers::    │  实例化 Provider
│ create_provider│
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Factory 创建    │
│ channels::     │  实例化 Channel
│ create_channel │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Factory 创建    │
│ tools::        │  实例化 Tools
│ create_tools   │
└─────────────────┘
```

### 5.3 工具执行流程

```
LLM 发出 Tool Call
      │
      ▼
┌─────────────────┐
│ ToolSpec 匹配   │  查找工具
│ (by name)      │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Security Check │  权限检查
│ (Policy)       │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Approval Check │  审批检查
│ (if enabled)   │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Tool.execute   │  执行工具
│ (with args)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ ToolResult      │  返回结果
│ (success/error)│
└─────────────────┘
```

### 5.4 记忆存储流程

```
用户交互
      │
      ▼
┌─────────────────┐
│ Memory.store   │  存储记忆
│ (key, content)│
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Chunker.split   │  文本分块
│ (if large)     │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Embeddings.gen │  生成向量
│ (if vector db)│
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Backend.save   │  持久化存储
│ (SQL/MD/PG)   │
└─────────────────┘
```

---

## 6. 扩展点指南

### 6.1 添加新的 Provider

1. 在 `src/providers/` 创建新文件
2. 实现 `Provider` trait
3. 在 `src/providers/mod.rs` 注册工厂

### 6.2 添加新的 Channel

1. 在 `src/channels/` 创建新文件
2. 实现 `Channel` trait
3. 在 `src/channels/mod.rs` 注册工厂

### 6.3 添加新的 Tool

1. 在 `src/tools/` 创建新文件
2. 实现 `Tool` trait
3. 在 `src/tools/mod.rs` 注册

### 6.4 添加新的 Memory Backend

1. 在 `src/memory/` 创建新文件
2. 实现 `Memory` trait
3. 在 `src/memory/mod.rs` 注册工厂

---

## 7. 关键文件索引

| 功能 | 文件路径 |
|------|----------|
| 入口点 | `src/main.rs` |
| 模块导出 | `src/lib.rs` |
| 配置 Schema | `src/config/schema.rs` |
| Provider Trait | `src/providers/traits.rs` |
| Channel Trait | `src/channels/traits.rs` |
| Tool Trait | `src/tools/traits.rs` |
| Memory Trait | `src/memory/traits.rs` |
| Runtime Trait | `src/runtime/traits.rs` |
| 安全策略 | `src/security/policy.rs` |
| Agent 主循环 | `src/agent/loop_.rs` |
| Gateway 服务器 | `src/gateway/mod.rs` |

---

## 8. 总结

ZeroClaw 是一个精心设计的 Rust Agent 运行时，其核心优势在于：

1. **极小的资源占用** - 适用于 10 美元硬件
2. **Trait 驱动架构** - 高度可扩展和可插拔
3. **安全优先** - 多种沙箱和配对机制
4. **多平台支持** - 丰富的 Channel 和 Provider 支持
5. **生产级特性** - 可观测性、成本追踪、审计日志

该项目遵循严格的工程原则，适合作为构建自主 Agent 系统的基础框架。
