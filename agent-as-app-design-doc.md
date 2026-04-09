# Agent as APP：个人化、安全、可组合的 AI Agent 平台设计文档

> 版本：v3.0 | 更新日期：2026-04-09

---

## 1. 概述

### 1.1 背景与目标

设计一个去中心化、高安全、可扩展的 AI Agent 运行时平台。核心思想是将每个 Agent 视为一个独立的"应用包"（类似 Android APP），由统一的 Agent Runtime 进程加载执行，运行在客户端（用户电脑）并由轻量级 Gateway 管理生命周期。

**核心类比——Android 模型：**

| Android | Rollball | 作用 |
|---------|----------|------|
| zygote / ART | Agent Runtime 二进制 | 通用执行引擎，只有一个 |
| APK (DEX + resources) | .agent 包 (config + prompts + skills) | 声明式，无自定义代码 |
| ActivityManagerService | Gateway | 生命周期管理 |
| Binder IPC | Gateway Service API | 进程间通信（传输层由平台实现） |
| ContentProvider | 系统 Agent (com.rollball.system) | 系统级数据服务（身份、偏好等） |
| PackageManagerService | Package Manager | 安装/卸载 |
| AndroidManifest.xml | manifest.json | 权限声明 |

### 1.2 核心特性

- **标准化打包**：Agent 以压缩包（.agent）分发，内含配置、Prompt、Skill、工具声明，**不含可执行文件**。
- **统一执行引擎**：Agent Runtime 是平台提供的唯一二进制，负责加载 .agent 包并执行 Agent 逻辑（LLM 交互、工具调度、记忆读写）。
- **进程级隔离**：每个 Agent 由 Gateway 启动为独立 Agent Runtime 进程，拥有独立工作区、私有 Grafeo 数据库、文件系统隔离、可选资源限制（cgroups/容器）。
- **Agent 自治**：Agent 进程内直连 LLM API、自主执行工具、自主管理权限校验，不依赖 Gateway 代理业务逻辑。
- **分层 Memory**：每个 Agent 内嵌私有 Grafeo（情景记忆 + 语义记忆），系统 Agent 提供身份与偏好等系统级数据服务，云端提供跨设备同步。
- **权限声明与授权**：Agent 在清单中声明所需权限（网络、文件、调用其他 Agent 等），Gateway 在启动时配置沙箱，Agent 在运行时自主校验。
- **跨平台支持**：.agent 包格式和 Gateway Service API 合同跨平台统一，各平台运行时机制（进程模型、传输层、沙箱）可按平台特性适配。

---

## 2. 总体架构

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
                    │  - 增量同步              │
                    │  - 冲突解决 (CRDT/LWW)   │
                    │  - 联邦共享 (可选)       │
                    └─────────────────────────┘
```

### 2.1 职责划分原则

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

---

## 3. 组件详细设计

### 3.1 Agent 打包格式（.agent）

#### 3.1.1 包结构

`.agent` 文件本质是一个 ZIP 压缩包。Agent 包**不含可执行文件**，只包含配置、Prompt 和数据。由 Agent Runtime 二进制加载执行。

```
<agent_id>.agent
├── manifest.json          # 必需，元数据 + LLM 配置 + 权限 + 工具声明
├── prompts/               # System prompt 模板
│   ├── system.md          # 主系统提示词
│   ├── tools.md           # 工具使用说明
│   └── constraints.md     # 约束和安全规则
├── config/                # 默认配置文件（用户可覆盖）
│   └── settings.toml
├── data/                  # 初始数据（如空 Grafeo 快照）
├── skills/                # Skill 定义
│   └── weather-query/
│       ├── SKILL.toml
│       └── SKILL.md
├── tools/                 # 自定义工具（WASM，可选）
│   └── image_filter.wasm
└── resources/             # 图标、本地化等
```

#### 3.1.2 manifest.json 架构

```json
{
  "agent_id": "com.example.weather",
  "version": "1.0.0",
  "name": "Weather Agent",
  "description": "查询实时天气并建议穿衣",
  "author": "example@domain.com",
  "runtime_version": "^1.0.0",
  "permissions": [
    "network:https://api.weather.com",
    "filesystem:read:~/Documents",
    "memory:read",
    "memory:write",
    "intent:send:com.example.calendar"
  ],
  "triggers": [
    {"type": "schedule", "cron": "0 7 * * *"},
    {"type": "message", "pattern": "天气|weather"}
  ],
  "llm": {
    "default_provider": "openai",
    "providers": {
      "openai": {
        "model": "gpt-4o",
        "api_key_ref": "vault:openai_key",
        "base_url": "https://api.openai.com/v1",
        "params": {"temperature": 0.7, "max_tokens": 4096}
      },
      "claude": {
        "model": "claude-sonnet-4-20250514",
        "api_key_ref": "vault:anthropic_key"
      },
      "fallback": {
        "provider": "ollama",
        "model": "qwen3:8b",
        "base_url": "http://localhost:11434"
      }
    },
    "routing": {
      "strategy": "cost_priority",
      "fallback_on_error": true,
      "retry": {"max_attempts": 3, "backoff": "exponential"}
    },
    "budget": {
      "daily_token_limit": 100000,
      "daily_cost_limit_usd": 5.0,
      "action_on_exhaust": "fallback_to_local"
    }
  },
  "memory": {
    "sync_mode": "auto",
    "cache_ttl": 3600,
    "required": false
  },
  "identity_deps": ["name", "city", "language", "timezone"],
  "tools": [
    {
      "name": "http_get",
      "type": "builtin",
      "permissions": ["network:https://api.weather.com"]
    },
    {
      "name": "image_filter",
      "type": "wasm",
      "binary": "./tools/image_filter.wasm",
      "permissions": ["memory:read"],
      "resource_limits": {
        "max_memory_mb": 50,
        "max_execution_time_ms": 5000
      }
    }
  ],
  "capabilities": {
    "query_weather": {
      "input": {"city": "string", "date": "date?"},
      "output": {"temperature": "float", "condition": "string"}
    }
  },
  "resources": {
    "max_memory_mb": 200,
    "max_cpu_percent": 50,
    "network": true
  },
  "sandbox": {
    "enable": true,
    "allow_ptrace": false,
    "read_only_root": true
  }
}
```

**关键字段说明：**

- `runtime_version`：声明兼容的 Agent Runtime 版本（语义版本约束）。
- `llm.providers`：支持配置多个 LLM Provider，每个引用 Vault 中的密钥。
- `llm.routing.strategy`：LLM 路由策略（cost_priority / quality_priority / latency_priority）。
- `llm.budget`：Token 和费用预算，超限后的动作（stop / fallback_to_local / warn）。
- `memory.shared`：已移除。用户身份与偏好由系统 Agent 管理，其他 Agent 通过 Intent 查询或提报。
- `identity_deps`：声明启动时需要的用户身份字段（如 name、city、language），Gateway 在启动前向系统 Agent 查询并注入。
- `tools`：工具声明，支持 builtin（内置）和 wasm（自定义沙箱）两种类型。
- `capabilities`：声明本 Agent 可被其他 Agent 通过 Intent 调用的能力，含类型信息。

---

### 3.2 Agent Runtime（统一执行引擎）

Agent Runtime 是平台提供的唯一二进制可执行文件，类似 Android 的 ART 虚拟机。Gateway 为每个 Agent 启动一个 Agent Runtime 进程，将 .agent 包路径作为启动参数传入。

#### 3.2.1 启动方式

```bash
agent-runtime \
    /path/to/agent-package \
    --socket /tmp/agent-gateway.sock \
    --agent-id com.example.weather \
    --workspace /home/user/.local/share/agent-gateway/agents/com.example.weather/workspace \
    --config-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/config \
    --identity '{"name":"张三","city":"Shanghai","language":"zh-CN","timezone":"Asia/Shanghai"}'
```

#### 3.2.2 内部结构

```
Agent Runtime 二进制
├── Package Loader      # 解析 .agent ZIP，加载 manifest + prompts + config
├── Prompt Builder      # 组装 system prompt（identity + tools + skills + memory context）
├── History Manager     # 对话历史管理（token 预算、trim、压缩）
├── LLM Client          # 直连 LLM Provider API（OpenAI/Claude/Ollama 等）
├── Tool Dispatcher     # 解析 LLM 输出的 tool_calls，路由到工具实现
│   ├── Built-in Tools  # 内置工具（memory_recall, memory_store, http_get, shell...）
│   ├── WASM Tools      # .agent 包中声明的 WASM 工具（沙箱内执行）
│   └── Gateway Tools   # 需要 Gateway 协调的工具（Intent 收发）
├── Permission Checker  # 根据 manifest 权限表校验工具调用权限
├── Memory Client       # 读写私有 Grafeo
├── Grafeo (嵌入式)     # 私有 Memory（情景记忆 + 语义记忆）
├── Skill Loader        # 加载 .agent 包中的 Skills
├── Budget Manager      # 本地预算预检 + 用量上报
└── Loop Controller     # 主循环控制（迭代次数、超时、循环检测）
```

#### 3.2.3 主循环

Agent Runtime 的核心是 LLM 交互循环（参考 ZeroClaw 的 `run_tool_call_loop`）：

```
用户消息 / Intent / 定时触发
       │
       ▼
┌─────────────────────────────────────────┐
│  Agent Runtime 主循环 [iteration: 0..N]  │
│                                         │
│  ① 预算预检                             │
│     └─ 本地预算缓存不足 → fallback 或报错 │
│                                         │
│  ② 构建上下文                            │
│     ├─ System Prompt (from prompts/)    │
│     ├─ Memory RAG (from 私有 Grafeo)    │
│     ├─ Identity Context (from 启动注入) │
│     ├─ Skills (from skills/)            │
│     └─ 对话历史                          │
│                                         │
│  ③ 调用 LLM (直连 API)                  │
│     ├─ RateAcquire速率协调              │
│     └─ streaming 或 blocking            │
│                                         │
│  ④ 解析响应                              │
│     ├─ text → 返回结果/回复用户          │
│     └─ tool_calls → ⑤                  │
│                                         │
│  ⑤ 工具调度与执行                        │
│     ├─ Permission Check (manifest)      │
│     ├─ Built-in Tool → 直接执行         │
│     ├─ WASM Tool → Wasmtime 沙箱执行    │
│     └─ Gateway Tool → Unix Socket 调用  │
│                                         │
│  ⑥ 结果追加到历史                        │
│                                         │
│  ⑦ 用量上报（异步，不阻塞）              │
│                                         │
│  ⑧ 循环检测（防止重复工具调用）          │
│                                         │
│  └─→ 回到 ①（下一轮迭代）               │
└─────────────────────────────────────────┘
```

---

### 3.3 Gateway 组件

Gateway 是一个常驻的系统级进程（可表现为系统托盘应用），使用 Rust 实现。Gateway **不代理 Agent 的业务逻辑**（不代理 LLM 调用、不代理工具执行），只负责必须集中化的协调工作。

#### 3.3.1 Package Manager

- **安装**：解压 `.agent` 到 `~/.local/share/agent-gateway/agents/<agent_id>/`，校验 manifest 完整性，记录版本。
- **卸载**：删除对应目录，可选备份用户数据（含私有 Grafeo）。
- **升级**：保留 `data/` 和用户修改的 `config/`，替换其他文件。若 runtime_version 不兼容则提示用户。
- **仓库支持**：可配置多个 HTTP 仓库源（类似 apt），定期检查更新。

#### 3.3.2 生命周期管理器

**启动策略：**
- 按需启动：当收到匹配 trigger 的消息或用户显式调用时启动。
- 常驻：用户可标记某 Agent 开机自启。
- 定时启动：由 cron 表达式触发。

**进程管理：**
- 使用 `std::process::Command` 创建子进程，设置独立工作目录、环境变量。
- 启动参数注入：Agent 包路径、Gateway Socket 路径、Agent ID、工作区路径。
- API Key 分发：Agent Runtime 连接 Gateway 后，通过 Socket 传输 Key（不通过环境变量，避免 ps 泄露）。
- 健康检查：如果 Agent 进程退出，根据退出代码决定是否自动重启（可配置）。

**休眠与唤醒：**
- 采用杀死重启策略：空闲超时后直接杀死 Agent Runtime 进程，下次需要时重新 spawn。
- Agent 的状态通过私有 Grafeo 持久化，启动时从 Memory 恢复上下文。
- 不使用 SIGSTOP/SIGCONT（Windows 不支持、进程仍占内存、状态序列化困难）。
- Agent 可在 manifest 中声明 `startup_timeout_ms`，Gateway 据此判断是否需要预热（提前拉起）。

#### 3.3.3 Intent Router

**输入源：**
- 用户界面（CLI/GUI）发出的请求。
- 定时任务触发器。
- 其他 Agent 通过 Gateway 转发的 Intent 消息（见 3.6）。

**路由规则：**
- 根据消息中的 `target` 字段直接路由到目标 Agent。
- 若目标 Agent 未运行，则按需启动。
- 若未指定 target，则匹配已安装 Agent 的 manifest 中 `triggers.message.pattern`。

#### 3.3.4 沙箱配置器

Gateway 在启动 Agent Runtime 时根据 manifest 配置沙箱参数，之后由 OS 层面执行隔离。各平台实现方式不同，但隔离目标一致。

**跨平台隔离策略对照：**

| 隔离维度 | Linux | Windows | macOS | Android | iOS |
|---------|-------|---------|-------|---------|-----|
| **进程模型** | spawn 独立进程 | spawn 独立进程 | spawn 独立进程 | 单进程多线程 / Service | 单进程多线程 / Extension |
| **文件隔离** | bubblewrap `--bind` | 受限令牌 + ACL | App Sandbox | 系统沙箱 | 系统沙箱 |
| **网络隔离** | `--unshare-net` | Firewall API | Network Extension | 系统沙箱兜底 | 系统沙箱兜底 |
| **系统调用限制** | seccomp-bpf | 无（靠 Job Object） | sandbox-exec | 系统级 | 系统级 |
| **资源限制** | cgroups / rlimit | Job Object limits | rlimit | 系统级 | 系统级 |
| **WASM 引擎** | Wasmtime (JIT) | Wasmtime (JIT) | Wasmtime (JIT) | wasmi (解释器) | wasmi (解释器，iOS 禁止 JIT) |
| **数据目录** | XDG (`~/.local/share/`) | `%APPDATA%\AgentGateway\` | `~/Library/Application Support/AgentGateway/` | `context.getFilesDir()` | appSupportDir |

**路径解析统一接口：**

```rust
fn app_data_dir() -> PathBuf {
    // 各平台返回符合系统规范的路径
    // Linux:   ~/.local/share/agent-gateway/
    // Windows: C:\Users\<user>\AppData\Local\AgentGateway\
    // macOS:   ~/Library/Application Support/AgentGateway/
    // Android: /data/data/com.rollball.gateway/files/
    // iOS:     <appSupportDir>/AgentGateway/
}
```

**平台实现示例：**
```bash
bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /bin /bin \
    --ro-bind /usr/lib/agent-gateway/agent-runtime /app \
    --bind <agent_workspace> /workspace \
    --dev /dev \
    --proc /proc \
    --unshare-net \              # 默认禁止网络（需网络时按 manifest 白名单配置）
    --die-with-parent \
    agent-runtime /workspace/agent-package --socket /tmp/gateway.sock
```

**Windows：**
- `CreateRestrictedToken` + Job Object + 文件系统 ACL

**macOS：**
- `sandbox-exec` 配置文件

#### 3.3.5 Key Vault

集中管理所有 LLM API Key，加密存储：

```
~/.config/agent-gateway/vault/
├── openai_key.enc
├── anthropic_key.enc
└── vault.key               # 主密钥，用户密码派生
```

- Agent manifest 中用 `vault:openai_key` 引用 Key，不存明文。
- Agent Runtime 启动后通过 Gateway Socket 获取 Key（一次性传输，不通过环境变量）。
- Key 在 Rust 侧零拷贝/密封存储（使用 secrecy::SecretString），LLM Client 直接使用该 Secret 签名请求，WASM 插件层绝对没有 API 能读取到该字符串。

#### 3.3.6 Budget Tracker

接收 Agent Runtime 上报的 LLM 用量，维护跨 Agent 的统计：

- 每个 Agent 有独立的日/月 Token 和费用限额。
- 超限时向 Agent 发送信号（stop / fallback / warn）。
- 提供预算查询接口供 Agent 本地预检。

#### 3.3.7 Rate Limiter

协调多 Agent 对同一 LLM Provider 的并发请求，避免触发 API Rate Limit：

- Agent 调 LLM 前通过 Gateway 申请速率令牌（极轻量 RPC，< 0.1ms）。
- Gateway 基于 Provider 的 RPM/TPM 限制分配令牌。

#### 3.3.8 配置与数据存储

- **Gateway 自身配置**：`~/.config/agent-gateway/config.toml`（含 Vault 配置、仓库列表、默认 LLM 配置等）。
- **每个 Agent 的工作区**：`~/.local/share/agent-gateway/agents/<agent_id>/workspace/`：
  - `data/`：从包中复制，可读写。
  - `config/`：用户可修改的配置（初始来自包内 config）。
  - `memory/`：私有 Grafeo 数据库文件（`private.grafeo`）。
  - `runtime/`：临时文件（socket、pid）。
- **日志**：Gateway 收集所有 Agent 的 stdout/stderr，写入 `~/.local/share/agent-gateway/logs/`，支持按 Agent 过滤。

---

### 3.4 Memory 分层架构

Memory 采用**本地优先（Local-First）**设计，以 Grafeo 图数据库为存储引擎，按归属和生命周期分为三层。每个 Agent 拥有完全独立的私有 Memory，不存在 Gateway 维护的公共数据库。跨 Agent 的数据共享通过 Intent 查询和系统 Agent 服务实现，而非共享存储。

```
┌────────────────────────────────────────────┐
│           第一层：工作记忆                   │
│  Agent Runtime 进程内存                     │
│  当前对话历史、上下文窗口                    │
│  生命周期：单次会话                         │
├────────────────────────────────────────────┤
│           第二层：私有记忆                   │
│  Agent 进程内嵌 Grafeo                      │
│  情景记忆 (HNSW 向量索引)                   │
│  语义记忆 (LPG 知识图谱)                    │
│  全文检索 (BM25 倒排索引)                   │
│  生命周期：数据持久化到磁盘，进程级隔离      │
├────────────────────────────────────────────┤
│           第三层：云端同步                   │
│  Memory Sync Service                       │
│  跨设备增量同步、冲突解决 (CRDT/LWW)        │
│  联邦共享（可选，需授权）                   │
│  生命周期：永久                             │
└────────────────────────────────────────────┘
```

**设计原则**：每个 Agent 是一个独立的"数字人"，保有对用户完全独立的个性化记忆。不同 Agent 对同一用户的认知可以不同——天气 Agent 记住你住北京，日历 Agent 记住你常去上海出差——这是符合仿生设计的自然结果。基础身份信息（姓名、语言等）的一致性通过系统 Agent 的 ContentProvider 服务保障，而非共享数据库。

#### 3.4.1 私有 Memory（Agent 内嵌 Grafeo）

每个 Agent Runtime 进程内嵌一个独立的 Grafeo 实例，数据文件存储在 Agent 工作区：

```
~/.local/share/agent-gateway/agents/<agent_id>/workspace/memory/private.grafeo
```

**核心能力：**
- **情景记忆（HNSW 向量索引）**：存储 Agent 与用户的交互片段，支持语义相似性检索。
- **语义记忆（LPG 知识图谱）**：存储从交互中提取的结构化知识（事实、偏好、关系）。
- **全文检索（BM25 倒排索引）**：支持对记忆内容的精确关键词搜索。
- **混合搜索**：融合向量检索 + 全文检索，通过 Reciprocal Rank Fusion (RRF) 排序。
- **Embedding 生成**：Grafeo 内置 ONNX Runtime，可在本地生成向量（如 all-MiniLM-L6-v2），无需外部 embedding 服务。

**隔离保证：**
- 数据隔离：每个 Agent 的 Grafeo 文件在独立工作区，沙箱层面文件系统隔离。
- 进程隔离：一个 Agent 的 Grafeo 崩溃不影响其他 Agent。
- OS 级保证：Agent A 的沙箱内无法访问 Agent B 的 Grafeo 文件。

#### 3.4.2 跨 Agent 知识共享

不同 Agent 之间不共享数据库，知识共享通过两种机制实现：

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

用户身份和偏好等系统级信息由系统 Agent（`com.rollball.system`）统一管理，其他 Agent 通过 Intent 查询。详见 3.7 系统 Agent。

**路径 3：云端 Memory Sync 同步**

云端作为知识同步层，Agent 写入的知识可按规则广播给订阅了该信息的其他 Agent，各 Agent 的本地 Grafeo 各自更新。

---

### 3.5 通信协议：Gateway Service API

Agent Runtime 与 Gateway 通过 **Gateway Service API** 通信。API 的消息格式和交互语义是**平台无关的合同**，传输层由各平台自行选择。

#### 3.5.1 合同层 vs 实现层

| 层次 | 内容 | 说明 |
|------|------|------|
| **合同层**（所有平台必须遵守） | 帧格式、消息类型、请求/响应 JSON schema、握手协议 | Agent 开发者只需关心此层 |
| **实现层**（各平台自行决定） | 传输方式、进程模型、沙箱方式 | 不影响 .agent 包兼容性 |

**传输层实现选择：**

| 平台 | 传输方式 | Endpoint 格式 |
|------|---------|--------------|
| Linux | Unix Domain Socket | `unix:///tmp/agent-gateway.sock` |
| macOS | Unix Domain Socket | `unix:///tmp/agent-gateway.sock` |
| Windows | Named Pipe | `pipe://agent-gateway` |
| Android | Abstract Namespace Socket / Local TCP | `abstract://agent-gateway` / `tcp://127.0.0.1:19876` |
| iOS | Local TCP | `tcp://127.0.0.1:19876` |

Agent Runtime 启动时通过参数接收 endpoint 字符串，内部根据 scheme 选择传输实现。

#### 3.5.2 握手协议

连接建立后第一条消息用于协商：

```json
// Agent Runtime → Gateway
{
    "type": "handshake",
    "agent_id": "com.example.weather",
    "runtime_version": "1.0.0",
    "protocol_version": 1
}

// Gateway → Agent Runtime
{
    "type": "handshake_ack",
    "capabilities": ["streaming"],
    "key_delivery": "in_band"
}
```

握手之后的所有消息，不管底层传输是什么，格式完全一致。

#### 3.5.3 帧格式

```
[4 bytes: body length (u32 big-endian)]
[1 byte:  message type (0=request, 1=response, 2=stream_chunk, 3=error)]
[N bytes: JSON body]
```

#### 3.5.4 API 定义

Agent Runtime 只在这些操作上和 Gateway 通信（不代理 LLM 调用和工具执行）：

```rust
enum GatewayRequest {
    // --- 密钥 ---
    KeyRelease { provider: String },           // 获取 API Key（启动时一次性）

    // --- Intent ---
    IntentSend {
        target: String,
        action: String,
        params: serde_json::Value,
        async_: bool,
    },

    // --- 预算协调 ---
    BudgetQuery { provider: String },           // 查询剩余预算
    UsageReport(UsageReport),                   // 上报 LLM 用量

    // --- 速率协调 ---
    RateAcquire { provider: String },           // 申请速率令牌

    // --- 运行时权限请求 ---
    PermissionRequest {
        permission: String,
        reason: String,
    },
}

enum GatewayResponse {
    KeyReleaseResult { api_key: String },
    IntentDelivered { message_id: String },
    IntentReceived { from: String, action: String, params: serde_json::Value },
    BudgetInfo { remaining_tokens: u64, remaining_cost_usd: f64 },
    UsageReportAck {},
    RateToken { granted: bool, retry_after_ms: Option<u64> },
    PermissionResult { granted: bool, reason: Option<String> },
}
```

---

### 3.6 跨 Agent 通信（Intent 机制）

Agent 通过 Gateway 的 Intent Router 发送消息请求调用另一个 Agent 的能力。

#### 3.6.1 Intent 消息格式

```json
{
  "type": "intent",
  "target": "com.example.calendar",
  "action": "create_event",
  "params": {"title": "Meeting", "time": "2025-01-01T10:00Z"},
  "async": true,
  "id": "msg-456"
}
```

#### 3.6.2 Capability Registry

每个 Agent 的 manifest 中声明 `capabilities`，Gateway 维护一个 Capability Registry：

```json
{
  "capabilities": {
    "create_event": {
      "input": {"title": "string", "time": "datetime", "remind_before": "duration?"},
      "output": {"event_id": "string", "status": "created|failed"}
    }
  }
}
```

- Agent 安装时，Gateway 检查其 Intent 依赖的 capabilities 是否可用。
- 调用时，Gateway 校验参数类型是否匹配。
- 类似 Android 的 IntentFilter + ContentProvider 机制。

#### 3.6.3 Intent 路由流程

1. Agent A 通过 Unix Socket 发送 Intent 到 Gateway。
2. Gateway 查找 target Agent B，若未运行则启动。
3. Gateway 将 Intent 转发给 Agent B。
4. Agent B 处理后返回结果。
5. Gateway 将结果返回给 Agent A（同步模式）或缓存等待 Agent A 下次查询（异步模式）。

---

### 3.7 系统 Agent（com.rollball.system）

系统 Agent 是 Rollball 的"系统应用"，类似 Android 的系统内置 App（SystemUI / Settings / Contacts）。它是一个特殊的 .agent 包，随 Gateway 一起分发，拥有特权，Gateway 启动时自动拉起。

**核心定位**：系统 Agent 是用户与 Rollball 平台之间的默认交互入口和系统级数据服务的提供者。当没有安装任何第三方 Agent 时，系统 Agent 就是用户和 Rollball 交互的唯一界面。所有需要"智能"的系统级服务——身份识别、偏好管理、帮助导航——都由系统 Agent 的 LLM 处理，Gateway 本身不承担任何业务逻辑推理。

#### 3.7.1 系统 Agent 的特殊性

```json
{
  "agent_id": "com.rollball.system",
  "system": true,
  "privileges": [
    "content_provider",       // 可注册 ContentProvider 服务
    "auto_start",             // Gateway 启动时自动拉起
    "uninstallable": false,   // 不可卸载
    "priority": "system"      // 最高启动优先级
  ]
}
```

**与普通 Agent 的区别：**

| 属性 | 普通 Agent | 系统 Agent |
|------|-----------|-----------|
| 安装方式 | 用户从仓库安装 | 随 Gateway 分发，不可卸载 |
| 启动时机 | 按需 / 定时 / 手动 | Gateway 启动时自动拉起 |
| 生命周期 | 空闲可被杀死 | 常驻（空闲超时后也可杀死，但下次 Gateway 检测到需求时立即拉起） |
| ContentProvider | 不可以 | 可以注册，其他 Agent 可通过 Intent 查询 |
| 身份提报 | 可以向系统 Agent 提报身份信息 | 接收提报，LLM 判断后存入私有 Grafeo |

#### 3.7.2 ContentProvider 机制

系统 Agent 通过 Intent + Capability 机制对外提供数据服务，标记 `"provider": true` 的 Capability 表示这是 ContentProvider 语义——只读数据服务，不是一次性动作。

**系统 Agent 声明的 Capabilities：**

```json
{
  "capabilities": {
    "identity:query": {
      "input": { "fields": ["string"] },
      "output": { "values": "map<string, string>", "confidence": "map<string, float>" },
      "provider": true
    },
    "identity:observe": {
      "input": { "fields": ["string"], "callback_intent": "string" },
      "output": { "subscribed": "bool" },
      "provider": true
    }
  }
}
```

**其他 Agent 查询用户信息：**

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:query",
  "params": { "fields": ["name", "city", "language"] }
}
```

系统 Agent 从私有 Grafeo 查询并返回：

```json
{
  "values": {
    "name": "张三",
    "city": "Shanghai",
    "language": "zh-CN"
  },
  "confidence": {
    "name": 1.0,
    "city": 0.85,
    "language": 1.0
  }
}
```

#### 3.7.3 冷启动身份注入

新安装的 Agent 首次运行时，如果 manifest 中声明了 `identity_deps`：

```json
{
  "identity_deps": ["name", "city", "language", "timezone"]
}
```

Gateway 在启动该 Agent 之前，先向系统 Agent 查询这些字段，把结果作为启动上下文注入，Agent 开箱即有用户信息：

```bash
agent-runtime /path/to/agent-package \
    --socket /tmp/gateway.sock \
    --agent-id com.example.weather \
    --identity '{"name":"张三","city":"Shanghai","language":"zh-CN","timezone":"Asia/Shanghai"}'
```

#### 3.7.4 身份信息的获取与更新

系统 Agent 获取用户身份信息有两条路径：

**路径 1：直接对话（默认入口）**

当没有安装任何第三方 Agent 时，系统 Agent 就是用户的交互入口。用户直接和系统 Agent 聊天，系统 Agent 的 LLM 自然地识别并提取身份信息，存入私有 Grafeo。

```
用户: "你好，我叫张三，住北京"
          │
          ▼
系统 Agent LLM:
  - 识别到身份信息
  - 调用内置工具 identity_store(name="张三", city="北京", confidence=0.95)
  - 写入私有 Grafeo 语义记忆
```

**路径 2：从其他 Agent 的对话中学习**

用户身份信息往往在和具体 Agent 聊天时自然透露。其他 Agent 判断有身份信息变更时，向系统 Agent 发送轻量的身份更新 Intent：

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:update",
  "params": {
    "updates": { "city": "Shanghai" },
    "evidence": "用户说'我刚搬到上海'",
    "confidence": 0.9
  }
}
```

**关键设计：系统 Agent 用 LLM 做二次判断，而非用户仲裁。**

系统 Agent 收到提报后，用自己的 LLM 判断信息的语义——区分"搬家"和"出差"，判断信息是否值得更新。这是用 LLM 推理替代用户确认弹窗，不打扰用户体验：

```
提报: city = "Shanghai", evidence = "我刚搬到上海", confidence = 0.9
          │
          ▼
系统 Agent LLM: "搬家" → 确实是居住地变更 → 更新 user.city

提报: city = "Shanghai", evidence = "我下周去上海出差", confidence = 0.7
          │
          ▼
系统 Agent LLM: "出差" → 临时行程，非居住地变更 → 不更新 user.city
```

系统 Agent 的 prompt 被设定为保守策略——拿不准就不更新，而不是错误更新。

#### 3.7.5 变更通知（observe 机制）

类似 Android ContentProvider 的 `registerContentObserver`，Agent 可以订阅特定身份字段的变更：

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:observe",
  "params": {
    "fields": ["city"],
    "callback_intent": "com.example.weather"
  }
}
```

当系统 Agent 更新了 city 字段，通过 Gateway 向订阅者广播：

```json
{
  "type": "notification",
  "from": "com.rollball.system",
  "action": "identity:changed",
  "params": {
    "field": "city",
    "old_value": "Beijing",
    "new_value": "Shanghai"
  }
}
```

#### 3.7.6 系统 Agent 的能力边界

| 能力 | 说明 | 类比 Android |
|------|------|-------------|
| 身份管理 | 用户姓名、语言、时区、城市等 | Contacts / Settings |
| 偏好管理 | 回复风格、默认模型等 | Settings |
| 帮助与导航 | "我该怎么用？"、"你能做什么？" | Settings 的帮助页 |
| Agent 推荐 | 根据用户需求推荐安装新 Agent | Play Store 的推荐 |
| 默认交互 | 无第三方 Agent 时的用户入口 | Launcher |

系统 Agent 只做"系统级"的事，每个具体领域的能力留给专门的 Agent。

#### 3.7.7 对架构的简化效果

去掉公共 Grafeo、引入系统 Agent 后，Gateway 彻底回归"纯基础设施"定位：

| 之前（公共 Grafeo） | 现在（系统 Agent） |
|---|---|
| Gateway 维护 Grafeo 实例 | Gateway 不维护任何数据库 |
| Gateway 提供 SharedMemory API | Gateway 只做 Intent 路由 |
| Gateway 管理只读视图、写入权限 | 权限交给系统 Agent 自治 |
| Agent 提报 → Gateway 仲裁 → 用户确认 | Agent 提报 → 系统 Agent LLM 判断 |
| 需要确认策略配置 | LLM 推理替代策略配置 |
| Gateway 承担业务逻辑 | Gateway 纯基础设施，零业务逻辑 |

连系统级服务本身也是一个 Agent——这才是 Agent as APP 模型最自洽的设计。

## 4. 安全设计

### 4.1 进程隔离

- 每个 Agent 独立进程，一个崩溃不影响其他。
- Agent Runtime 是平台信任的二进制，.agent 包无可执行代码。

### 4.2 文件系统隔离

- Agent 只能写入自己的工作区目录和用户明确授权的目录。
- 私有 Grafeo 文件在工作区内，沙箱层面强制隔离。

### 4.3 网络隔离

- 默认禁止网络（bwrap `--unshare-net`），仅按 manifest 授权域名配置代理或白名单。
- LLM API 调用需要 `network:https://api.openai.com` 等显式声明。

### 4.4 权限最小化

- manifest 必须声明所有权限，用户安装时可拒绝。
- 运行时权限请求：Agent 可通过 Gateway 请求额外权限，Gateway 弹出对话框让用户确认。

```json
{
  "type": "permission_request",
  "permission": "filesystem:read:~/Downloads",
  "reason": "需要读取下载的 CSV 文件进行分析"
}
```

### 4.5 API Key 安全

- Key 集中存储在 Gateway Vault（加密）。
- 不通过环境变量分发（避免 ps/procfs 泄露）。
- Agent Runtime 启动后通过 Unix Socket 一次性获取，存于进程内存。
- Agent Runtime 是可信二进制，.agent 包无可执行代码，WASM 工具在沙箱内无法读取宿主内存。

### 4.6 WASM 工具沙箱

- 自定义工具以 WASM 形式运行在 Wasmtime 沙箱中。
- 天然内存隔离、系统调用限制、资源限制（max_memory_mb, max_execution_time_ms）。
- 无法访问宿主进程内存、文件系统、网络。

### 4.7 沙箱强化

- Linux：seccomp-bpf 限制危险系统调用（clone、ptrace 等）。
- bubblewrap 提供文件系统级隔离。

### 4.8 Prompt Injection 防护

- Agent Runtime 内置 Prompt Guard（参考 ZeroClaw），检测和过滤可疑输入。
- 高风险工具执行（文件写入、网络请求、Intent 发送）需用户确认（approval 机制）。
- 审计日志：Agent 发出的所有工具调用和 Intent 都被记录和可回溯。

### 4.9 Memory 传输加密

- 云端同步使用 HTTPS / gRPC TLS。
- 本地 Grafeo 文件可选加密（使用用户密钥派生）。

---

## 5. 实现路线图

### Phase 1: 基础框架 + LLM 交互（MVP）

- 定义 manifest v1 规范，实现 ZIP 解析。
- 实现 Agent Runtime 核心：加载 .agent 包、组装 prompt、LLM 主循环、内置工具（memory, http, shell）。
- Gateway 基础功能：安装、卸载、启动/停止进程、Unix Socket 通信。
- Key Vault 基础功能：加密存储、一次性分发。
- 实现一个示例 Agent（天气查询，能调 LLM + 用工具）。
- 本地目录隔离（不使用命名空间，仅 `--work-dir`）。

### Phase 2: Memory 分层 + 系统 Agent

- Agent Runtime 内嵌 Grafeo：私有 Memory 初始化、情景记忆写入/检索、语义记忆图操作。
- 系统 Agent 实现：身份管理 ContentProvider、默认交互入口、身份提报接收与 LLM 判断、observe 通知机制。
- 冷启动身份注入：Gateway 启动 Agent 前向系统 Agent 查询 identity_deps 并注入。
- Embedding 生成：集成 ONNX Runtime，本地向量生成。
- 离线工作：所有 Memory 操作本地完成，不依赖网络。

### Phase 3: 权限与沙箱

- 集成 bubblewrap（Linux）实现文件系统隔离。
- 实现权限声明和用户授权对话框（CLI 或简单 GUI）。
- 运行时权限请求机制。
- 资源限制（cgroups 或 rlimit）。
- WASM 工具沙箱（Wasmtime 集成）。
- Prompt Guard 和 approval 机制。

### Phase 4: 通信与协调

- Intent 跨 Agent 消息转发 + Capability Registry。
- Budget Tracker（用量上报 + 超限信号 + 本地预检）。
- Rate Limiter（速率令牌分配）。
- 定时触发器（cron 解析）。

### Phase 5: 云端与生态

- Memory Sync Service（云端增量同步、冲突解决）。
- 远程仓库支持（添加仓库、更新、自动下载）。
- 图形化管理界面（Tauri 或 egui）。
- Agent 商店原型。

### Phase 6: 跨平台适配

- Windows 适配：Named Pipe 传输、Job Object 隔离、Windows 路径规范。
- macOS 适配：App Sandbox 隔离、macOS 路径规范。
- 移动端适配（Android/iOS）：SingleProcess 运行模式、Local TCP 传输、wasmi WASM 引擎、移动端路径规范。
- 注意：.agent 包格式和 Gateway Service API 合同无需修改，适配仅在实现层。

---

## 6. 使用场景示例

**场景**：用户安装"天气 Agent"和"日历 Agent"。每天早上 7 点，天气 Agent 自动获取当地天气，并通过 Intent 调用日历 Agent 创建提醒"今天带伞"。

**流程：**

1. Gateway 的 cron 触发器 spawn 天气 Agent 的 Agent Runtime 进程（若未运行）。启动前，Gateway 先向系统 Agent 查询 identity_deps，将用户身份信息注入启动参数。
2. Agent Runtime 加载天气 Agent 的 .agent 包，从 Vault 获取 API Key，连接 Gateway Socket。
3. 天气 Agent 从私有 Grafeo 读取用户上次保存的城市（情景记忆），调 LLM 规划查询天气。
4. LLM 返回 tool_call: `http_get("https://api.weather.com/...?city=Beijing")`，权限校验通过，执行。
5. 天气 Agent 将结果写入私有 Grafeo（情景 + 语义记忆）。
6. 天气 Agent 通过 Gateway 发送 Intent：

```json
{"type":"intent", "target":"com.example.calendar", "action":"create_event", "params":{"summary":"带伞","time":"07:30"}}
```

7. Gateway 查找日历 Agent，若未运行则 spawn（同样先注入身份信息），转发 Intent。
8. 日历 Agent 的 Agent Runtime 加载包、接收 Intent、调 LLM 处理。
9. 日历 Agent 调用本地日历 API 创建事件，返回成功。
10. Gateway 将响应返回给天气 Agent（可选），天气 Agent 空闲超时后被杀死。

**补充场景：身份信息更新**

用户对天气 Agent 说"我搬到上海了"：
1. 天气 Agent 从对话中识别到居住地变更，向系统 Agent 发送身份更新 Intent：`{"action": "identity:update", "params": {"updates": {"city": "Shanghai"}, "evidence": "用户说我搬到上海了", "confidence": 0.9}}`
2. 系统 Agent 的 LLM 二次判断确认"搬家"语义，更新私有 Grafeo 中的用户城市。
3. 系统 Agent 通知所有订阅了 city 变更的 Agent（如日历 Agent），日历 Agent 更新本地缓存。

---

## 7. 与现有方案对比

| 特性 | Agent as APP (Rollball) | ZeroClaw | OpenClaw | Docker + 微服务 |
|------|------------------------|----------|----------|----------------|
| 隔离级别 | 进程 + 沙箱 + WASM | 单进程内逻辑隔离 | 单进程内逻辑隔离 | 操作系统容器 |
| 执行模型 | 统一 Runtime + 声明式包 | 单体二进制 | Node.js 进程 | 容器镜像 |
| 资源开销 | 极低（空闲可杀死） | 低（常驻 ~5MB） | 中（Node.js 常驻） | 较高 |
| Memory | 分层 Grafeo（私有+系统Agent+云端） | SQLite/PG/Markdown | ContextEngine | 外部数据库 |
| LLM 集成 | Agent 直连 + Gateway 协调 | 内置 Provider | 内置 Provider | 各服务自连 |
| 分发模型 | 应用商店式 .agent 包 | 代码库/配置 | 代码库/配置 | 镜像仓库 |
| 跨Agent通信 | Intent + Capability Registry | 无（单 Agent） | 无（单 Agent） | HTTP/gRPC |
| 适用规模 | 个人/小团队 | 单 Agent 场景 | 单 Agent 场景 | 任意（较重） |

---

## 8. 未来扩展

- **Agent 商店**：建立公开仓库，用户可一键安装，含评分和评论。
- **付费 Agent**：支持许可证验证（Gateway 内集成）。
- **联邦 Memory**：多个用户之间的 Memory 共享（需授权）。
- **Agent 组合**：多个 Agent 编排为工作流（DAG 调度）。
- **多模态 Agent**：支持图像、音频、视频输入输出的 Agent。
- **移动端深度适配**：Android 多进程 Service 架构、iOS App Extension 集成、移动端 UI 交互优化。

---
